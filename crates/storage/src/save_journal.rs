//! Crash protection for a small change written straight into a large file.
//! The bytes about to be replaced are recorded and flushed before the file is
//! touched. A record left behind by a crash is used at the next start to put
//! the file back as it was, so a half-written change never stays on disk.
use crate::Stamp;
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        RwLock,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

static FOLDER: RwLock<Option<PathBuf>> = RwLock::new(None);
static SERIAL: AtomicU64 = AtomicU64::new(0);
const MAGIC: &[u8; 8] = b"LWSAVE01";
const EXTENSION: &str = "save";
const FIXED: usize = 8 + 4 + 8 + 8 + 16 + 8 + 8 + 4 + 4 + 4;
const LARGEST: usize = crate::transactional::MAX_PATCH_BYTES;

/// Where records are kept. Setting a folder also settles any record a crashed
/// session left there, so call it before opening files. With no folder, small
/// in-place saves use an NTFS transaction instead.
pub fn set_save_journal(folder: Option<&Path>) {
    *FOLDER.write().unwrap() = folder.map(Path::to_owned);
    if let Some(folder) = folder {
        replay(folder);
    }
}

#[cfg(test)]
thread_local! {
    /// Lets one test choose the journal for its own saves while the tests on
    /// other threads keep the transaction path.
    pub(crate) static FOR_THIS_THREAD: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn folder() -> Option<PathBuf> {
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
    let name: Vec<u16> = target.as_os_str().encode_wide().collect();
    let mut out = Vec::with_capacity(FIXED + name.len() * 2 + before.len() + after.len() + 32);
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
    for unit in name {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out.extend_from_slice(before);
    out.extend_from_slice(after);
    let sum = Sha256::digest(&out);
    out.extend_from_slice(&sum);
    out
}

fn decode(bytes: &[u8]) -> Option<Record> {
    let body = bytes.len().checked_sub(32)?;
    if body < FIXED || &bytes[..8] != MAGIC || Sha256::digest(&bytes[..body])[..] != bytes[body..] {
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
    if before > LARGEST || after > LARGEST || name > 32_768 || FIXED + name * 2 + before + after != body {
        return None;
    }
    // No real file reaches the end of the offset range; later sums rely on it.
    offset.checked_add(before.max(after) as u64)?;
    let units: Vec<u16> = take(name * 2).chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
    Some(Record {
        target: OsString::from_wide(&units).into(),
        old: Stamp { size, modified, id, volume },
        offset,
        new_len,
        before: take(before).to_vec(),
        after: take(after).to_vec(),
    })
}

/// A flushed record of one save in progress. It is held open without sharing
/// for as long as the save runs, which is how another Linework window tells a
/// save in progress from one that died.
pub(crate) struct Pending {
    file: Option<File>,
    path: PathBuf,
}

impl Pending {
    pub(crate) fn record(
        folder: &Path, target: &Path, old: &Stamp, offset: u64, before: &[u8], after: &[u8], new_len: u64,
    ) -> io::Result<Self> {
        fs::create_dir_all(folder)?;
        let clock = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let name = format!(
            "{:x}-{clock:x}-{:x}.{EXTENSION}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        );
        let path = folder.join(name);
        let mut file = OpenOptions::new().write(true).create_new(true).share_mode(0).open(&path)?;
        let written = file
            .write_all(&encode(target, old, offset, before, after, new_len))
            .and_then(|()| file.sync_all());
        if let Err(error) = written {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        Ok(Self { file: Some(file), path })
    }

    /// The file is in a whole state again, new or old: the record is not needed.
    pub(crate) fn finish(mut self) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }

    /// The save succeeded, so the file is whole and is what its document holds.
    /// An older record for the same file may still be in the folder: one whose
    /// removal failed, or one left by a failed write that could not be put back.
    /// Replayed after this save it could match by chance and put superseded
    /// bytes back, so it goes too.
    pub(crate) fn finish_and_retire(self, folder: &Path, now: &Stamp) {
        let own = self.path.clone();
        self.finish();
        for (path, bytes) in records(folder) {
            let same_file = decode(&bytes).is_some_and(|r| r.old.volume == now.volume && r.old.id == now.id);
            if path != own && same_file {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

/// Every record in the folder that no running save holds, with its contents.
/// A running save keeps its record open without sharing, so opening it fails.
/// Delete sharing lets the owner of a finished record remove it meanwhile.
fn records(folder: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    const FILE_SHARE_DELETE: u32 = 4;
    let Ok(entries) = fs::read_dir(folder) else { return Vec::new() };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(EXTENSION) {
            continue;
        }
        let Ok(file) = OpenOptions::new().read(true).share_mode(FILE_SHARE_DELETE).open(&path) else { continue };
        let mut bytes = Vec::new();
        if file.take((FIXED + 65_536 + 2 * LARGEST + 32) as u64 + 1).read_to_end(&mut bytes).is_ok() {
            found.push((path, bytes));
        }
    }
    found
}

/// Put back the replaced bytes, the length and the time through an open writer.
pub(crate) fn restore(file: &mut File, old: &Stamp, offset: u64, before: &[u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(before)?;
    file.set_len(old.size)?;
    let nanos = old.modified;
    let time = UNIX_EPOCH + Duration::new((nanos / 1_000_000_000) as u64, (nanos % 1_000_000_000) as u32);
    file.set_modified(time)?;
    file.sync_all()
}

/// True when the record has done its job and can be deleted.
fn settle(record: &Record) -> bool {
    let mut file = match OpenOptions::new().read(true).write(true).share_mode(0).open(&record.target) {
        Ok(file) => file,
        // The file itself is gone. An unreachable folder or a file in use is
        // tried again at the next start.
        Err(error) => return error.raw_os_error() == Some(2),
    };
    let Ok(now) = Stamp::from(&file) else { return false };
    if now.volume != record.old.volume || now.id != record.old.id {
        return true;
    }
    let widest = record.before.len().max(record.after.len()) as u64;
    let readable = now.size.saturating_sub(record.offset).min(widest) as usize;
    let mut current = vec![0; readable];
    if file.seek(SeekFrom::Start(record.offset)).is_err() || file.read_exact(&mut current).is_err() {
        return false;
    }
    let holds = |bytes: &[u8], len: u64| now.size == len && current.len() >= bytes.len() && current[..bytes.len()] == *bytes;
    if holds(&record.after, record.new_len) || holds(&record.before, record.old.size) {
        return true;
    }
    // A write cut short leaves each byte either old or new, and a length that
    // is the old one, the new one, or where the write alone would have ended.
    // Past the old end of the file a zero is also ours: after a power cut NTFS
    // can show the new length with the added part not yet written, as zeros.
    let reached = record.old.size.max(record.offset + record.after.len() as u64);
    let torn = [record.old.size, record.new_len, reached].contains(&now.size)
        && current.iter().enumerate().all(|(i, byte)| {
            record.before.get(i) == Some(byte)
                || record.after.get(i) == Some(byte)
                || (*byte == 0 && record.offset + i as u64 >= record.old.size)
        });
    // Anything else was changed by another program after the crash: leave it.
    !torn || restore(&mut file, &record.old, record.offset, &record.before).is_ok()
}

pub(crate) fn replay(folder: &Path) {
    for (path, bytes) in records(folder) {
        // A record that does not check out was never finished, and the file is
        // only written after its record is complete and flushed.
        if decode(&bytes).is_none_or(|record| settle(&record)) {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(path: &Path) -> Stamp {
        Stamp::get(path).unwrap().unwrap()
    }

    #[test]
    fn a_record_only_undoes_a_write_that_was_cut_short() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let journal = dir.path().join("journal");
        let file = dir.path().join("document.txt");
        let leave = |pending: Pending| drop(pending.file);

        // Cut short inside the changed bytes: put back, time included.
        fs::write(&file, b"prefix:old-tail")?;
        let old = stamp(&file);
        leave(Pending::record(&journal, &file, &old, 7, b"old-tail", b"REPLACEMENT-TAIL", 23)?);
        fs::write(&file, b"prefix:REPLACEM")?;
        replay(&journal);
        assert_eq!(fs::read(&file)?, b"prefix:old-tail");
        assert_eq!(stamp(&file), old);
        assert_eq!(fs::read_dir(&journal)?.count(), 0);

        // A power cut can leave the new length with the added part unwritten,
        // which reads as zeros: also put back. So is an addition at the very
        // end that never reached the disk.
        fs::write(&file, b"prefix:old-tail")?;
        let old = stamp(&file);
        leave(Pending::record(&journal, &file, &old, 7, b"old-tail", b"REPLACEMENT-TAIL", 23)?);
        fs::write(&file, b"prefix:REPLACEM\0\0\0\0\0\0\0\0")?;
        replay(&journal);
        assert_eq!(fs::read(&file)?, b"prefix:old-tail");
        assert_eq!(stamp(&file), old);
        leave(Pending::record(&journal, &file, &old, 15, b"", b"MORE", 19)?);
        fs::write(&file, b"prefix:old-tail\0\0\0\0")?;
        replay(&journal);
        assert_eq!(fs::read(&file)?, b"prefix:old-tail");
        assert_eq!(stamp(&file), old);
        assert_eq!(fs::read_dir(&journal)?.count(), 0);

        // The same length with other text added there is someone else's work.
        leave(Pending::record(&journal, &file, &old, 15, b"", b"MORE", 19)?);
        fs::write(&file, b"prefix:old-tailELSE")?;
        replay(&journal);
        assert_eq!(fs::read(&file)?, b"prefix:old-tailELSE");
        assert_eq!(fs::read_dir(&journal)?.count(), 0);

        // Written in full, or never started: nothing to do.
        for state in [b"prefix:REPLACEMENT-TAIL".as_slice(), b"prefix:old-tail"] {
            fs::write(&file, b"prefix:old-tail")?;
            let old = stamp(&file);
            leave(Pending::record(&journal, &file, &old, 7, b"old-tail", b"REPLACEMENT-TAIL", 23)?);
            fs::write(&file, state)?;
            let seen = stamp(&file);
            replay(&journal);
            assert_eq!(fs::read(&file)?, state);
            assert_eq!(stamp(&file), seen);
            assert_eq!(fs::read_dir(&journal)?.count(), 0);
        }

        // Changed by someone else since: not ours to undo.
        fs::write(&file, b"prefix:old-tail")?;
        let old = stamp(&file);
        leave(Pending::record(&journal, &file, &old, 7, b"old-tail", b"REPLACEMENT-TAIL", 23)?);
        fs::write(&file, b"prefix:somebody")?;
        replay(&journal);
        assert_eq!(fs::read(&file)?, b"prefix:somebody");
        assert_eq!(fs::read_dir(&journal)?.count(), 0);

        // A save still running holds its record: another window leaves it alone.
        fs::write(&file, b"prefix:old-tail")?;
        let old = stamp(&file);
        let running = Pending::record(&journal, &file, &old, 7, b"old-tail", b"REPLACEMENT-TAIL", 23)?;
        fs::write(&file, b"prefix:REPLACEM")?;
        replay(&journal);
        assert_eq!(fs::read(&file)?, b"prefix:REPLACEM");
        assert_eq!(fs::read_dir(&journal)?.count(), 1);
        running.finish();
        assert_eq!(fs::read_dir(&journal)?.count(), 0);

        // A record that was itself cut short means the file was never touched.
        fs::write(journal.join("half.save"), &encode(&file, &old, 7, b"old-tail", b"NEW", 10)[..40])?;
        replay(&journal);
        assert_eq!(fs::read(&file)?, b"prefix:REPLACEM");
        assert_eq!(fs::read_dir(&journal)?.count(), 0);
        Ok(())
    }
}
