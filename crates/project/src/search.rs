use crate::text::{self, Encoding};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::{Regex, RegexBuilder};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, Metadata, OpenOptions};
use std::io::{self, BufWriter, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

#[derive(Clone, Debug)]
pub struct OpenBuffer {
    pub path: PathBuf,
    pub text: String,
    pub revision: u64,
}

/// Logical UTF-8 from an immutable open document, including unsaved edits.
/// Reading happens on the search worker with bounded memory.
pub struct OpenStream<'a> {
    pub path: PathBuf,
    pub buffer_id: Option<u64>,
    pub revision: u64,
    pub reader: Box<dyn io::BufRead + Send + 'a>,
}
impl OpenBuffer {
    pub fn new(path: PathBuf, text: String, revision: u64) -> Self {
        Self {
            path,
            text,
            revision,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub root: PathBuf,
    pub query: String,
    pub regex: bool,
    pub match_case: bool,
    pub whole_word: bool,
    pub include: String,
    pub exclude: String,
    pub replacement: Option<String>,
    pub buffers: Vec<OpenBuffer>,
    /// Clean large files can be searched from disk and edited in their open buffer.
    pub disk_buffers: Vec<(PathBuf, u64)>,
    /// Dirty buffers that cannot cheaply supply a snapshot must never be read from disk.
    pub skip: Vec<PathBuf>,
    pub max_hits: usize,
    pub hidden: bool,
    pub buffers_only: bool,
}
impl SearchRequest {
    pub fn new(root: PathBuf, query: String) -> Self {
        Self {
            root,
            query,
            regex: false,
            match_case: false,
            whole_word: false,
            include: String::new(),
            exclude: String::new(),
            replacement: None,
            buffers: Vec::new(),
            disk_buffers: Vec::new(),
            skip: Vec::new(),
            max_hits: 10_000,
            hidden: false,
            buffers_only: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub path: PathBuf,
    pub buffer_id: Option<u64>,
    pub line: u64,
    pub column: u64,
    pub offset: u64,
    pub length: usize,
    pub preview: String,
    pub preview_after: Option<String>,
    /// Byte counts of the contextual text surrounding the shortened match.
    pub preview_context: (usize, usize),
    pub matched: String,
    pub replacement: Option<String>,
    physical: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
}
impl From<Metadata> for Stamp {
    fn from(meta: Metadata) -> Self {
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            created: meta.created().ok(),
        }
    }
}
#[derive(Clone, Debug)]
enum Source {
    Disk(Stamp, Encoding, Option<blake3::Hash>),
    Open(u64),
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum SourceKey {
    File(PathBuf),
    Buffer(u64),
}
impl SourceKey {
    fn at(path: &Path, id: Option<u64>) -> Self {
        id.map(Self::Buffer)
            .unwrap_or_else(|| Self::File(path.into()))
    }
}

fn file_hash(path: &Path, cancel: &AtomicBool) -> Result<blake3::Hash, String> {
    let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut h = blake3::Hasher::new();
    let mut bytes = vec![0; 256 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("Search canceled".into());
        }
        let n = file.read(&mut bytes).map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(h.finalize());
        }
        h.update(&bytes[..n]);
    }
}

#[derive(Clone, Debug, Default)]
pub struct SearchReport {
    pub hits: Vec<Hit>,
    pub files_scanned: usize,
    pub binary_skipped: usize,
    pub errors: Vec<String>,
    pub truncated: bool,
    pub cancelled: bool,
    sources: HashMap<SourceKey, Source>,
}

pub(crate) fn walker(root: &Path, hidden: bool) -> ignore::Walk {
    let mut b = WalkBuilder::new(root);
    b.hidden(!hidden)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|e| {
            e.depth() == 0
                || !(e.file_type().is_some_and(|f| f.is_dir())
                    && matches!(
                        e.file_name().to_str(),
                        Some(".git" | ".hg" | ".svn" | "node_modules" | ".venv")
                    ))
        });
    b.build()
}
fn glob_set(filters: &str) -> Result<GlobSet, String> {
    let mut b = GlobSetBuilder::new();
    for pattern in filters.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let pattern = pattern.replace('\\', "/");
        b.add(
            GlobBuilder::new(&pattern)
                .case_insensitive(cfg!(windows))
                .literal_separator(true)
                .build()
                .map_err(|e| e.to_string())?,
        );
        if !pattern.contains('/') {
            b.add(
                GlobBuilder::new(&format!("**/{pattern}"))
                    .case_insensitive(cfg!(windows))
                    .literal_separator(true)
                    .build()
                    .map_err(|e| e.to_string())?,
            );
        }
    }
    b.build().map_err(|e| e.to_string())
}
pub fn path_key(path: &Path) -> String {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy().replace('/', "\\").to_lowercase();
        if let Some(name) = s.strip_prefix(r"\\?\unc\") {
            return format!(r"\\{name}");
        }
        s.strip_prefix(r"\\?\").unwrap_or(&s).to_owned()
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
    }
}
pub fn relative_path<'a>(path: &'a Path, root: &Path) -> Option<&'a Path> {
    if let Ok(p) = path.strip_prefix(root) {
        return Some(p);
    }
    #[cfg(windows)]
    {
        let key = path_key(root);
        // Compare complete ancestor components. Preserve original spelling in the result.
        let same = path.ancestors().find(|a| path_key(a) == key)?;
        path.strip_prefix(same).ok()
    }
    #[cfg(not(windows))]
    {
        None
    }
}
fn allowed(path: &Path, root: &Path, include: &GlobSet, exclude: &GlobSet) -> bool {
    let relative = relative_path(path, root).unwrap_or(path);
    (include.is_empty() || include.is_match(relative)) && !exclude.is_match(relative)
}

