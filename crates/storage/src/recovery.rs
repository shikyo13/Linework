use super::recovery_blocks::{self, Packed};
use super::{codec::Codec, hash, invalid, tree, Document, Source, Stamp};
use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use tempfile::NamedTempFile;

#[derive(Serialize, Deserialize, Clone)]
struct Base {
    file: String,
    bytes: u64,
    digest: [u8; 32],
    #[serde(default)]
    hash: hash::Kind,
    #[serde(default)]
    format: Format,
}
#[derive(Serialize, Deserialize, Default, Clone, Copy)]
enum Format {
    #[default]
    Deflate,
    Blocks,
    Clone {
        offset: u64,
    },
}
#[derive(Serialize, Deserialize, Clone)]
struct SavedPatch {
    digest: [u8; 32],
    #[serde(default)]
    hash: hash::Kind,
    #[serde(flatten)]
    plan: PatchPlan,
}
#[derive(Serialize, Deserialize, Clone)]
#[serde(untagged)]
enum PatchPlan {
    // The field layout of a single edit is the original LWR3 format.
    Single {
        at: u64,
        removed: u64,
        inserted: Vec<u8>,
    },
    Ranges {
        spans: Vec<SavedSpan>,
    },
}
#[derive(Serialize, Deserialize, Clone)]
enum SavedSpan {
    Copy(u64, u64),
    Insert(Vec<u8>),
}
const MAX_PATCH: u64 = 64 * 1024;
const MAX_SPANS: usize = 1024;

impl SavedPatch {
    fn length(&self, base: u64) -> io::Result<u64> {
        let bad = || invalid("Invalid saved revision patch");
        match &self.plan {
            PatchPlan::Single {
                at,
                removed,
                inserted,
            } => {
                if *at > base
                    || *removed > base - at
                    || (*removed).max(inserted.len() as u64) > MAX_PATCH
                {
                    return Err(bad());
                }
                (base - removed)
                    .checked_add(inserted.len() as u64)
                    .ok_or_else(bad)
            }
            PatchPlan::Ranges { spans } => {
                if spans.len() > MAX_SPANS {
                    return Err(bad());
                }
                let mut total = 0u64;
                let mut literals = 0u64;
                for span in spans {
                    let size = match span {
                        SavedSpan::Copy(at, len) if *at <= base && *len <= base - at => *len,
                        SavedSpan::Insert(bytes) => {
                            let n = bytes.len() as u64;
                            literals = literals
                                .checked_add(n)
                                .filter(|v| *v <= MAX_PATCH)
                                .ok_or_else(bad)?;
                            n
                        }
                        _ => return Err(bad()),
                    };
                    total = total.checked_add(size).ok_or_else(bad)?;
                }
                if total.abs_diff(base) > MAX_PATCH {
                    return Err(bad());
                }
                Ok(total)
            }
        }
    }
    fn apply(&self, root: tree::Root) -> io::Result<tree::Root> {
        self.length(root.as_ref().map_or(0, |n| n.len))?;
        let saved = match &self.plan {
            PatchPlan::Single {
                at,
                removed,
                inserted,
            } => {
                let added = tree::leaf(Source::memory(inserted), 0, inserted.len() as u64)?;
                tree::replace(root, *at, *removed, added)?
            }
            PatchPlan::Ranges { spans } => {
                let mut result = None;
                for span in spans {
                    let piece = match span {
                        SavedSpan::Copy(at, len) => tree::slice(root.clone(), *at, *len)?,
                        SavedSpan::Insert(bytes) => {
                            tree::leaf(Source::memory(bytes), 0, bytes.len() as u64)?
                        }
                    };
                    result = tree::join(result, piece);
                }
                result
            }
        };
        if digest_root(&saved, io::sink(), &AtomicBool::new(false), self.hash)? != self.digest {
            return Err(invalid("Saved revision failed its integrity check"));
        }
        Ok(saved)
    }
}

enum Span<'a> {
    Copy(u64, u64),
    Literal(&'a Source, u64, u64),
}

