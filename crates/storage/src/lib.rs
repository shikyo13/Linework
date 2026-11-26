mod change_map;
pub use change_map::ChangeMap;
mod unicode;
mod spill;
mod codec;
mod selection_set;
pub use selection_set::{SelectionSet,SelectionBuilder};
mod folds;
pub use folds::{Fold,FoldMap};
mod eol;
mod recovery;
mod recovery_blocks;
#[cfg(windows)]
mod recovery_clone;
mod revision;
#[cfg(windows)]
mod checkpoint;
#[cfg(windows)]
mod transactional;
#[cfg(windows)]
mod delta_save;
#[cfg(windows)]
mod save_journal;
#[cfg(windows)]
mod inplace;
#[cfg(windows)]
pub use save_journal::set_save_journal;
mod parallel;
mod patched;
mod search;
mod source;
#[cfg(windows)]
mod write_lease;
mod tree;
pub use search::{FoundLine, LineEnds, SearchOptions};

use codec::Codec;
pub use codec::{set_detection, ENCODINGS};
pub use source::{set_index_cache, set_line_indexing};
use serde::{Deserialize, Serialize};
use source::{read_at, Source};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use tempfile::NamedTempFile;
use tree::Root;

pub(crate) fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn conflict() -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        "The file changed on disk. Save As to preserve both versions.",
    )
}