pub fn search(req: &SearchRequest, cancel: &AtomicBool) -> Result<SearchReport, String> {
    search_with_streams(req, Vec::new(), cancel)
}

pub fn search_with_streams<'a>(
    req: &'a SearchRequest,
    streams: Vec<OpenStream<'a>>,
    cancel: &AtomicBool,
) -> Result<SearchReport, String> {
    if req.query.is_empty() {
        return Err("Enter text to search for.".into());
    }
    if !req.buffers_only && !req.root.is_dir() {
        return Err("Open a folder to search the workspace.".into());
    }
    let pattern = if req.regex {
        req.query.clone()
    } else {
        regex::escape(&req.query)
    };
    let pattern = if req.whole_word {
        format!(r"\b(?:{pattern})\b")
    } else {
        pattern
    };
    let re = RegexBuilder::new(&pattern)
        .case_insensitive(!req.match_case)
        .build()
        .map_err(|e| e.to_string())?;
    let include = glob_set(&req.include)?;
    let exclude = glob_set(&req.exclude)?;
    let mut skip: HashSet<_> = req.skip.iter().map(|p| path_key(p)).collect();
    let clean: HashMap<_, _> = req
        .disk_buffers
        .iter()
        .map(|(p, r)| (path_key(p), *r))
        .collect();
    let mut report = SearchReport::default();
    let mut memory = 0;
    let buffers = req.buffers.iter().map(|buf| OpenStream {
        path: buf.path.clone(),
        buffer_id: None,
        revision: buf.revision,
        reader: Box::new(Cursor::new(buf.text.as_bytes())),
    });
    for buf in buffers.chain(streams) {
        if (!req.buffers_only && relative_path(&buf.path, &req.root).is_none())
            || !allowed(&buf.path, &req.root, &include, &exclude)
            || (!req.buffers_only && !skip.insert(path_key(&buf.path)))
        {
            continue;
        }
        let position = report.hits.len();
        let result = scan(
            &mut report,
            &mut memory,
            req,
            &re,
            &buf.path,
            buf.reader,
            Encoding::Utf8,
            0,
            cancel,
        );
        if let Err(e) = result {
            report.errors.push(format!("{}: {e}", buf.path.display()));
        }
        if report.hits.len() > position {
            for hit in &mut report.hits[position..] {
                hit.buffer_id = buf.buffer_id;
            }
            report.sources.insert(
                SourceKey::at(&buf.path, buf.buffer_id),
                Source::Open(buf.revision),
            );
        }
        report.files_scanned += 1;
        if cancel.load(Ordering::Relaxed) || report.truncated {
            break;
        }
    }
    if req.buffers_only {
        report.cancelled = cancel.load(Ordering::Relaxed);
        return Ok(report);
    }
    if report.truncated || cancel.load(Ordering::Relaxed) {
        report.cancelled = cancel.load(Ordering::Relaxed);
        return Ok(report);
    }
    let (files, full) = search_disk(req, &re, &skip, &clean, &include, &exclude, memory, report.hits.len(), cancel)?;
    for file in files {
        report.files_scanned += file.scanned as usize;
        report.binary_skipped += file.binary as usize;
        report.truncated |= file.truncated;
        if let Some(e) = file.error {
            report.errors.push(format!("{}: {e}", file.path.display()));
        }
        if let Some(source) = file.source {
            report.sources.insert(SourceKey::File(file.path), source);
        }
        report.hits.extend(file.hits);
    }
    if report.hits.len() > req.max_hits {
        report.hits.truncate(req.max_hits);
        report.truncated = true;
    }
    report.truncated |= full;
    report.cancelled = cancel.load(Ordering::Relaxed);
    Ok(report)
}

