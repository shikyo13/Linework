//! The editor's search over a folder and its open tabs. A search is lw's words
//! (options, the pattern, paths), compiled into the same plan lw uses, so the
//! same words find the same lines in both. What the editor adds: open tabs
//! searched with their unsaved text, files read in the encoding the editor
//! would open them in, positions as the editor counts lines, and a record of
//! each file's state so a replacement is applied only to what was previewed.
use crate::change;
use crate::text::{self, Encoding};
use linework_search::matcher::{Groups, Syntax, Template};
use linework_search::plan::Plan;
use linework_search::searcher::{self, Searcher, Sink};
use linework_search::settings::Settings;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, Metadata};
use std::io::{self, Read};
use std::ops::Range;
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

/// The words every editor search starts with: `.gitignore` files count
/// outside Git work trees too, and globs ignore letter case, as Windows does
/// with file names. They are part of the command a search shows for lw.
pub const EDITOR_WORDS: &[&str] = &["--no-require-git", "--glob-case-insensitive"];

#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub root: PathBuf,
    /// The search as lw's words: options, the pattern, then any paths below
    /// the root to search instead of all of it. lw given `EDITOR_WORDS` and
    /// these words in the root folder finds the same lines.
    pub words: Vec<String>,
    /// The replacement to preview. `$1`, `${1}`, `$name` and `$$` work as in
    /// lw's `-r`, unless the pattern is plain text, when it is taken as written.
    pub replacement: Option<String>,
    pub buffers: Vec<OpenBuffer>,
    /// Clean large files can be searched from disk and edited in their open buffer.
    pub disk_buffers: Vec<(PathBuf, u64)>,
    /// Dirty buffers that cannot cheaply supply a snapshot must never be read from disk.
    pub skip: Vec<PathBuf>,
    pub max_hits: usize,
    pub buffers_only: bool,
}
impl SearchRequest {
    pub fn new(root: PathBuf, words: Vec<String>) -> Self {
        Self {
            root,
            words,
            replacement: None,
            buffers: Vec::new(),
            disk_buffers: Vec::new(),
            skip: Vec::new(),
            max_hits: 10_000,
            buffers_only: false,
        }
    }

    /// The whole command, as it would be typed for lw in the root folder.
    pub fn command(&self) -> Vec<String> {
        EDITOR_WORDS.iter().map(|w| (*w).to_owned()).chain(self.words.iter().cloned()).collect()
    }
}

/// One match.
#[derive(Clone, Debug)]
pub struct Hit {
    pub path: PathBuf,
    pub buffer_id: Option<u64>,
    /// The line and column as the editor counts them, from 1: CR, LF and CRLF
    /// each end a line, and columns count characters.
    pub line: u64,
    pub column: u64,
    /// Where the match starts in the file's text, as UTF-8.
    pub offset: u64,
    pub length: usize,
    pub preview: String,
    pub preview_after: Option<String>,
    /// Byte counts of the contextual text surrounding the shortened match.
    pub preview_context: (usize, usize),
    pub matched: String,
    pub replacement: Option<String>,
}

/// What a line in the results is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// A line the search selected: one with a match, or with `-v` one without.
    Match,
    /// A line of context before or after one (`-A`, `-B`, `-C`).
    Context,
    /// Between groups of lines that are not next to each other.
    Break,
}

/// A line as the results show it.
#[derive(Clone, Debug)]
pub struct ResultLine {
    pub kind: LineKind,
    /// The line's number as the editor counts lines, from 1.
    pub line: u64,
    /// Where the shown text starts in the file's text.
    pub offset: u64,
    /// The line without its line end. A line over `LONG_LINE` bytes is cut to
    /// the part around its first match.
    pub text: String,
    /// Bytes of the line left out before and after `text`.
    pub omitted: (usize, usize),
    /// Each match as a byte range in `text`.
    pub ranges: Vec<(usize, usize)>,
    /// The matches that start on this line, as indices into `SearchReport::hits`.
    pub hits: Range<usize>,
    /// The definitions the line is inside, outermost first (with `--scope`).
    pub scope: Vec<crate::symbols::Scope>,
}

/// One file's lines in the results.
#[derive(Clone, Debug)]
pub struct FileGroup {
    pub path: PathBuf,
    pub buffer_id: Option<u64>,
    pub lines: Vec<ResultLine>,
    /// Matching (selected) lines and matches in the file.
    pub matched_lines: usize,
    pub matches: usize,
}

