use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::thread;

#[cfg(windows)]
#[path = "source_delta.rs"]
mod delta;

pub(crate) const PAGE: usize = 65536;
const BATCH: usize = PAGE * 32;
const SLOTS: usize = 12;

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

// Count CR, LF and CRLF as single terminators. On AVX2 hosts 32 input
// positions, including pairs crossing adjacent vectors, are tested together.
fn count(bytes: &[u8], next: Option<u8>) -> u64 {
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
}
struct Page {
    number: u64,
    data: Box<[u8]>,
    ends: std::sync::OnceLock<Vec<u32>>,
}
struct External {
    path: PathBuf,
    snapshot: std::sync::OnceLock<File>,
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
}
impl Source {
    pub fn file(file: Arc<File>, origin: u64, len: u64) -> io::Result<Arc<Self>> {
        Self::from_file(file,origin,len,None)
    }
    pub fn external(file: Arc<File>, origin: u64, len: u64, path: &Path) -> io::Result<Arc<Self>> {
        let external = External {
            path: path.into(),
            snapshot: Default::default(),
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
        });
        if len <= PAGE as u64 {
            let mut buf = vec![0; len as usize];
            s.raw(0, &mut buf)?;
            if len != 0 {
                s.counts.write().unwrap().push(count(&buf, None));
            }
            s.prefix.store(len, Ordering::Release);
        } else {
            let weak = Arc::downgrade(&s);
            thread::Builder::new()
                .name("Linework line index".into())
                .spawn(move || Self::index(weak))?;
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
        })
    }
    pub(crate) fn disk_at(&self,at:u64)->io::Result<Option<(File,u64)>>{
        if let Some(file) = self.private_file() {return Ok(Some((file.try_clone()?,at)));}
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
            Backing::Memory(_) | Backing::Recovery(_) => Ok(None),
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
    fn index(weak: Weak<Source>) {
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
                if let Some(copy) = self.private_file() {return read_at(copy,pos,dst);}
                #[cfg(windows)]
                let result = match &self.external {
                    Some(external) => external.delta.read().unwrap().read(f, *origin, pos, dst),
                    None => read_at(f, origin + pos, dst),
                };
                #[cfg(not(windows))]
                let result = read_at(f, origin + pos, dst);
                // A conflicting open may have been released during this read.
                if let Some(copy) = self.private_file() {return read_at(copy,pos,dst);}
                result
            }
            Backing::Recovery(archive) => archive.read(pos, dst),
            Backing::Memory(m) => {
                dst.copy_from_slice(&m[pos as usize..pos as usize + dst.len()]);
                Ok(())
            }
        }
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