/// Folders below the root that a folder search never enters.
const SKIPPED_FOLDERS: &[&str] = &[".git", ".hg", ".svn", "node_modules", ".venv"];
/// The most memory a search's results may take.
const RESULT_MEMORY: usize = 32 * 1024 * 1024;

/// What searching one file on disk produced.
#[derive(Default)]
struct FileResult {
    path: PathBuf,
    hits: Vec<Hit>,
    source: Option<Source>,
    error: Option<String>,
    scanned: bool,
    binary: bool,
    memory: usize,
    truncated: bool,
}

/// Searches the files under the root on several threads, with the shared
/// walker and the editor's rules for which folders and files count. Results
/// come back in path order, whatever order the threads finished in. The flag is
/// true when the result limit or the memory limit stopped the search early.
#[allow(clippy::too_many_arguments)]
fn search_disk(
    req: &SearchRequest,
    re: &Regex,
    skip: &HashSet<String>,
    clean: &HashMap<String, u64>,
    include: &GlobSet,
    exclude: &GlobSet,
    memory_so_far: usize,
    hits_so_far: usize,
    cancel: &AtomicBool,
) -> Result<(Vec<FileResult>, bool), String> {
    use std::sync::atomic::AtomicUsize;
    let options = linework_search::walk::Options {
        hidden: req.hidden,
        ignore: true,
        batch: 8,
        gitignore_anywhere: true,
        skip_folders: SKIPPED_FOLDERS,
        ..Default::default()
    };
    let (hits, memory, full) = (AtomicUsize::new(hits_so_far), AtomicUsize::new(memory_so_far), AtomicBool::new(false));
    let separator = std::path::MAIN_SEPARATOR_STR;
    let lists = linework_search::walk::walk(&req.root, options, Vec::<FileResult>::new, |done, found| {
        if cancel.load(Ordering::Relaxed) || full.load(Ordering::Relaxed) {
            return;
        }
        let path = req.root.join(found.path.replace('/', separator));
        if skip.contains(&path_key(&path)) || !allowed(&path, &req.root, include, exclude) {
            return;
        }
        let file = search_file(req, re, path, clean, cancel);
        let total = hits.fetch_add(file.hits.len(), Ordering::Relaxed) + file.hits.len();
        let used = memory.fetch_add(file.memory, Ordering::Relaxed) + file.memory;
        if total >= req.max_hits || used > RESULT_MEMORY || file.truncated {
            full.store(true, Ordering::Relaxed);
        }
        if file.scanned || file.binary || file.error.is_some() {
            done.push(file);
        }
    })
    .map_err(|e| e.to_string())?;
    let mut files: Vec<FileResult> = lists.into_iter().flatten().collect();
    // Folder by folder, names compared as Windows compares them, so the list
    // reads like the folder tree.
    let key = |path: &Path| -> Vec<String> { path.components().map(|c| c.as_os_str().to_string_lossy().to_uppercase()).collect() };
    files.sort_by_cached_key(|file| key(&file.path));
    Ok((files, full.load(Ordering::Relaxed)))
}

