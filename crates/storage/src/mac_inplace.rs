//! A small change written straight into a large file, as Windows does it.
//!
//! The file stays the same file: programs that hold it open see the change,
//! what the system has of it in memory stays, and its name never points at
//! anything else. A save by clone (`mac_save.rs`) gives none of these.
//!
//! The price is that a crash in the middle of the write could leave some
//! bytes new and some old. So the bytes about to be replaced are recorded
//! first, and a record left behind by a crash is used at the next start to
//! put the file back as it was.
//!
//! One save has to wait once for the drive to empty its cache, about four
//! milliseconds on this hardware, and that wait is nearly all of the save.
//! The record does not need a second such wait. It has to reach the drive
//! before the change does, which a barrier ensures at a seventh of the
//! cost, and it is written into a file kept for the purpose, since making
//! and removing a file each time measured most of a millisecond more.
//!
//! Other parts of the document are not disturbed by the write: every piece
//! that came from the file reads a private clone of it, and writing to a
//! file puts the written part in new blocks when a clone shares the old.
use crate::{conflict, mac_save::Examined, tree::shared_bytes, Document, Stamp};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{FileExt, MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

static FOLDER: RwLock<Option<PathBuf>> = RwLock::new(None);
static SERIAL: AtomicU64 = AtomicU64::new(0);
/// Record files this process has made and is not using for a save just now.
static FREE: Mutex<Vec<Slot>> = Mutex::new(Vec::new());
const MAGIC: &[u8; 8] = b"LWSAVM01";
const EXTENSION: &str = "save";
const FIXED: usize = 8 + 4 + 8 + 8 + 16 + 8 + 8 + 4 + 4 + 4;
/// The most one in-place save replaces or writes, as on Windows.
const LARGEST: usize = 1024 * 1024;
/// Files up to this size are held in memory and saved whole.
const SMALL: u64 = 24 * 1024 * 1024;
/// Past this much of the file read at each save to know it unchanged, a new
/// clone is taken, after which nothing has to be read again.
const RENEW: u64 = 16 * 1024 * 1024;

/// Where records are kept. Setting a folder also settles any record a crashed
/// session left there, so call it before opening files. With no folder, a
/// small change to a large file is saved by clone.
pub fn set_save_journal(folder: Option<&Path>) {
    *FOLDER.write().unwrap() = folder.map(Path::to_owned);
    if let Some(folder) = folder {
        replay(folder);
    }
}

#[cfg(test)]
thread_local! {
    /// Lets one test choose the journal for its own saves while the tests on
    /// other threads save by clone.
    pub(crate) static FOR_THIS_THREAD: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn folder() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(folder) = FOR_THIS_THREAD.with(|folder| folder.borrow().clone()) {
        return Some(folder);
    }
    FOLDER.read().unwrap().clone()
}

struct Record {
    target: PathBuf,
    old: Stamp,
    offset: u64,
    new_len: u64,
    before: Vec<u8>,
    after: Vec<u8>,
}

fn encode(target: &Path, old: &Stamp, offset: u64, before: &[u8], after: &[u8], new_len: u64) -> Vec<u8> {
    let name = target.as_os_str().as_bytes();
    let mut out = Vec::with_capacity(FIXED + name.len() + before.len() + after.len() + 32);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&old.volume.to_le_bytes());
    out.extend_from_slice(&old.id.to_le_bytes());
    out.extend_from_slice(&old.size.to_le_bytes());
    out.extend_from_slice(&old.modified.to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&new_len.to_le_bytes());
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(&(before.len() as u32).to_le_bytes());
    out.extend_from_slice(&(after.len() as u32).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(before);
    out.extend_from_slice(after);
    let sum = Sha256::digest(&out);
    out.extend_from_slice(&sum);
    out
}

/// A record file is used again and again, so what follows a record in it
/// may be the end of a longer one: a record's length is the one it states.
fn decode(bytes: &[u8]) -> Option<Record> {
    if bytes.len() < FIXED + 32 || &bytes[..8] != MAGIC {
        return None;
    }
    let length = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
    let body = FIXED.checked_add(length(FIXED - 12))?.checked_add(length(FIXED - 8))?.checked_add(length(FIXED - 4))?;
    let sum = bytes.get(body..body.checked_add(32)?)?;
    if Sha256::digest(&bytes[..body])[..] != *sum {
        return None;
    }
    let mut at = 8;
    let mut take = |n: usize| {
        at += n;
        &bytes[at - n..at]
    };
    let volume = u32::from_le_bytes(take(4).try_into().ok()?);
    let id = u64::from_le_bytes(take(8).try_into().ok()?);
    let size = u64::from_le_bytes(take(8).try_into().ok()?);
    let modified = u128::from_le_bytes(take(16).try_into().ok()?);
    let offset = u64::from_le_bytes(take(8).try_into().ok()?);
    let new_len = u64::from_le_bytes(take(8).try_into().ok()?);
    let name = u32::from_le_bytes(take(4).try_into().ok()?) as usize;
    let before = u32::from_le_bytes(take(4).try_into().ok()?) as usize;
    let after = u32::from_le_bytes(take(4).try_into().ok()?) as usize;
    if before > LARGEST || after > LARGEST || name > 65_536 || FIXED + name + before + after != body {
        return None;
    }
    // No real file reaches the end of the offset range; later sums rely on it.
    offset.checked_add(before.max(after) as u64)?;
    Some(Record {
        target: OsString::from_vec(take(name).to_vec()).into(),
        old: Stamp { size, modified, id, volume },
        offset,
        new_len,
        before: take(before).to_vec(),
        after: take(after).to_vec(),
    })
}

/// A file that holds one record at a time, locked for as long as this
/// process has it open. The lock is how another Linework tells a record
/// whose save may still be running from one whose process died: the system
/// drops the lock with the process.
struct Slot {
    file: File,
    path: PathBuf,
    folder: PathBuf,
}

impl Slot {
    fn take(folder: &Path) -> io::Result<Self> {
        let mut free = FREE.lock().unwrap();
        // One whose file was removed with its folder would record nothing.
        free.retain(|slot| slot.file.metadata().is_ok_and(|status| status.nlink() != 0));
        if let Some(at) = free.iter().position(|slot| slot.folder == folder) {
            return Ok(free.swap_remove(at));
        }
        drop(free);
        fs::create_dir_all(folder)?;
        let clock = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let name = format!(
            "{:x}-{clock:x}-{:x}.{EXTENSION}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        );
        let path = folder.join(name);
        let file = OpenOptions::new().write(true).create_new(true).custom_flags(libc::O_EXLOCK).open(&path)?;
        // Making the file and locking it are two steps to the system: a
        // Linework just starting can come between them, find the file empty
        // and unlocked, and remove it. A record in a file with no name would
        // never be found, so such a file is not used.
        if file.metadata()?.nlink() == 0 {
            return Err(io::Error::other("The record file was removed as it was made"));
        }
        // The barrier each record relies on orders what is written into a
        // file. That the file exists has to be on the drive first, once.
        file.write_all_at(&[0; 8], 0)?;
        file.sync_all()?;
        Ok(Self { file, path, folder: folder.to_owned() })
    }
}

/// The record of one save in progress, already ahead of the change on its
/// way to the drive.
struct Pending {
    slot: Option<Slot>,
}

impl Pending {
    fn record(
        folder: &Path, target: &File, name: &Path, old: &Stamp, offset: u64, before: &[u8], after: &[u8], new_len: u64,
    ) -> io::Result<Self> {
        let slot = Slot::take(folder)?;
        let written = slot.file.write_all_at(&encode(name, old, offset, before, after, new_len), 0).and_then(|()| {
            // A barrier orders what goes to one drive. A record on another
            // drive than the file has to be there before the file is touched.
            let same_drive = slot.file.metadata()?.dev() == target.metadata()?.dev();
            let ordered = same_drive && unsafe { libc::fcntl(slot.file.as_raw_fd(), libc::F_BARRIERFSYNC) } == 0;
            if ordered { Ok(()) } else { slot.file.sync_all() }
        });
        match written {
            Ok(()) => Ok(Self { slot: Some(slot) }),
            Err(error) => {
                let _ = fs::remove_file(&slot.path);
                Err(error)
            }
        }
    }

    /// The file is in a whole state again, new or old: the record is not
    /// needed. It is struck out and sent to the drive without waiting for
    /// it, so that the barrier of whichever save comes next, in any process,
    /// puts the striking-out on the drive before that save's change. A
    /// record still standing there after a later save could match that
    /// save's bytes by chance and put superseded ones back.
    fn finish(mut self) {
        let Some(slot) = self.slot.take() else { return };
        let struck = slot.file.write_all_at(&[0; 8], 0).is_ok() && unsafe { libc::fsync(slot.file.as_raw_fd()) } == 0;
        if struck {
            FREE.lock().unwrap().push(slot);
        } else {
            let _ = fs::remove_file(&slot.path);
        }
    }

    /// The save succeeded, so the file is whole and is what its document
    /// holds. An older record for the same file may still be in the folder:
    /// one left by a failed write that could not be put back. Replayed after
    /// this save it could undo it, so it goes too.
    fn finish_and_retire(self, folder: &Path, now: &Stamp) {
        self.finish();
        for (path, bytes, _held) in records(folder) {
            if decode(&bytes).is_some_and(|r| r.old.volume == now.volume && r.old.id == now.id) {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

/// Every record file in the folder that no running process holds, with its
/// contents. This process's own are held, so they are not among them. Each
/// comes locked, and is to stay so until the caller has done with it: a file
/// let go could be taken up by a save before the caller removes it.
fn records(folder: &Path) -> Vec<(PathBuf, Vec<u8>, File)> {
    let Ok(entries) = fs::read_dir(folder) else { return Vec::new() };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(EXTENSION) {
            continue;
        }
        let Ok(file) = File::open(&path) else { continue };
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            continue;
        }
        let mut bytes = Vec::new();
        if (&file).take((FIXED + 65_536 + 2 * LARGEST + 32) as u64 + 1).read_to_end(&mut bytes).is_ok() {
            found.push((path, bytes, file));
        }
    }
    found
}

/// Put back the replaced bytes and, when `whole`, the length and the time,
/// through an open writer.
fn restore(file: &File, old: &Stamp, offset: u64, before: &[u8], whole: bool) -> io::Result<()> {
    file.write_all_at(before, offset)?;
    if !whole {
        return file.sync_all();
    }
    file.set_len(old.size)?;
    let nanos = old.modified;
    let time = UNIX_EPOCH + Duration::new((nanos / 1_000_000_000) as u64, (nanos % 1_000_000_000) as u32);
    file.set_modified(time)?;
    file.sync_all()
}

/// True when the record has done its job and can be deleted.
fn settle(record: &Record) -> bool {
    let file = match OpenOptions::new().read(true).write(true).open(&record.target) {
        Ok(file) => file,
        // The file itself is gone. An unreachable folder is tried again at
        // the next start.
        Err(error) => return error.kind() == io::ErrorKind::NotFound,
    };
    let Ok(now) = Stamp::from(&file) else { return false };
    if now.volume != record.old.volume || now.id != record.old.id {
        return true;
    }
    let widest = record.before.len().max(record.after.len()) as u64;
    let readable = now.size.saturating_sub(record.offset).min(widest) as usize;
    let mut current = vec![0; readable];
    if file.read_exact_at(&mut current, record.offset).is_err() {
        return false;
    }
    let holds = |bytes: &[u8], len: u64| now.size == len && current.len() >= bytes.len() && current[..bytes.len()] == *bytes;
    if holds(&record.after, record.new_len) || holds(&record.before, record.old.size) {
        return true;
    }
    // A write cut short leaves each byte either old or new, and a length that
    // is the old one, the new one, or where the write alone would have ended.
    // Past the old end of the file a zero is also ours: the length can reach
    // the drive before the added part does.
    // A file being lengthened by the write may have been caught anywhere
    // between its old end and where the write would have ended.
    let reached = record.old.size.max(record.offset + record.after.len() as u64);
    let growing = now.size > record.old.size && now.size <= reached;
    let torn = (growing || [record.old.size, record.new_len].contains(&now.size))
        && current.iter().enumerate().all(|(i, byte)| {
            record.before.get(i) == Some(byte)
                || record.after.get(i) == Some(byte)
                || (*byte == 0 && record.offset + i as u64 >= record.old.size)
        });
    // Anything else was changed by another program after the crash: leave it.
    !torn || restore(&file, &record.old, record.offset, &record.before, true).is_ok()
}

fn replay(folder: &Path) {
    for (path, bytes, _held) in records(folder) {
        // A record that does not check out was never finished, and the file is
        // only written after its record is complete and on its way.
        if decode(&bytes).is_none_or(|record| settle(&record)) {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Whether the file is on a disk of this computer. A file server applies its
/// own rules to locks and flushes, and none has been tried.
fn local(file: &File) -> bool {
    let mut volume: libc::statfs = unsafe { std::mem::zeroed() };
    unsafe { libc::fstatfs(file.as_raw_fd(), &mut volume) == 0 && volume.f_flags & libc::MNT_LOCAL as u32 != 0 }
}

impl Document {
    /// Save over the file this document was opened from by writing the one
    /// small part that changed into the file itself. Declines (`false`),
    /// before anything is touched, whatever it cannot do safely.
    pub(crate) fn save_in_place(&mut self, dest: &Path, expected: &Stamp) -> io::Result<bool> {
        let Some(journal) = folder() else {
            return Ok(false);
        };
        if self.path.as_deref() != Some(dest)
            || expected.size <= SMALL
            || !self.codec.native()
            || self.codec != self.saved_codec
            || self.emit_bom != self.saved_bom
        {
            return Ok(false);
        }
        let header = if self.emit_bom { self.codec.bom().len() as u64 } else { 0 };
        let old_len = self.saved.as_ref().map_or(0, |n| n.len);
        if header + old_len != expected.size {
            return Ok(false);
        }
        // The change: either a replacement of equal length somewhere, or all
        // of the file from its first changed byte on.
        let new_len = self.len();
        let start = shared_bytes(&self.saved, &self.root, false);
        let suffix = if old_len == new_len {
            shared_bytes(&self.saved, &self.root, true).min(old_len - start)
        } else {
            0
        };
        let removed = old_len - start - suffix;
        let added = new_len - start - suffix;
        if removed.max(added) > LARGEST as u64 {
            return Ok(false);
        }
        // Writing into the file must not change what any piece of this
        // document reads: each piece from a file has to read a private copy.
        let mut exposed = false;
        for root in [&self.saved, &self.root].into_iter().flatten() {
            root.pieces(&mut |source, _, _| {
                exposed |= source.on_disk() && source.settled().is_none();
                Ok(())
            })?;
        }
        if exposed {
            return Ok(false);
        }
        let Ok(file) = OpenOptions::new().read(true).write(true).open(dest) else {
            return Ok(false);
        };
        if Stamp::from(&file)? != *expected {
            return Err(conflict());
        }
        // Under another name too, the file would change there as well.
        let status = file.metadata()?;
        if status.nlink() != 1 || !status.is_file() || !local(&file) {
            return Ok(false);
        }
        self.check_original()?;
        // The clone the file is told from: the one kept since the last save,
        // or the one that protects the text as it was opened. Without one,
        // or where the volume does not say where blocks lie, the file is
        // saved by clone, which reads all of it and leaves a clone to go by.
        if self.saved_clone.is_none() {
            self.saved_clone = self.opened_reference(expected).map(Arc::new);
        }
        if self.saved_clone.is_none() {
            return Ok(false);
        }
        let read = match self.examine(&file, expected) {
            Examined::Untouched(read) => read,
            Examined::Changed => return Err(conflict()),
            Examined::Unknown => return Ok(false),
        };
        // All that differs between the file and the clone kept is what this
        // document wrote into the file since, which each save reads again.
        // When that has grown, a new clone takes over, shown to hold the
        // saved text before the file is written to. The first write after a
        // clone costs a millisecond on a large file, so not at every save.
        if read > RENEW {
            if let Some(renewed) = self.renewed_reference(&file, dest, expected) {
                self.saved_clone = Some(renewed);
            }
        }
        let mut before = vec![0; removed as usize];
        if let Some(saved) = &self.saved {
            saved.read(start, &mut before)?;
        }
        let after = self.read(start, added as usize)?;
        let offset = header + start;
        let mut previous = vec![0; before.len()];
        file.read_exact_at(&mut previous, offset)?;
        if previous != before {
            return Err(conflict());
        }
        let total = header + new_len;
        let next = if before == after && total == expected.size {
            expected.clone()
        } else {
            // Without a record there is no crash protection, and nothing has
            // been changed yet: let the save by clone take over.
            let Ok(pending) = Pending::record(&journal, &file, dest, expected, offset, &before, &after, total) else {
                return Ok(false);
            };
            // macOS keeps no other program from writing meanwhile. What is
            // checked last is what the file's length and dates show, the
            // moment before the write.
            if Stamp::from(&file)? != *expected {
                pending.finish();
                return Err(conflict());
            }
            let mut lengthened = false;
            let written = (|| {
                #[cfg(test)]
                tests::interruptible_write(dest, &file, offset, &after)?;
                file.write_all_at(&after, offset)?;
                if total != expected.size {
                    file.set_len(total)?;
                }
                // The one wait for the drive in this save.
                file.sync_all()?;
                let next = Stamp::from(&file)?;
                if next.volume != expected.volume || next.id != expected.id || next.size != total {
                    lengthened = true;
                    return Err(conflict());
                }
                Ok(next)
            })();
            match written {
                Ok(next) => {
                    pending.finish_and_retire(&journal, &next);
                    next
                }
                Err(error) => {
                    // Put the old bytes back now if the disk allows it. If it
                    // does not, the record stays and the next start tries
                    // again. A file another program lengthened during the
                    // write keeps that program's length and date: only the
                    // bytes written here are taken back.
                    if restore(&file, expected, offset, &before, !lengthened).is_ok() {
                        pending.finish();
                    }
                    return Err(error);
                }
            }
        };
        // The pieces that read this file where it lies are told it was this
        // document that wrote to it, and what it looks like now.
        for root in [&self.saved, &self.root].into_iter().flatten() {
            root.pieces(&mut |source, _, _| {
                source.wrote_in_place(&next);
                Ok(())
            })?;
        }
        self.stamp = Some(next);
        self.saved_disk_digest = None;
        self.saved = self.root.clone();
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, process::{Command, Stdio}, thread, time::Instant};

    // Only the sacrificial process gets these variables. It stops in the real
    // write path with half of the new bytes flushed to disk, and is then killed.
    const KILL_TARGET: &str = "LINEWORK_INPLACE_TEST_KILL_TARGET";
    const KILL_READY: &str = "LINEWORK_INPLACE_TEST_KILL_READY";
    const KILL_JOURNAL: &str = "LINEWORK_INPLACE_TEST_KILL_JOURNAL";
    const AT: usize = 9 * 1024 * 1024 + 5;

    pub(super) fn interruptible_write(path: &Path, file: &File, offset: u64, after: &[u8]) -> io::Result<()> {
        if env::var_os(KILL_TARGET).as_deref() != Some(path.as_os_str()) {
            return Ok(());
        }
        file.write_all_at(&after[..after.len() / 2], offset)?;
        file.sync_all()?;
        fs::write(env::var_os(KILL_READY).expect("missing crash-test signal path"), b"half written")?;
        thread::sleep(Duration::from_secs(30));
        Err(io::Error::new(io::ErrorKind::TimedOut, "Crash test did not terminate its child"))
    }

    fn stamp(path: &Path) -> Stamp {
        Stamp::get(path).unwrap().unwrap()
    }

    fn large(dir: &Path) -> io::Result<(PathBuf, Vec<u8>)> {
        let file = dir.join("large.log");
        let text = b"abcdefgh".repeat(25 * 1024 * 1024 / 8);
        fs::write(&file, &text)?;
        Ok((file.canonicalize()?, text))
    }

    #[test]
    fn half_writer_child() -> io::Result<()> {
        let Some(target) = env::var_os(KILL_TARGET) else { return Ok(()) };
        let path = Path::new(&target);
        FOR_THIS_THREAD.set(Some(env::var_os(KILL_JOURNAL).expect("missing crash-test journal folder").into()));
        let mut doc = Document::open(path)?;
        doc.replace(AT as u64, 8, b"REPLACED")?;
        let saved = doc.save(path);
        Err(io::Error::other(format!("Child passed the write barrier: {saved:?}")))
    }

    #[test]
    fn process_death_in_the_middle_of_the_write_is_undone_at_the_next_start() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let (path, original) = large(dir.path())?;
        let ready = dir.path().join("writer-ready");
        let journal = dir.path().join("journal");
        let before = stamp(&path);
        let mut child = Command::new(env::current_exe()?)
            .args(["--exact", "mac_inplace::tests::half_writer_child", "--test-threads=1"])
            .env(KILL_TARGET, &path)
            .env(KILL_READY, &ready)
            .env(KILL_JOURNAL, &journal)
            .stdin(Stdio::null())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while !ready.exists() && child.try_wait()?.is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let half = ready.exists();
        let _ = child.kill();
        let status = child.wait()?;
        assert!(half && !status.success(), "The write was never interrupted");
        let mut torn = original.clone();
        torn[AT..AT + 4].copy_from_slice(b"REPL");
        assert!(fs::read(&path)? == torn, "The test did not leave a half-written file");
        replay(&journal);
        assert!(fs::read(&path)? == original, "The half-written change was not undone");
        assert_eq!(stamp(&path), before);
        assert_eq!(records(&journal).len(), 0);
        Ok(())
    }

    /// What writing into the file is for, and what it must not cost: the
    /// file stays the same file, a program holding it open sees the change,
    /// earlier views and undo still read what they did, and a file another
    /// program changed is left exactly as that program left it.
    #[test]
    fn a_small_change_is_written_into_the_file_itself() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let (file, mut text) = large(dir.path())?;
        let journal = dir.path().join("journal");
        FOR_THIS_THREAD.set(Some(journal.clone()));
        let first = stamp(&file);
        let held_open = File::open(&file)?;
        let mut doc = Document::open(&file)?;
        let opened = doc.clone();
        for (at, count, new) in [
            (AT, 3, b"XYZ".as_slice()),
            (text.len() - 16, 3, b"a longer ending".as_slice()),
            (text.len() - 40, 30, b"".as_slice()),
        ] {
            doc.replace(at as u64, count as u64, new)?;
            let now = stamp(&file);
            let mut worker = doc.clone();
            assert!(worker.save_in_place(&file, &now)?, "save in place declined");
            doc.adopt_save(worker);
            text.splice(at..at + count, new.iter().copied());
            assert!(fs::read(&file)? == text);
            assert_eq!(stamp(&file).id, first.id);
            let mut seen = vec![0; new.len()];
            held_open.read_exact_at(&mut seen, at as u64)?;
            assert_eq!(seen, new);
        }
        assert_eq!(held_open.metadata()?.len(), text.len() as u64);
        assert_eq!(opened.read(AT as u64, 3)?, b"fgh");
        assert_eq!(records(&journal).len(), 0);
        // Undo after a save goes into the file too, and reads right.
        for _ in 0..3 {
            assert!(doc.undo());
            doc.save(&file)?;
            assert_eq!(stamp(&file).id, first.id);
        }
        assert!(fs::read(&file)? == b"abcdefgh".repeat(25 * 1024 * 1024 / 8));
        text = fs::read(&file)?;

        // Many saves into the file: the clone it is told from is renewed
        // once enough of the file differs from it, the file stays the same
        // file and stays right, and a change by another program is still seen.
        let told_from = doc.saved_clone.clone().unwrap();
        let megabyte = vec![b'm'; LARGEST];
        for round in 0..20 {
            let at = round * LARGEST;
            doc.replace(at as u64, LARGEST as u64, &megabyte)?;
            doc.save(&file)?;
            text[at..at + LARGEST].copy_from_slice(&megabyte);
        }
        assert_eq!(stamp(&file).id, first.id);
        assert!(fs::read(&file)? == text);
        assert!(!Arc::ptr_eq(&told_from, doc.saved_clone.as_ref().unwrap()), "the clone was never renewed");
        let old_time = fs::metadata(&file)?.modified()?;
        doc.replace(7, 1, b"!")?;
        let outside = OpenOptions::new().write(true).open(&file)?;
        outside.write_all_at(b"EXT", 3_000_000)?;
        outside.set_modified(old_time)?;
        drop(outside);
        assert_eq!(doc.save(&file).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        text[3_000_000..3_000_003].copy_from_slice(b"EXT");
        assert!(fs::read(&file)? == text);
        let mut doc = Document::open(&file)?;

        // A change too large for a record is saved the other way, and right.
        let many = vec![b'#'; LARGEST + 1];
        doc.replace(100, many.len() as u64, &many)?;
        assert!(!doc.clone().save_in_place(&file, &stamp(&file))?);
        doc.save(&file)?;
        text[100..100 + many.len()].copy_from_slice(&many);
        assert!(fs::read(&file)? == text);

        // Another program writes to the file and puts its date back: nothing
        // of ours goes into the file.
        let mut doc = Document::open(&file)?;
        doc.replace(AT as u64, 3, b"new")?;
        let old_time = fs::metadata(&file)?.modified()?;
        let outside = OpenOptions::new().write(true).open(&file)?;
        outside.write_all_at(b"EXT", 1000)?;
        outside.set_modified(old_time)?;
        drop(outside);
        assert_eq!(doc.save(&file).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        text[1000..1003].copy_from_slice(b"EXT");
        assert!(fs::read(&file)? == text);

        // Under a second name the file is not written into: that name would
        // change with it. It is saved as a new file and the other name keeps
        // the old text.
        let mut doc = Document::open(&file)?;
        let alias = dir.path().join("other-name.log");
        fs::hard_link(&file, &alias)?;
        doc.replace(AT as u64, 3, b"new")?;
        assert!(!doc.clone().save_in_place(&file, &stamp(&file))?);
        doc.save(&file)?;
        assert!(fs::read(&alias)? == text);
        text[AT..AT + 3].copy_from_slice(b"new");
        assert!(fs::read(&file)? == text);
        assert_eq!(records(&journal).len(), 0);
        FOR_THIS_THREAD.set(None);
        Ok(())
    }

    #[test]
    fn a_record_only_undoes_a_write_that_was_cut_short() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let journal = dir.path().join("journal");
        let file = dir.path().join("document.txt");
        let leave = |mut pending: Pending| drop(pending.slot.take());
        let target = |path: &Path| File::open(path).unwrap();

        // Cut short inside the changed bytes: put back, time included.
        fs::write(&file, b"prefix:old-tail")?;
        let old = stamp(&file);
        leave(Pending::record(&journal, &target(&file), &file, &old, 7, b"old-tail", b"REPLACEMENT-TAIL", 23)?);
        fs::write(&file, b"prefix:REPLACEM")?;
        replay(&journal);
        assert_eq!(fs::read(&file)?, b"prefix:old-tail");
        assert_eq!(stamp(&file), old);
        assert_eq!(records(&journal).len(), 0);

        // The new length with the added part unwritten reads as zeros: also
        // put back. So is an addition at the very end that never arrived.
        leave(Pending::record(&journal, &target(&file), &file, &old, 7, b"old-tail", b"REPLACEMENT-TAIL", 23)?);
        fs::write(&file, b"prefix:REPLACEM\0\0\0\0\0\0\0\0")?;
        replay(&journal);
        assert_eq!(fs::read(&file)?, b"prefix:old-tail");
        assert_eq!(stamp(&file), old);
        leave(Pending::record(&journal, &target(&file), &file, &old, 15, b"", b"MORE", 19)?);
        fs::write(&file, b"prefix:old-tail\0\0\0\0")?;
        replay(&journal);
        assert_eq!(fs::read(&file)?, b"prefix:old-tail");
        assert_eq!(stamp(&file), old);
        assert_eq!(records(&journal).len(), 0);

        // The same length with other text added there is someone else's work.
        leave(Pending::record(&journal, &target(&file), &file, &old, 15, b"", b"MORE", 19)?);
        fs::write(&file, b"prefix:old-tailELSE")?;
        replay(&journal);
        assert_eq!(fs::read(&file)?, b"prefix:old-tailELSE");
        assert_eq!(records(&journal).len(), 0);

        // Written in full, or never started: nothing to do.
        for state in [b"prefix:REPLACEMENT-TAIL".as_slice(), b"prefix:old-tail"] {
            fs::write(&file, b"prefix:old-tail")?;
            let old = stamp(&file);
            leave(Pending::record(&journal, &target(&file), &file, &old, 7, b"old-tail", b"REPLACEMENT-TAIL", 23)?);
            fs::write(&file, state)?;
            let seen = stamp(&file);
            replay(&journal);
            assert_eq!(fs::read(&file)?, state);
            assert_eq!(stamp(&file), seen);
            assert_eq!(records(&journal).len(), 0);
        }

        // Changed by someone else since: not ours to undo.
        fs::write(&file, b"prefix:old-tail")?;
        let old = stamp(&file);
        leave(Pending::record(&journal, &target(&file), &file, &old, 7, b"old-tail", b"REPLACEMENT-TAIL", 23)?);
        fs::write(&file, b"prefix:somebody")?;
        replay(&journal);
        assert_eq!(fs::read(&file)?, b"prefix:somebody");
        assert_eq!(records(&journal).len(), 0);

        // A save still running holds its record: another window leaves it alone.
        fs::write(&file, b"prefix:old-tail")?;
        let old = stamp(&file);
        let running = Pending::record(&journal, &target(&file), &file, &old, 7, b"old-tail", b"REPLACEMENT-TAIL", 23)?;
        fs::write(&file, b"prefix:REPLACEM")?;
        replay(&journal);
        assert_eq!(fs::read(&file)?, b"prefix:REPLACEM");
        assert_eq!(fs::read_dir(&journal)?.filter(|entry| decode(&fs::read(entry.as_ref().unwrap().path()).unwrap()).is_some()).count(), 1);
        running.finish();
        assert_eq!(fs::read_dir(&journal)?.filter(|entry| decode(&fs::read(entry.as_ref().unwrap().path()).unwrap()).is_some()).count(), 0);
        assert_eq!(records(&journal).len(), 0);

        // A record that was itself cut short means the file was never touched.
        fs::write(journal.join("half.save"), &encode(&file, &old, 7, b"old-tail", b"NEW", 10)[..40])?;
        replay(&journal);
        assert_eq!(fs::read(&file)?, b"prefix:REPLACEM");
        assert_eq!(records(&journal).len(), 0);

        // An older record that was left standing must not undo a later save.
        fs::write(&file, b"prefix:cat")?;
        leave(Pending::record(&journal, &target(&file), &file, &stamp(&file), 7, b"cat", b"dog", 10)?);
        let handle = OpenOptions::new().write(true).open(&file)?;
        handle.write_all_at(b"dog", 7)?;
        let second = stamp(&file);
        let later = Pending::record(&journal, &target(&file), &file, &second, 7, b"dog", b"dot", 10)?;
        handle.write_all_at(b"dot", 7)?;
        later.finish_and_retire(&journal, &stamp(&file));
        assert_eq!(records(&journal).len(), 0, "The older record outlived a later save");
        replay(&journal);
        assert_eq!(fs::read(&file)?, b"prefix:dot");
        Ok(())
    }
}
