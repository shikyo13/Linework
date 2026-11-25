//! A kept copy of each project's text, so a folder search opens only the
//! files that have changed since the last one.
//!
//! On macOS every open of a file passes through one lock shared by the whole
//! system, which gives the machine about 150,000 opens a second however many
//! threads ask; opening is two thirds of a folder search. The walk already
//! learns each file's size and times from the folder listing, without opening
//! it. A file whose size, modified time and change time are those of its
//! record here is answered with the bytes kept: exactly what reading it would
//! give. Any other file is read as before, and its bytes are kept for next time.
//!
//! One store per project (the nearest folder above with a `.git`, else the
//! folder searched): an index and the text files its records point into, all
//! mapped into memory, so a lookup is a hash and a compare and nothing is
//! copied. An update is written beside the old store, flushed to the disk and
//! swapped in whole; a search running meanwhile goes on reading the old one.
//!
//! Not kept: files over 4 MiB, and files changed in the last two seconds, so
//! that a file still being written is never trusted.
use std::{
    collections::HashSet,
    ffi::OsStr,
    fs::{self, File},
    io::{self, BufWriter, Write},
    os::unix::{ffi::OsStrExt, fs::FileExt, io::AsRawFd},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};

const MAGIC: &[u8; 8] = b"LWKEPT01";
/// A file larger than this is not kept.
pub const MOST: u64 = 4 << 20;
/// A file changed more recently than this (nanoseconds) is not kept yet.
const SETTLE: u64 = 2_000_000_000;
/// Space of replaced files is reclaimed when it passes this much and half the store.
const WASTE: u64 = 64 << 20;
const HEADER: usize = 56;
const RECORD: usize = 56;

/// What a record's bytes are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// The whole file.
    Whole = 1,
    /// The first bytes of a larger file whose head holds a zero byte: what a
    /// search reads of it unless it searches binary files.
    Head = 2,
}

static HOME: OnceLock<PathBuf> = OnceLock::new();

/// Turns the kept copy on for this process, with its stores under `folder`.
pub fn keep_in(folder: &Path) {
    let _ = HOME.set(folder.to_path_buf());
}

fn mix(bytes: &[u8], seed: u64) -> u64 {
    let mut h = seed ^ (bytes.len() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        h = (h.rotate_left(5) ^ u64::from_le_bytes(chunk.try_into().unwrap())).wrapping_mul(0x517C_C1B7_2722_0A95);
    }
    let mut last = [0u8; 8];
    let rest = chunks.remainder();
    last[..rest.len()].copy_from_slice(rest);
    h = (h.rotate_left(5) ^ u64::from_le_bytes(last)).wrapping_mul(0x517C_C1B7_2722_0A95);
    h ^ (h >> 32)
}

/// A file mapped into memory to be read.
struct Map {
    at: *const u8,
    len: usize,
}

unsafe impl Send for Map {}
unsafe impl Sync for Map {}

impl Map {
    fn of(file: &File) -> io::Result<Self> {
        let len = file.metadata()?.len() as usize;
        if len == 0 {
            return Ok(Self { at: std::ptr::NonNull::<u8>::dangling().as_ptr(), len: 0 });
        }
        let at = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, file.as_raw_fd(), 0) };
        if at == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { at: at.cast(), len })
    }

    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.at, self.len) }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        if self.len != 0 {
            unsafe { libc::munmap(self.at as *mut libc::c_void, self.len) };
        }
    }
}

#[derive(Clone, Copy)]
struct Record {
    name_at: u64,
    name_len: u32,
    text: u32,
    at: u64,
    len: u64,
    size: u64,
    modified: u64,
    changed: u64,
    kind: u32,
}