// Both checkpoint records and saved revisions refer to immutable source ranges.
// The visitor can stop before reading a literal that would exceed its budget.
fn visit_spans(
    basis: &tree::Root,
    current: &tree::Root,
    cancel: &AtomicBool,
    mut visit: impl FnMut(Span<'_>) -> io::Result<bool>,
) -> io::Result<bool> {
    let mut index: HashMap<usize, Vec<(u64, u64, u64)>> = HashMap::new();
    let mut offset = 0;
    let mut leaves = tree::Leaves::new(basis, false);
    while let Some((source, start, len)) = leaves.next() {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        index
            .entry(Arc::as_ptr(source) as usize)
            .or_default()
            .push((start, start + len, offset));
        offset += len;
    }
    for spans in index.values_mut() {
        spans.sort_unstable();
    }
    let mut leaves = tree::Leaves::new(current, false);
    while let Some((source, mut pos, len)) = leaves.next() {
        let end = pos + len;
        let ranges = index
            .get(&(Arc::as_ptr(source) as usize))
            .map(Vec::as_slice)
            .unwrap_or_default();
        while pos < end {
            if cancel.load(Ordering::Relaxed) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let i = ranges.partition_point(|v| v.0 <= pos);
            let (span, next) = if i > 0 && ranges[i - 1].1 > pos {
                let (from, to, offset) = ranges[i - 1];
                let next = to.min(end);
                (Span::Copy(offset + (pos - from), next - pos), next)
            } else {
                let next = ranges.get(i).map_or(end, |p| p.0.min(end));
                (Span::Literal(source, pos, next - pos), next)
            };
            if !visit(span)? {
                return Ok(false);
            }
            pos = next;
        }
    }
    Ok(true)
}

fn saved_ranges(
    basis: &tree::Root,
    current: &tree::Root,
    cancel: &AtomicBool,
) -> io::Result<Option<PatchPlan>> {
    let mut spans = Vec::new();
    let mut literal_bytes = 0;
    let complete = visit_spans(basis, current, cancel, |span| {
        match span {
            Span::Copy(at, len) => {
                if let Some(SavedSpan::Copy(start, size)) = spans.last_mut() {
                    if *start + *size == at {
                        *size += len;
                        return Ok(true);
                    }
                }
                if spans.len() == MAX_SPANS {
                    return Ok(false);
                }
                spans.push(SavedSpan::Copy(at, len));
            }
            Span::Literal(source, at, len) => {
                if len > MAX_PATCH - literal_bytes {
                    return Ok(false);
                }
                if !matches!(spans.last(), Some(SavedSpan::Insert(_))) {
                    if spans.len() == MAX_SPANS {
                        return Ok(false);
                    }
                    spans.push(SavedSpan::Insert(Vec::new()));
                }
                let Some(SavedSpan::Insert(bytes)) = spans.last_mut() else {
                    unreachable!()
                };
                let start = bytes.len();
                bytes.resize(start + len as usize, 0);
                source.read(at, &mut bytes[start..])?;
                literal_bytes += len;
            }
        }
        Ok(true)
    })?;
    Ok(complete.then_some(PatchPlan::Ranges { spans }))
}
pub(super) struct Archive {
    root: tree::Root,
    path: PathBuf,
    base: Base,
    patch: Option<(tree::Root, SavedPatch)>,
    // A detached draft may migrate a verified private archive to its new session.
    // The handle identifies the exact file, even after a path has been replaced.
    verified: Option<Arc<File>>,
}

fn digest_root(
    root: &tree::Root,
    mut out: impl Write,
    cancel: &AtomicBool,
    kind: hash::Kind,
) -> io::Result<[u8; 32]> {
    let mut hash = hash::Any::new(kind);
    let mut buf = vec![0; 512 * 1024];
    if let Some(root) = root {
        root.pieces(&mut |src, mut at, size| {
            let end = at + size;
            while at < end {
                if cancel.load(Ordering::Relaxed) {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let n = (end - at).min(buf.len() as u64) as usize;
                src.read(at, &mut buf[..n])?;
                hash.update(&buf[..n]);
                out.write_all(&buf[..n])?;
                at += n as u64;
            }
            Ok(())
        })?;
    }
    Ok(hash.finish())
}
impl Base {
    fn path(&self, checkpoint: &Path) -> io::Result<PathBuf> {
        let name = Path::new(&self.file);
        if name.file_name() != Some(name.as_os_str())
            || !self.file.starts_with("base-")
            || !self.file.ends_with(".lwbase")
        {
            return Err(invalid("Invalid recovery base name"));
        }
        Ok(checkpoint.with_file_name(name))
    }
    fn restore(&self, file: Arc<File>) -> io::Result<tree::Root> {
        let source = match self.format {
            Format::Blocks => Some(Source::recovery(
                Packed::open(file.try_clone()?, self.bytes)?,
                self.bytes,
            )?),
            Format::Clone { offset } => {
                if offset > 3 || self.bytes.checked_add(offset) != Some(file.metadata()?.len()) {
                    return Err(invalid("Invalid cloned recovery extent"));
                }
                Some(Source::file(file.clone(), offset, self.bytes)?)
            }
            Format::Deflate => None,
        };
        if let Some(source) = source {
            let root = tree::leaf(source, 0, self.bytes)?;
            if digest_root(&root, io::sink(), &AtomicBool::new(false), self.hash)? != self.digest {
                return Err(invalid("Recovery base failed its integrity check"));
            }
            return Ok(root);
        }
        let mut input = DeflateDecoder::new(&*file);
        let mut tmp = tempfile::tempfile()?;
        let mut hash = hash::Any::new(self.hash);
        let mut block = vec![0; 512 * 1024];
        let mut total = 0u64;
        loop {
            let n = input.read(&mut block)?;
            if n == 0 {
                break;
            }
            total = total
                .checked_add(n as u64)
                .filter(|v| *v <= self.bytes)
                .ok_or_else(|| invalid("Invalid recovery base size"))?;
            hash.update(&block[..n]);
            tmp.write_all(&block[..n])?;
        }
        if total != self.bytes || hash.finish() != self.digest {
            return Err(invalid("Recovery base failed its integrity check"));
        }
        tree::leaf(Source::file(Arc::new(tmp), 0, total)?, 0, total)
    }
}

#[derive(Serialize, Deserialize)]
struct Header {
    #[serde(default)]
    base: Option<Base>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    saved_patch: Option<SavedPatch>,
    path: Option<PathBuf>,
    stamp: Option<Stamp>,
    saved_encoding: String,
    encoding: String,
    bom: bool,
    length: u64,
}
fn number(r: &mut impl Read) -> io::Result<u64> {
    let mut a = [0; 8];
    r.read_exact(&mut a)?;
    Ok(u64::from_le_bytes(a))
}
fn read_archive(path: &Path) -> io::Result<Arc<File>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ};
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE);
    }
    options.open(path).map(Arc::new)
}

