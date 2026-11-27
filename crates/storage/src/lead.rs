//! Reading a large file from start to end on several threads without making
//! Windows unmap, during the scan, the views it has just read.
//!
//! Windows' file cache holds a file in views of 256 KiB. Whenever a read makes
//! it create the view that starts at a 1 MiB boundary (past the first MiB),
//! `CcGetVirtualAddress` unmaps every view of the 8 MiB before that boundary
//! that is not in use this instant (`CcUnmapVacbArray`; 8 MiB is
//! `CcUnmapBehindLength`). A scan in file order therefore unmaps, as it goes,
//! almost everything it reads, and that work is part of the time a person
//! waits. A `Lead` reads the first quarter of every MiB (the views that start
//! on a boundary) a little ahead of the scan and hands those bytes to the
//! scan's readers, so the scan's own reads never create such a view and leave
//! their views mapped.
//!
//! Measured on the 10 GB log, 12 threads taking 1 MiB chunks in order as this
//! engine does (probe `lead` in Linework-bench, 2026-11-25): one pass 0.333 to
//! 0.357 s with the lead against 0.486 to 0.526 s without. The views stay
//! mapped afterwards, and whatever scans the file next pays for unmapping them:
//! a second pass right after was 0.61 to 0.64 s with the lead and 0.68 to 0.70
//! s without. Touching one byte at every boundary afterwards (the tidy) unmaps
//! them in 0.28 to 0.36 s on one thread (more threads do not help: Windows does
//! it one view at a time per file), and every pass after a tidy took 0.35 to
//! 0.39 s. So the source tidies in the background after every scan with a lead,
//! and a scan that starts while the tidy runs waits for it. Another process gets
//! the fast case 2 s or more after a scan's process ends, the slow one at 0 to
//! 1 s.
use super::{Backing, Lane, Source};
use std::io;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Condvar, Mutex, Weak,
};
use std::thread;

const MIB: u64 = 1 << 20;
const QUARTER: u64 = 256 << 10;
/// How far ahead of the furthest chunk taken the first quarters are read, in
/// MiB: past the 8 MiB a boundary's view unmaps and the spread of the chunks in
/// flight, so no scan read is behind a boundary created later.
const AHEAD: u64 = 32;
/// First quarters kept: the lead's distance plus the spread of chunks in
/// flight (up to three per thread, 2 MiB each for the line index), with room.
const RING: u64 = 160;
/// Smaller files are scanned in plain order: the lead would read most of them
/// before the scan starts, and their views are few. Tests lead from 3 MiB, so
/// every scan test of a multi-MiB file reads through a lead.
const SMALLEST: u64 = if cfg!(test) { 3 * MIB } else { 4 * AHEAD * MIB };

/// What scans have left in Windows' cache for one file.
#[derive(Default)]
pub(crate) struct Views {
    /// Scans with a lead now running.
    scans: usize,
    /// A scan with a lead has finished and its views have not been unmapped yet.
    untidy: bool,
    tidy: Option<thread::JoinHandle<()>>,
}

enum Slot {
    Empty,
    Fetching(u64),
    Ready(u64, Arc<[u8]>),
}

pub(crate) struct Lead {
    source: Weak<Source>,
    /// The file offset of the source's first byte (after a byte-order mark).
    origin: u64,
    len: u64,
    /// The next MiB whose first quarter no thread has read yet.
    next: AtomicU64,
    slots: Mutex<Vec<Slot>>,
    ready: Condvar,
}

