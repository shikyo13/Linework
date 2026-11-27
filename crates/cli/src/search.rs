//! `lw find` (and `lw PATTERN [PATH...]`): search files, folders and standard
//! input, and print what is found as the options say.
//!
//! Each named path is searched in the order given. A folder is walked on
//! several threads and its files' output is put in path order (or as
//! `--sort` says). A named file of up to 4 MiB, or any file the options need
//! whole, is read and searched in memory; a larger one goes through the
//! editor's parallel engine when the options allow it, and through the
//! streaming searcher otherwise; standard input, compressed files and
//! preprocessor output always stream.
use crate::{
    printer::{FileOut, Look},
    settings::{Binary, Folders, Mode, Settings, SortBy},
};
use linework_search::{
    input,
    matcher::{self, Query},
    plan::{NearLines, Plan},
    searcher,
    sweep::{self, FileRead, Order, Outcome},
    walk,
};
use linework_storage::{Document, LineEnds, SearchOptions};
use std::{
    io::{self, Read, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::Instant,
};

/// The first part of a file that decides whether it is binary.
const HEAD: usize = 64 * 1024;
/// Files up to this size are read whole; larger named files go through the engine or stream.
const WHOLE: u64 = 4 << 20;
/// How much of a stream is read at a time.
const BLOCK: usize = 1 << 20;

/// How a file is treated when it holds a zero byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Detect {
    /// Stop at the zero byte; a head that holds one makes the file skipped (walked files).
    Quit,
    /// Search on, print no lines past it, say "binary file matches" (named files, standard input, --binary).
    Convert,
    /// No detection (-a, --null-data).
    None,
    /// Binary files never match (grep's --binary-files=without-match).
    Skip,
}

/// Totals of a run, for --stats, the JSON summary and the exit code.
#[derive(Default)]
struct Totals {
    files_searched: u64,
    files_matched: u64,
    lines: u64,
    matches: u64,
    bytes_searched: u64,
    bytes_printed: u64,
    listed: u64,
    errors: bool,
    searching: f64,
    /// Bytes of ripgrep JSON messages, the end and summary messages apart: what ripgrep calls printed.
    messages: u64,
}

/// What one search run needs, shared by every file.
struct Run<'a> {
    s: &'a Settings,
    /// The patterns, file choice, conditions and line selection, as the editor has them too.
    plan: Plan,
    look: Look,
    input: input::Config,
    /// Stop everything at the first match (-q).
    stop_all: AtomicBool,
    /// Bytes of ripgrep JSON messages written by all files, on whichever thread.
    messages: std::sync::atomic::AtomicU64,
}

/// Where output goes: the writer, the --budget allowance, and whether a file has printed before.
struct Output<'w> {
    out: &'w mut dyn Write,
    budget: crate::report::Budget,
    any: bool,
    line_buffered: bool,
    printed: u64,
}

impl Output<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.printed += bytes.len() as u64;
        self.budget.write(&mut self.out, bytes)?;
        if self.line_buffered {
            self.out.flush()?;
        }
        Ok(())
    }

    /// One file's output, with what parts it from the file before.
    fn file(&mut self, look: &Look, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self.any {
            let mut between = Vec::new();
            look.between_files(&mut between);
            self.write(&between)?;
        }
        self.any = true;
        self.write(bytes)
    }
}

/// Whether standard input is a file or a pipe, so that with no path it is what is searched.
#[cfg(unix)]
fn stdin_readable() -> bool {
    // As ripgrep judges it: a file, a pipe or a socket.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let known = unsafe { libc::fstat(0, &mut st) } == 0;
    known && matches!(st.st_mode & libc::S_IFMT, libc::S_IFREG | libc::S_IFIFO | libc::S_IFSOCK)
}

/// Whether standard input is a file or a pipe, so that with no path it is what is searched.
#[cfg(windows)]
fn stdin_readable() -> bool {
    use windows_sys::Win32::{
        Storage::FileSystem::{GetFileType, FILE_TYPE_DISK, FILE_TYPE_PIPE},
        System::Console::{GetStdHandle, STD_INPUT_HANDLE},
    };
    let kind = unsafe { GetFileType(GetStdHandle(STD_INPUT_HANDLE)) };
    kind == FILE_TYPE_DISK || kind == FILE_TYPE_PIPE
}