/// A line longer than this is shown as the part around its first match, as lw shows it.
pub const LONG_LINE: usize = 1000;
/// How much of a long line is shown.
const LONG_WINDOW: usize = 800;

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
    Disk(Stamp, Option<blake3::Hash>),
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
    /// Every match, in the order of `groups`.
    pub hits: Vec<Hit>,
    /// The lines to show, file by file, in path order (open tabs first).
    pub groups: Vec<FileGroup>,
    /// The words searched with, `EDITOR_WORDS` first: the lw command for the same search.
    pub command: Vec<String>,
    pub files_scanned: usize,
    pub binary_skipped: usize,
    pub errors: Vec<String>,
    pub truncated: bool,
    pub cancelled: bool,
    sources: HashMap<SourceKey, Source>,
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

/// A path below the root as the file choice reads it: `/` between names.
fn slashed(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

pub fn search(req: &SearchRequest, cancel: &AtomicBool) -> Result<SearchReport, String> {
    search_with_streams(req, Vec::new(), cancel)
}

/// What one search needs, shared by every file it reads.
struct Job<'a> {
    req: &'a SearchRequest,
    settings: Settings,
    plan: Plan,
    /// None when the replacement is taken as written.
    template: Option<Template>,
    /// Search whole files: the plan needs it, or `--scope` does.
    whole: bool,
    /// The folder searched, made absolute, so every path found compares with
    /// the open tabs' paths: a file open in the editor is never written on disk.
    root: PathBuf,
}

pub fn search_with_streams<'a>(
    req: &'a SearchRequest,
    streams: Vec<OpenStream<'a>>,
    cancel: &AtomicBool,
) -> Result<SearchReport, String> {
    let command = req.command();
    let settings = Settings::from_words(command.iter().cloned())?;
    let (patterns, paths) = settings.patterns_and_paths()?;
    if patterns.iter().all(String::is_empty) {
        return Err("Enter text to search for.".into());
    }
    if !req.buffers_only && !req.root.is_dir() {
        return Err("Open a folder to search the workspace.".into());
    }
    let plan = Plan::new(&settings, patterns)?;
    let literal = settings.matcher.syntax == Syntax::Fixed || plan.query.plain_text().is_some();
    let template = req.replacement.as_deref().filter(|_| !literal).map(Template::new);
    let whole = plan.needs_whole_text() || settings.scope;
    let root = std::path::absolute(&req.root).unwrap_or_else(|_| req.root.clone());
    let job = Job { req, settings, plan, template, whole, root };
    // The places searched: the paths given, below the root, or the root itself.
    let places: Vec<PathBuf> = if paths.is_empty() {
        vec![job.root.clone()]
    } else {
        paths.iter().map(|p| job.root.join(p)).collect()
    };
    let inside = |path: &Path| {
        places.iter().any(|place| path_key(path) == path_key(place) || relative_path(path, place).is_some())
    };
    let chosen = |path: &Path| {
        let Some(pick) = job.plan.pick.as_ref() else { return true };
        let relative = relative_path(path, &job.root).unwrap_or(path);
        pick.keeps(&slashed(relative))
    };
    let mut skip: HashSet<_> = req.skip.iter().map(|p| path_key(p)).collect();
    let clean: HashMap<_, _> = req
        .disk_buffers
        .iter()
        .map(|(p, r)| (path_key(p), *r))
        .collect();
    let mut report = SearchReport { command, ..SearchReport::default() };
    let mut memory = 0;
    let buffers = req.buffers.iter().map(|buf| OpenStream {
        path: buf.path.clone(),
        buffer_id: None,
        revision: buf.revision,
        reader: Box::new(io::Cursor::new(buf.text.as_bytes())),
    });
    for buf in buffers.chain(streams) {
        if (!req.buffers_only && (!inside(&buf.path) || !chosen(&buf.path)))
            || (!req.buffers_only && !skip.insert(path_key(&buf.path)))
        {
            continue;
        }
        let allowance = Allowance { hits: req.max_hits.saturating_sub(report.hits.len()), memory: RESULT_MEMORY.saturating_sub(memory) };
        let mut file = FileScan::new(&job, &buf.path, allowance, cancel);
        let result = file.read(buf.reader);
        report.files_scanned += 1;
        match result {
            Ok(()) => {
                memory += file.memory;
                report.truncated |= file.truncated;
                if let Some(mut group) = file.finish(buf.buffer_id, report.hits.len()) {
                    for hit in &mut file.hits {
                        hit.buffer_id = buf.buffer_id;
                    }
                    group.buffer_id = buf.buffer_id;
                    report.hits.append(&mut file.hits);
                    report.groups.push(group);
                    report.sources.insert(SourceKey::at(&buf.path, buf.buffer_id), Source::Open(buf.revision));
                }
            }
            Err(e) => report.errors.push(format!("{}: {e}", buf.path.display())),
        }
        if cancel.load(Ordering::Relaxed) || report.truncated {
            break;
        }
    }
    if req.buffers_only || report.truncated || cancel.load(Ordering::Relaxed) {
        report.cancelled = cancel.load(Ordering::Relaxed);
        return Ok(report);
    }
    let (files, full) = search_disk(&job, &places, &skip, &clean, memory, report.hits.len(), cancel)?;
    for mut file in files {
        report.files_scanned += file.scanned as usize;
        report.binary_skipped += file.binary as usize;
        report.truncated |= file.truncated;
        if let Some(e) = file.error {
            report.errors.push(format!("{}: {e}", file.path.display()));
        }
        if let Some(source) = file.source {
            report.sources.insert(SourceKey::File(file.path.clone()), source);
        }
        if let Some(mut group) = file.group {
            let shift = report.hits.len();
            for line in &mut group.lines {
                line.hits = line.hits.start + shift..line.hits.end + shift;
            }
            report.groups.push(group);
        }
        report.hits.append(&mut file.hits);
    }
    if report.hits.len() > req.max_hits {
        // Files searched at the same time can pass the limit together: drop the matches beyond it, and their lines.
        report.hits.truncate(req.max_hits);
        let most = req.max_hits;
        for group in &mut report.groups {
            group.lines.retain(|l| l.hits.is_empty() || l.hits.start < most);
            for line in &mut group.lines {
                line.hits = line.hits.start.min(most)..line.hits.end.min(most);
            }
        }
        report.groups.retain(|g| g.lines.iter().any(|l| l.kind == LineKind::Match));
        report.truncated = true;
    }
    report.truncated |= full;
    report.cancelled = cancel.load(Ordering::Relaxed);
    Ok(report)
}