impl Source {
    /// A lead for a scan of this file from `from` to its end, or None when the
    /// scan should read in plain order: the file is in memory or small, another
    /// scan with a lead is running, or an earlier one's views were not tidied.
    /// Waits for a tidy in progress.
    pub(crate) fn lead(self: &Arc<Self>, from: u64) -> Option<Lead> {
        let Backing::Disk(_, origin) = &self.backing else {
            return None;
        };
        // The lead answers a habit of Windows' file cache. Where that cache
        // is not, it is only more reads: the same search of 10 GB in memory
        // on a Mac measured 0.18 s through a lead and 0.12 s in plain order.
        if cfg!(not(windows)) && !cfg!(test) {
            return None;
        }
        if self.len < SMALLEST || from >= self.len {
            return None;
        }
        let mut views = self.views.lock().unwrap();
        while let Some(tidy) = views.tidy.take() {
            drop(views);
            let _ = tidy.join();
            views = self.views.lock().unwrap();
        }
        if views.scans > 0 || views.untidy {
            return None;
        }
        views.scans += 1;
        Some(Lead {
            source: Arc::downgrade(self),
            origin: *origin,
            len: self.len,
            next: AtomicU64::new(((origin + from) / MIB).max(1)),
            slots: Mutex::new((0..RING).map(|_| Slot::Empty).collect()),
            ready: Condvar::new(),
        })
    }
}

impl Lead {
    pub(crate) fn is(&self, source: &Arc<Source>) -> bool {
        std::ptr::eq(self.source.as_ptr(), Arc::as_ptr(source))
    }

    fn source(&self) -> io::Result<Arc<Source>> {
        self.source.upgrade().ok_or_else(|| io::Error::from(io::ErrorKind::Interrupted))
    }

    /// The source range of MiB `m`'s first quarter (`m` counts in the file, from 1).
    fn quarter(&self, m: u64) -> (u64, u64) {
        let from = m * MIB - self.origin;
        (from, (from + QUARTER).min(self.len))
    }

    /// Reads, on this thread, the first quarters up to `AHEAD` MiB past source
    /// position `pos` that no thread has read yet. Called before each chunk.
    pub(crate) fn advance(&self, lane: &mut Lane, pos: u64) -> io::Result<()> {
        let last = (self.origin + self.len - 1) / MIB;
        let target = ((self.origin + pos) / MIB + AHEAD).min(last);
        loop {
            let m = self.next.load(Ordering::Relaxed);
            if m > target {
                return Ok(());
            }
            if self.next.compare_exchange(m, m + 1, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                self.fetch(lane, m)?;
            }
        }
    }

    fn fetch(&self, lane: &mut Lane, m: u64) -> io::Result<Arc<[u8]>> {
        let slot = (m % RING) as usize;
        self.slots.lock().unwrap()[slot] = Slot::Fetching(m);
        let (from, to) = self.quarter(m);
        let mut bytes = vec![0; (to - from) as usize];
        let read = self.source().and_then(|source| source.read_lane(lane, from, &mut bytes));
        let mut slots = self.slots.lock().unwrap();
        let result = match read {
            Ok(()) => {
                let bytes: Arc<[u8]> = bytes.into();
                slots[slot] = Slot::Ready(m, bytes.clone());
                Ok(bytes)
            }
            Err(e) => {
                slots[slot] = Slot::Empty;
                Err(e)
            }
        };
        drop(slots);
        self.ready.notify_all();
        result
    }

    /// MiB `m`'s first quarter: kept, or waited for while another thread reads
    /// it, or read now (the scan then creates that view itself, as without a lead).
    fn get(&self, lane: &mut Lane, m: u64) -> io::Result<Arc<[u8]>> {
        let slot = (m % RING) as usize;
        let mut slots = self.slots.lock().unwrap();
        loop {
            match &slots[slot] {
                Slot::Ready(n, bytes) if *n == m => return Ok(bytes.clone()),
                Slot::Fetching(n) if *n == m => slots = self.ready.wait(slots).unwrap(),
                _ => break,
            }
        }
        drop(slots);
        self.fetch(lane, m)
    }