/// Runs a search. Returns the exit code: 0 when something was found (or
/// listed), 1 when nothing was, 2 after an error unless -q found a match.
pub fn run(s: &Settings, out: &mut dyn Write) -> Result<i32, String> {
    let began = Instant::now();
    if s.type_list {
        for (name, globs) in s.types.list() {
            let mut globs = globs.to_vec();
            globs.sort();
            writeln!(out, "{name}: {}", globs.join(", ")).map_err(|e| e.to_string())?;
        }
        return Ok(0);
    }
    if s.pcre2_version {
        writeln!(out, "{}", matcher::pcre2_version()).map_err(|e| e.to_string())?;
        return Ok(0);
    }
    let (patterns, mut words) = s.patterns_and_paths()?;
    // With no path: standard input when something is piped or redirected, else this folder.
    let implicit = words.is_empty();
    if implicit {
        words.push(if stdin_readable() && !s.list_files { "-".into() } else { ".".into() });
    }
    let any_folder = words.iter().any(|w| w != "-" && Path::new(w).is_dir());
    let paths_shown = words.len() > 1 || any_folder;
    let console = io::stdout().is_terminal_console();
    let plan = Plan::new(s, patterns)?;
    let stdin_only = words.iter().all(|w| w == "-");
    let look = Look::new(s, paths_shown, console, stdin_only)?;
    if let Some(pre) = &s.pre {
        input::find_program(pre)?;
    }
    let run = Run { s, plan, look, input: s.input()?, stop_all: AtomicBool::new(false), messages: Default::default() };
    let mut output = Output {
        out,
        budget: crate::report::Budget::new(s.budget),
        any: false,
        line_buffered: s.line_buffered.unwrap_or(console),
        printed: 0,
    };
    let mut totals = Totals::default();
    if s.max_count == Some(0) {
        return Ok(1);
    }
    for word in &words {
        if run.stop_all.load(Ordering::Relaxed) {
            break;
        }
        let searching = Instant::now();
        if word == "-" {
            let name = s.label.clone().unwrap_or_else(|| "<stdin>".into());
            let mut source = run.input.stdin();
            run.stream(&name, None, &mut source.reader, 0, 0, run.detect(false), &mut output, &mut totals).map_err(|e| e.to_string())?;
        } else {
            run.path(word, implicit, &mut output, &mut totals).map_err(|e| e.to_string())?;
        }
        totals.searching += searching.elapsed().as_secs_f64();
    }
    // As ripgrep counts it: nothing is printed by -c, -l and -q.
    totals.bytes_printed = if s.mode == Mode::Lines && !s.quiet { output.printed } else { 0 };
    let mut tail = Vec::new();
    if run.look.ripgrep {
        totals.messages = run.messages.load(Ordering::Relaxed);
        tail.extend_from_slice(ripgrep_summary(&totals, began).as_bytes());
    } else if s.json {
        let summary = json_summary(&totals, &output.budget, began);
        tail.extend_from_slice(summary.as_bytes());
    } else if s.stats {
        tail.extend_from_slice(stats(&totals, began).as_bytes());
    }
    output.out.write_all(&tail).map_err(|e| e.to_string())?;
    output.out.flush().map_err(|e| e.to_string())?;
    if output.budget.cut && !s.json {
        crate::note(format!("lw: output cut at {} bytes (--budget)", output.budget.written));
    }
    let found = if s.list_files {
        totals.listed > 0
    } else if s.mode == Mode::WithoutMatch {
        totals.listed > 0
    } else {
        totals.lines > 0
    };
    Ok(if s.quiet && found {
        0
    } else if totals.errors {
        2
    } else {
        i32::from(!found)
    })
}

trait ConsoleCheck {
    fn is_terminal_console(&self) -> bool;
}

impl ConsoleCheck for io::Stdout {
    fn is_terminal_console(&self) -> bool {
        use std::io::IsTerminal;
        self.is_terminal()
    }
}

/// rg's --stats block, after an empty line.
fn stats(t: &Totals, began: Instant) -> String {
    format!(
        "\n{} matches\n{} matched lines\n{} files contained matches\n{} files searched\n{} bytes printed\n{} bytes searched\n{:.6} seconds spent searching\n{:.6} seconds total\n",
        t.matches, t.lines, t.files_matched, t.files_searched, t.bytes_printed, t.bytes_searched, t.searching, began.elapsed().as_secs_f64()
    )
}

/// The last message of ripgrep's JSON output.
fn ripgrep_summary(t: &Totals, began: Instant) -> String {
    use crate::printer::ripgrep_elapsed;
    format!(
        "{{\"data\":{{\"elapsed_total\":{},\"stats\":{{\"bytes_printed\":{},\"bytes_searched\":{},\"elapsed\":{},\"matched_lines\":{},\"matches\":{},\"searches\":{},\"searches_with_match\":{}}}}},\"type\":\"summary\"}}\n",
        ripgrep_elapsed(began.elapsed(), true),
        t.messages,
        t.bytes_searched,
        ripgrep_elapsed(std::time::Duration::from_secs_f64(t.searching), true),
        t.lines,
        t.matches,
        t.files_searched,
        t.files_matched
    )
}