impl Record {
    fn read(bytes: &[u8]) -> Self {
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        // The kind rides in the top bits of the name length: names are far shorter than 2^28 bytes.
        let named = u32_at(8);
        Self { name_at: u64_at(0), name_len: named & ((1 << 28) - 1), text: u32_at(12), at: u64_at(16), len: u64_at(24), size: u64_at(32), modified: u64_at(40), changed: u64_at(48), kind: named >> 28 }
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.name_at.to_le_bytes());
        out.extend_from_slice(&(self.name_len | (self.kind << 28)).to_le_bytes());
        out.extend_from_slice(&self.text.to_le_bytes());
        for value in [self.at, self.len, self.size, self.modified, self.changed] {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
}

/// The store as one search reads it.
struct Shelf {
    index: Map,
    count: usize,
    slots: usize,
    records_at: usize,
    table_at: usize,
    names_at: usize,
    /// Each text file as mapped, and how many bytes of it the index speaks for.
    texts: Vec<Option<(Map, u64)>>,
}

impl Shelf {
    /// The store in `folder`, or None when there is none or it is not whole.
    fn open(folder: &Path) -> Option<Self> {
        let index = Map::of(&File::open(folder.join("index")).ok()?).ok()?;
        let bytes = index.bytes();
        if bytes.len() < HEADER || &bytes[..8] != MAGIC {
            return None;
        }
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let (count, slots, names_len, text_count, sum) = (u64_at(16) as usize, u64_at(24) as usize, u64_at(32) as usize, u64_at(40) as usize, u64_at(48));
        let lens_at = HEADER;
        let records_at = lens_at.checked_add(text_count.checked_mul(8)?)?;
        let table_at = records_at.checked_add(count.checked_mul(RECORD)?)?;
        let names_at = table_at.checked_add(slots.checked_mul(4)?)?;
        if !slots.is_power_of_two() || names_at.checked_add(names_len)? != bytes.len() || mix(&bytes[HEADER..], 0) != sum {
            return None;
        }
        let mut texts = Vec::with_capacity(text_count);
        for number in 0..text_count {
            let len = u64_at(lens_at + number * 8);
            if len == 0 {
                texts.push(None);
                continue;
            }
            let map = Map::of(&File::open(folder.join(format!("text-{number}"))).ok()?).ok()?;
            if (map.len as u64) < len {
                return None;
            }
            texts.push(Some((map, len)));
        }
        Some(Self { index, count, slots, records_at, table_at, names_at, texts })
    }

    fn record(&self, number: usize) -> Record {
        let at = self.records_at + number * RECORD;
        Record::read(&self.index.bytes()[at..at + RECORD])
    }

    fn name(&self, record: &Record) -> &[u8] {
        let at = self.names_at + record.name_at as usize;
        &self.index.bytes()[at..at + record.name_len as usize]
    }

    fn find(&self, name: &[u8]) -> Option<Record> {
        let bytes = self.index.bytes();
        let mask = self.slots - 1;
        let mut slot = mix(name, 1) as usize & mask;
        loop {
            let at = self.table_at + slot * 4;
            let number = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            if number == 0 {
                return None;
            }
            let record = self.record(number - 1);
            if self.name(&record) == name {
                return Some(record);
            }
            slot = (slot + 1) & mask;
        }
    }

    fn bytes(&self, record: &Record) -> Option<&[u8]> {
        let (map, len) = self.texts.get(record.text as usize)?.as_ref()?;
        let end = record.at.checked_add(record.len)?;
        (end <= *len).then(|| &map.bytes()[record.at as usize..end as usize])
    }
}

/// The kept copy of one project, for one walk of a folder in it.
pub struct Kept {
    folder: PathBuf,
    /// The project's top folder, as the system names it.
    top: Vec<u8>,
    /// The walked folder as named from the project's top, with a final `/`; empty for the top itself.
    prefix: Vec<u8>,
    shelf: Option<Shelf>,
    /// For naming the files each thread writes.
    writers: AtomicU64,
}

/// What one thread found changed, with the new bytes already written to a file of its own.
#[derive(Default)]
pub struct Fresh {
    names: Vec<u8>,
    records: Vec<Record>,
    file: Option<(BufWriter<File>, PathBuf)>,
    written: u64,
    /// The path as the store names it, rebuilt for each file.
    pub key: Vec<u8>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

impl Kept {
    /// The kept copy for a walk of `root`, when keeping is on.
    pub fn for_walk(root: &Path) -> Option<Self> {
        let home = HOME.get()?;
        let absolute = std::path::absolute(root).ok()?;
        let top = absolute.ancestors().find(|folder| folder.join(".git").exists()).unwrap_or(&absolute);
        let mut prefix = absolute.strip_prefix(top).ok()?.as_os_str().as_bytes().to_vec();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        let named = top.as_os_str().as_bytes();
        let folder = home.join(format!("{:016x}", mix(named, 2)));
        // The folder's own name is written beside the store; another folder with the same number is left alone.
        match fs::read(folder.join("root")) {
            Ok(kept) if kept != named => return None,
            _ => {}
        }
        let shelf = Shelf::open(&folder);
        Some(Self { folder, top: named.to_vec(), prefix, shelf, writers: AtomicU64::new(0) })
    }