    /// The same bytes as `Source::read_lane`, with the first quarter of every
    /// MiB past the first taken from the lead.
    pub(crate) fn read(&self, source: &Source, lane: &mut Lane, mut pos: u64, mut dst: &mut [u8]) -> io::Result<()> {
        while !dst.is_empty() {
            let file = self.origin + pos;
            let m = file / MIB;
            let n = if m >= 1 && file - m * MIB < QUARTER {
                let quarter = self.get(lane, m)?;
                let at = (pos - self.quarter(m).0) as usize;
                let n = dst.len().min(quarter.len() - at);
                dst[..n].copy_from_slice(&quarter[at..at + n]);
                n
            } else {
                // Up to the next boundary, which lies in the next quarter the lead holds.
                let n = dst.len().min(((m + 1) * MIB - file) as usize);
                source.read_lane(lane, pos, &mut dst[..n])?;
                n
            };
            pos += n as u64;
            dst = &mut dst[n..];
        }
        Ok(())
    }
}

impl Drop for Lead {
    /// The scan's views are still mapped: unmap them in the background, so the
    /// next scan does not pay for it.
    fn drop(&mut self) {
        let Some(source) = self.source.upgrade() else {
            return;
        };
        let mut views = source.views.lock().unwrap();
        views.scans -= 1;
        views.untidy = true;
        if views.scans == 0 && views.tidy.is_none() {
            let (weak, origin) = (self.source.clone(), self.origin);
            views.tidy = thread::Builder::new().name("Linework cache tidy".into()).spawn(move || tidy(weak, origin)).ok();
        }
    }
}

/// Touches one byte at every 1 MiB boundary of the file, which makes Windows
/// unmap the views behind each. Ends early when the document closes; a failed
/// read leaves the views untidy, and later scans read in plain order.
fn tidy(weak: Weak<Source>, origin: u64) {
    let mut lane = Lane::default();
    let mut byte = [0u8; 1];
    let mut m = 1u64;
    loop {
        let Some(source) = weak.upgrade() else {
            return;
        };
        if m * MIB >= origin + source.len {
            source.views.lock().unwrap().untidy = false;
            return;
        }
        if source.read_lane(&mut lane, m * MIB - origin, &mut byte).is_err() {
            return;
        }
        m += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Bytes read through a lead, at any offset and length, across quarter and
    /// MiB edges, are the bytes read without one; after the scan the views are tidied.
    #[test]
    fn a_lead_reads_the_same_bytes_and_tidies_afterwards() -> io::Result<()> {
        let mut file = tempfile::NamedTempFile::new()?;
        // A byte-order mark puts the source three bytes into the file, off the boundaries.
        let mut data = vec![0xEF, 0xBB, 0xBF];
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        while data.len() < (40 << 20) + 12_345 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            data.push(b'a' + (x % 26) as u8);
        }
        file.write_all(&data)?;
        file.flush()?;
        let handle = Arc::new(std::fs::File::open(file.path())?);
        let source = Source::file(handle, 3, data.len() as u64 - 3)?;
        let lead = source.lead(0).expect("a 40 MiB file on disk leads");
        assert!(source.lead(0).is_none(), "a second scan at the same time reads in plain order");
        let (mut lane, mut plain) = (Lane::default(), Lane::default());
        let len = source.len;
        let mut cases: Vec<(u64, usize)> = vec![(0, 5), (MIB - 3 - 1, 2), (MIB - 3, 10), (MIB - 3 + QUARTER - 1, 2), (len - 7, 7), (len - QUARTER - 9, (QUARTER + 9) as usize)];
        for i in 0..400u64 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let at = x % len;
            let n = ((x >> 32) % (3 * MIB)).min(len - at) as usize;
            cases.push((at, n));
            if i % 7 == 0 {
                lead.advance(&mut lane, at)?;
            }
        }
        for (at, n) in cases {
            let (mut led, mut direct) = (vec![0; n], vec![0; n]);
            lead.read(&source, &mut lane, at, &mut led)?;
            source.read_lane(&mut plain, at, &mut direct)?;
            assert!(led == direct, "bytes differ at {at} for {n}");
        }
        drop(lead);
        // The tidy runs, and a new scan waits for it and then leads again.
        let again = source.lead(0);
        assert!(again.is_some(), "after the tidy a scan leads again");
        assert!(!source.views.lock().unwrap().untidy, "the tidy finished before the new scan began");
        Ok(())
    }
}