fn json_summary(t: &Totals, budget: &crate::report::Budget, began: Instant) -> String {
    let value = serde_json::json!({"summary": {
        "files_searched": t.files_searched,
        "files_matched": t.files_matched,
        "lines": t.lines,
        "matches": t.matches,
        "bytes_searched": t.bytes_searched,
        "output_bytes": budget.written,
        "cut_short": budget.cut,
        "seconds": (began.elapsed().as_secs_f64() * 1000.0).round() / 1000.0,
    }});
    format!("{value}\n")
}

impl Run<'_> {
    /// How files of a given origin are treated when they hold a zero byte.
    fn detect(&self, walked: bool) -> Detect {
        match self.s.binary {
            Binary::Text => Detect::None,
            Binary::Skip => Detect::Skip,
            Binary::Convert => Detect::Convert,
            Binary::Auto if walked => Detect::Quit,
            Binary::Auto => Detect::Convert,
        }
    }

    fn error(&self, totals: &mut Totals, text: &str) {
        totals.errors = true;
        if self.s.messages {
            crate::note(format!("lw: {text}"));
        }
    }

    /// One path from the command line: a file, a folder, or a device.
    fn path(&self, word: &str, implicit: bool, output: &mut Output, totals: &mut Totals) -> io::Result<()> {
        let path = Path::new(word);
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(e) => {
                self.error(totals, &format!("{word}: {e}"));
                return Ok(());
            }
        };
        if meta.is_dir() {
            return match self.s.folders {
                Folders::Read => {
                    self.error(totals, &format!("{word}: Is a directory"));
                    Ok(())
                }
                Folders::Skip => Ok(()),
                Folders::Recurse => self.folder(word, implicit, output, totals),
            };
        }
        if !meta.is_file() {
            if self.s.skip_devices {
                return Ok(());
            }
        }
        if self.s.list_files {
            let mut line = self.look.shown_path(word);
            line.extend_from_slice(if self.look.null { b"\0" } else { self.look.end });
            totals.listed += 1;
            return output.write(&line);
        }
        self.named_file(word, meta.len(), output, totals)
    }

    /// A file named on the command line: no filter applies to it.
    fn named_file(&self, word: &str, size: u64, output: &mut Output, totals: &mut Totals) -> io::Result<()> {
        let path = Path::new(word);
        let absolute = std::path::absolute(path).ok().map(|p| p.to_string_lossy().into_owned());
        let detect = self.detect(false);
        let span = self.s.since.is_some() || self.s.until.is_some();
        // Compressed, preprocessed or encoded files are streams of their text.
        // With -z every named file goes through the input layer, which knows compression by name or by its first bytes.
        if !self.input.plain_for(word) || self.s.unpack {
            let source = match self.input.open(path, word) {
                Ok(source) => source,
                Err(e) => {
                    self.error(totals, &format!("{word}: {e}"));
                    return Ok(());
                }
            };
            return self.stream_source(word, absolute, source, detect, output, totals);
        }
        if span || (size > WHOLE && !self.plan.query.multiline() && !self.s.scope) {
            if self.engine_can(span) {
                return self.engine(word, absolute, output, totals);
            }
            let (start, lines) = if span { self.span_start(word, totals)? } else { (Some(0), 0) };
            let Some(start) = start else { return Ok(()) };
            let mut file = std::fs::File::open(path)?;
            io::Seek::seek(&mut file, io::SeekFrom::Start(start))?;
            let end = if span { self.span_end(word)? } else { None };
            let mut reader: Box<dyn Read + Send> = match end {
                Some(end) => Box::new(file.take(end - start)),
                None => Box::new(file),
            };
            return self.stream(word, absolute, &mut reader, start, lines, detect, output, totals);
        }
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.error(totals, &format!("{word}: {e}"));
                return Ok(());
            }
        };
        let text = input::decode_bytes(&bytes, self.input.encoding);
        let mut file = FileOut::new(&self.look, &self.plan.query, word, absolute, Some(size));
        let outcome = self.search_bytes(&mut file, &text, true, detect, word);
        self.add(totals, &outcome, text.len() as u64);
        output.file(&self.look, &file.out)
    }

    /// Whether lw's parallel engine can run this search: matching lines and
    /// their numbers, with or without a time span, and nothing it cannot do.
    fn engine_can(&self, span: bool) -> bool {
        let s = self.s;
        let (before, after) = s.context_lines();
        let plain_or_pattern = self.plan.query.plain_text().is_some() || self.plan.query.final_pattern().is_some();
        plain_or_pattern
            && !self.plan.query.is_pcre()
            && !self.plan.query.multiline()
            && !s.invert
            && before == 0
            && after == 0
            && !s.passthru
            && !s.stop_on_nonmatch
            && !s.matcher.null_data
            && (span || !matches!(self.detect(false), Detect::Skip))
    }

    /// Where a time span starts: its offset and the line ends before it; None when the span is empty.
    fn span_start(&self, word: &str, totals: &mut Totals) -> io::Result<(Option<u64>, u64)> {
        let doc = match Document::open(Path::new(word)) {
            Ok(doc) => doc,
            Err(e) => {
                self.error(totals, &format!("{word}: {e}"));
                return Ok((None, 0));
            }
        };
        let range = crate::time::span(&doc, self.s.since.as_deref(), self.s.until.as_deref()).map_err(io::Error::other)?;
        if range.is_empty() {
            return Ok((None, 0));
        }
        let lines = doc.count_ends(range.start, LineEnds::LineFeed, &AtomicBool::new(false))?;
        Ok((Some(range.start), lines))
    }

    fn span_end(&self, word: &str) -> io::Result<Option<u64>> {
        let doc = Document::open(Path::new(word))?;
        let range = crate::time::span(&doc, self.s.since.as_deref(), self.s.until.as_deref()).map_err(io::Error::other)?;
        Ok(Some(range.end))
    }

    /// A large named file through the editor's engine: matching lines found
    /// in parallel and numbered in the same pass.
    fn engine(&self, word: &str, absolute: Option<String>, output: &mut Output, totals: &mut Totals) -> io::Result<()> {
        linework_storage::set_line_indexing(false);
        let doc = match Document::open(Path::new(word)) {
            Ok(doc) => doc,
            Err(e) => {
                self.error(totals, &format!("{word}: {e}"));
                return Ok(());
            }
        };
        let cancel = AtomicBool::new(false);
        let scope = if self.s.since.is_some() || self.s.until.is_some() {
            let range = crate::time::span(&doc, self.s.since.as_deref(), self.s.until.as_deref()).map_err(io::Error::other)?;
            let before = doc.count_ends(range.start, LineEnds::LineFeed, &cancel)?;
            (range, before)
        } else {
            (0..doc.len(), 0)
        };
        let mut file = FileOut::new(&self.look, &self.plan.query, word, absolute.clone(), Some(doc.len()));
        // --and and --not look at the whole file first.
        // The engine takes a plain string with its case choice, or the final pattern, whose options are written into it.
        let options = |insensitive: bool, regex: bool| SearchOptions { case_sensitive: !insensitive, regex, whole_word: false, backwards: false, wrap: false };
        let engine_form = |q: &Query| -> (String, SearchOptions) {
            let (text, regex, case_sensitive) = q.engine_form().unwrap_or_default();
            (text, options(!case_sensitive, regex))
        };
        for (q, wanted) in self.plan.and.iter().map(|q| (q, true)).chain(self.plan.not.iter().map(|q| (q, false))) {
            let (text, opts) = engine_form(q);
            if doc.find(&text, 0, opts)?.is_some() != wanted {
                totals.files_searched += 1;
                totals.bytes_searched += scope.0.end - scope.0.start;
                return Ok(());
            }
        }
        // --near: the line numbers of each nearby pattern's matches, over the whole file.
        let mut near_lines = Vec::new();
        for (q, reach, side) in &self.plan.near {
            let (text, opts) = engine_form(q);
            let mut numbers = Vec::new();
            doc.find_lines(&text, opts, 1, LineEnds::LineFeed, &cancel, &mut |line| {
                numbers.push(line.line + 1);
                Ok(false)
            })?;
            near_lines.push((numbers, *reach, *side));
        }
        let near_lines = NearLines::new(near_lines);
        let (text, opts) = engine_form(&self.plan.query);
        let detect = self.detect(false);
        let head = doc.read(0, (HEAD as u64).min(doc.len()) as usize)?;
        if detect == Detect::Convert && memchr::memchr(0, &head).is_some() {
            // A binary head: the streaming searcher counts and reports it properly.
            let mut reader = std::fs::File::open(word)?;
            return self.stream(word, absolute, &mut reader, 0, 0, detect, output, totals);
        }
        let mut binary_at = None;
        let mut selected_after = 0u64;
        let widest = if self.s.whole_lines { 256 << 20 } else { 1 << 20 };
        let length = doc.len();
        doc.find_lines_in(&text, opts, scope.0.clone(), scope.1, widest, LineEnds::LineFeed, &cancel, &mut |found| {
            let number = found.line + 1;
            if !near_lines.keeps(number) {
                return Ok(false);
            }
            // Named files are checked as ripgrep checks memory-mapped ones: each line about to be printed.
            if detect == Detect::Convert && binary_at.is_none() {
                if let Some(at) = memchr::memchr(0, &found.text) {
                    binary_at = Some(found.text_start + at as u64);
                    file.silenced = true;
                }
            }
            if binary_at.is_some() {
                selected_after += 1;
            }
            // Only the file's last line can lack a line end.
            let ended = found.cut || found.text_start + (found.text.len() as u64) < length;
            let go_on = file.part_of_line(number, found.start, found.text_start, &found.text, found.cut, ended);
            let limit = self.s.max_count.is_some_and(|most| file.found.lines >= most);
            Ok(!go_on || limit || file.satisfied())
        })?;
        if let (Some(at), true) = (binary_at, selected_after > 0) {
            file.binary_matches(at);
        }
        file.finish(None, word);
        totals.files_searched += 1;
        totals.bytes_searched += scope.0.end - scope.0.start;
        self.tally(totals, &file);
        if self.s.quiet && !self.s.stats && file.found.lines > 0 {
            self.stop_all.store(true, Ordering::Relaxed);
        }
        output.file(&self.look, &file.out)
    }

    fn tally(&self, totals: &mut Totals, file: &FileOut) {
        totals.lines += file.found.lines;
        totals.matches += file.found.matches;
        totals.files_matched += u64::from(file.found.lines > 0);
        match self.s.mode {
            Mode::WithMatches if file.found.lines > 0 => totals.listed += 1,
            Mode::WithoutMatch if file.found.lines == 0 => totals.listed += 1,
            _ => {}
        }
    }

    fn add(&self, totals: &mut Totals, outcome: &FileOutcome, bytes: u64) {
        totals.files_searched += 1;
        totals.bytes_searched += bytes;
        totals.lines += outcome.lines;
        totals.matches += outcome.matches;
        totals.files_matched += u64::from(outcome.lines > 0);
        totals.listed += u64::from(outcome.listed);
        if self.s.quiet && !self.s.stats && outcome.lines > 0 {
            self.stop_all.store(true, Ordering::Relaxed);
        }
    }

    /// A folder: every file the walk keeps, searched on several threads, put in order.
    fn folder(&self, word: &str, implicit: bool, output: &mut Output, totals: &mut Totals) -> io::Result<()> {
        let s = self.s;
        // The prefix printed before each relative path: as typed, without a trailing slash; none for an implied ".".
        let typed = word.trim_end_matches(['/', '\\']);
        let prefix = if implicit { String::new() } else { format!("{}/", if typed.is_empty() { word } else { typed }) };
        let errors = Mutex::new(Vec::<String>::new());
        let report = |path: &str, why: &str| crate::note(format!("lw: DEBUG: skipping {prefix}{path}: {why}"));
        let failure = |path: &str, why: &str| errors.lock().unwrap_or_else(|e| e.into_inner()).push(format!("{prefix}{path}: {why}"));
        let options = walk::Options {
            report: s.debug.then_some(&report as walk::Report),
            failure: Some(&failure),
            ..self.plan.walk_options(s)
        };
        let root = Path::new(word);
        let order = match s.sort {
            SortBy::Default => Order::Path,
            SortBy::Path => Order::Bytes,
            SortBy::Modified => Order::Modified,
            SortBy::Accessed => Order::Accessed,
            SortBy::Created => Order::Created,
        };
        if s.list_files {
            return self.list(root, &prefix, options, order, output, totals, &errors);
        }
        let detect = self.detect(true);
        let absolute_root = std::path::absolute(root).ok();
        let visit = |read: &FileRead, out: &mut Vec<u8>| -> Outcome {
            if self.stop_all.load(Ordering::Relaxed) {
                return Outcome::default();
            }
            let shown = format!("{prefix}{}", read.found.path);
            let absolute = absolute_root.as_ref().map(|r| r.join(read.found.path).to_string_lossy().into_owned());
            let mut file = FileOut::new(&self.look, &self.plan.query, &shown, absolute, Some(read.found.size));
            // With -z, a file whose name does not say it is compressed may still say so in its first bytes.
            let packed = if self.input.plain_for(read.found.path) { self.input.packed_bytes(read.bytes) } else { None };
            let streamed = |source: io::Result<input::Source<'_>>, file: &mut FileOut, out: &mut Vec<u8>| match source {
                Ok(source) => {
                    let mut failed = None;
                    let outcome = self.stream_source_into(file, source, detect, out, &mut failed).unwrap_or_default();
                    if let Some(e) = failed {
                        errors.lock().unwrap_or_else(|e| e.into_inner()).push(format!("{shown}: {e}"));
                    }
                    outcome
                }
                Err(e) => {
                    errors.lock().unwrap_or_else(|e| e.into_inner()).push(format!("{shown}: {e}"));
                    FileOutcome::default()
                }
            };
            let outcome = if !self.input.plain_for(read.found.path) {
                // Compressed or preprocessed by name: the file's text is a stream.
                streamed(self.input.open(&root.join(read.found.path), read.found.path), &mut file, out)
            } else if let Some(packed) = packed {
                streamed(self.input.from_bytes(read.bytes.to_vec(), packed), &mut file, out)
            } else {
                let text = input::decode_bytes(read.bytes, self.input.encoding);
                let outcome = self.search_bytes(&mut file, &text, read.complete, detect, read.found.path);
                out.extend_from_slice(&file.out);
                outcome
            };
            if s.quiet && !s.stats && outcome.lines > 0 {
                self.stop_all.store(true, Ordering::Relaxed);
            }
            Outcome { lines: outcome.lines, matches: outcome.matches }
        };
        let only_changed = if s.changed {
            let profile = crate::profile();
            let name = s.mark.as_deref().unwrap_or("last");
            let before = crate::marks::load(&profile, root, name)?
                .ok_or_else(|| io::Error::other(format!("no snapshot named {name} for this folder yet; lw changes takes one")))?;
            let now = crate::marks::list(root, options)?;
            let changes = crate::marks::compare(&before, &now);
            Some(changes.added.into_iter().chain(changes.changed).map(|path| (now[&path].0, path)).map(|(size, path)| (path, size)).collect::<Vec<_>>())
        } else {
            None
        };
        // Compressed files hold zero bytes; with -z they are read whole and their marks checked.
        let whole = detect == Detect::Convert || detect == Detect::None || s.unpack;
        let gathered = match &only_changed {
            Some(list) => sweep::files_listed(root, list, options.threads, whole, &visit)?,
            None => sweep::files(root, options, whole, &visit)?,
        };
        for g in &gathered {
            totals.files_searched += g.files;
            totals.bytes_searched += g.bytes;
            totals.lines += g.matched_lines;
            totals.matches += g.matches;
            totals.files_matched += g.files_matched;
        }
        if matches!(s.mode, Mode::WithMatches) {
            totals.listed += gathered.iter().map(|g| g.files_matched).sum::<u64>();
        } else if matches!(s.mode, Mode::WithoutMatch) {
            totals.listed += gathered.iter().map(|g| g.parts.len() as u64).sum::<u64>();
        }
        for (_, part) in sweep::ordered(&gathered, order, s.sort_reverse) {
            output.file(&self.look, part)?;
        }
        for e in errors.into_inner().unwrap_or_default() {
            self.error(totals, &e);
        }
        Ok(())
    }

    /// --files: the files a search would read, in order.
    #[allow(clippy::too_many_arguments)]
    fn list(
        &self, root: &Path, prefix: &str, options: walk::Options, order: Order, output: &mut Output, totals: &mut Totals, errors: &Mutex<Vec<String>>,
    ) -> io::Result<()> {
        let look = &self.look;
        // On macOS a walk for names alone is the quicker one: sizes and times are asked for only when the order needs them.
        #[cfg(target_os = "macos")]
        let options = walk::Options { details: matches!(order, Order::Modified | Order::Accessed | Order::Created), ..options };
        // Listing reads no file: the walk alone. Each thread gathers its paths, each one a part of the output.
        let gathered = walk::walk(root, options, sweep::Gathered::default, |gathered, found| {
            let from = gathered.out.len();
            gathered.out.extend_from_slice(&look.shown_path(&format!("{prefix}{}", found.path)));
            gathered.out.extend_from_slice(if look.null { b"\0" } else { look.end });
            gathered.parts.push((found.path.to_owned(), from..gathered.out.len()));
            gathered.times.push((found.modified, found.accessed, found.created));
        })?;
        let parts = sweep::ordered(&gathered, order, self.s.sort_reverse);
        totals.listed += parts.len() as u64;
        if self.s.quiet {
            return Ok(());
        }
        for (_, part) in parts {
            output.write(part)?;
        }
        for e in std::mem::take(&mut *errors.lock().unwrap_or_else(|e| e.into_inner())) {
            self.error(totals, &e);
        }
        Ok(())
    }

    /// Searches a file's whole text, with the binary handling of `detect`.
    fn search_bytes(&self, file: &mut FileOut, text: &[u8], complete: bool, detect: Detect, source_path: &str) -> FileOutcome {
        // --and and --not decide over the whole file.
        if !self.plan.file_passes(text) {
            return self.finish(file, None, source_path);
        }
        let near = self.plan.near_lines(text);
        let keep = |number: u64| near.keeps(number);
        let nul = if detect == Detect::None { None } else { memchr::memchr(0, text) };
        let searcher = || self.plan.searcher((!near.is_empty()).then_some(&keep as searcher::Keep));
        match (nul, detect) {
            (None, _) | (_, Detect::None) => {
                searcher().search(text, file);
            }
            (Some(_), Detect::Skip) => return FileOutcome::default(),
            (Some(at), Detect::Quit) => {
                if at < HEAD || !complete {
                    if self.s.debug {
                        crate::note(format!("lw: DEBUG: skipping {}: found binary data at offset {at}", String::from_utf8_lossy(file.path())));
                    }
                    return FileOutcome::default();
                }
                let cut = memchr::memrchr(self.plan.query.line_end(), &text[..at]).map_or(0, |e| e + 1);
                searcher().piece(text, 0, 0, cut, true, file);
                // Counts of a binary file are dropped, and a file without a match is not listed, as in ripgrep.
                if matches!(self.s.mode, Mode::Count | Mode::CountMatches | Mode::WithoutMatch) {
                    return FileOutcome::default();
                }
                if file.found.lines > 0 && !file.satisfied() {
                    file.binary_stopped(at as u64);
                }
            }
            (Some(at), Detect::Convert) => {
                let mut s = searcher();
                if at < HEAD {
                    file.silenced = true;
                    s.search(text, file);
                    if file.found.lines > 0 {
                        file.binary_matches(at as u64);
                    }
                } else {
                    let cut = memchr::memrchr(self.plan.query.line_end(), &text[..at]).map_or(0, |e| e + 1);
                    s.piece(text, 0, 0, cut, false, file);
                    let before = file.found.lines;
                    file.silenced = true;
                    s.piece(text, 0, cut, text.len(), true, file);
                    if file.found.lines > before {
                        file.binary_matches(at as u64);
                    }
                }
            }
        }
        if let Some(failure) = self.plan.query.failure() {
            crate::note(format!("lw: {}: PCRE2: {failure}", String::from_utf8_lossy(file.path())));
        }
        self.finish(file, Some(text), source_path)
    }

    fn finish(&self, file: &mut FileOut, whole: Option<&[u8]>, source_path: &str) -> FileOutcome {
        file.finish(whole, source_path);
        let listed = match self.s.mode {
            Mode::WithMatches => file.found.lines > 0,
            Mode::WithoutMatch => file.found.lines == 0,
            _ => false,
        };
        self.messages.fetch_add(file.ripgrep_printed(), Ordering::Relaxed);
        FileOutcome { lines: file.found.lines, matches: file.found.matches, listed, bytes: 0 }
    }

    /// Searches a stream (standard input, part of a file), printing as it goes.
    #[allow(clippy::too_many_arguments)]
    fn stream(
        &self, name: &str, absolute: Option<String>, reader: &mut dyn Read, start: u64, lines: u64, detect: Detect, output: &mut Output,
        totals: &mut Totals,
    ) -> io::Result<()> {
        let mut file = FileOut::new(&self.look, &self.plan.query, name, absolute, None);
        let mut failed = None;
        let mut live = Live { output, started: false };
        let outcome = self.stream_pieces(&mut file, reader, start, lines, detect, &mut live, &mut failed)?;
        if let Some(e) = failed {
            self.error(totals, &format!("{name}: {e}"));
        }
        self.add(totals, &outcome, outcome.bytes);
        Ok(())
    }

    /// A compressed or preprocessed named file, printing as it goes; a preprocessor that fails is an error.
    fn stream_source(&self, name: &str, absolute: Option<String>, source: input::Source<'_>, detect: Detect, output: &mut Output, totals: &mut Totals) -> io::Result<()> {
        let mut file = FileOut::new(&self.look, &self.plan.query, name, absolute, None);
        let mut failed = None;
        let mut live = Live { output, started: false };
        let outcome = self.stream_source_into(&mut file, source, detect, &mut live, &mut failed)?;
        if let Some(e) = failed {
            self.error(totals, &format!("{name}: {e}"));
        }
        self.add(totals, &outcome, outcome.bytes);
        Ok(())
    }

    /// A source searched as a stream, then its preprocessor waited for and checked.
    fn stream_source_into(
        &self, file: &mut FileOut, mut source: input::Source<'_>, detect: Detect, emit: &mut dyn Emit, failed: &mut Option<String>,
    ) -> io::Result<FileOutcome> {
        let outcome = self.stream_pieces(file, &mut source.reader, 0, 0, detect, emit, failed)?;
        if let Err(e) = source.finish() {
            failed.get_or_insert(e.to_string());
        }
        Ok(outcome)
    }

    /// The streaming search: the text read a block at a time, searched up to
    /// its last complete line, with the lines context may need kept for the
    /// next block, and output handed on after each block.
    #[allow(clippy::too_many_arguments)]
    fn stream_pieces(
        &self, file: &mut FileOut, reader: &mut dyn Read, start: u64, lines: u64, detect: Detect, emit: &mut dyn Emit, failed: &mut Option<String>,
    ) -> io::Result<FileOutcome> {
        // -U, --scope, --near, --and and --not need the whole text at once.
        if self.plan.needs_whole_text() || self.s.scope {
            let mut text = Vec::new();
            if let Err(e) = reader.read_to_end(&mut text) {
                *failed = Some(e.to_string());
            }
            let path = String::from_utf8_lossy(file.path()).into_owned();
            let outcome = self.search_bytes(file, &text, true, detect, &path);
            emit.emit(&self.look, &file.out)?;
            return Ok(FileOutcome { bytes: text.len() as u64, ..outcome });
        }
        let mut searcher = self.plan.searcher(None).starting_at(start, lines);
        let mut buf: Vec<u8> = Vec::with_capacity(BLOCK * 2);
        let mut base = start;
        let mut searched = 0usize;
        let mut ended = false;
        let mut binary_at: Option<u64> = None;
        let mut selected_at_nul = 0u64;
        let line_end = self.plan.query.line_end();
        // Set when the text read so far holds no line end past what was searched: read on, past a block.
        let mut grow = false;
        while !ended && !searcher.done() && !file.satisfied() {
            // One read per step, of up to a block: a file fills it, a pipe returns what its writer has sent so
            // far, and that is searched at once, so a slow writer's lines are found as they arrive (as grep and
            // ripgrep do) instead of waiting for a whole block or the end of the stream. A line longer than what
            // is held is read on in the next step (`grow`).
            let target = if grow { buf.len() + BLOCK } else { searched + BLOCK };
            loop {
                let have = buf.len();
                buf.resize(target.max(have + 1), 0);
                match reader.read(&mut buf[have..]) {
                    Ok(0) => {
                        buf.truncate(have);
                        ended = true;
                    }
                    Ok(n) => buf.truncate(have + n),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                        buf.truncate(have);
                        continue;
                    }
                    Err(e) => {
                        buf.truncate(have);
                        *failed = Some(e.to_string());
                        ended = true;
                    }
                }
                break;
            }
            // A zero byte in the first 64 KiB makes the whole stream binary; a later one stops lines printing at its line.
            if binary_at.is_none() && detect != Detect::None {
                if let Some(at) = memchr::memchr(0, &buf[searched..]) {
                    let absolute = base + (searched + at) as u64;
                    binary_at = Some(absolute);
                    let early = absolute - start < HEAD as u64;
                    if detect == Detect::Skip || (detect == Detect::Quit && early) {
                        return Ok(FileOutcome { bytes: base + buf.len() as u64 - start, ..FileOutcome::default() });
                    }
                    let cut = memchr::memrchr(line_end, &buf[..searched + at]).map_or(0, |e| e + 1).max(searched);
                    if !early && cut > searched {
                        searcher.piece(&buf, base, searched, cut, detect == Detect::Quit, file);
                        searched = cut;
                    }
                    selected_at_nul = file.found.lines;
                    if detect == Detect::Quit {
                        if file.found.lines > 0 && !file.satisfied() {
                            file.binary_stopped(absolute);
                        }
                        emit.emit(&self.look, &file.out)?;
                        file.out.clear();
                        break;
                    }
                    file.silenced = true;
                }
            }
            let end = if ended { buf.len() } else { memchr::memrchr(line_end, &buf[searched..]).map_or(searched, |e| searched + e + 1) };
            grow = !ended && end == searched;
            if end > searched || ended {
                searcher.piece(&buf, base, searched, end, ended, file);
            }
            emit.emit(&self.look, &file.out)?;
            file.out.clear();
            let keep = searcher.keep_from(&buf, end);
            buf.drain(..keep);
            base += keep as u64;
            searched = end - keep;
        }
        let bytes = base + buf.len() as u64 - start;
        if let (Some(at), Detect::Convert) = (binary_at, detect) {
            if file.found.lines > selected_at_nul {
                file.binary_matches(at);
            }
        }
        if let Some(failure) = self.plan.query.failure() {
            *failed = Some(format!("PCRE2: {failure}"));
        }
        let path = String::from_utf8_lossy(file.path()).into_owned();
        let outcome = self.finish(file, None, &path);
        emit.emit(&self.look, &file.out)?;
        Ok(FileOutcome { bytes, ..outcome })
    }
}

/// Where a stream's output goes as it is found.
trait Emit {
    fn emit(&mut self, look: &Look, bytes: &[u8]) -> io::Result<()>;
}

impl Emit for Vec<u8> {
    fn emit(&mut self, _look: &Look, bytes: &[u8]) -> io::Result<()> {
        self.extend_from_slice(bytes);
        Ok(())
    }
}

/// Straight to the run's output, parted from the file before at its first bytes.
struct Live<'o, 'w> {
    output: &'o mut Output<'w>,
    started: bool,
}

impl Emit for Live<'_, '_> {
    fn emit(&mut self, look: &Look, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        if !self.started {
            self.started = true;
            return self.output.file(look, bytes);
        }
        self.output.write(bytes)
    }
}

/// What one file's search gave.
#[derive(Clone, Copy, Debug, Default)]
struct FileOutcome {
    lines: u64,
    matches: u64,
    listed: bool,
    bytes: u64,
}