    /// Puts a file's name as the store knows it into `key`.
    pub fn name_into(&self, path: &str, key: &mut Vec<u8>) {
        key.clear();
        key.extend_from_slice(&self.prefix);
        key.extend_from_slice(path.as_bytes());
    }

    /// The bytes a read of the file would give, when the file is as it was
    /// when they were kept. With `whole` the caller wants every byte of a file
    /// even if its head holds a zero byte, which a kept head cannot answer.
    pub fn bytes(&self, key: &[u8], size: u64, modified: u64, changed: u64, whole: bool) -> Option<&[u8]> {
        let shelf = self.shelf.as_ref()?;
        let record = shelf.find(key)?;
        if record.size != size || record.modified != modified || record.changed != changed || (whole && record.kind == Kind::Head as u32) {
            return None;
        }
        shelf.bytes(&record)
    }

    /// Whether the file as it is now already has a record, whatever its kind:
    /// a search of binary files reads past a kept head every time, and must
    /// not keep the same head again.
    pub fn has(&self, key: &[u8], size: u64, modified: u64, changed: u64) -> bool {
        self.shelf.as_ref().and_then(|shelf| shelf.find(key)).is_some_and(|r| r.size == size && r.modified == modified && r.changed == changed)
    }

    /// Keeps the bytes just read of a file. `bytes` must be all of it, or its
    /// first `head` bytes when it is larger and they hold a zero byte.
    #[allow(clippy::too_many_arguments)]
    pub fn keep(&self, fresh: &mut Fresh, size: u64, modified: u64, changed: u64, bytes: &[u8], kind: Kind) {
        if size > MOST || modified == 0 || now().saturating_sub(modified.max(changed)) < SETTLE {
            return;
        }
        if fresh.file.is_none() {
            let number = self.writers.fetch_add(1, Ordering::Relaxed);
            let path = self.folder.join(format!("new-{}-{number}", std::process::id()));
            let Ok(file) = fs::create_dir_all(&self.folder).and_then(|()| File::create(&path)) else { return };
            fresh.file = Some((BufWriter::with_capacity(1 << 20, file), path));
        }
        let Some((writer, _)) = fresh.file.as_mut() else { return };
        if writer.write_all(bytes).is_err() {
            return;
        }
        let name_at = fresh.names.len() as u64;
        fresh.names.extend_from_slice(&fresh.key);
        fresh.records.push(Record { name_at, name_len: fresh.key.len() as u32, text: 0, at: fresh.written, len: bytes.len() as u64, size, modified, changed, kind: kind as u32 });
        fresh.written += bytes.len() as u64;
    }

    /// Joins what the threads kept into the store. Nothing here can fail a
    /// search: on any error the new files are removed and the store stays as it was.
    pub fn settle(self, fresh: Vec<Fresh>) {
        let mut fresh: Vec<Fresh> = fresh.into_iter().filter(|f| f.file.is_some()).collect();
        if fresh.is_empty() {
            return;
        }
        let new_files: Vec<PathBuf> = fresh.iter().filter_map(|f| f.file.as_ref().map(|(_, path)| path.clone())).collect();
        if self.join(&mut fresh).is_err() {
            for path in new_files {
                let _ = fs::remove_file(path);
            }
        }
    }