/// Folders below the root that a folder search never enters.
pub(crate) const SKIPPED_FOLDERS: &[&str] = &[".git", ".hg", ".svn", "node_modules", ".venv"];
/// The most memory a search's results may take.
const RESULT_MEMORY: usize = 32 * 1024 * 1024;
/// How much of a file is read at a time when it is not read whole.
const BLOCK: usize = 1 << 20;

/// What searching one file on disk produced.
#[derive(Default)]
struct FileResult {
    path: PathBuf,
    hits: Vec<Hit>,
    group: Option<FileGroup>,
    source: Option<Source>,
    error: Option<String>,
    scanned: bool,
    binary: bool,
    memory: usize,
    truncated: bool,
}

/// Searches the files under each place on several threads, with lw's walk,
/// ignore rules and file choice, and the editor's folders never entered.
/// Results come back in path order, whatever order the threads finished in.
/// The flag is true when the result limit or the memory limit stopped the
/// search early.
#[allow(clippy::too_many_arguments)]
fn search_disk(
    job: &Job,
    places: &[PathBuf],
    skip: &HashSet<String>,
    clean: &HashMap<String, u64>,
    memory_so_far: usize,
    hits_so_far: usize,
    cancel: &AtomicBool,
) -> Result<(Vec<FileResult>, bool), String> {
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;
    let req = job.req;
    let (hits, memory, full) = (AtomicUsize::new(hits_so_far), AtomicUsize::new(memory_so_far), AtomicBool::new(false));
    let failures = Mutex::new(Vec::<FileResult>::new());
    let mut files = Vec::new();
    let one = |path: PathBuf| -> Option<FileResult> {
        if cancel.load(Ordering::Relaxed) || full.load(Ordering::Relaxed) || skip.contains(&path_key(&path)) {
            return None;
        }
        let allowance = Allowance {
            hits: req.max_hits.saturating_sub(hits.load(Ordering::Relaxed)),
            memory: RESULT_MEMORY.saturating_sub(memory.load(Ordering::Relaxed)),
        };
        let file = search_file(job, path, clean, allowance, cancel);
        let total = hits.fetch_add(file.hits.len(), Ordering::Relaxed) + file.hits.len();
        let used = memory.fetch_add(file.memory, Ordering::Relaxed) + file.memory;
        if total >= req.max_hits || used > RESULT_MEMORY || file.truncated {
            full.store(true, Ordering::Relaxed);
        }
        (file.scanned || file.binary || file.error.is_some()).then_some(file)
    };
    for place in places {
        if place.is_file() {
            files.extend(one(place.clone()));
            continue;
        }
        if !place.is_dir() {
            files.push(FileResult { path: place.clone(), error: Some("no such file or folder".into()), ..FileResult::default() });
            continue;
        }
        let failure = |path: &str, why: &str| {
            failures.lock().unwrap_or_else(|e| e.into_inner()).push(FileResult {
                path: place.join(path),
                error: Some(why.to_owned()),
                ..FileResult::default()
            });
        };
        let options = linework_search::walk::Options {
            base: Some(&job.root),
            skip_folders: SKIPPED_FOLDERS,
            failure: Some(&failure),
            ..job.plan.walk_options(&job.settings)
        };
        let separator = std::path::MAIN_SEPARATOR_STR;
        let lists = linework_search::walk::walk(place, options, Vec::<FileResult>::new, |done, found| {
            if let Some(file) = one(place.join(found.path.replace('/', separator))) {
                done.push(file);
            }
        })
        .map_err(|e| e.to_string())?;
        files.extend(lists.into_iter().flatten());
    }
    files.extend(failures.into_inner().unwrap_or_default());
    // Folder by folder, names compared as Windows compares them, so the list
    // reads like the folder tree.
    let key = |path: &Path| -> Vec<String> { path.components().map(|c| c.as_os_str().to_string_lossy().to_uppercase()).collect() };
    files.sort_by_cached_key(|file| key(&file.path));
    Ok((files, full.load(Ordering::Relaxed)))
}