#[derive(PartialEq, Eq, Clone, Debug, Serialize, Deserialize)]
struct Stamp {
    size: u64,
    modified: u128,
    id: u64,
    volume: u32,
}
impl Stamp {
    fn from(f: &File) -> io::Result<Self> {
        let meta = f.metadata()?;
        let modified = meta
            .modified()?
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        #[cfg(windows)]
        let (id, volume) = {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
            };
            let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut info) } == 0 {
                return Err(io::Error::last_os_error());
            }
            (
                (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
                info.dwVolumeSerialNumber,
            )
        };
        #[cfg(unix)]
        let (id, volume) = {
            use std::os::unix::fs::MetadataExt;
            (meta.ino(), meta.dev() as u32)
        };
        Ok(Self {
            size: meta.len(),
            modified,
            id,
            volume,
        })
    }
    fn get(p: &Path) -> io::Result<Option<Self>> {
        match File::open(p) {
            Ok(f) => Ok(Some(Self::from(&f)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// The exact existing destination selected for explicit overwrite, or a
/// vacant name for explicit recreation. Hold the write exclusion until publish.
pub struct OverwriteTarget {
    path: PathBuf,
    stamp: Option<Stamp>,
    _guard: Option<File>,
}
impl OverwriteTarget {
    pub fn inspect(path: &Path) -> io::Result<Self> {
        let path = absolute(path)?;
        let stamp = Stamp::get(&path)?;
        let guard = stamp
            .as_ref()
            .map(|s| revision::guard_destination(&path, s))
            .transpose()?;
        Ok(Self {
            path,
            stamp,
            _guard: guard,
        })
    }
}
/// UTF-8 logical offsets, excluding the BOM. Read snapshots retain their
/// piece tree and source handles even when an editor mutates or closes a tab.
pub struct Document {
    root: Root,
    recovery_base: Arc<std::sync::Mutex<Option<recovery::Archive>>>,
    saved: Root,
    undo: Vec<Root>,
    group: Option<(usize, Root)>,
    redo: Vec<Root>,
    path: Option<PathBuf>,
    stamp: Option<Stamp>,
    // Digest of the exact disk encoding, including its BOM, when available.
    saved_disk_digest: Option<[u8;32]>,
    #[cfg(windows)]
    previous_disk: Option<Arc<checkpoint::DiskCheckpoint>>,
    accepted_appends: Arc<HashMap<(u32, u64), Stamp>>,
    /// Original disk extents which must be isolated before the first edit.
    unprotected: Vec<Arc<Source>>,
    codec: Codec,
    saved_codec: Codec,
    emit_bom: bool,
    saved_bom: bool,
    newline: &'static str,
    version: u64,
}
impl Clone for Document {
    fn clone(&self) -> Self {
        Self {
            recovery_base: self.recovery_base.clone(),
            root: self.root.clone(),
            saved: self.saved.clone(),
            undo: vec![],
            group: None,
            redo: vec![],
            path: self.path.clone(),
            stamp: self.stamp.clone(),
            saved_disk_digest: self.saved_disk_digest,
            #[cfg(windows)]
            previous_disk: self.previous_disk.clone(),
            accepted_appends: self.accepted_appends.clone(),
            unprotected: self.unprotected.clone(),
            codec: self.codec,
            saved_codec: self.saved_codec,
            emit_bom: self.emit_bom,
            saved_bom: self.saved_bom,
            newline: self.newline,
            version: self.version,
        }
    }
}
impl Document {
    pub fn empty() -> Self {
        Self {
            recovery_base: Default::default(),
            root: None,
            saved: None,
            undo: vec![],
            group: None,
            redo: vec![],
            path: None,
            stamp: None,
            saved_disk_digest: None,
            #[cfg(windows)]
            previous_disk: None,
            accepted_appends: Arc::default(),
            unprotected: Vec::new(),
            codec: Codec::Utf8,
            saved_codec: Codec::Utf8,
            emit_bom: false,
            saved_bom: false,
            newline: if cfg!(windows) { "\r\n" } else { "\n" },
            version: 0,
        }
    }
    /// An empty document that will be saved in `encoding`. It starts clean.
    pub fn empty_as(encoding: &str) -> io::Result<Self> {
        let mut d = Self::empty();
        d.codec = match Codec::named(encoding)? {
            Codec::Hex => return Err(invalid("A new file cannot start as hexadecimal")),
            c => c,
        };
        d.saved_codec = d.codec;
        d.emit_bom = !d.codec.bom().is_empty();
        d.saved_bom = d.emit_bom;
        Ok(d)
    }
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_core(path, None)
    }
    /// `open`, with the background line count held until `start_indexing`,
    /// so a program can show the text before the count competes with it.
    pub fn open_deferred(path: &Path) -> io::Result<Self> {
        source::DEFER_INDEXING.set(true);
        let doc = Self::open_core(path, None);
        source::DEFER_INDEXING.set(false);
        doc
    }
    /// Start the background line count of every part of the text that has
    /// not started one (after `open_deferred`); does nothing for the rest.
    pub fn start_indexing(&self) -> io::Result<()> {
        let mut sources = Vec::new();
        if let Some(root) = &self.root {
            root.pieces(&mut |s, _, _| {
                sources.push(s.clone());
                Ok(())
            })?;
        }
        for s in sources {
            s.start_index()?;
        }
        Ok(())
    }
    pub fn open_as(path: &Path, encoding: &str) -> io::Result<Self> {
        Self::open_core(path, Some(Codec::named(encoding)?))
    }
    fn open_core(path: &Path, forced: Option<Codec>) -> io::Result<Self> {
        // Rust's Windows default shares read, write and delete, enabling logs
        // with active writers. Both ends of every save validate the disk stamp.
        let file = OpenOptions::new().read(true).open(path)?;
        let stamp = Stamp::from(&file)?;
        let size = stamp.size;
        let mut first = vec![0; size.min(32768) as usize];
        read_at(&file, 0, &mut first)?;
        let mut codec = match forced {
            Some(x) => x,
            None => match Codec::detect(&first, size == first.len() as u64) {
                // Zero bytes at the start of a large file: judged by three
                // samples from within it, a quarter, half and three quarters in.
                Err(e) if first.contains(&0) && size >= 4 * first.len() as u64 => {
                    let mut samples = Vec::with_capacity(3);
                    for k in 1..=3 {
                        let mut s = vec![0; first.len()];
                        read_at(&file, size / 4 * k, &mut s)?;
                        samples.push(s);
                    }
                    if !Codec::text_inside(&samples) {
                        return Err(e);
                    }
                    Codec::detect_text(&first, false)
                }
                r => r?,
            },
        };
        // The regular editing surface transfers the entire document to a UTF-8
        // control. Confirm the encoding before that transfer so an isolated
        // legacy character after a long ASCII header survives an unrelated edit.
        let mut ordinary = None;
        if codec.native() && size <= 24 * 1024 * 1024 {
            let mut bytes = vec![0; size as usize];
            read_at(&file, 0, &mut bytes)?;
            if forced.is_none() && std::str::from_utf8(&bytes).is_err() {
                if codec == Codec::Bom {
                    return Err(invalid("The UTF-8 BOM conflicts with invalid text. Reopen with its original encoding."));
                }
                codec = codec::guess(&bytes, true);
            } else {
                ordinary = Some(bytes);
            }
        }
        let bom = codec.bom();
        let emit = !bom.is_empty() && first.starts_with(bom);
        let head = if emit { bom.len() as u64 } else { 0 };
        let path = path.canonicalize()?;
        let mut saved_disk_digest = None;
        let (src, logical_len, unprotected) = if let Some(mut bytes) = ordinary {
            if Stamp::from(&file)? != stamp { return Err(conflict()); }
            bytes.drain(..head as usize);
            (Source::owned(bytes), size - head, vec![])
        } else if codec.native() {
            let len = size - head;
            let source = Source::external(Arc::new(file), head, len, &path)?;
            (source.clone(), len, vec![source])
        } else {
            let guessed = forced.is_none() && matches!(codec, Codec::Other(_) | Codec::Table(_));
            let (f, digest) = match codec.decode(file.try_clone()?, &first[..head as usize]) {
                // A guess that does not decode cleanly falls back to Windows
                // 1252, which gives every byte a character and saves it back.
                Err(_) if guessed && codec != Codec::named("windows-1252")? => {
                    codec = Codec::named("windows-1252")?;
                    codec.decode(file, &first[..head as usize])?
                }
                r => r?,
            };
            saved_disk_digest = Some(digest);
            let n = f.metadata()?.len();
            first.resize(n.min(32768) as usize, 0);
            read_at(&f, 0, &mut first)?;
            (Source::file(Arc::new(f), 0, n)?, n, vec![])
        };
        let newline = memchr::memchr2(10, 13, &first)
            .map(|i| match (first[i], first.get(i + 1)) {
                (13, Some(10)) => "\r\n",
                (13, _) => "\r",
                _ => "\n",
            })
            .unwrap_or(if cfg!(windows) { "\r\n" } else { "\n" });
        let root = tree::leaf(src, 0, logical_len)?;
        Ok(Self {
            recovery_base: Default::default(),
            root: root.clone(),
            saved: root,
            undo: vec![],
            group: None,
            redo: vec![],
            path: Some(path),
            stamp: Some(stamp),
            saved_disk_digest,
            #[cfg(windows)]
            previous_disk: None,
            accepted_appends: Arc::default(),
            unprotected,
            codec,
            saved_codec: codec,
            emit_bom: emit,
            saved_bom: emit,
            newline,
            version: 0,
        })
    }
    pub fn len(&self) -> u64 {
        self.root.as_ref().map_or(0, |n| n.len)
    }
    /// The file this document is, unchanged: its only piece is the whole of one
    /// source on disk, so its offsets are that source's. Scans of such a
    /// document can read with a lead (`lead.rs`).
    pub(crate) fn sole_source(&self) -> Option<&std::sync::Arc<source::Source>> {
        let node = self.root.as_ref()?;
        match &node.body {
            tree::Body::Leaf(source, 0) if node.len == source.len && source.on_disk() => Some(source),
            _ => None,
        }
    }
    /// A lead for a scan of this document from `from` to its end, when it is one file on disk.
    pub(crate) fn lead(&self, from: u64) -> Option<source::Lead> {
        self.sole_source()?.lead(from)
    }
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }
    pub fn newline(&self) -> &'static str {
        self.newline
    }
    pub fn encoding_name(&self) -> &'static str {
        self.codec.name()
    }
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub fn detach_path(&mut self) {
        self.path = None;
        self.stamp = None;
        self.saved = None;
        self.saved_disk_digest = None;
        #[cfg(windows)]
        {
            self.previous_disk = None;
        }
    }
    pub fn version(&self) -> u64 {
        self.version
    }
    pub fn dirty(&self) -> bool {
        !tree::same(&self.root, &self.saved)
            || self.codec != self.saved_codec
            || self.emit_bom != self.saved_bom
    }
    pub fn set_encoding(&mut self, encoding: &str) -> io::Result<()> {
        let codec = Codec::named(encoding)?;
        self.protect_edit_base()?;
        self.codec = codec;
        self.emit_bom = !self.codec.bom().is_empty();
        Ok(())
    }
    pub fn indexed_bytes(&self) -> u64 {
        self.root.as_ref().map_or(0, |n| n.indexed())
    }
    pub fn indexing_complete(&self) -> bool {
        self.indexed_bytes() == self.len()
    }
    pub fn index_error(&self) -> Option<String> {
        let mut e = None;
        if let Some(n) = &self.root {
            let _ = n.pieces(&mut |s, _, _| {
                if e.is_none() {
                    e = s.error();
                }
                Ok(())
            });
        }
        e
    }
    pub fn line_count(&self) -> u64 {
        1 + self.line_of(self.indexed_bytes())
    }
    pub fn line_of(&self, pos: u64) -> u64 {
        self.root
            .as_ref()
            .and_then(|n| n.line(pos.min(n.indexed())).ok())
            .unwrap_or(0)
    }
    pub fn line_start(&self, line: u64) -> u64 {
        let Some(n) = &self.root else { return 0 };
        let indexed = n.indexed();
        if line > n.line(indexed).unwrap_or(0) {
            return indexed;
        }
        n.start(line, indexed).unwrap_or(indexed)
    }
    /// Where line `line` (from 0) would start if every line had the length
    /// of the first, before the line count reaches it. Given only when the
    /// line feeds in the first MiB are exactly that far apart and the bytes at
    /// the estimate are one whole line of that length; it is still an
    /// estimate until the count confirms it.
    pub fn estimate_line_start(&self, line: u64) -> Option<u64> {
        const HEAD: u64 = 1 << 20;
        let len = self.len();
        let head = self.read(0, len.min(HEAD) as usize).ok()?;
        let mut ends = memchr::memchr_iter(b'\n', &head);
        let length = ends.next()? as u64 + 1;
        let mut lines = 1u64;
        for (k, end) in ends.enumerate() {
            if end as u64 + 1 != (k as u64 + 2) * length {
                return None;
            }
            lines += 1;
        }
        // Enough lines to judge by.
        if lines < 256 {
            return None;
        }
        let at = line.checked_mul(length)?;
        if at == 0 || at >= len {
            return None;
        }
        // The byte before the estimate ends a line, and the line there is the same length.
        let take = (length + 1).min(len - at + 1);
        let bytes = self.read(at - 1, take as usize).ok()?;
        let inner = &bytes[1..];
        let whole = match memchr::memchr(b'\n', inner) {
            Some(i) => i as u64 + 1 == length,
            None => at + inner.len() as u64 == len && (inner.len() as u64) < length,
        };
        (bytes[0] == b'\n' && whole).then_some(at)
    }
    pub fn char_boundary(&self, pos: u64) -> u64 {
        let pos = pos.min(self.len());
        if pos == self.len() || pos == 0 {
            return pos;
        }
        let start = pos.saturating_sub(3);
        let Ok(bytes) = self.read(start, (pos - start + 1) as usize) else {
            return pos;
        };
        let mut at = bytes.len() - 1;
        while at > 0 && bytes[at] & 0xc0 == 0x80 {
            at -= 1;
        }
        start + at as u64
    }
    pub fn read_into(&self, offset: u64, dst: &mut [u8]) -> io::Result<()> {
        if offset > self.len() || dst.len() as u64 > self.len() - offset {
            return Err(invalid("Read outside document"));
        }
        if let Some(root) = &self.root {
            root.read(offset, dst)?;
        }
        Ok(())
    }
    pub fn read(&self, offset: u64, count: usize) -> io::Result<Vec<u8>> {
        let n = self.len().saturating_sub(offset).min(count as u64) as usize;
        let mut bytes = vec![0; n];
        if n != 0 {
            self.read_into(offset, &mut bytes)?;
        }
        Ok(bytes)
    }
    fn edited_root(&self, old: Root, offset: u64, delete: u64, insert: &[u8]) -> io::Result<Root> {
        let len = old.as_ref().map_or(0, |n| n.len);
        if offset > len || delete > len - offset {
            return Err(invalid("Edit outside document"));
        }
        let add = if insert.is_empty() {
            None
        } else {
            tree::leaf(Source::memory(insert), 0, insert.len() as u64)?
        };
        tree::replace(old, offset, delete, add)
    }
    pub fn prepare_without_copy(&mut self) -> io::Result<bool> {
        for src in &self.unprotected {
            if !src.try_protect(
                src.file_id().and_then(|id| self.accepted_appends.get(&id)),
                false,
            )? {
                return Ok(false);
            }
        }
        self.unprotected.clear();
        Ok(true)
    }
    pub fn prepare_edit(
        &mut self,
        mut progress: impl FnMut(u64, u64) -> io::Result<()>,
    ) -> io::Result<()> {
        for src in &self.unprotected {
            let accepted = src.file_id().and_then(|id| self.accepted_appends.get(&id));
            if !src.try_protect(accepted, true)? {
                src.copy_with_progress(
                    accepted.or(src.recorded.as_ref()).unwrap(),
                    true,
                    &mut progress,
                )?;
            }
        }
        self.unprotected.clear();
        Ok(())
    }
    fn protect_edit_base(&mut self) -> io::Result<()> {
        for source in &self.unprotected {
            let accepted = source
                .file_id()
                .and_then(|id| self.accepted_appends.get(&id));
            source.protect(accepted)?;
        }
        self.unprotected.clear();
        Ok(())
    }
    fn commit(&mut self, next: Root) -> io::Result<()> {
        self.protect_edit_base()?;
        let old = std::mem::replace(&mut self.root, next);
        if self.group.is_none() {
            self.undo.push(old);
        }
        self.redo.clear();
        self.version += 1;
        Ok(())
    }
    pub fn replace(&mut self, offset: u64, delete: u64, insert: &[u8]) -> io::Result<()> {
        if delete == 0 && insert.is_empty() {
            return Ok(());
        }
        let next = self.edited_root(self.root.clone(), offset, delete, insert)?;
        self.commit(next)
    }
    pub fn replace_many(&mut self, edits: &[(u64, u64, Vec<u8>)]) -> io::Result<()> {
        if edits.is_empty() {
            return Ok(());
        }
        let mut root = self.root.clone();
        let mut ceiling = self.len();
        for (pos, len, bytes) in edits.iter().rev() {
            if *pos > ceiling || *len > ceiling - pos {
                return Err(invalid("Edit ranges overlap or are out of order"));
            }
            root = self.edited_root(root, *pos, *len, bytes)?;
            ceiling = *pos;
        }
        self.commit(root)
    }
    /// Apply one edit computed on a read snapshot to the original history.
    pub fn apply_snapshot(&mut self, edited: Self) -> io::Result<()> {
        if edited.undo.len() != 1 || !tree::same(&self.root, &edited.undo[0]) {
            return Err(invalid("The document changed while the edit was running"));
        }
        self.commit(edited.root)
    }
    #[cfg(windows)]
    pub fn history_position(&self, redo: bool) -> Option<u64> {
        let next = if redo {
            self.redo.last()?
        } else {
            self.undo.last()?
        };
        Some(tree::shared_bytes(&self.root, next, false))
    }
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn begin_undo_group(&mut self) {
        if let Some((depth, _)) = &mut self.group {
            *depth += 1;
        } else {
            self.group = Some((1, self.root.clone()));
        }
    }
    pub fn end_undo_group(&mut self) {
        let Some((depth, initial)) = self.group.take() else {
            return;
        };
        if depth > 1 {
            self.group = Some((depth - 1, initial));
        } else if !tree::same(&initial, &self.root) {
            self.undo.push(initial);
        }
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn undo(&mut self) -> bool {
        if let Some(old) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.root, old));
            self.version += 1;
            true
        } else {
            false
        }
    }
    pub fn redo(&mut self) -> bool {
        if let Some(old) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.root, old));
            self.version += 1;
            true
        } else {
            false
        }
    }
    pub fn save(&mut self, path: &Path) -> io::Result<()> {
        self.save_internal(path, None)
    }
    pub fn overwrite(&mut self, target: OverwriteTarget) -> io::Result<()> {
        self.save_internal(&target.path, Some(&target))
    }
    pub fn disk_changed(&self) -> io::Result<bool> {
        match &self.path {
            Some(p) => Ok(Stamp::get(p)? != self.stamp),
            None => Ok(false),
        }
    }
    fn save_internal(
        &mut self,
        path: &Path,
        overwrite: Option<&OverwriteTarget>,
    ) -> io::Result<()> {
        let dest = absolute(path)?;
        let before = Stamp::get(&dest)?;
        if overwrite.is_some_and(|t| t.path != dest || t.stamp != before) {
            return Err(conflict());
        }
        let same_file = overwrite.is_none()
            && (self.path.as_ref() == Some(&dest)
                || self
                    .stamp
                    .as_ref()
                    .zip(before.as_ref())
                    .is_some_and(|(a, b)| a.id == b.id && a.volume == b.volume));
        if same_file && before != self.stamp {
            return Err(conflict());
        }
        self.check_original()?;
        let dir = dest
            .parent()
            .ok_or_else(|| invalid("Missing destination folder"))?;
        // Guard an existing Save As target against in-place writes while the output
        // is being staged. A target created after the initial check is protected
        // by persist_noclobber below.
        let mut target_guard = if let Some(stamp) = before.as_ref().filter(|_| !same_file) {
            Some(revision::guard_destination(&dest, stamp)?)
        } else {
            None
        };
        #[cfg(windows)]
        if same_file && self.restore_checkpoint(&dest)? { return Ok(()); }
        #[cfg(windows)]
        if same_file && self.save_small_delta(&dest)? { return Ok(()); }
        let mut tmp = NamedTempFile::new_in(dir)?;
        let head = if self.emit_bom { self.codec.bom().len() as u64 } else { 0 };
        let expected = self.codec.native().then(|| self.len() + head);
        let staged = revision::Staged::new(tmp.as_file(), tmp.path(), expected);
        let checkpoint = revision::stage(staged, |mut output| {
            self.codec.write(self, &mut output, self.emit_bom)
        })?;
        tmp.flush()?;
        tmp.as_file().sync_all()?;
        let checkpoint_stamp = Stamp::from(tmp.as_file())?;
        self.check_original()?;
        if let Some(stamp) = before.as_ref().filter(|_| same_file) {
            target_guard = Some(self.verify_disk_revision(&dest, stamp)?);
        }
        if Stamp::get(&dest)? != before {
            return Err(conflict());
        }
        if before.is_some() {
            #[cfg(windows)]
            {
                let tmp_path = tmp.into_temp_path();
                let preserved = if same_file {
                    self.publish_with_checkpoint(&dest, &tmp_path, before.as_ref().unwrap())?
                } else { false };
                if !preserved {
                    if Stamp::get(&dest)? != before { return Err(conflict()); }
                    checkpoint::atomic_replace(&dest, &tmp_path)?;
                }
            }
            #[cfg(not(windows))]
            {
                tmp.persist(&dest).map_err(|e| e.error)?;
            }
        } else {
            tmp.persist_noclobber(&dest).map_err(|e| e.error)?;
        }
        self.stamp = Some(checkpoint_stamp);
        drop(target_guard);
        self.path = Some(dest);
        self.saved_disk_digest = Some(checkpoint);
        self.saved = self.root.clone();
        self.saved_codec = self.codec;
        self.saved_bom = self.emit_bom;
        Ok(())
    }
    /// A worker saved this document's snapshot. Transfer its disk checkpoint
    /// without discarding the editor's earlier undo/redo roots.
    pub fn adopt_save(&mut self, saved: Self) {
        self.path = saved.path;
        self.stamp = saved.stamp;
        self.saved_disk_digest = saved.saved_disk_digest;
        #[cfg(windows)]
        { self.previous_disk = saved.previous_disk; }
        self.saved = saved.saved;
        self.saved_codec = saved.saved_codec;
        self.saved_bom = saved.saved_bom;
    }
    fn check_original(&self) -> io::Result<()> {
        let mut seen = HashSet::new();
        if let Some(root) = &self.root {
            root.pieces(&mut |source, _, _| {
                if let Some(id) = source.file_id() {
                    if seen.insert(id) {
                        source.validate(self.accepted_appends.get(&id))?;
                    }
                }
                Ok(())
            })?;
        }
        Ok(())
    }
    fn continuous_prefix(&self, file: &File, stamp: &Stamp) -> io::Result<bool> {
        // The small-file source is owned. A live, disk-backed piece cannot
        // authenticate itself after an outside writer has already changed it.
        if !self.unprotected.is_empty() {
            return Ok(false);
        }
        let head = if self.saved_bom {
            self.saved_codec.bom()
        } else {
            &[]
        };
        if stamp.size != self.len() + head.len() as u64 {
            return Ok(false);
        }
        let mut buf = vec![0; 256 * 1024];
        read_at(file, 0, &mut buf[..head.len()])?;
        if &buf[..head.len()] != head {
            return Ok(false);
        }
        let mut at = 0;
        while at < self.len() {
            let n = (self.len() - at).min(buf.len() as u64) as usize;
            read_at(file, at + head.len() as u64, &mut buf[..n])?;
            if self.read(at, n)? != buf[..n] {
                return Ok(false);
            }
            at += n as u64;
        }
        Ok(true)
    }
    /// Refreshes a clean reader. On append, preserve every existing indexed
    /// page and add a disk-backed tail. A rotated log reopens as a new document.
    pub fn refresh_from_disk(&mut self) -> io::Result<bool> {
        let Some(path) = self.path.clone() else {
            return Ok(false);
        };
        let file = File::open(&path)?;
        let now = Stamp::from(&file)?;
        if self.stamp.as_ref() == Some(&now) {
            return Ok(false);
        }
        if self.dirty() {
            return Err(conflict());
        }
        let before = self.stamp.as_ref().unwrap();
        if before.id == now.id
            && before.volume == now.volume
            && before.size < now.size
            && before.size <= 24 * 1024 * 1024
            && self.codec.native()
            && self.continuous_prefix(&file, before)?
        {
            let len = now.size - before.size;
            let src = if now.size <= 24 * 1024 * 1024 {
                let mut bytes = vec![0; len as usize];
                read_at(&file, before.size, &mut bytes)?;
                if Stamp::from(&file)? != now { return Err(conflict()); }
                Source::owned(bytes)
            } else {
                let src = Source::external(Arc::new(file), before.size, len, &path)?;
                self.unprotected.push(src.clone());
                src
            };
            self.root = tree::join(self.root.clone(), tree::leaf(src, 0, len)?);
            self.saved = self.root.clone();
            self.undo.clear();
            self.redo.clear();
            Arc::make_mut(&mut self.accepted_appends).insert((now.volume, now.id), now.clone());
            self.stamp = Some(now);
            self.saved_disk_digest = None;
            self.version += 1;
        } else {
            let mut fresh = Self::open(&path)?;
            fresh.version = self.version + 1;
            *self = fresh;
        }
        Ok(true)
    }
}
fn absolute(p: &Path) -> io::Result<PathBuf> {
    if p.exists() {
        p.canonicalize()
    } else {
        let dir = p
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .canonicalize()?;
        Ok(dir.join(p.file_name().ok_or_else(|| invalid("Missing file name"))?))
    }
}