    fn join(&self, fresh: &mut [Fresh]) -> io::Result<()> {
        // One updater at a time; a second one gives up and keeps nothing this run.
        let lock = File::create(self.folder.join("lock"))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::other("another search is updating the store"));
        }
        fs::write(self.folder.join("root"), &self.top)?;
        // The store as it is now: another search may have updated it since this one began.
        let base = Shelf::open(&self.folder);
        let mut lens: Vec<u64> = base.as_ref().map_or_else(Vec::new, |b| b.texts.iter().map(|t| t.as_ref().map_or(0, |(_, len)| *len)).collect());
        let mut names: Vec<u8> = Vec::new();
        let mut records: Vec<Record> = Vec::new();
        let mut live = vec![0u64; lens.len()];
        {
            let replaced: HashSet<&[u8]> =
                fresh.iter().flat_map(|f| f.records.iter().map(move |r| &f.names[r.name_at as usize..r.name_at as usize + r.name_len as usize])).collect();
            if let Some(base) = &base {
                for number in 0..base.count {
                    let mut record = base.record(number);
                    let name = base.name(&record);
                    if replaced.contains(name) || base.bytes(&record).is_none() {
                        continue;
                    }
                    live[record.text as usize] += record.len;
                    record.name_at = names.len() as u64;
                    names.extend_from_slice(name);
                    records.push(record);
                }
            }
        }
        // Each thread's file becomes a text file of the store.
        let mut renames = Vec::new();
        for f in fresh.iter_mut() {
            let Some((writer, path)) = f.file.take() else { continue };
            let file = writer.into_inner().map_err(|e| e.into_error())?;
            flush(&file)?;
            let number = lens.len();
            lens.push(f.written);
            live.push(f.written);
            renames.push((path, self.folder.join(format!("text-{number}"))));
            for record in &f.records {
                let name = &f.names[record.name_at as usize..record.name_at as usize + record.name_len as usize];
                records.push(Record { name_at: names.len() as u64, text: number as u32, ..*record });
                names.extend_from_slice(name);
            }
        }
        for (from, to) in &renames {
            fs::rename(from, to)?;
        }
        // Text files nothing points into any more are dropped; when replaced
        // bytes pass half the store, what is live is gathered into one file.
        let total: u64 = lens.iter().sum();
        let alive: u64 = live.iter().sum();
        let mut retired: Vec<usize> = (0..lens.len()).filter(|&n| lens[n] != 0 && live[n] == 0).collect();
        for &number in &retired {
            lens[number] = 0;
        }
        if total - alive > WASTE.max(total / 2) || lens.iter().filter(|&&len| len != 0).count() > 32 {
            let gathered = self.gather(&mut records, &names, &mut lens)?;
            retired.extend(gathered);
        }
        self.write_index(&records, &names, &lens)?;
        drop(base);
        for number in retired {
            let _ = fs::remove_file(self.folder.join(format!("text-{number}")));
        }
        Ok(())
    }

    /// Copies every live record's bytes into one new text file, and leaves out
    /// records of files that are gone. Returns the text files it emptied.
    fn gather(&self, records: &mut Vec<Record>, names: &[u8], lens: &mut Vec<u64>) -> io::Result<Vec<usize>> {
        let maps: Vec<Option<Map>> = (0..lens.len())
            .map(|n| if lens[n] == 0 { None } else { File::open(self.folder.join(format!("text-{n}"))).ok().and_then(|f| Map::of(&f).ok()) })
            .collect();
        let number = lens.len();
        let path = self.folder.join(format!("text-{number}"));
        let mut writer = BufWriter::with_capacity(1 << 20, File::create(&path)?);
        let mut written = 0u64;
        let top = Path::new(OsStr::from_bytes(&self.top));
        records.retain_mut(|record| {
            let name = &names[record.name_at as usize..record.name_at as usize + record.name_len as usize];
            let Some(map) = maps.get(record.text as usize).and_then(Option::as_ref) else { return false };
            let (from, to) = (record.at as usize, (record.at + record.len) as usize);
            if to > map.len || !top.join(OsStr::from_bytes(name)).exists() || writer.write_all(&map.bytes()[from..to]).is_err() {
                return false;
            }
            record.text = number as u32;
            record.at = written;
            written += record.len;
            true
        });
        flush(&writer.into_inner().map_err(|e| e.into_error())?)?;
        let emptied: Vec<usize> = (0..lens.len()).filter(|&n| lens[n] != 0).collect();
        for &n in &emptied {
            lens[n] = 0;
        }
        lens.push(written);
        Ok(emptied)
    }