/// Searches one file on disk as the editor reads it: its encoding found from
/// its first bytes, Windows 1252 when it turns out not to be UTF-8, and a check
/// that it did not change while it was read.
fn search_file(job: &Job, path: PathBuf, clean: &HashMap<String, u64>, allowance: Allowance, cancel: &AtomicBool) -> FileResult {
    let mut out = FileResult { path: path.clone(), ..FileResult::default() };
    let result = (|| -> Result<(), String> {
        let stamp: Stamp = fs::metadata(&path).map_err(|e| e.to_string())?.into();
        let (reader, mut enc, bom) = match text::open_text(&path) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::InvalidData && e.to_string() == "binary file" => {
                out.binary = true;
                return Ok(());
            }
            Err(e) => return Err(e.to_string()),
        };
        out.scanned = true;
        let mut file = FileScan::new(job, &path, allowance, cancel);
        let mut scanned = file.read(reader);
        if enc == Encoding::Utf8 && bom == 0 && scanned.as_ref().err().map(String::as_str) == Some(NOT_UTF8) {
            enc = Encoding::Windows1252;
            let (reader, _, _) = text::reader_as(fs::File::open(&path).map_err(|e| e.to_string())?, enc, 0).map_err(|e| e.to_string())?;
            file = FileScan::new(job, &path, allowance, cancel);
            scanned = file.read(reader);
        }
        scanned?;
        out.truncated = file.truncated;
        out.memory = file.memory;
        if let Some(group) = file.finish(None, 0) {
            let now: Stamp = fs::metadata(&path).map_err(|e| e.to_string())?.into();
            if stamp != now {
                return Err("changed during search".into());
            }
            out.source = Some(if let Some(v) = clean.get(&path_key(&path)) {
                Source::Open(*v)
            } else {
                // Hash only files with replacement candidates. The writer
                // verifies this hash while copying the original bytes.
                let digest = if job.req.replacement.is_some() { Some(file_hash(&path, cancel)?) } else { None };
                Source::Disk(stamp, digest)
            });
            out.hits = std::mem::take(&mut file.hits);
            out.group = Some(group);
        }
        Ok(())
    })();
    if let Err(e) = result {
        out.error = Some(e);
        out.hits.clear();
        out.group = None;
    }
    out
}