/// Searches one file on disk as the editor always has: its encoding found
/// from its first bytes, a late switch to Windows 1252 when UTF-8 fails, and
/// a check that it did not change while it was read.
fn search_file(req: &SearchRequest, re: &Regex, path: PathBuf, clean: &HashMap<String, u64>, cancel: &AtomicBool) -> FileResult {
    let mut report = SearchReport::default();
    let mut memory = 0;
    {
        let path = path.as_path();
        let from = 0;
        let result = (|| -> Result<(), String> {
            let stamp: Stamp = fs::metadata(path).map_err(|e| e.to_string())?.into();
            let (reader, mut enc, bom) = match text::open_text(path) {
                Ok(r) => r,
                Err(e)
                    if e.kind() == io::ErrorKind::InvalidData && e.to_string() == "binary file" =>
                {
                    report.binary_skipped += 1;
                    return Ok(());
                }
                Err(e) => return Err(e.to_string()),
            };
            report.files_scanned += 1;
            let prior_memory = memory;
            let mut scanned = scan(
                &mut report,
                &mut memory,
                req,
                &re,
                path,
                reader,
                enc,
                bom,
                cancel,
            );
            if enc == Encoding::Utf8
                && bom == 0
                && scanned.as_ref().err().map(String::as_str) == Some("invalid UTF-8 text")
            {
                report.hits.truncate(from);
                memory = prior_memory;
                enc = Encoding::Windows1252;
                let (reader, _, _) =
                    text::reader_as(fs::File::open(path).map_err(|e| e.to_string())?, enc, 0)
                        .map_err(|e| e.to_string())?;
                scanned = scan(
                    &mut report,
                    &mut memory,
                    req,
                    &re,
                    path,
                    reader,
                    enc,
                    0,
                    cancel,
                );
            }
            if report.hits.len() > from {
                let now: Stamp = fs::metadata(path).map_err(|e| e.to_string())?.into();
                if stamp != now {
                    report.hits.truncate(from);
                    return Err("changed during search".into());
                }
                let source = if let Some(v) = clean.get(&path_key(path)) {
                    Source::Open(*v)
                } else {
                    // Hash only files with selected-replacement candidates. The
                    // writer verifies this hash while copying the original bytes.
                    let digest = if req.replacement.is_some() {
                        Some(file_hash(path, cancel)?)
                    } else {
                        None
                    };
                    Source::Disk(stamp, enc, digest)
                };
                report.sources.insert(SourceKey::File(path.into()), source);
            }
            scanned
        })();
        if let Err(e) = result {
            report.errors.push(e);
        }
    }
    let source = report.sources.remove(&SourceKey::File(path.clone()));
    FileResult {
        source,
        error: report.errors.pop(),
        scanned: report.files_scanned > 0,
        binary: report.binary_skipped > 0,
        truncated: report.truncated,
        hits: report.hits,
        memory,
        path,
    }
}