impl Archive {
    fn create(
        doc: &Document,
        root: tree::Root,
        dir: &Path,
        cancel: &AtomicBool,
    ) -> io::Result<Self> {
        let mut tmp = tempfile::Builder::new()
            .prefix("base-")
            .suffix(".lwbase")
            .tempfile_in(dir)?;
        let bytes = root.as_ref().map_or(0, |r| r.len);
        let file = tmp.as_file_mut();
        let (format, digest) = if bytes < recovery_blocks::BLOCK as u64 {
            let mut encoder = DeflateEncoder::new(&mut *file, Compression::fast());
            let digest = digest_root(&root, &mut encoder, cancel, hash::Kind::Blake3)?;
            encoder.finish()?;
            (Format::Deflate, digest)
        } else {
            #[cfg(windows)]
            let clone = if tree::same(&root, &doc.saved) {
                match super::recovery_clone::capture(doc, file, cancel) {
                    Ok(value) => value,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => return Err(e),
                    Err(_) => None,
                }
            } else {
                None
            };
            #[cfg(not(windows))]
            let clone: Option<(u64, [u8; 32])> = None;
            if let Some((offset, digest)) = clone {
                (Format::Clone { offset }, digest)
            } else {
                file.set_len(0)?;
                (Format::Blocks, recovery_blocks::write(file, &root, cancel)?)
            }
        };
        if cancel.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        file.sync_all()?;
        let (_, path) = tmp.keep().map_err(|e| e.error)?;
        let base = Base {
            file: path.file_name().unwrap().to_string_lossy().into_owned(),
            bytes,
            digest,
            hash: hash::Kind::Blake3,
            format,
        };
        Ok(Self {
            root,
            base,
            path,
            patch: None,
            verified: None,
        })
    }
    fn describe_saved(
        &mut self,
        doc: &Document,
        cancel: &AtomicBool,
    ) -> io::Result<Option<SavedPatch>> {
        if let Some((root, patch)) = &self.patch {
            if tree::same(root, &doc.saved) {
                return Ok(Some(patch.clone()));
            }
        }
        let old_len = self.base.bytes;
        let new_len = doc.saved.as_ref().map_or(0, |n| n.len);
        let at = tree::shared_bytes(&self.root, &doc.saved, false);
        let suffix =
            tree::shared_bytes(&self.root, &doc.saved, true).min(old_len.min(new_len) - at);
        let removed = old_len - at - suffix;
        let added = new_len - at - suffix;
        if old_len < recovery_blocks::BLOCK as u64 || old_len.abs_diff(new_len) > MAX_PATCH {
            return Ok(None);
        }
        let plan = if removed.max(added) <= MAX_PATCH {
            let mut inserted = vec![0; added as usize];
            if let Some(root) = &doc.saved {
                root.read(at, &mut inserted)?;
            }
            PatchPlan::Single {
                at,
                removed,
                inserted,
            }
        } else if let Some(plan) = saved_ranges(&self.root, &doc.saved, cancel)? {
            plan
        } else {
            return Ok(None);
        };
        let digest = match (
            doc.saved_codec.native(),
            doc.saved_bom,
            doc.saved_disk_digest,
        ) {
            (true, false, Some(digest)) => digest,
            _ => digest_root(&doc.saved, io::sink(), cancel, hash::Kind::Blake3)?,
        };
        let patch = SavedPatch { plan, digest, hash: hash::Kind::Blake3 };
        self.patch = Some((doc.saved.clone(), patch.clone()));
        Ok(Some(patch))
    }
    fn link_verified(&mut self, dir: &Path) -> io::Result<bool> {
        let Some(held) = &self.verified else {
            return Ok(false);
        };
        let target = dir.join(&self.base.file);
        if self.path != target {
            match fs::hard_link(&self.path, &target) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(_) => return Ok(false),
            }
        }
        let matches = read_archive(&target)
            .and_then(|f| Stamp::from(&f))
            .is_ok_and(|s| Stamp::from(held).is_ok_and(|expected| expected == s));
        if matches {
            self.path = target;
        }
        Ok(matches)
    }
}

