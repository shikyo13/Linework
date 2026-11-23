//! Preserve the original piece-tree source across an atomic in-place transaction.
//! Readers continue to see its original bytes, including during writer handoff.
use super::{read_at, Backing, Source};
use crate::{conflict, revision::guard_destination, transactional::{self, ReadSnapshot}, write_lease::Lease, Stamp};
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
const MAX_ORIGINAL_BYTES: u64 = 1024 * 1024;

#[derive(Default)]
pub(super) struct OriginalView {
    // Coordinates are physical source-file offsets, including any original BOM.
    start: u64,
    bytes: Vec<u8>,
    current: Option<Stamp>,
    pinned: Option<Arc<ReadSnapshot>>,
}

impl OriginalView {
    pub(super) fn current(&self) -> Option<&Stamp> { self.current.as_ref() }
    pub(super) fn is_pinned(&self) -> bool { self.pinned.is_some() }
    pub(super) fn is_virtual(&self) -> bool { self.is_pinned() || self.current.is_some() }
    pub(super) fn clear_pin(&mut self) { self.pinned = None; }

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
        let dir = tempfile::tempdir()?;
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
        Ok(())
    }
}