/// What may still be added to the results.
#[derive(Clone, Copy)]
struct Allowance {
    hits: usize,
    memory: usize,
}

const NOT_UTF8: &str = "invalid UTF-8 text";

/// Where the search has counted to in a file: the offset in its text, and
/// the line as the editor counts lines and where that line starts.
#[derive(Clone, Copy, Default)]
struct Position {
    offset: u64,
    line: u64,
    line_start: u64,
    after_cr: bool,
}

/// One file being searched: what it found so far and where it has counted to.
struct FileScan<'j> {
    job: &'j Job<'j>,
    path: &'j Path,
    allowance: Allowance,
    cancel: &'j AtomicBool,
    position: Position,
    hits: Vec<Hit>,
    lines: Vec<ResultLine>,
    matched_lines: usize,
    memory: usize,
    truncated: bool,
    groups: Groups,
}

impl<'j> FileScan<'j> {
    fn new(job: &'j Job<'j>, path: &'j Path, allowance: Allowance, cancel: &'j AtomicBool) -> Self {
        Self {
            job,
            path,
            allowance,
            cancel,
            position: Position { line: 1, ..Position::default() },
            hits: Vec::new(),
            lines: Vec::new(),
            matched_lines: 0,
            memory: 0,
            truncated: false,
            groups: Groups::new(),
        }
    }

    /// Reads and searches the text from `reader`: whole when the plan needs
    /// it, otherwise a block at a time with the lines context may need kept.
    fn read(&mut self, mut reader: impl Read) -> Result<(), String> {
        let job = self.job;
        let plan = &job.plan;
        if job.whole {
            let mut text = Vec::new();
            reader.read_to_end(&mut text).map_err(|e| e.to_string())?;
            if std::str::from_utf8(&text).is_err() {
                return Err(NOT_UTF8.into());
            }
            if !plan.file_passes(&text) {
                return Ok(());
            }
            let near = plan.near_lines(&text);
            let keep = |number: u64| near.keeps(number);
            let mut searcher = plan.searcher((!near.is_empty()).then_some(&keep as searcher::Keep));
            searcher.search(&text, &mut Piece { scan: self, buf: &text, base: 0 });
            self.add_scopes(&text);
            return Ok(());
        }
        let mut searcher: Searcher = plan.searcher(None);
        let line_end = plan.query.line_end();
        let mut buf: Vec<u8> = Vec::with_capacity(BLOCK * 2);
        let (mut base, mut searched, mut checked, mut ended) = (0u64, 0usize, 0usize, false);
        // Set when what was read holds no line end past what was searched: read on, past a block.
        let mut grow = false;
        while !ended && !searcher.done() && !self.stopped() {
            let target = if grow { buf.len() + BLOCK } else { searched + BLOCK };
            while buf.len() < target {
                let have = buf.len();
                buf.resize(target, 0);
                match reader.read(&mut buf[have..]) {
                    Ok(0) => {
                        buf.truncate(have);
                        ended = true;
                        break;
                    }
                    Ok(n) => buf.truncate(have + n),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => buf.truncate(have),
                    Err(e) => return Err(e.to_string()),
                }
            }
            // The new bytes must be UTF-8; a character cut at the end of what was read waits for the rest.
            match std::str::from_utf8(&buf[checked..]) {
                Ok(_) => checked = buf.len(),
                Err(e) if e.error_len().is_none() && !ended => checked += e.valid_up_to(),
                Err(_) => return Err(NOT_UTF8.into()),
            }
            let end = if ended { buf.len() } else { memchr::memrchr(line_end, &buf[searched..checked]).map_or(searched, |e| searched + e + 1) };
            grow = !ended && end == searched;
            if end > searched || ended {
                searcher.piece(&buf, base, searched, end, ended, &mut Piece { scan: self, buf: &buf, base });
            }
            let keep = searcher.keep_from(&buf, end);
            // Count up to what is let go, while its text is still here.
            self.advance(&buf, base, base + keep as u64);
            buf.drain(..keep);
            base += keep as u64;
            searched = end - keep;
            checked -= keep;
        }
        Ok(())
    }

