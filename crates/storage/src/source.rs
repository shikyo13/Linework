use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::thread;

#[cfg(windows)]
#[path = "source_delta.rs"]
mod delta;
#[path = "line_cache.rs"]
mod cache;
pub use cache::set_index_cache;
#[path = "lead.rs"]
mod lead;
pub(crate) use lead::Lead;

pub(crate) const PAGE: usize = 65536;
const BATCH: usize = PAGE * 32;
/// Batches per thread below which a line index is not worth more threads.
const PARALLEL_BATCHES: usize = 8;
const SLOTS: usize = 12;
/// Whether opening a large file starts counting its lines in the background.
static LINE_INDEXING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Whether documents opened from now on count their lines in the background.
/// A program that only searches, and numbers lines as it goes, turns this off
/// so the count does not read the whole file a second time beside the search.
/// Line numbers, line starts and the line count of such a document are not
/// available until it is reopened with indexing on.
pub fn set_line_indexing(on: bool) {
    LINE_INDEXING.store(on, Ordering::Relaxed);
}
thread_local! {
    /// Set while `Document::open_deferred` opens a file on this thread: its
    /// line count waits for `Document::start_indexing`.
    pub(crate) static DEFER_INDEXING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn read_at(f: &File, mut pos: u64, mut dst: &mut [u8]) -> io::Result<()> {
    while !dst.is_empty() {
        #[cfg(windows)]
        let n = std::os::windows::fs::FileExt::seek_read(f, dst, pos)?;
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::read_at(f, dst, pos)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "File was truncated while open",
            ));
        }
        pos += n as u64;
        dst = &mut dst[n..];
    }
    Ok(())
}

/// One thread's own file objects for a disk source. Windows completes reads
/// on a synchronous handle one at a time, so threads sharing the document's
/// handle cannot read in parallel. A file that cannot be opened again, such as
/// an exclusive temporary file, is read through the shared handle.
#[derive(Default)]
pub(crate) struct Lane {
    original: Option<Option<File>>,
    copy: Option<Option<File>>,
}
#[cfg(windows)]
fn reopen(file: &File) -> Option<File> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::{
            ReOpenFile, FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        },
    };
    // Share everything, as the document's own handle does: this handle must
    // never be the reason another program's open or rename fails.
    let handle = unsafe {
        ReOpenFile(
            file.as_raw_handle(),
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            0,
        )
    };
    (handle != INVALID_HANDLE_VALUE).then(|| unsafe { File::from_raw_handle(handle) })
}
#[cfg(not(windows))]
fn reopen(_: &File) -> Option<File> {
    None
}
fn own<'a>(slot: &'a mut Option<Option<File>>, shared: &'a File) -> &'a File {
    slot.get_or_insert_with(|| reopen(shared))
        .as_ref()
        .unwrap_or(shared)
}