    fn write_index(&self, records: &[Record], names: &[u8], lens: &[u64]) -> io::Result<()> {
        let slots = (records.len() * 2).max(16).next_power_of_two();
        let mut table = vec![0u32; slots];
        for (number, record) in records.iter().enumerate() {
            let name = &names[record.name_at as usize..record.name_at as usize + record.name_len as usize];
            let mut slot = mix(name, 1) as usize & (slots - 1);
            while table[slot] != 0 {
                slot = (slot + 1) & (slots - 1);
            }
            table[slot] = number as u32 + 1;
        }
        let mut body = Vec::with_capacity(lens.len() * 8 + records.len() * RECORD + slots * 4 + names.len());
        for len in lens {
            body.extend_from_slice(&len.to_le_bytes());
        }
        for record in records {
            record.write(&mut body);
        }
        for slot in &table {
            body.extend_from_slice(&slot.to_le_bytes());
        }
        body.extend_from_slice(names);
        let mut head = Vec::with_capacity(HEADER);
        head.extend_from_slice(MAGIC);
        for value in [now(), records.len() as u64, slots as u64, names.len() as u64, lens.len() as u64, mix(&body, 0)] {
            head.extend_from_slice(&value.to_le_bytes());
        }
        let path = self.folder.join(format!("index-{}", std::process::id()));
        let file = File::create(&path)?;
        file.write_all_at(&head, 0)?;
        file.write_all_at(&body, HEADER as u64)?;
        flush(&file)?;
        fs::rename(&path, self.folder.join("index"))
    }
}