    fn stopped(&self) -> bool {
        self.truncated || self.cancel.load(Ordering::Relaxed)
    }

    /// Counts lines from where the count stands to `to`,
    /// with `buf` holding the text from `base`.
    fn advance(&mut self, buf: &[u8], base: u64, to: u64) {
        let p = &mut self.position;
        if to <= p.offset {
            return;
        }
        let from = (p.offset - base) as usize;
        let text = &buf[from..(to - base) as usize];
        for at in memchr::memchr2_iter(b'\n', b'\r', text) {
            let crlf = text[at] == b'\n' && (if at == 0 { p.after_cr } else { text[at - 1] == b'\r' });
            if !crlf {
                p.line += 1;
            }
            p.line_start = p.offset + at as u64 + 1;
        }
        if let Some(&last) = text.last() {
            p.after_cr = last == b'\r';
        }
        p.offset = to;
    }

    /// The column of `offset` as the editor counts it: characters from the start of its line, from 1.
    fn column(&self, buf: &[u8], base: u64, offset: u64) -> u64 {
        let start = (self.position.line_start.max(base) - base) as usize;
        let text = &buf[start..(offset - base) as usize];
        1 + std::str::from_utf8(text).map_or(text.len(), |s| s.chars().count()) as u64
    }

    /// A selected block (a line, or with -U the lines one match covers): each
    /// match recorded, and each of its lines added to the results.
    fn selected(&mut self, buf: &[u8], base: u64, offset: u64, block: &[u8]) -> bool {
        let job = self.job;
        let mut matches = Vec::new();
        job.plan.query.each(block, |s, e| {
            matches.push((s, e));
            true
        });
        self.matched_lines += 1;
        let mut next = 0;
        let mut start = 0usize;
        for row in block.split(|&b| b == b'\n') {
            let row_end = start + row.len();
            let row_offset = offset + start as u64;
            self.advance(buf, base, row_offset);
            let line = self.position.line;
            let first_hit = self.hits.len();
            // The matches that start on this row become its hits.
            while next < matches.len() && matches[next].0 <= row_end {
                let (s, e) = matches[next];
                if !self.hit(buf, base, offset, block, s, e) {
                    return false;
                }
                next += 1;
            }
            // Every match that covers part of the row is marked on it.
            let ranges: Vec<(usize, usize)> = matches
                .iter()
                .filter(|&&(s, e)| if s == e { s >= start && s <= row_end } else { s < row_end && e > start })
                .map(|&(s, e)| (s.max(start) - start, e.min(row_end) - start))
                .collect();
            let hits = first_hit..self.hits.len();
            if !self.row(LineKind::Match, line, row_offset, row, ranges, hits) {
                return false;
            }
            start = row_end + 1;
        }
        true
    }

    /// Records one match: its position as the editor counts it, and its replacement.
    fn hit(&mut self, buf: &[u8], base: u64, offset: u64, block: &[u8], s: usize, e: usize) -> bool {
        if self.hits.len() >= self.allowance.hits {
            self.truncated = true;
            return false;
        }
        let at = offset + s as u64;
        self.advance(buf, base, at);
        let column = self.column(buf, base, at);
        let line = self.position.line;
        let text = String::from_utf8_lossy(block);
        let replacement = self.job.req.replacement.as_ref().map(|written| match &self.job.template {
            None => written.clone(),
            Some(template) => {
                let mut out = Vec::new();
                if self.job.plan.query.captures(block, s, &mut self.groups).is_some() {
                    template.expand(&self.job.plan.query, block, &self.groups, &mut out);
                }
                String::from_utf8_lossy(&out).into_owned()
            }
        });
        // The preview shows the line the match starts on.
        let line_from = block[..s].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let line_to = block[s..].iter().position(|&b| b == b'\n').map_or(block.len(), |i| s + i);
        let shown = &text[line_from..line_to.max(line_from)];
        let (preview, after, preview_context) = preview(shown, s - line_from, e.min(line_to).max(s) - line_from, replacement.as_deref());
        let matched = text[s..e].to_owned();
        self.memory += matched.len() + preview.len() + after.as_ref().map_or(0, String::len) + replacement.as_ref().map_or(0, String::len);
        if self.memory > self.allowance.memory {
            self.truncated = true;
            return false;
        }
        self.hits.push(Hit {
            path: self.path.to_owned(),
            buffer_id: None,
            line,
            column,
            offset: at,
            length: e - s,
            matched,
            replacement,
            preview,
            preview_context,
            preview_after: after,
        });
        true
    }

