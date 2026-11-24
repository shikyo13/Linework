//! Preserve the original piece-tree source across an atomic in-place transaction.
//! Readers continue to see its original bytes, including during writer handoff.
use super::{read_at, Backing, Source};
use crate::{conflict, revision::guard_destination, transactional::{self, ReadSnapshot}, write_lease::{Lease, ReadRange, Uncut}, Stamp};
use std::{
    fs::{File, OpenOptions},
    io,
    os::windows::fs::OpenOptionsExt,
    path::Path,
    sync::Arc,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

#[cfg(test)]
thread_local! {static FAIL_CERTIFICATION:std::cell::Cell<bool>=const {std::cell::Cell::new(false)};}
// Runs as another program would at the one moment an in-place save holds
// neither its lease nor its writer: the file path and the start of the change.
#[cfg(test)]
thread_local! {static DURING_HANDOVER:std::cell::Cell<Option<fn(&Path, u64)>>=const {std::cell::Cell::new(None)};}
const MAX_ORIGINAL_BYTES: u64 = 1024 * 1024;

#[derive(Default)]
pub(super) struct OriginalView {
    // Coordinates are physical source-file offsets, including any original BOM.
    start: u64,
    bytes: Vec<u8>,
    current: Option<Stamp>,
    pinned: Option<Arc<ReadSnapshot>>,
    // An in-place save without a transaction is under way. Range locks hold
    // every byte outside the saved interval still, so the file plus the saved
    // interval is the original even while its size and time are changing.
    shielded: bool,
}

impl OriginalView {
    pub(super) fn current(&self) -> Option<&Stamp> { self.current.as_ref() }
    pub(super) fn is_pinned(&self) -> bool { self.pinned.is_some() || self.shielded }
    pub(super) fn is_virtual(&self) -> bool { self.is_pinned() || self.current.is_some() }
    pub(super) fn clear_pin(&mut self) { self.pinned = None; self.shielded = false; }

    pub(super) fn read(&self, original: &File, origin: u64, pos: u64, dst: &mut [u8]) -> io::Result<()> {
        let backing = self.pinned.as_ref().map_or(original, |snapshot| snapshot.file());
        let from = origin + pos;
        let end = from + dst.len() as u64;
        let lo = from.max(self.start);
        let hi = end.min(self.start + self.bytes.len() as u64);
        if lo >= hi { return read_at(backing, from, dst); }
        let left = (lo - from) as usize;
        let right = (hi - from) as usize;
        read_at(backing, from, &mut dst[..left])?;
        dst[left..right].copy_from_slice(&self.bytes[(lo-self.start) as usize..(hi-self.start) as usize]);
        read_at(backing, hi, &mut dst[right..])
    }

    fn capture(&mut self, snapshot: &File, original_end: u64, start: u64, end: u64) -> io::Result<bool> {
        let mut lo = start.min(original_end);
        let mut hi = end.min(original_end);
        if !self.bytes.is_empty() {
            // Previously protected bytes remain authoritative even after Undo.
            lo = lo.min(self.start);
            hi = hi.max(self.start + self.bytes.len() as u64);
        }
        if hi - lo > MAX_ORIGINAL_BYTES { return Ok(false); }
        let mut captured = vec![0; (hi-lo) as usize];
        self.read(snapshot, 0, lo, &mut captured)?;
        self.start = lo;
        self.bytes = captured;
        Ok(true)
    }
}

fn same_range(live: &File, before: &File, mut at: u64, end: u64, a: &mut [u8], b: &mut [u8]) -> io::Result<bool> {
    while at < end {
        let n = (end-at).min(a.len() as u64) as usize;
        read_at(live, at, &mut a[..n])?;
        read_at(before, at, &mut b[..n])?;
        if a[..n] != b[..n] { return Ok(false); }
        at += n as u64;
    }
    Ok(true)
}

/// Certify the public revision under its new deny-write guard before retaining
/// it as the backing for any original bytes outside the protected interval.
fn matches_public(live: &File, before: &File, old_size: u64, change: Option<(u64,u64,&[u8])>) -> io::Result<bool> {
    let mut a = vec![0; 1024*1024];
    let mut b = vec![0; a.len()];
    let Some((at, removed, replacement)) = change else {
        return same_range(live, before, 0, old_size, &mut a, &mut b);
    };
    if !same_range(live, before, 0, at, &mut a, &mut b)? { return Ok(false); }
    for (i, chunk) in replacement.chunks(a.len()).enumerate() {
        let offset = at + (i * a.len()) as u64;
        read_at(live, offset, &mut a[..chunk.len()])?;
        if a[..chunk.len()] != *chunk { return Ok(false); }
    }
    // A non-tail patch was accepted only if it preserves length.
    same_range(live, before, at+removed, old_size, &mut a, &mut b)
}

impl Source {
    pub(crate) fn transact_range(
        self: &Arc<Self>, path: &Path, expected: &Stamp, start: u64, removed: u64,
        replacement: &[u8], write: impl FnOnce(&File) -> io::Result<Option<(Stamp, Option<(u32, u32)>)>>,
    ) -> io::Result<Option<Stamp>> {
        let Some(external) = &self.external else { return Ok(None) };
        let Backing::Disk(_,origin) = &self.backing else { return Ok(None) };
        let Some(opened) = self.recorded.as_ref() else { return Ok(None) };
        let Some(end) = start.checked_add(removed) else { return Ok(None) };
        let changing_length = removed != replacement.len() as u64;
        if self.len + *origin != opened.size || opened.id != expected.id || opened.volume != expected.volume
            || start < *origin || end > expected.size || (changing_length && end != expected.size)
            || removed.max(replacement.len() as u64) > MAX_ORIGINAL_BYTES {
            return Ok(None);
        }
        let mut lease = external.lease.lock().unwrap();
        if lease.is_none() || external.snapshot.get().is_some() || external.retained_guard.lock().unwrap().is_some() {
            return Ok(None);
        }
        {
            let view = external.delta.read().unwrap();
            if view.is_pinned() || view.current.as_ref().unwrap_or(opened) != expected { return Ok(None); }
        }
        // Disallow renaming or replacing this public name across both handoffs.
        let name_guard = OpenOptions::new().read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE).open(path)?;
        if &Stamp::from(&name_guard)? != expected { return Err(conflict()); }
        let Some(snapshot) = ReadSnapshot::open(path, expected)? else { return Ok(None) };
        let snapshot = Arc::new(snapshot);
        {
            let mut view = external.delta.write().unwrap();
            if !view.capture(snapshot.file(),opened.size,start,if changing_length { opened.size } else { end })? {
                return Ok(None);
            }
            view.pinned = Some(snapshot.clone());
        }
        drop(lease.take());
        if external.snapshot.get().is_some() {
            external.delta.write().unwrap().clear_pin();
            return Ok(None);
        }
        let result = write(snapshot.file());
        let committed = result.as_ref().ok().and_then(|s| s.as_ref());
        let disk = committed.map_or(expected, |(stamp, _)| stamp);
        let certified = guard_destination(path, disk).and_then(|guard| {
            #[cfg(test)]
            if committed.is_some() && FAIL_CERTIFICATION.replace(false) {
                return Err(io::Error::from_raw_os_error(32));
            }
            // The old transactional reader has stayed open since the original
            // lease was held. With the public stream now guarded, exactly our
            // expected version advance rules out an intervening writer.
            if let Some((_, Some(expected_versions))) = committed {
                if transactional::versions(snapshot.file()) == Some(*expected_versions) {
                    return Ok(guard);
                }
            }
            let change = committed.map(|_| (start,removed,replacement));
            if matches_public(&guard,snapshot.file(),expected.size,change)? { Ok(guard) } else { Err(conflict()) }
        });
        let guard = match certified {
            Ok(guard) => guard,
            Err(error) => {
                // An unrelated writer or namespace change won the handoff.
                // The pre-save TxF stream and the saved interval still supply
                // the original; keep the pinned version if disk is exhausted.
                let _ = self.materialize(expected, false);
                // The commit is irreversible. The retained immutable backing still
                // authenticates our saved tree on the next Save, even if a new
                // writer already superseded our committed disk revision.
                if let Some((stamp, _)) = committed {
                    return Ok(Some(stamp.clone()));
                }
                return Err(error);
            }
        };
        {
            let mut view = external.delta.write().unwrap();
            if committed.is_some() { view.current = Some(disk.clone()); }
            view.clear_pin();
        }
        drop(snapshot);
        *lease = Lease::acquire(path,disk,self);
        if lease.is_none() && self.materialize(disk,false).is_err() {
            *external.retained_guard.lock().unwrap() = Some(guard);
        }
        result.map(|saved| saved.map(|(stamp, _)| stamp))
    }

    /// The same handover as `transact_range` for a write made without a
    /// transaction. Before the deny-write lease ends, a second handle takes
    /// shared range locks on every byte outside the changed range, and it holds
    /// them until the next deny-write guard is open. In between, another
    /// program can at most open the file: it cannot change it, rename it or
    /// delete it, so nothing has to be compared afterwards except the range.
    /// `write` receives two functions: one to call when its writer is open and
    /// so alone with the file, and one to call once its change is on disk and
    /// before that writer closes, which locks the changed range as well.
    pub(crate) fn patch_range(
        self: &Arc<Self>, path: &Path, expected: &Stamp, start: u64, removed: u64,
        replacement: &[u8], write: impl FnOnce(&dyn Fn(), &dyn Fn()) -> io::Result<Option<Stamp>>,
    ) -> io::Result<Option<Stamp>> {
        // No file offset reaches this, and a lock may extend past the end of a file.
        const FAR: u64 = i64::MAX as u64;
        let Some(external) = &self.external else { return Ok(None) };
        let Backing::Disk(original, origin) = &self.backing else { return Ok(None) };
        let Some(opened) = self.recorded.as_ref() else { return Ok(None) };
        let Some(end) = start.checked_add(removed) else { return Ok(None) };
        let changing_length = removed != replacement.len() as u64;
        if self.len + *origin != opened.size || opened.id != expected.id || opened.volume != expected.volume
            || start < *origin || end > expected.size || (changing_length && end != expected.size)
            || removed.max(replacement.len() as u64) > MAX_ORIGINAL_BYTES {
            return Ok(None);
        }
        let mut lease = external.lease.lock().unwrap();
        if lease.is_none() || external.snapshot.get().is_some() || external.retained_guard.lock().unwrap().is_some() {
            return Ok(None);
        }
        {
            let view = external.delta.read().unwrap();
            if view.is_pinned() || view.current.as_ref().unwrap_or(opened) != expected { return Ok(None); }
        }
        // Without delete sharing, this also keeps the public name in place.
        let shield = OpenOptions::new().read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE).open(path)?;
        if &Stamp::from(&shield)? != expected { return Err(conflict()); }
        let lock = |from: u64, to: u64| -> io::Result<Option<ReadRange>> {
            if from < to { ReadRange::lock(&shield, from, to - from).map(Some) } else { Ok(None) }
        };
        // An oplock would be broken by the range locks, so it ends first. Its
        // handle goes on denying writers until those locks are in place.
        let Some(held) = lease.take().and_then(Lease::demote) else { return Ok(None) };
        let mut on_disk = vec![0; removed as usize];
        // Range locks stop writes but not a cut to the file's length. An open
        // section does, for us as well, so it is let go while our own writer,
        // which excludes all others, is open.
        let uncut = std::cell::RefCell::new(None);
        let prepared = (|| -> io::Result<_> {
            let head = lock(0, start)?;
            let tail = if changing_length { None } else { lock(end, FAR)? };
            *uncut.borrow_mut() = Some(Uncut::hold(&shield)?);
            read_at(&shield, start, &mut on_disk)?;
            let mut view = external.delta.write().unwrap();
            if !view.capture(original, opened.size, start, if changing_length { opened.size } else { end })? {
                return Ok(None);
            }
            view.shielded = true;
            Ok(Some((head, tail)))
        })();
        let outside = match prepared.ok().flatten() {
            Some(locks) => locks,
            None => {
                // Nothing has changed, so whatever went wrong the ordinary save
                // can take over. The range locks are gone again, and the old
                // handle still denies writers while the lease is renewed.
                drop(uncut.take());
                *lease = Lease::acquire(path, expected, self);
                if lease.is_none() && self.materialize(expected, false).is_err() {
                    // Neither a lease nor a private copy: the old handle stays
                    // as the original's only protection.
                    *external.retained_guard.lock().unwrap() = Some(held);
                }
                return Ok(None);
            }
        };
        drop(held);
        #[cfg(test)]
        if let Some(intrude) = DURING_HANDOVER.take() { intrude(path, start); }
        let sealed = std::cell::RefCell::new(None);
        let alone = || drop(uncut.borrow_mut().take());
        let seal = || {
            let mut uncut = uncut.borrow_mut();
            if uncut.is_none() { *uncut = Uncut::hold(&shield).ok(); }
            let mut sealed = sealed.borrow_mut();
            if sealed.is_none() { *sealed = lock(start, FAR).ok().flatten(); }
        };
        let result = write(&alone, &seal);
        let committed = result.as_ref().ok().and_then(|s| s.as_ref());
        let disk = committed.unwrap_or(expected);
        seal();
        let certified = guard_destination(path, disk).and_then(|guard| {
            #[cfg(test)]
            if committed.is_some() && FAIL_CERTIFICATION.replace(false) {
                return Err(io::Error::from_raw_os_error(32));
            }
            // Without both holds the file was open to changes we cannot see.
            if uncut.borrow().is_none() || sealed.borrow().is_none() { return Err(conflict()); }
            // Only the changed range was ever open to another writer, and only
            // while ours was not open. It must hold exactly what we left there.
            let wanted = if committed.is_some() { replacement } else { on_disk.as_slice() };
            let mut found = vec![0; wanted.len()];
            read_at(&guard, start, &mut found)?;
            if found == wanted { Ok(guard) } else { Err(conflict()) }
        });
        let saved = committed.cloned();
        let guard = match certified {
            Ok(guard) => guard,
            Err(error) => {
                // Another program won the handover. While the range locks last,
                // the file and the saved interval still give the original.
                let copied = self.materialize(expected, false).is_ok();
                {
                    let mut view = external.delta.write().unwrap();
                    if let Some(stamp) = &saved { view.current = Some(stamp.clone()); }
                    if copied { view.clear_pin(); }
                }
                if !copied {
                    // No room for a private copy. The range locks and the
                    // section are then all that keeps the original readable.
                    // They last as long as their handle, which is kept; the
                    // view stays marked as shielded to match.
                    std::mem::forget(sealed);
                    std::mem::forget(uncut);
                    std::mem::forget(outside);
                    *external.retained_guard.lock().unwrap() = Some(shield);
                }
                // A committed save is reported as saved. Otherwise the write's
                // own error says more than the failed handover does.
                return match (saved, result) {
                    (Some(stamp), _) => Ok(Some(stamp)),
                    (None, Err(failure)) => Err(failure),
                    (None, Ok(_)) => Err(error),
                };
            }
        };
        {
            let mut view = external.delta.write().unwrap();
            if committed.is_some() { view.current = Some(disk.clone()); }
            view.clear_pin();
        }
        // No oplock is granted on a file with range locks. The guard denies
        // writers from here until the new lease does.
        drop(sealed.take());
        drop(uncut.take());
        drop(outside);
        *lease = Lease::acquire(path, disk, self);
        if lease.is_none() && self.materialize(disk, false).is_err() {
            *external.retained_guard.lock().unwrap() = Some(guard);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Document;
    use std::{
        fs,
        io::{Seek, SeekFrom, Write},
    };
    #[test]
    fn committed_save_survives_failed_recertification_and_external_rewrite() -> io::Result<()> {
        lost_handover(false)
    }

    #[test]
    fn save_written_in_place_survives_failed_recertification_and_external_rewrite() -> io::Result<()> {
        lost_handover(true)
    }

    #[test]
    fn another_program_in_the_handover_reaches_only_the_changed_range_and_is_noticed() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        crate::save_journal::FOR_THIS_THREAD.set(Some(dir.path().join("journal")));
        let file = dir.path().join("edit.txt");
        let mut bytes = b"abcdefgh\n".repeat(3_000_000);
        fs::write(&file, &bytes)?;
        let mut d = Document::open(&file)?;
        d.replace(9, 3, b"XYZ")?;
        DURING_HANDOVER.set(Some(|path, start| {
            let refused = |result: io::Result<()>| result.is_err_and(|e| e.raw_os_error() == Some(33));
            let mut other = OpenOptions::new().write(true).open(path).expect("the file stays openable");
            let len = other.metadata().unwrap().len();
            for at in [0, start - 1, start + 3, len - 1, len] {
                other.seek(SeekFrom::Start(at)).unwrap();
                assert!(refused(other.write_all(b"n")), "byte {at}, outside the change, could be written");
            }
            assert!(other.set_len(start + 1).is_err(), "the file could be cut short");
            assert!(fs::rename(path, path.with_extension("moved")).is_err(), "the file could be renamed");
            assert!(fs::remove_file(path).is_err(), "the file could be deleted");
            other.seek(SeekFrom::Start(start)).unwrap();
            other.write_all(b"!").unwrap();
        }));
        assert_eq!(d.save(&file).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert!(DURING_HANDOVER.take().is_none(), "the save never reached its handover");
        // The other program's byte stays, and nothing of ours was written.
        bytes[9] = b'!';
        assert!(fs::read(&file)? == bytes);
        assert_eq!(fs::read_dir(dir.path().join("journal")).map_or(0, |d| d.count()), 0);
        // The document still holds the original with its own edit.
        let copy = dir.path().join("copy.txt");
        d.save(&copy)?;
        bytes[9..12].copy_from_slice(b"XYZ");
        assert!(fs::read(&copy)? == bytes);
        assert!(d.undo());
        d.save(&copy)?;
        assert!(fs::read(&copy)? == b"abcdefgh\n".repeat(3_000_000));
        crate::save_journal::FOR_THIS_THREAD.set(None);
        Ok(())
    }

    fn lost_handover(in_place: bool) -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        crate::save_journal::FOR_THIS_THREAD.set(in_place.then(|| dir.path().join("journal")));
        let file = dir.path().join("edit.txt");
        let mut bytes = b"abcdefgh\n".repeat(3_000_000);
        fs::write(&file, &bytes)?;
        let mut d = Document::open(&file)?;
        d.replace(9, 3, b"XYZ")?;
        FAIL_CERTIFICATION.set(true);
        d.save(&file)?;
        assert!(
            !FAIL_CERTIFICATION.get(),
            "the real NTFS commit must reach post-commit certification"
        );
        bytes[9..12].copy_from_slice(b"XYZ");
        assert!(fs::read(&file)? == bytes);
        assert!(!d.dirty());
        assert!(d.undo());
        let time = fs::metadata(&file)?.modified()?;
        let mut outside = OpenOptions::new().write(true).open(&file)?;
        outside.seek(SeekFrom::Start(800))?;
        outside.write_all(b"new")?;
        outside.set_modified(time)?;
        drop(outside);
        assert_eq!(
            d.save(&file).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        bytes[800..803].copy_from_slice(b"new");
        assert!(fs::read(&file)? == bytes);
        let copy = dir.path().join("undo.txt");
        d.save(&copy)?;
        assert!(fs::read(copy)? == b"abcdefgh\n".repeat(3_000_000));
        assert_eq!(in_place, dir.path().join("journal").exists());
        crate::save_journal::FOR_THIS_THREAD.set(None);
        Ok(())
    }
}