// Count CR, LF and CRLF as single terminators. On AVX2 hosts 32 input
// positions, including pairs crossing adjacent vectors, are tested together.
pub(crate) fn count(bytes: &[u8], next: Option<u8>) -> u64 {
    #[cfg(target_arch = "x86_64")]
    if bytes.len() >= 64 && std::is_x86_feature_detected!("avx2") {
        return unsafe { count_avx2(bytes, next) };
    }
    count_tail(bytes, next)
}
fn count_tail(bytes: &[u8], next: Option<u8>) -> u64 {
    memchr::memchr2_iter(b'\n', b'\r', bytes)
        .filter(|&i| bytes[i] == b'\n' || bytes.get(i + 1).copied().or(next) != Some(b'\n'))
        .count() as u64
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn count_avx2(bytes: &[u8], next: Option<u8>) -> u64 {
    use std::arch::x86_64::*;
    let lf = _mm256_set1_epi8(10);
    let cr = _mm256_set1_epi8(13);
    let mut i = 0;
    let mut n = 0u64;
    while i + 32 < bytes.len() {
        let block = _mm256_loadu_si256(bytes.as_ptr().add(i).cast());
        let following = _mm256_loadu_si256(bytes.as_ptr().add(i + 1).cast());
        let mask = _mm256_or_si256(
            _mm256_cmpeq_epi8(block, lf),
            _mm256_andnot_si256(
                _mm256_cmpeq_epi8(following, lf),
                _mm256_cmpeq_epi8(block, cr),
            ),
        );
        n += (_mm256_movemask_epi8(mask) as u32).count_ones() as u64;
        i += 32;
    }
    n + count_tail(&bytes[i..], next)
}

enum Backing {
    Disk(Arc<File>, u64),
    Memory(Box<[u8]>),
    Recovery(Box<crate::recovery_blocks::Packed>),
    /// Old text with many matches replaced, put together on read.
    Patched(Box<crate::patched::Patched>),
}
struct Page {
    number: u64,
    data: Box<[u8]>,
    ends: std::sync::OnceLock<Vec<u32>>,
}
/// What reading from the original file, in place of its clone, goes by.
#[cfg(target_os = "macos")]
#[derive(Default)]
struct Warm {
    /// How many more times a read from the original may turn out to have
    /// been for nothing before the clone is read for good. None at the start:
    /// reading from the original begins when the snapshot is a clone.
    left: std::sync::atomic::AtomicU32,
    /// The original's size and date when it was last known to hold nothing
    /// but the clone's text and what this document itself wrote into it.
    known: Mutex<Option<super::Stamp>>,
    view: std::sync::OnceLock<Option<crate::mac_save::View>>,
}
struct External {
    path: PathBuf,
    snapshot: std::sync::OnceLock<File>,
    /// Where this source's first byte is in `snapshot`: 0 in a copy of just
    /// this text, the original's own offset in a clone of the whole file.
    snapshot_origin: AtomicU64,
    /// Whether text may be read from the original where its blocks show it
    /// to hold what the snapshot, a clone of it, holds.
    #[cfg(target_os = "macos")]
    warm: Warm,
    copy_lock: Mutex<()>,
    #[cfg(windows)]
    lease: Mutex<Option<crate::write_lease::Lease>>,
    #[cfg(windows)]
    retained_guard: Mutex<Option<File>>,
    #[cfg(windows)]
    checkpoint_folder: Mutex<Option<Arc<tempfile::TempDir>>>,
    #[cfg(windows)]
    delta: RwLock<delta::OriginalView>,
}
pub(crate) struct Source {
    backing: Backing,
    external: Option<External>,
    // A plugin can request a durable mapping even for an ordinary captured file.
    ordinary_mapping: Option<Mutex<Option<File>>>,
    unicode: std::sync::OnceLock<Result<crate::unicode::UnicodeIndex, String>>,
    pub(super) recorded: Option<super::Stamp>,
    pub len: u64,
    prefix: AtomicU64,
    counts: RwLock<Vec<u64>>,
    cache: Mutex<[Option<Arc<Page>>; SLOTS]>,
    error: Mutex<Option<String>>,
    /// What scans with a lead have left mapped in Windows' cache (see `lead.rs`).
    views: Mutex<lead::Views>,
    /// Whether the background line count has been started.
    index_started: std::sync::atomic::AtomicBool,
}
impl Source {
    pub fn file(file: Arc<File>, origin: u64, len: u64) -> io::Result<Arc<Self>> {
        Self::from_file(file,origin,len,None)
    }
    pub fn external(file: Arc<File>, origin: u64, len: u64, path: &Path) -> io::Result<Arc<Self>> {
        let external = External {
            path: path.into(),
            snapshot: Default::default(),
            snapshot_origin: Default::default(),
            #[cfg(target_os = "macos")]
            warm: Default::default(),
            copy_lock: Default::default(),
            #[cfg(windows)]
            lease: Default::default(),
            #[cfg(windows)]
            retained_guard: Default::default(),
            #[cfg(windows)]
            checkpoint_folder: Default::default(),
            #[cfg(windows)]
            delta: Default::default(),
        };
        Self::from_file(file,origin,len,Some(external))
    }
    fn from_file(file: Arc<File>, origin: u64, len: u64, external: Option<External>) -> io::Result<Arc<Self>> {
        let recorded = Some(super::Stamp::from(&file)?);
        Self::indexed_backing(Backing::Disk(file, origin), len, external, recorded)
    }
    pub(crate) fn patched(text: crate::patched::Patched) -> io::Result<Arc<Self>> {
        let len = text.len();
        Self::indexed_backing(Backing::Patched(Box::new(text)), len, None, None)
    }
    pub(crate) fn recovery(packed: crate::recovery_blocks::Packed, len: u64) -> io::Result<Arc<Self>> {
        Self::indexed_backing(Backing::Recovery(Box::new(packed)), len, None, None)
    }
    fn indexed_backing(backing: Backing, len: u64, external: Option<External>, recorded: Option<super::Stamp>) -> io::Result<Arc<Self>> {
        let s = Arc::new(Self {
            unicode: Default::default(),
            backing,
            external,
            ordinary_mapping: None,
            recorded,
            len,
            prefix: AtomicU64::new(0),
            counts: RwLock::new(vec![0]),
            cache: Default::default(),
            error: Mutex::new(None),
            views: Default::default(),
            index_started: Default::default(),
        });
        if len <= PAGE as u64 {
            let mut buf = vec![0; len as usize];
            s.raw(0, &mut buf)?;
            if len != 0 {
                s.counts.write().unwrap().push(count(&buf, None));
            }
            s.prefix.store(len, Ordering::Release);
        } else if LINE_INDEXING.load(Ordering::Relaxed) && !DEFER_INDEXING.get() {
            s.start_index()?;
        }
        Ok(s)
    }
    pub fn memory(bytes: &[u8]) -> Arc<Self> {
        Self::from_bytes(bytes.to_vec(), false)
    }
    pub fn owned(bytes: Vec<u8>) -> Arc<Self> {
        Self::from_bytes(bytes, true)
    }
    fn from_bytes(bytes: Vec<u8>, original: bool) -> Arc<Self> {
        let mut counts = vec![0];
        let mut total = 0;
        for (i, block) in bytes.chunks(PAGE).enumerate() {
            total += count(block, bytes.get((i + 1) * PAGE).copied());
            counts.push(total);
        }
        let len = bytes.len() as u64;
        Arc::new(Self {
            unicode: Default::default(),
            backing: Backing::Memory(bytes.into_boxed_slice()),
            external: None,
            ordinary_mapping: original.then(Mutex::default),
            recorded: None,
            len,
            prefix: AtomicU64::new(len),
            counts: RwLock::new(counts),
            cache: Default::default(),
            error: Mutex::new(None),
            views: Default::default(),
            // Counted already, as it was made.
            index_started: std::sync::atomic::AtomicBool::new(true),
        })
    }
    pub(crate) fn disk_at(&self,at:u64)->io::Result<Option<(File,u64)>>{
        if let Some(file) = self.private_file() {return Ok(Some((file.try_clone()?,self.private_origin()+at)));}
        if let (Some(slot), Backing::Memory(bytes)) = (&self.ordinary_mapping,&self.backing) {
            let mut slot = slot.lock().unwrap();
            if slot.is_none() {
                let mut file = tempfile::tempfile()?;
                file.write_all(bytes)?;
                *slot = Some(file);
            }
            return Ok(Some((slot.as_ref().unwrap().try_clone()?, at)));
        }
        match &self.backing {
            Backing::Memory(_) | Backing::Recovery(_) | Backing::Patched(_) => Ok(None),
            Backing::Disk(file, origin) => Ok(Some((file.try_clone()?, origin + at))),
        }
    }
    pub(crate) fn memory_backed(&self) -> bool {
        self.ordinary_mapping.is_none() && matches!(self.backing, Backing::Memory(_))
    }
    pub(crate) fn unicode(&self) -> io::Result<&crate::unicode::UnicodeIndex> {
        self.unicode_cancel(&std::sync::atomic::AtomicBool::new(false))
    }
    pub(crate) fn unicode_cancel(
        &self,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> io::Result<&crate::unicode::UnicodeIndex> {
        if self.unicode.get().is_none() {
            let result = crate::unicode::UnicodeIndex::build(self, cancel);
            if result
                .as_ref()
                .is_err_and(|e| e.kind() == io::ErrorKind::Interrupted)
            {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let _ = self.unicode.set(result.map_err(|e| e.to_string()));
        }
        self.unicode
            .get()
            .unwrap()
            .as_ref()
            .map_err(|e| super::invalid(e.clone()))
    }
    pub fn file_id(&self) -> Option<(u32, u64)> {
        if self.private_file().is_some() {
            return None;
        }
        self.recorded.as_ref().map(|s| (s.volume, s.id))
    }
    fn private_file(&self) -> Option<&File> {
        self.external.as_ref().and_then(|e| e.snapshot.get())
    }
    fn private_origin(&self) -> u64 {
        self.external.as_ref().map_or(0, |e| e.snapshot_origin.load(Ordering::Acquire))
    }
    /// Reads from the original file and says whether what was read is the
    /// snapshot's text. The system keeps what it has of a file in memory for
    /// that file alone, so a clone of a file that is in memory is read from
    /// the drive. The original holds the clone's text wherever its blocks
    /// are still the clone's: any write to it, by anyone, moves the written
    /// part to new blocks. Asked after the read, with what other programs
    /// changed through memory maps written out first, that settles what was
    /// read. It is only asked while the original's size and date are the
    /// ones last known: a program writing to the file in the ordinary way
    /// changes them, and beside such a program the system was found not to
    /// write out every mapped change it is asked for.
    #[cfg(target_os = "macos")]
    fn read_warm(&self, original: &File, clone: &File, origin: u64, pos: u64, dst: &mut [u8]) -> bool {
        // Asking costs a few calls to the system, about as much as copying
        // a hundred kilobytes that are in memory. A small read gains nothing
        // from it and is left to the clone, which holds what it has read.
        const WORTH_ASKING: usize = 256 * 1024;
        let Some(external) = &self.external else { return false };
        let warm = &external.warm;
        if dst.len() < WORTH_ASKING || warm.left.load(Ordering::Acquire) == 0 || self.private_origin() != origin {
            return false;
        }
        let spend = |strikes: u32| {
            let _ = warm.left.fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| Some(left.saturating_sub(strikes)));
        };
        let Some(view) = warm.view.get_or_init(|| crate::mac_save::View::of(original)) else {
            spend(u32::MAX);
            return false;
        };
        if read_at(original, origin + pos, dst).is_err() {
            spend(1);
            return false;
        }
        let began = std::time::Instant::now();
        let same = crate::mac_save::same_blocks(original, view, clone, origin + pos, dst.len() as u64);
        // Writing out a busy writer's changes at every read would cost more
        // than the clone, and the other program too. That takes milliseconds;
        // many threads asking at once take some hundreds of microseconds.
        if began.elapsed() > std::time::Duration::from_millis(3) {
            spend(8);
        }
        match same {
            // A part this document or another program has written to.
            Some(false) => false,
            Some(true) => {
                let unwritten = super::Stamp::from(original).ok() == *warm.known.lock().unwrap();
                if !unwritten {
                    spend(1);
                }
                unwritten
            }
            // A volume that will not say: no use asking again.
            None => {
                spend(u32::MAX);
                false
            }
        }
    }
    /// This document has written into the original itself, which now has
    /// this size and date.
    #[cfg(target_os = "macos")]
    pub(crate) fn wrote_in_place(&self, now: &super::Stamp) {
        if let Some(external) = &self.external {
            if self.recorded.as_ref().is_some_and(|was| was.id == now.id && was.volume == now.volume) {
                *external.warm.known.lock().unwrap() = Some(now.clone());
            }
        }
    }
    /// A file holding this source's bytes that no other program can change,
    /// and where the source starts in it: what a save can clone.
    #[cfg(target_os = "macos")]
    pub(crate) fn settled(&self) -> Option<(&File, u64)> {
        self.private_file().map(|file| (file, self.private_origin()))
    }
    #[cfg(windows)]
    pub(crate) fn holds_unchanged_file(&self, stamp: &super::Stamp) -> bool {
        self.recorded.as_ref().is_some_and(|r| r.volume == stamp.volume && r.id == stamp.id)
            && self.external.as_ref().is_some_and(|e| {
                let lease = e.lease.lock().unwrap();
                let view = e.delta.read().unwrap();
                (lease.is_some() || e.retained_guard.lock().unwrap().is_some())
                    && e.snapshot.get().is_none() && !view.is_pinned()
                    && view.current().is_none_or(|current| current == stamp)
            })
    }
    /// Temporarily suspend our own rename notification under a second deny-write
    /// handle to the same file. Rearm on the actual surviving name before opening
    /// the stream to external writers again, including on publication failure.
    #[cfg(windows)]
    pub(crate) fn publish_guarded<T>(
        self: &Arc<Self>, from: &Path, after: &Path, expected: &super::Stamp,
        folder: Option<Arc<tempfile::TempDir>>, publish: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<Option<T>> {
        let Some(external) = &self.external else { return Ok(None) };
        let guard = super::revision::guard_destination(from, expected)?;
        let mut slot = external.lease.lock().unwrap();
        if external.snapshot.get().is_some()
            || (slot.is_none() && external.retained_guard.lock().unwrap().is_none()) {
            return Ok(None);
        }
        drop(slot.take());
        // The new guard pins the same checked object throughout this exchange.
        drop(external.retained_guard.lock().unwrap().take());
        // A genuine competing break might have finished before cancellation.
        if external.snapshot.get().is_some() { return Ok(None); }
        let result = publish();
        let name = if result.is_ok() { after } else { from };
        if result.is_ok() {
            let private = folder.is_some() && super::checkpoint::link_count(&guard).ok() == Some(1);
            *external.checkpoint_folder.lock().unwrap() = folder;
            if private {
                // The sole remaining name is our private Undo checkpoint. Keep
                // the existing deny-write handle; acquiring a new oplock here
                // could otherwise force an entire extra snapshot on a break.
                *external.retained_guard.lock().unwrap() = Some(guard);
                return result.map(Some);
            }
        }
        *slot = crate::write_lease::Lease::acquire(name, expected, self);
        if slot.is_none() && self.materialize(expected, false).is_err() {
            // Preserve the only known-good original if both the filesystem's
            // rearm and the private copy fail. The draft remains available.
            *external.retained_guard.lock().unwrap() = Some(guard);
        }
        result.map(Some)
    }
    /// Save and undo retain the exact bytes visible before the first local edit.
    /// Windows normally guards an original without copying it until a writer opens.
    pub fn protect(self: &Arc<Self>, accepted: Option<&super::Stamp>) -> io::Result<()> {
        if self.try_protect(accepted, true)? {
            return Ok(());
        }
        self.materialize(
            accepted
                .or(self.recorded.as_ref())
                .expect("disk source stamp"),
            true,
        )
    }
    pub(crate) fn try_protect(
        self: &Arc<Self>,
        accepted: Option<&super::Stamp>,
        wait: bool,
    ) -> io::Result<bool> {
        let Some(external) = &self.external else {
            return Ok(true);
        };
        if external.snapshot.get().is_some() {
            return Ok(true);
        }
        let expected = accepted
            .or(self.recorded.as_ref())
            .expect("disk source stamp");
        #[cfg(windows)]
        if self.len >= 1024 * 1024 {
            let mut lease = if wait {
                external.lease.lock().unwrap()
            } else {
                match external.lease.try_lock() {
                    Ok(guard) => guard,
                    Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
                    Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
                }
            };
            if lease.is_some()
                || external.retained_guard.lock().unwrap().is_some()
                || external.snapshot.get().is_some()
            {
                return Ok(true);
            }
            if let Some(guard) = crate::write_lease::Lease::acquire(&external.path, expected, self)
            {
                *lease = Some(guard);
                return Ok(true);
            }
        }
        Ok(false)
    }
    pub(crate) fn materialize(&self, expected: &super::Stamp, lock_region: bool) -> io::Result<()> {
        self.copy_with_progress(expected, lock_region, &mut |_, _| Ok(()))
    }
    pub(crate) fn copy_with_progress(
        &self,
        expected: &super::Stamp,
        lock_region: bool,
        progress: &mut impl FnMut(u64, u64) -> io::Result<()>,
    ) -> io::Result<()> {
        let Some(external) = &self.external else {
            return Ok(());
        };
        let _exclusive = external.copy_lock.lock().unwrap();
        if external.snapshot.get().is_some() {
            return Ok(());
        }
        let Backing::Disk(original, origin) = &self.backing else {
            unreachable!()
        };
        #[cfg(windows)]
        let pinned = external.delta.read().unwrap().is_pinned();
        #[cfg(not(windows))]
        let pinned = false;
        if !pinned && &super::Stamp::from(original)? != expected {
            return Err(super::conflict());
        }
        // On APFS a clone is that copy without the reading and writing: it
        // shares the file's blocks until one of the two is written to.
        #[cfg(target_os = "macos")]
        if let Some(clone) = crate::mac_save::private_clone(original, &external.path) {
            if &super::Stamp::from(original)? != expected {
                return Err(super::conflict());
            }
            external.snapshot_origin.store(*origin, Ordering::Release);
            *external.warm.known.lock().unwrap() = Some(expected.clone());
            external.warm.left.store(64, Ordering::Release);
            let _ = external.snapshot.set(clone);
            return Ok(());
        }
        // When another writer already has a handle, no deny-write lease can be
        // granted. A shared range lock keeps ordinary overlapped writes out of
        // the stream while this fallback takes its independent checkpoint.
        #[cfg(windows)]
        let _range = if lock_region {
            Some(crate::write_lease::ReadRange::lock(
                original, *origin, self.len,
            )?)
        } else {
            None
        };
        let mut temp = tempfile::tempfile()?;
        let mut block = vec![0; (self.len.min(1024 * 1024)) as usize];
        let mut offset = 0;
        while offset < self.len {
            progress(offset, self.len)?;
            let count = (self.len - offset).min(block.len() as u64) as usize;
            self.raw(offset, &mut block[..count])?;
            temp.write_all(&block[..count])?;
            offset += count as u64;
        }
        temp.sync_data()?;
        if !pinned && &super::Stamp::from(original)? != expected {
            return Err(super::conflict());
        }
        let _ = external.snapshot.set(temp);
        #[cfg(windows)]
        external.delta.write().unwrap().clear_pin();
        Ok(())
    }
    pub(crate) fn prepare_mapping(&self, accepted: Option<&super::Stamp>) -> io::Result<bool> {
        if let Some(external) = &self.external {
            // A mapped pointer outlives this call and cannot follow a later backing swap.
            #[cfg(windows)]
            let lease = external.lease.lock().unwrap();
            #[cfg(windows)]
            if self.private_file().is_none() && external.delta.read().unwrap().is_virtual() {
                // file_range's caller assembles this requested range from pieces.
                // Returning the physical old handle would expose the edited bytes.
                return Ok(false);
            }
            #[cfg(windows)]
            let lock_region = lease.is_none();
            #[cfg(not(windows))]
            let lock_region = false;
            self.materialize(accepted.or(self.recorded.as_ref()).expect("disk source stamp"), lock_region)?;
        }
        Ok(true)
    }
    pub fn validate(&self, accepted: Option<&super::Stamp>) -> io::Result<()> {
        if self.private_file().is_some() {return Ok(());}
        #[cfg(windows)]
        if let Some(external) = &self.external {
            let view = external.delta.read().unwrap();
            if view.is_pinned() { return Ok(()); }
            if let Some(current) = view.current() {
                if let Backing::Disk(file, _) = &self.backing {
                    if &super::Stamp::from(file)? == accepted.unwrap_or(current) { return Ok(()); }
                    return Err(super::conflict());
                }
            }
        }
        if let (Backing::Disk(file, _), Some(expected)) =
            (&self.backing, accepted.or(self.recorded.as_ref()))
        {
            if &super::Stamp::from(file)? != expected {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists,
                    "A source file was modified in place. Reload it before saving edits that use its contents."));
            }
        }
        Ok(())
    }
    /// Count the line breaks of one batch, page by page.
    fn tally(&self, lane: &mut Lane, lead: Option<&Lead>, batch: u64, buffer: &mut Vec<u8>) -> io::Result<Vec<u32>> {
        let pos = batch * BATCH as u64;
        let length = (self.len - pos).min(BATCH as u64) as usize;
        let read = length + usize::from(pos + (length as u64) < self.len);
        buffer.resize(BATCH + 1, 0);
        match lead {
            Some(lead) => {
                lead.advance(lane, pos)?;
                lead.read(self, lane, pos, &mut buffer[..read])?;
            }
            None => self.read_lane(lane, pos, &mut buffer[..read])?,
        }
        Ok((0..length)
            .step_by(PAGE)
            .map(|i| {
                let end = (i + PAGE).min(length);
                count(&buffer[i..end], buffer[..read].get(end).copied()) as u32
            })
            .collect())
    }
    /// A large file is counted by several threads, each with its own handle.
    /// The directory still grows from the start, so the indexed part of the
    /// document is usable while the rest is counted.
    /// Start counting lines in the background, once.
    pub(crate) fn start_index(self: &Arc<Self>) -> io::Result<()> {
        if self.len <= PAGE as u64 || self.index_started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let weak = Arc::downgrade(self);
        thread::Builder::new()
            .name("Linework line index".into())
            .spawn(move || Self::index(weak))?;
        Ok(())
    }
    fn index(weak: Weak<Source>) {
        let Some(len) = weak.upgrade().map(|s| s.len) else {
            return;
        };
        if weak.upgrade().is_some_and(|source| source.load_index()) {
            return;
        }
        let batches = len.div_ceil(BATCH as u64) as usize;
        let workers = crate::parallel::threads().min(batches / PARALLEL_BATCHES);
        if workers < 2 {
            return Self::index_alone(weak);
        }
        // Every step takes the source again, so closing the document ends the
        // work after the batches in hand.
        let gone = || io::Error::from(io::ErrorKind::Interrupted);
        let mut total = 0u64;
        // The lead holds the source weakly, so closing the document still ends the count.
        let lead = weak.upgrade().and_then(|source| source.lead(0));
        let result = crate::parallel::ordered(
            batches,
            workers,
            || (Lane::default(), Vec::new()),
            |batch, (lane, buffer)| {
                let source = weak.upgrade().ok_or_else(gone)?;
                source.tally(lane, lead.as_ref(), batch as u64, buffer)
            },
            |batch, pages| {
                let source = weak.upgrade().ok_or_else(gone)?;
                let directory: Vec<u64> = pages
                    .into_iter()
                    .map(|n| {
                        total += u64::from(n);
                        total
                    })
                    .collect();
                source.counts.write().unwrap().extend(directory);
                let done = ((batch + 1) as u64 * BATCH as u64).min(source.len);
                // The finished count is kept before it is said to be finished: a program that
                // only waited for the count ends then, and would end this thread in the keeping.
                if done == source.len {
                    source.store_index();
                }
                source.prefix.store(done, Ordering::Release);
                Ok(false)
            },
        );
        if let (Some(source), Err(e)) = (weak.upgrade(), result) {
            *source.error.lock().unwrap() = Some(e.to_string());
        }
    }
    fn index_alone(weak: Weak<Source>) {
        let mut buffer = vec![0; BATCH + 1];
        let mut pos = 0;
        let mut count_total = 0;
        while let Some(s) = weak.upgrade() {
            if pos == s.len {
                return;
            }
            let length = (s.len - pos).min(BATCH as u64) as usize;
            let read = length + usize::from(pos + (length as u64) < s.len);
            if let Err(e) = s.raw(pos, &mut buffer[..read]) {
                *s.error.lock().unwrap() = Some(e.to_string());
                return;
            }
            let mut directory = Vec::with_capacity(length.div_ceil(PAGE));
            for i in (0..length).step_by(PAGE) {
                let end = (i + PAGE).min(length);
                count_total += count(&buffer[i..end], buffer[..read].get(end).copied());
                directory.push(count_total);
            }
            s.counts.write().unwrap().extend(directory);
            pos += length as u64;
            // Kept before it is said to be finished, as in `index`.
            if pos == s.len {
                s.store_index();
            }
            s.prefix.store(pos, Ordering::Release);
        }
    }
    pub fn indexed(&self) -> u64 {
        self.prefix.load(Ordering::Acquire)
    }
    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
    fn raw(&self, pos: u64, dst: &mut [u8]) -> io::Result<()> {
        match &self.backing {
            Backing::Disk(f, origin) => {
                if let Some(copy) = self.private_file() {
                    #[cfg(target_os = "macos")]
                    if self.read_warm(f, copy, *origin, pos, dst) { return Ok(()); }
                    return read_at(copy,self.private_origin()+pos,dst);
                }
                #[cfg(windows)]
                let result = match &self.external {
                    Some(external) => external.delta.read().unwrap().read(f, *origin, pos, dst),
                    None => read_at(f, origin + pos, dst),
                };
                #[cfg(not(windows))]
                let result = read_at(f, origin + pos, dst);
                // A conflicting open may have been released during this read.
                if let Some(copy) = self.private_file() {return read_at(copy,self.private_origin()+pos,dst);}
                result
            }
            Backing::Recovery(archive) => archive.read(pos, dst),
            Backing::Patched(text) => text.read(pos, dst),
            Backing::Memory(m) => {
                dst.copy_from_slice(&m[pos as usize..pos as usize + dst.len()]);
                Ok(())
            }
        }
    }
    pub(crate) fn on_disk(&self) -> bool {
        matches!(self.backing, Backing::Disk(..))
    }
    /// The same bytes as `raw`, read through the calling thread's own handles.
    pub(crate) fn read_lane(&self, lane: &mut Lane, pos: u64, dst: &mut [u8]) -> io::Result<()> {
        let Backing::Disk(shared, origin) = &self.backing else {
            return self.raw(pos, dst);
        };
        if let Some(copy) = self.private_file() {
            #[cfg(target_os = "macos")]
            if self.read_warm(shared, copy, *origin, pos, dst) { return Ok(()); }
            return read_at(own(&mut lane.copy, copy), self.private_origin() + pos, dst);
        }
        let file = own(&mut lane.original, shared);
        #[cfg(windows)]
        let result = match &self.external {
            Some(external) => external.delta.read().unwrap().read(file, *origin, pos, dst),
            None => read_at(file, origin + pos, dst),
        };
        #[cfg(not(windows))]
        let result = read_at(file, origin + pos, dst);
        // A conflicting open may have been released during this read.
        if let Some(copy) = self.private_file() {
            return read_at(own(&mut lane.copy, copy), self.private_origin() + pos, dst);
        }
        result
    }
    fn page(&self, index: u64) -> io::Result<Arc<Page>> {
        let slot = index as usize % SLOTS;
        if let Some(hit) = self.cache.lock().unwrap()[slot]
            .as_ref()
            .filter(|p| p.number == index)
            .cloned()
        {
            return Ok(hit);
        }
        let pos = index * PAGE as u64;
        let mut data = vec![0; (self.len - pos).min(PAGE as u64 + 1) as usize];
        self.raw(pos, &mut data)?;
        let p = Arc::new(Page {
            number: index,
            data: data.into_boxed_slice(),
            ends: Default::default(),
        });
        self.cache.lock().unwrap()[slot] = Some(p.clone());
        Ok(p)
    }
    pub fn at(&self, pos: u64) -> io::Result<u8> {
        if let Backing::Memory(m) = &self.backing {
            return Ok(m[pos as usize]);
        }
        Ok(self.page(pos / PAGE as u64)?.data[pos as usize % PAGE])
    }
    pub fn read(&self, mut pos: u64, mut dst: &mut [u8]) -> io::Result<()> {
        if dst.is_empty() {
            return Ok(());
        }
        if dst.len() >= PAGE || matches!(self.backing, Backing::Memory(_)) {
            return self.raw(pos, dst);
        }
        while !dst.is_empty() {
            let p = self.page(pos / PAGE as u64)?;
            let start = pos as usize % PAGE;
            let n = dst.len().min(PAGE - start);
            dst[..n].copy_from_slice(&p.data[start..start + n]);
            dst = &mut dst[n..];
            pos += n as u64;
        }
        Ok(())
    }
    fn ends(p: &Page) -> &[u32] {
        p.ends.get_or_init(|| {
            memchr::memchr2_iter(10, 13, &p.data[..p.data.len().min(PAGE)])
                .filter(|&i| p.data[i] == 10 || p.data.get(i + 1) != Some(&10))
                .map(|i| i as u32 + 1)
                .collect()
        })
    }
    pub fn rank(&self, pos: u64) -> io::Result<u64> {
        if pos == 0 {
            return Ok(0);
        }
        let i = pos as usize / PAGE;
        let j = pos as usize % PAGE;
        let counts = self.counts.read().unwrap();
        if pos == self.len && self.indexed() == self.len {
            return Ok(*counts.last().unwrap());
        }
        let prefix = counts[i];
        drop(counts);
        if j == 0 {
            return Ok(prefix);
        }
        if let Backing::Memory(m) = &self.backing {
            if m.len() <= 2048 {
                return Ok(count(&m[..j], m.get(j).copied()));
            }
        }
        Ok(prefix + Self::ends(&*self.page(i as u64)?).partition_point(|&v| v <= j as u32) as u64)
    }
    pub fn select(&self, n: u64) -> io::Result<u64> {
        let counts = self.counts.read().unwrap();
        let i = counts.partition_point(|&v| v < n);
        if i == counts.len() {
            return Ok(self.len);
        }
        let ordinal = (n - counts[i - 1] - 1) as usize;
        drop(counts);
        Ok(((i - 1) * PAGE) as u64 + Self::ends(&*self.page((i - 1) as u64)?)[ordinal] as u64)
    }
}