impl Document {
    fn archive(
        &self,
        dir: &Path,
        cancel: &AtomicBool,
    ) -> io::Result<(Option<Base>, Option<SavedPatch>, tree::Root)> {
        let mut cached = self.recovery_base.lock().unwrap();
        let named = self.path.is_some();
        if let Some(old) = cached.as_mut() {
            if !named && old.verified.is_some() {
                if !old.link_verified(dir)? {
                    *old = Archive::create(self, old.root.clone(), dir, cancel)?;
                    old.verified = Some(read_archive(&old.path)?);
                }
                return Ok((Some(old.base.clone()), None, old.root.clone()));
            }
            if named && old.path.parent() == Some(dir) && old.path.is_file() {
                if tree::same(&old.root, &self.saved) {
                    return Ok((Some(old.base.clone()), None, self.saved.clone()));
                }
                if let Some(patch) = old.describe_saved(self, cancel)? {
                    return Ok((Some(old.base.clone()), Some(patch), self.saved.clone()));
                }
            }
        }
        if !named {
            return Ok((None, None, None));
        }
        let new = Archive::create(self, self.saved.clone(), dir, cancel)?;
        let base = new.base.clone();
        *cached = Some(new);
        Ok((Some(base), None, self.saved.clone()))
    }

    /// Atomically checkpoint the current logical text. Unchanged spans are
    /// represented by offsets into the last saved document, including after
    /// several saves or an undo across a save point.
    pub fn save_recovery(&self, path: &Path) -> io::Result<()> {
        self.save_recovery_cancel(path, &AtomicBool::new(false))
    }
    pub fn save_recovery_cancel(&self, path: &Path, cancel: &AtomicBool) -> io::Result<()> {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let name = path
            .file_name()
            .ok_or_else(|| invalid("Expected a recovery filename"))?;
        // tempfile persists through native Windows APIs. Keep both names in the
        // extended directory form even when the destination doesn't exist yet.
        let parent = std::fs::canonicalize(parent)?;
        let path = parent.join(name);
        let file = NamedTempFile::new_in(&parent)?;
        let (base, saved_patch, basis) = self.archive(&parent, cancel)?;
        let v3 = saved_patch.is_some()
            || base
                .as_ref()
                .is_some_and(|b| !matches!(b.format, Format::Deflate))
            || (self.path.is_none() && base.is_some());
        let magic = if saved_patch
            .as_ref()
            .is_some_and(|p| matches!(&p.plan, PatchPlan::Ranges { .. }))
        {
            b"LWR4"
        } else if v3 {
            b"LWR3"
        } else {
            b"LWR2"
        };
        let h = serde_json::to_vec(&Header {
            base,
            saved_patch,
            path: self.path.clone(),
            stamp: self.stamp.clone(),
            saved_encoding: self.saved_codec.name().into(),
            encoding: self.codec.name().into(),
            bom: self.emit_bom,
            length: self.len(),
        })?;
        // A document assembled from many pieces has a short record for each.
        let mut out = io::BufWriter::with_capacity(256 * 1024, file.as_file());
        out.write_all(magic)?;
        out.write_all(&(h.len() as u64).to_le_bytes())?;
        out.write_all(&h)?;
        let mut scratch = vec![0; 256 * 1024];
        visit_spans(&basis, &self.root, cancel, |span| {
            match span {
                Span::Copy(at, len) => {
                    out.write_all(&[1])?;
                    out.write_all(&at.to_le_bytes())?;
                    out.write_all(&len.to_le_bytes())?;
                }
                Span::Literal(source, mut at, len) => {
                    out.write_all(&[2])?;
                    out.write_all(&len.to_le_bytes())?;
                    let end = at + len;
                    while at < end {
                        if cancel.load(Ordering::Relaxed) {
                            return Err(io::ErrorKind::Interrupted.into());
                        }
                        let n = (end - at).min(scratch.len() as u64) as usize;
                        source.read(at, &mut scratch[..n])?;
                        out.write_all(&scratch[..n])?;
                        at += n as u64;
                    }
                }
            }
            Ok(true)
        })?;
        out.write_all(&[0])?;
        out.flush()?;
        drop(out);
        file.as_file().sync_all()?;
        file.persist(path).map_err(|e| e.error)?;
        Ok(())
    }
    pub fn recover(path: &Path) -> io::Result<Self> {
        Self::recover_inner(path, false)
    }
    /// Preserve recoverable additions from an old delta without inventing lost
    /// base text. Gaps are explicit and the result always needs a new filename.
    pub fn salvage_legacy(path: &Path) -> io::Result<Self> {
        let (mut r, h, legacy) = read_header(path)?;
        if !legacy {
            return Err(invalid("This recovery file requires its verified base"));
        }
        let file = Arc::new(File::open(path)?);
        let size = r.metadata()?.len();
        let mut root = None;
        let mut total = 0u64;
        loop {
            let mut tag = [0];
            r.read_exact(&mut tag)?;
            let (len, piece) = match tag[0] {
                0 => break,
                1 => {
                    let at = number(&mut r)?;
                    let len = number(&mut r)?;
                    let marker = format!(
                        "[Missing original bytes {at}..{}]",
                        at.checked_add(len)
                            .ok_or_else(|| invalid("Invalid legacy range"))?
                    );
                    (
                        len,
                        tree::leaf(Source::memory(marker.as_bytes()), 0, marker.len() as u64)?,
                    )
                }
                2 => {
                    let len = number(&mut r)?;
                    let at = r.stream_position()?;
                    if len > size.saturating_sub(at) {
                        return Err(invalid("Truncated legacy draft"));
                    }
                    r.seek(SeekFrom::Start(at + len))?;
                    (
                        len,
                        tree::leaf(Source::file(file.clone(), at, len)?, 0, len)?,
                    )
                }
                _ => return Err(invalid("Invalid legacy record")),
            };
            total = total
                .checked_add(len)
                .filter(|n| *n <= h.length)
                .ok_or_else(|| invalid("Invalid legacy length"))?;
            root = tree::join(root, piece);
        }
        if total != h.length || r.stream_position()? != size {
            return Err(invalid("Incomplete legacy draft"));
        }
        let mut doc = Self::empty();
        doc.commit(root)?;
        Ok(doc)
    }
    /// Legacy checkpoints contain no proof of base content. Only use the
    /// selected original after the user explicitly accepts that limitation.
    pub fn recover_legacy(path: &Path) -> io::Result<Self> {
        Self::recover_inner(path, true)
    }
    pub fn recovery_archive(path: &Path) -> io::Result<Option<PathBuf>> {
        let (_, h, _) = read_header(path)?;
        h.base.map(|b| b.path(path)).transpose()
    }
    fn recover_inner(path: &Path, legacy_ok: bool) -> io::Result<Self> {
        let (mut r, h, legacy) = read_header(path)?;
        let mut d = Self::empty();
        let mut original = None;
        if h.saved_patch.is_some() && (h.base.is_none() || h.path.is_none()) {
            return Err(invalid("Missing saved revision for recovery patch"));
        }
        if let Some(p) = &h.path {
            if legacy && !legacy_ok {
                return Err(invalid("Legacy draft uses an unverified original"));
            }
            let opened = (|| -> io::Result<Document> {
                let mut fresh = Self::open_as(p, &h.saved_encoding)?;
                if fresh.stamp != h.stamp {
                    return Err(invalid("The original has changed"));
                }
                fresh.protect_edit_base()?;
                if let Some(base) = &h.base {
                    let (length, digest, kind) = match &h.saved_patch {
                        Some(patch) => (patch.length(base.bytes)?, patch.digest, patch.hash),
                        None => (base.bytes, base.digest, base.hash),
                    };
                    if fresh.len() != length
                        || digest_root(&fresh.root, io::sink(), &AtomicBool::new(false), kind)? != digest
                    {
                        return Err(invalid("The original has changed"));
                    }
                } else if !legacy {
                    return Err(invalid("Missing recovery base"));
                }
                Ok(fresh)
            })();
            match opened {
                Ok(doc) => {
                    original = doc.root.clone();
                    d = doc;
                }
                Err(e) if h.base.is_none() => return Err(e),
                Err(_) => {}
            }
        }
        if d.path.is_none() {
            if let Some(base) = &h.base {
                let name = base.path(path)?;
                let held = read_archive(&name)?;
                let anchor = base.restore(held.clone())?;
                original = match &h.saved_patch {
                    Some(patch) => patch.apply(anchor.clone())?,
                    None => anchor.clone(),
                };
                if base.bytes >= recovery_blocks::BLOCK as u64 {
                    *d.recovery_base.lock().unwrap() = Some(Archive {
                        root: anchor,
                        base: base.clone(),
                        path: name,
                        patch: None,
                        verified: Some(held),
                    });
                }
            }
        }
        let mut root = None;
        let mut total = 0u64;
        let size = r.metadata()?.len();
        // Positional Windows reads can advance a duplicated handle's shared
        // cursor. The record reader must own a separate file object.
        let file = Arc::new(File::open(path)?);
        loop {
            let mut tag = [0];
            r.read_exact(&mut tag)?;
            if tag == [0] {
                break;
            }
            let piece = match tag[0] {
                1 => {
                    let at = number(&mut r)?;
                    let len = number(&mut r)?;
                    let n = original.as_ref().map_or(0, |root| root.len);
                    if at > n || len > n - at {
                        return Err(invalid("Recovery references text outside the original"));
                    }
                    tree::slice(original.clone(), at, len)?
                }
                2 => {
                    let len = number(&mut r)?;
                    let at = r.stream_position()?;
                    if len > size - at {
                        return Err(invalid("Recovery file is truncated"));
                    }
                    if len <= 65536 {
                        let mut b = vec![0; len as usize];
                        r.read_exact(&mut b)?;
                        tree::leaf(Source::memory(&b), 0, len)?
                    } else {
                        let s = Source::file(file.clone(), at, len)?;
                        r.seek(SeekFrom::Start(at + len))?;
                        tree::leaf(s, 0, len)?
                    }
                }
                _ => return Err(invalid("Invalid recovery record")),
            };
            total = total
                .checked_add(piece.as_ref().map_or(0, |p| p.len))
                .ok_or_else(|| invalid("Invalid recovery length"))?;
            if total > h.length {
                return Err(invalid("Invalid recovery length"));
            }
            root = tree::join(root, piece);
        }
        if total != h.length || r.stream_position()? != size {
            return Err(invalid("Incomplete recovery checkpoint"));
        }
        d.commit(root)?;
        d.codec = Codec::named(&h.encoding)?;
        d.emit_bom = h.bom;
        Ok(d)
    }
}

fn read_header(path: &Path) -> io::Result<(File, Header, bool)> {
    let mut r = File::open(path)?;
    let mut magic = [0; 4];
    r.read_exact(&mut magic)?;
    if &magic != b"LWR1" && &magic != b"LWR2" && &magic != b"LWR3" && &magic != b"LWR4" {
        return Err(invalid("Unknown recovery file format"));
    }
    let size = number(&mut r)?;
    if size > 1024 * 1024 {
        return Err(invalid("Recovery header is too large"));
    }
    let mut bytes = vec![0; size as usize];
    r.read_exact(&mut bytes)?;
    let header: Header = serde_json::from_slice(&bytes)?;
    let has_ranges = header
        .saved_patch
        .as_ref()
        .is_some_and(|p| matches!(&p.plan, PatchPlan::Ranges { .. }));
    if has_ranges != (&magic == b"LWR4") {
        return Err(invalid("Invalid saved revision format"));
    }
    Ok((r, header, &magic == b"LWR1"))
}