#[allow(clippy::too_many_arguments)]
fn scan(
    report: &mut SearchReport,
    memory: &mut usize,
    req: &SearchRequest,
    re: &Regex,
    path: &Path,
    reader: impl io::BufRead,
    enc: Encoding,
    bom: usize,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let (mut line, mut logical, mut raw) = (1u64, 0u64, bom as u64);
    text::lines(reader, cancel, |s, eol| {
        let mut previous = 0;
        let mut column = 1u64;
        let mut disk = raw;
        if re.is_match(s) {
            for c in re.captures_iter(s) {
                if cancel.load(Ordering::Relaxed) {
                    return Ok(false);
                }
                let m = c.get(0).unwrap();
                column += s[previous..m.start()].chars().count() as u64;
                disk += enc.bytes(&s[previous..m.start()]);
                let replacement = req.replacement.as_ref().map(|replacement| {
                    if req.regex {
                        let mut value = String::new();
                        c.expand(replacement, &mut value);
                        value
                    } else {
                        replacement.clone()
                    }
                });
                let (preview, after, preview_context) = preview(s, m.start(), m.end(), replacement.as_deref());
                *memory += m.len()
                    + preview.len()
                    + after.as_ref().map_or(0, String::len)
                    + replacement.as_ref().map_or(0, String::len);
                if report.hits.len() >= req.max_hits || *memory > 32 * 1024 * 1024 {
                    report.truncated = true;
                    return Ok(false);
                }
                report.hits.push(Hit {
                    path: path.to_owned(),
                    buffer_id: None,
                    line,
                    column,
                    offset: logical + m.start() as u64,
                    length: m.len(),
                    physical: disk,
                    matched: m.as_str().to_owned(),
                    replacement,
                    preview,
                    preview_context,
                    preview_after: after,
                });
                previous = m.start();
            }
        }
        logical += (s.len() + eol) as u64;
        raw += enc.bytes(s)
            + if matches!(enc, Encoding::Utf16Le | Encoding::Utf16Be) {
                2 * eol as u64
            } else {
                eol as u64
            };
        line += 1;
        Ok(true)
    })
}
fn preview(
    line: &str,
    start: usize,
    end: usize,
    replacement: Option<&str>,
) -> (String, Option<String>, (usize, usize)) {
    let left = line[..start]
        .char_indices()
        .rev()
        .nth(51)
        .map_or(0, |(i, _)| i);
    let right = end
        + line[end..]
            .char_indices()
            .nth(65)
            .map_or(line.len() - end, |(i, _)| i);
    let before = format!("{}{}", if left > 0 { "…" } else { "" }, &line[left..start]);
    let after = format!(
        "{}{}",
        &line[end..right],
        if right < line.len() { "…" } else { "" }
    );
    let shorten = |s: &str| {
        let mut chars = s.chars();
        let mut text: String = chars.by_ref().take(120).collect();
        if chars.next().is_some() {
            text.push('…');
        }
        text.replace('\r', "␍")
            .replace('\n', "↵")
            .replace('\t', "⇥")
    };
    let render = |s: &str| format!("{before}{}{after}", shorten(s));
    (render(&line[start..end]), replacement.map(render), (before.len(), after.len()))
}