/// Has the system put a file's bytes on the disk itself, so that a store swapped in is whole even after a power cut.
fn flush(file: &File) -> io::Result<()> {
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == 0 {
        return Ok(());
    }
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(folder: &Path, top: &Path) -> Kept {
        Kept { folder: folder.to_path_buf(), top: top.as_os_str().as_bytes().to_vec(), prefix: Vec::new(), shelf: Shelf::open(folder), writers: AtomicU64::new(0) }
    }

    fn keep(kept: &Kept, fresh: &mut Fresh, name: &str, size: u64, stamp: u64, bytes: &[u8], kind: Kind) {
        kept.name_into(name, &mut fresh.key);
        kept.keep(fresh, size, stamp, stamp, bytes, kind);
    }

    #[test]
    fn kept_bytes_come_back_only_for_the_file_as_it_was() {
        let dir = std::env::temp_dir().join(format!("lw-kept-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (folder, top) = (dir.join("store"), dir.join("top"));
        fs::create_dir_all(&top).unwrap();
        for name in ["a.txt", "b.txt", "big.bin"] {
            fs::write(top.join(name), b"x").unwrap();
        }
        // Nothing is kept yet.
        let kept = store(&folder, &top);
        assert!(kept.bytes(b"a.txt", 5, 1000, 1000, false).is_none());
        let mut fresh = Fresh::default();
        keep(&kept, &mut fresh, "a.txt", 5, 1000, b"alpha", Kind::Whole);
        keep(&kept, &mut fresh, "b.txt", 4, 2000, b"beta", Kind::Whole);
        keep(&kept, &mut fresh, "big.bin", 99_999, 3000, b"he\0ad", Kind::Head);
        // Too large, and changed this instant: neither is kept.
        keep(&kept, &mut fresh, "huge.txt", MOST + 1, 1000, b"huge", Kind::Whole);
        keep(&kept, &mut fresh, "young.txt", 5, now(), b"young", Kind::Whole);
        kept.settle(vec![fresh]);

        let kept = store(&folder, &top);
        assert_eq!(kept.bytes(b"a.txt", 5, 1000, 1000, false), Some(&b"alpha"[..]));
        assert_eq!(kept.bytes(b"b.txt", 4, 2000, 2000, true), Some(&b"beta"[..]));
        // Another size, modified time or change time is another file.
        assert!(kept.bytes(b"a.txt", 6, 1000, 1000, false).is_none());
        assert!(kept.bytes(b"a.txt", 5, 1001, 1000, false).is_none());
        assert!(kept.bytes(b"a.txt", 5, 1000, 1001, false).is_none());
        // A kept head answers a search that stops at a zero byte, not one that reads on.
        assert_eq!(kept.bytes(b"big.bin", 99_999, 3000, 3000, false), Some(&b"he\0ad"[..]));
        assert!(kept.bytes(b"big.bin", 99_999, 3000, 3000, true).is_none());
        assert!(kept.has(b"big.bin", 99_999, 3000, 3000));
        assert!(kept.bytes(b"huge.txt", MOST + 1, 1000, 1000, false).is_none());
        assert!(kept.bytes(b"young.txt", 5, 0, 0, false).is_none());
        assert!(kept.bytes(b"missing.txt", 5, 1000, 1000, false).is_none());

        // A changed file replaces its record; the others stay.
        let mut fresh = Fresh::default();
        keep(&kept, &mut fresh, "a.txt", 7, 5000, b"alpha 2", Kind::Whole);
        kept.settle(vec![fresh]);
        let kept = store(&folder, &top);
        assert_eq!(kept.bytes(b"a.txt", 7, 5000, 5000, false), Some(&b"alpha 2"[..]));
        assert!(kept.bytes(b"a.txt", 5, 1000, 1000, false).is_none());
        assert_eq!(kept.bytes(b"b.txt", 4, 2000, 2000, false), Some(&b"beta"[..]));

        // Gathering copies what is live into one file and forgets files that are gone.
        fs::remove_file(top.join("b.txt")).unwrap();
        let shelf = kept.shelf.as_ref().unwrap();
        let mut names = Vec::new();
        let mut records: Vec<Record> = (0..shelf.count)
            .map(|n| {
                let mut record = shelf.record(n);
                let name = shelf.name(&record);
                record.name_at = names.len() as u64;
                names.extend_from_slice(name);
                record
            })
            .collect();
        let mut lens: Vec<u64> = shelf.texts.iter().map(|t| t.as_ref().map_or(0, |(_, len)| *len)).collect();
        let emptied = kept.gather(&mut records, &names, &mut lens).unwrap();
        kept.write_index(&records, &names, &lens).unwrap();
        for number in emptied {
            fs::remove_file(folder.join(format!("text-{number}"))).unwrap();
        }
        let kept = store(&folder, &top);
        assert_eq!(kept.bytes(b"a.txt", 7, 5000, 5000, false), Some(&b"alpha 2"[..]));
        assert_eq!(kept.bytes(b"big.bin", 99_999, 3000, 3000, false), Some(&b"he\0ad"[..]));
        assert!(kept.bytes(b"b.txt", 4, 2000, 2000, false).is_none());
        assert_eq!(fs::read_dir(&folder).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with("text-")).count(), 1);

        // A store cut short is not a store: nothing is answered from it.
        let index = fs::read(folder.join("index")).unwrap();
        fs::write(folder.join("index"), &index[..index.len() - 3]).unwrap();
        assert!(store(&folder, &top).bytes(b"a.txt", 7, 5000, 5000, false).is_none());
        let mut damaged = index.clone();
        let last = damaged.len() - 1;
        damaged[last] ^= 1;
        fs::write(folder.join("index"), &damaged).unwrap();
        assert!(store(&folder, &top).bytes(b"a.txt", 7, 5000, 5000, false).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