    /// Adds a line to the results, cut around its first match when it is long.
    fn row(&mut self, kind: LineKind, line: u64, offset: u64, text: &[u8], ranges: Vec<(usize, usize)>, hits: Range<usize>) -> bool {
        // The CR of a CRLF line end is not part of what is shown.
        let text = text.strip_suffix(b"\r").unwrap_or(text);
        let (from, to) = if text.len() > LONG_LINE {
            let first = ranges.first().map_or(0, |r| r.0);
            let from = floor(text, first.saturating_sub(LONG_WINDOW / 4));
            (from, floor(text, (from + LONG_WINDOW).min(text.len())))
        } else {
            (0, text.len())
        };
        let shown = String::from_utf8_lossy(&text[from..to]).into_owned();
        let ranges = ranges
            .into_iter()
            .filter(|(s, e)| *e >= from && *s <= to)
            .map(|(s, e)| (s.max(from) - from, e.min(to) - from))
            .collect();
        self.memory += shown.len() + 64;
        if self.memory > self.allowance.memory {
            self.truncated = true;
            return false;
        }
        self.lines.push(ResultLine {
            kind,
            line,
            offset: offset + from as u64,
            text: shown,
            omitted: (from, text.len() - to),
            ranges,
            hits,
            scope: Vec::new(),
        });
        true
    }

    /// With --scope, the definitions around each selected line, from the whole text.
    fn add_scopes(&mut self, text: &[u8]) {
        if !self.job.settings.scope || text.len() > crate::symbols::MAX_SYMBOL_SOURCE || self.lines.is_empty() {
            return;
        }
        let Ok(text) = std::str::from_utf8(text) else { return };
        let rows: Vec<usize> = (0..self.lines.len()).filter(|&i| self.lines[i].kind == LineKind::Match).collect();
        let offsets: Vec<usize> = rows.iter().map(|&i| self.lines[i].offset as usize).collect();
        for (i, chain) in rows.into_iter().zip(crate::symbols::scope_chains(self.path, text, &offsets)) {
            self.lines[i].scope = chain;
        }
    }

    /// The file's group of lines, its hits numbered from `first_hit`; None when nothing was selected.
    fn finish(&mut self, buffer_id: Option<u64>, first_hit: usize) -> Option<FileGroup> {
        if self.matched_lines == 0 {
            return None;
        }
        let mut lines = std::mem::take(&mut self.lines);
        for line in &mut lines {
            line.hits = line.hits.start + first_hit..line.hits.end + first_hit;
        }
        Some(FileGroup {
            path: self.path.to_owned(),
            buffer_id,
            lines,
            matched_lines: self.matched_lines,
            matches: self.hits.len(),
        })
    }
}

/// The largest character boundary at or before `at`.
fn floor(text: &[u8], at: usize) -> usize {
    let mut at = at.min(text.len());
    while at > 0 && at < text.len() && (text[at] & 0xC0) == 0x80 {
        at -= 1;
    }
    at
}

/// The searcher's lines, for one piece of a file's text.
struct Piece<'s, 'j, 'b> {
    scan: &'s mut FileScan<'j>,
    buf: &'b [u8],
    base: u64,
}