#[derive(Clone, Debug)]
pub struct TextEdit {
    pub offset: u64,
    pub matched: String,
    pub replacement: String,
}
#[derive(Clone, Debug)]
pub struct OpenPlan {
    pub path: PathBuf,
    pub buffer_id: Option<u64>,
    pub revision: u64,
    pub edits: Vec<TextEdit>,
}
impl OpenPlan {
    /// The caller can send the same validated edits to its editor in one undo group.
    pub fn validate(&self, text: &str, revision: u64) -> bool {
        revision == self.revision
            && self.edits.iter().all(|e| {
                let start = e.offset as usize;
                start
                    .checked_add(e.matched.len())
                    .and_then(|end| text.get(start..end))
                    == Some(e.matched.as_str())
            })
    }
    /// Edits are stored in descending order and preserve the original offsets.
    pub fn apply_to_text(&self, text: &mut String, revision: u64) -> Result<(), String> {
        if !self.validate(text, revision) {
            return Err("Buffer changed after preview. Search again.".into());
        }
        for e in &self.edits {
            let start = e.offset as usize;
            text.replace_range(start..start + e.matched.len(), &e.replacement);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Default)]
pub struct ApplyReport {
    pub applied: usize,
    pub changed_files: Vec<PathBuf>,
    pub open: Vec<OpenPlan>,
    pub errors: Vec<String>,
}
impl SearchReport {
    pub fn apply_selected(&self, indices: &[usize], open_paths: &[PathBuf]) -> ApplyReport {
        let mut report = ApplyReport::default();
        let mut groups: BTreeMap<SourceKey, Vec<&Hit>> = BTreeMap::new();
        let mut seen = HashSet::new();
        for &i in indices {
            if seen.insert(i) {
                if let Some(hit) = self.hits.get(i).filter(|h| h.replacement.is_some()) {
                    groups
                        .entry(SourceKey::at(&hit.path, hit.buffer_id))
                        .or_default()
                        .push(hit);
                }
            }
        }
        let open: HashSet<_> = open_paths.iter().map(|p| path_key(p)).collect();
        for (key, mut hits) in groups {
            hits.sort_by_key(|h| h.offset);
            let path = hits[0].path.as_path();
            let buffer_id = hits[0].buffer_id;
            let is_open = open.contains(&path_key(path));
            let error = match self.sources.get(&key) {
                Some(Source::Open(revision)) if buffer_id.is_some() || is_open => {
                    let edits = hits
                        .iter()
                        .rev()
                        .map(|h| TextEdit {
                            offset: h.offset,
                            matched: h.matched.clone(),
                            replacement: h.replacement.clone().unwrap(),
                        })
                        .collect();
                    report.open.push(OpenPlan {
                        path: path.into(),
                        buffer_id,
                        revision: *revision,
                        edits,
                    });
                    None
                }
                Some(Source::Disk(stamp, enc, Some(digest))) if !is_open => {
                    match replace_disk(path, stamp, *enc, digest, &hits) {
                        Ok(()) => {
                            report.applied += hits.len();
                            report.changed_files.push(path.into());
                            None
                        }
                        Err(e) => Some(e),
                    }
                }
                _ => Some("The file's open/closed state changed. Refresh the preview.".to_string()),
            };
            if let Some(e) = error {
                report.errors.push(format!("{}: {e}", path.display()));
            }
        }
        report
    }
}
struct VerifiedReader<R> {
    inner: R,
    hash: blake3::Hasher,
    pos: u64,
}
impl<R: Read> Read for VerifiedReader<R> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(b)?;
        self.hash.update(&b[..n]);
        self.pos += n as u64;
        Ok(n)
    }
}
fn replace_disk(
    path: &Path,
    stamp: &Stamp,
    enc: Encoding,
    digest: &blake3::Hash,
    hits: &[&Hit],
) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ};
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE);
    }
    let file = options.open(path).map_err(|e| e.to_string())?;
    let mut source = VerifiedReader {
        inner: file,
        hash: blake3::Hasher::new(),
        pos: 0,
    };
    let verify = || -> Result<(), String> {
        if Stamp::from(fs::metadata(path).map_err(|e| e.to_string())?) != *stamp {
            Err("File changed since preview. Refresh the results.".into())
        } else {
            Ok(())
        }
    };
    verify()?;
    let parent = path.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    {
        let mut out = BufWriter::new(tmp.as_file_mut());
        for hit in hits {
            let count = hit
                .physical
                .checked_sub(source.pos)
                .ok_or("Overlapping replacement ranges")?;
            let n =
                io::copy(&mut (&mut source).take(count), &mut out).map_err(|e| e.to_string())?;
            if n != count {
                return Err("Source changed during replacement".into());
            }
            let saved = enc.encode(&hit.matched)?;
            let mut bytes = vec![0; saved.len()];
            source.read_exact(&mut bytes).map_err(|e| e.to_string())?;
            if saved != bytes {
                return Err("Matched text changed. Refresh the results.".into());
            }
            out.write_all(&enc.encode(hit.replacement.as_ref().unwrap())?)
                .map_err(|e| e.to_string())?;
        }
        io::copy(&mut source, &mut out).map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
    }
    if &source.hash.finalize() != digest {
        return Err("File contents changed since preview. Refresh the results.".into());
    }
    tmp.as_file().sync_all().map_err(|e| e.to_string())?;
    verify()?;
    let permissions = source
        .inner
        .metadata()
        .map_err(|e| e.to_string())?
        .permissions();
    let published = publish(tmp, path, permissions);
    drop(source);
    published
}
#[cfg(windows)]
fn publish(
    tmp: tempfile::NamedTempFile,
    path: &Path,
    _permissions: fs::Permissions,
) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
    let tmp = tmp.into_temp_path();
    let src: Vec<_> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
    let dst: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let ptr = std::ptr::null_mut();
    // ReplaceFile preserves the destination's ACL, alternate streams, and attributes.
    if unsafe { ReplaceFileW(dst.as_ptr(), src.as_ptr(), std::ptr::null(), 0, ptr, ptr) } == 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(())
}
#[cfg(not(windows))]
fn publish(
    tmp: tempfile::NamedTempFile,
    path: &Path,
    permissions: fs::Permissions,
) -> Result<(), String> {
    tmp.as_file()
        .set_permissions(permissions)
        .map_err(|e| e.to_string())?;
    tmp.persist(path).map_err(|e| e.to_string())?;
    Ok(())
}