impl Sink for Piece<'_, '_, '_> {
    fn matched(&mut self, _number: u64, offset: u64, text: &[u8], _ended: bool) -> bool {
        !self.scan.stopped() && self.scan.selected(self.buf, self.base, offset, text)
    }
    fn context(&mut self, _number: u64, offset: u64, text: &[u8], _after: bool, _ended: bool) -> bool {
        if self.scan.stopped() {
            return false;
        }
        self.scan.advance(self.buf, self.base, offset);
        let line = self.scan.position.line;
        let at = self.scan.hits.len();
        self.scan.row(LineKind::Context, line, offset, text, Vec::new(), at..at)
    }
    fn context_break(&mut self) -> bool {
        let at = self.scan.hits.len();
        let line = self.scan.position.line;
        let offset = self.scan.position.offset;
        self.scan.lines.push(ResultLine {
            kind: LineKind::Break,
            line,
            offset,
            text: String::new(),
            omitted: (0, 0),
            ranges: Vec::new(),
            hits: at..at,
            scope: Vec::new(),
        });
        true
    }
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
    /// The change record of the files saved, which the editor's undo and lw undo both read.
    pub change: Option<String>,
}
impl SearchReport {
    /// Applies the replacements of the hits at `indices`. A file open in the
    /// editor gets a plan for its buffer, undone there with the editor's undo.
    /// Any other file is changed and saved by the file engine, as lw replace
    /// saves it, with its edits kept first in a change record in `changes`, so
    /// the whole change can be undone, by the editor or by lw undo.
    pub fn apply_selected(&self, indices: &[usize], open_paths: &[PathBuf], changes: Option<&Path>) -> ApplyReport {
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
        let mut record: Option<change::Record> = None;
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
                Some(Source::Disk(stamp, Some(digest))) if !is_open => {
                    // The record starts with the first file to be saved.
                    if let (None, Some(folder)) = (&record, changes) {
                        match change::Record::start(folder, &self.change_command()) {
                            Ok(started) => record = Some(started),
                            Err(e) => {
                                report.errors.push(format!("The change record could not be started, so nothing was saved: {e}"));
                                return report;
                            }
                        }
                    }
                    match replace_disk(path, stamp, digest, &hits, record.as_mut()) {
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
        if let Some(mut record) = record {
            if let Err(e) = record.finish() {
                report.errors.push(format!("The change record was not closed: {e}"));
            }
            report.change = Some(record.id.clone());
        }
        report
    }

    /// How a change from the editor is listed by lw undo --list.
    fn change_command(&self) -> Vec<String> {
        ["Linework", "folder", "replace,", "search:", "lw"].iter().map(|w| (*w).to_owned()).chain(self.command.iter().cloned()).collect()
    }
}

/// Replaces `hits` in a file on disk and saves it with the file engine, as
/// the editor and lw replace save: in its encoding, and refused if it changed
/// on disk meanwhile. Before that the file must be as it was searched: the
/// same size, times and contents, and each match still where it was found.
/// The edits go into `record` before the file is saved.
fn replace_disk(path: &Path, stamp: &Stamp, digest: &blake3::Hash, hits: &[&Hit], record: Option<&mut change::Record>) -> Result<(), String> {
    let changed = || "File changed since preview. Refresh the results.".to_string();
    if Stamp::from(fs::metadata(path).map_err(|e| e.to_string())?) != *stamp {
        return Err(changed());
    }
    if file_hash(path, &AtomicBool::new(false))? != *digest {
        return Err(changed());
    }
    let mut doc = linework_storage::Document::open(path).map_err(|e| e.to_string())?;
    let mut back = Vec::with_capacity(hits.len());
    let mut edits = Vec::with_capacity(hits.len());
    let (mut shift, mut free_from) = (0i64, 0u64);
    for hit in hits {
        let old = hit.matched.as_bytes();
        if hit.offset < free_from {
            return Err("Overlapping replacement ranges".into());
        }
        // The engine reads the file as the editor opens it; a match not where
        // the search saw it means the two read the file differently.
        if doc.read(hit.offset, old.len()).map_err(|e| e.to_string())? != old {
            return Err("The file reads differently from how it was searched (its encoding). Open it and replace there.".into());
        }
        let new = hit.replacement.clone().unwrap_or_default().into_bytes();
        back.push((hit.offset, old.len() as u64, new.clone()));
        edits.push(change::Edit { at: (hit.offset as i64 + shift) as u64, new: new.clone(), old: old.to_vec() });
        shift += new.len() as i64 - old.len() as i64;
        free_from = hit.offset + old.len() as u64;
    }
    doc.replace_many(&back).map_err(|e| e.to_string())?;
    let absolute = std::path::absolute(path).map_err(|e| e.to_string())?;
    let mut record = record;
    if let Some(record) = record.as_deref_mut() {
        record.keep_file(&absolute, doc.len(), &edits).map_err(|e| format!("not saved, its edits could not be recorded: {e}"))?;
    }
    doc.save(path).map_err(|e| e.to_string())?;
    if let Some(record) = record {
        // Undo checks the text before it reverts, so a lost note does no harm.
        let _ = record.saved(&absolute);
    }
    Ok(())
}
