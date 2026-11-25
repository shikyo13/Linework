//! lw: Linework's file engine from the command line, for people, scripts and
//! AI agents. The commands and their help are in `help.rs`; `lw help` shows them.
use linework_storage::{Document, SearchOptions};
use std::{
    env,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Duration,
};

/// Matches kept in memory for one command: 16 bytes each.
const MOST: usize = 50_000_000;
/// Bytes of one line read from a single file before the rest is left: enough
/// to hold the match a long line is shown around, and with --whole-lines the
/// most a line may be.
const WIDEST: usize = 1 << 20;
const WIDEST_WHOLE: usize = 256 << 20;

/// Messages for whoever runs lw (an output cut short, a file that could not be
/// changed): on the error stream, or while `lw serve` runs a tool, kept to go
/// back with the tool's result, since an agent sees nothing else.
static NOTES: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);

macro_rules! note {
    ($($words:tt)*) => { crate::note(format!($($words)*)) };
}

fn note(text: String) {
    match NOTES.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        Some(kept) => kept.push(text),
        None => eprintln!("{text}"),
    }
}

/// From now on keep messages instead of printing them.
fn keep_notes() {
    *NOTES.lock().unwrap_or_else(|e| e.into_inner()) = Some(Vec::new());
}

/// The messages kept since `keep_notes`; printing resumes.
fn kept_notes() -> Vec<String> {
    NOTES.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default()
}

struct Args {
    ignore_case: bool,
    fixed: bool,
    word: bool,
    dry_run: bool,
    hidden: bool,
    no_ignore: bool,
    account: bool,
    samples: Option<String>,
    most: usize,
    threads: Option<usize>,
    json: bool,
    heading: bool,
    scope: bool,
    stats: bool,
    budget: Option<usize>,
    list: bool,
    since: Option<String>,
    until: Option<String>,
    changed: bool,
    mark: Option<String>,
    near: Vec<(String, u64)>,
    and: Vec<String>,
    not: Vec<String>,
    /// -g, -t and -T: which files of a folder are searched.
    globs: Vec<String>,
    types: Vec<String>,
    not_types: Vec<String>,
    /// -l: the names of the files with matching lines, not the lines.
    names_only: bool,
    whole_lines: bool,
    rest: Vec<String>,
}

impl Args {
    /// A pattern as the main pattern is read: plain text unless it holds a
    /// special character, with the same case and whole-word choices.
    fn query(&self, pattern: &str) -> Result<linework_search::sweep::Query, String> {
        let special = pattern.chars().any(|c| r"\^$.|?*+()[]{}".contains(c));
        linework_search::sweep::Query::new(pattern, self.fixed || !special, self.ignore_case, self.word)
    }
    /// The --near, --and and --not conditions.
    fn filters(&self) -> Result<report::Filters, String> {
        Ok(report::Filters {
            near: self.near.iter().map(|(p, n)| Ok((self.query(p)?, *n))).collect::<Result<_, String>>()?,
            and: self.and.iter().map(|p| self.query(p)).collect::<Result<_, _>>()?,
            not: self.not.iter().map(|p| self.query(p)).collect::<Result<_, _>>()?,
        })
    }
    fn widest(&self) -> usize {
        if self.whole_lines { WIDEST_WHOLE } else { WIDEST }
    }
    /// --scope alone groups lines under their file and definition for people.
    fn style(&self) -> report::Style {
        report::Style { json: self.json, heading: self.heading || (self.scope && !self.json), scope: self.scope }
    }
    /// Ends the output: the totals as a last JSON object, and with --stats as a line on stderr.
    fn finish(&self, out: &mut impl Write, totals: &report::Totals, budget: &report::Budget) -> io::Result<()> {
        if self.json {
            totals.json(out, budget)?;
        }
        out.flush()?;
        if self.stats {
            note!("{}", totals.text(budget));
        } else if budget.cut && !self.json {
            note!("lw: output cut at {} bytes (--budget)", budget.written);
        }
        Ok(())
    }
}

fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args { ignore_case: false, fixed: false, word: false, dry_run: false, hidden: false, no_ignore: false, account: false, samples: None, most: usize::MAX, threads: None, json: false, heading: false, scope: false, stats: false, budget: None, list: false, since: None, until: None, changed: false, mark: None, near: Vec::new(), and: Vec::new(), not: Vec::new(), globs: Vec::new(), types: Vec::new(), not_types: Vec::new(), names_only: false, whole_lines: false, rest: Vec::new() };
    let mut args = args.peekable();
    let mut plain = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            _ if plain => a.rest.push(arg),
            "--" => plain = true,
            "-i" => a.ignore_case = true,
            "-F" => a.fixed = true,
            "-w" => a.word = true,
            "--dry-run" => a.dry_run = true,
            "--hidden" => a.hidden = true,
            "--no-ignore" => a.no_ignore = true,
            "--account" => a.account = true,
            "--json" => a.json = true,
            "--heading" => a.heading = true,
            "--scope" => a.scope = true,
            "--stats" => a.stats = true,
            "--list" => a.list = true,
            "--changed" => a.changed = true,
            "--mark" => a.mark = Some(args.next().ok_or("--mark needs a name")?),
            "--since" => a.since = Some(args.next().ok_or("--since needs a time")?),
            "--until" => a.until = Some(args.next().ok_or("--until needs a time")?),
            "--budget" => {
                let n = args.next().ok_or("--budget needs a number of bytes")?;
                a.budget = Some(n.parse().map_err(|_| format!("--budget needs a number of bytes, not {n}"))?);
            }
            "--near" => {
                let pattern = args.next().ok_or("--near needs a pattern and a number of lines")?;
                let n = args.next().ok_or("--near needs a number of lines after its pattern")?;
                a.near.push((pattern, n.parse().map_err(|_| format!("--near needs a number of lines, not {n}"))?));
            }
            "-g" | "--glob" => a.globs.push(args.next().ok_or("-g needs a glob, such as *.rs or !*.min.js")?),
            "-t" | "--type" => a.types.push(args.next().ok_or("-t needs a file type, such as rust or ts")?),
            "-T" | "--type-not" => a.not_types.push(args.next().ok_or("-T needs a file type, such as md")?),
            "-l" | "--files-with-matches" => a.names_only = true,
            "--whole-lines" => a.whole_lines = true,
            "--and" => a.and.push(args.next().ok_or("--and needs a pattern")?),
            "--not" => a.not.push(args.next().ok_or("--not needs a pattern")?),
            "--samples" => a.samples = Some(args.next().ok_or("--samples needs a file to write to")?),
            "-j" => {
                let n = args.next().ok_or("-j needs a number of threads")?;
                a.threads = Some(n.parse().map_err(|_| format!("-j needs a number, not {n}"))?);
            }
            "-m" => {
                let n = args.next().ok_or("-m needs a number")?;
                a.most = n.parse().map_err(|_| format!("-m needs a number, not {n}"))?;
            }
            _ => a.rest.push(arg),
        }
    }
    Ok(a)
}

fn options(a: &Args, pattern: &str) -> SearchOptions {
    let special = pattern.chars().any(|c| r"\^$.|?*+()[]{}".contains(c));
    SearchOptions { case_sensitive: !a.ignore_case, regex: special && !a.fixed, whole_word: a.word, backwards: false, wrap: false }
}

/// The same profile folder as the editor, so a line count made by either is
/// there for the other and a save cut short by a crash is settled by either.
fn profile() -> PathBuf {
    env::var_os("LINEWORK_PROFILE").map(PathBuf::from).unwrap_or_else(|| {
        env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(env::temp_dir).join("Linework")
    })
}

fn open(path: &str) -> io::Result<Document> {
    Document::open(Path::new(path))
}

/// The part of a file a search covers: all of it, or with --since and --until
/// the span of a time-ordered log; and the number of line breaks before it.
fn span(doc: &Document, a: &Args, cancel: &AtomicBool) -> Result<(std::ops::Range<u64>, u64), String> {
    if a.since.is_none() && a.until.is_none() {
        return Ok((0..doc.len(), 0));
    }
    let range = time::span(doc, a.since.as_deref(), a.until.as_deref())?;
    let before = doc.count_breaks(range.start, cancel).map_err(|e| e.to_string())?;
    Ok((range, before))
}

/// The lines of one file that match and pass the conditions, at most `a.most`,
/// in `scope` (a span and the line breaks before it). Each --and, --not and
/// --near pattern is searched in the whole file.
fn file_lines(
    doc: &Document, a: &Args, pattern: &str, scope: &(std::ops::Range<u64>, u64), cancel: &AtomicBool,
) -> io::Result<Vec<linework_storage::FoundLine>> {
    let mut found = Vec::new();
    if a.most == 0 {
        return Ok(found);
    }
    let conditions = !(a.near.is_empty() && a.and.is_empty() && a.not.is_empty());
    let holds = |p: &str| doc.find(p, 0, options(a, p)).map(|hit| hit.is_some());
    for p in &a.and {
        if !holds(p)? {
            return Ok(found);
        }
    }
    for p in &a.not {
        if holds(p)? {
            return Ok(found);
        }
    }
    doc.find_lines_in(pattern, options(a, pattern), scope.0.clone(), scope.1, a.widest(), cancel, &mut |line| {
        found.push(line.clone());
        Ok(!conditions && found.len() == a.most)
    })?;
    for (p, reach) in &a.near {
        let mut numbers = Vec::new();
        doc.find_lines(p, options(a, p), 1, cancel, &mut |line| {
            numbers.push(line.line + 1);
            Ok(false)
        })?;
        found.retain(|f| report::close(&numbers, f.line + 1, *reach));
    }
    found.truncate(a.most);
    Ok(found)
}

fn counted(doc: &Document) -> io::Result<()> {
    while !doc.indexing_complete() {
        if let Some(error) = doc.index_error() {
            return Err(io::Error::other(error));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

fn run() -> Result<i32, String> {
    let argv: Vec<String> = env::args().skip(1).collect();
    if argv.first().is_some_and(|c| c == "serve") {
        return serve::run().map(|()| 0).map_err(|e| e.to_string());
    }
    let out = io::stdout();
    let mut out = io::BufWriter::with_capacity(1 << 16, out.lock());
    execute(argv, &mut out)
}

/// Runs one lw command, given as its words after `lw`, writing its output to `out`.
fn execute(argv: Vec<String>, mut out: &mut dyn Write) -> Result<i32, String> {
    let began = std::time::Instant::now();
    // Help: lw, lw help [NAME | --markdown], lw COMMAND --help.
    let first = argv.first().map(String::as_str).unwrap_or("help");
    let asks = argv.iter().take_while(|w| *w != "--").any(|w| w == "--help" || w == "-h");
    if matches!(first, "help" | "--help" | "-h") || asks {
        let name = if asks && help::find(first).is_some() { Some(first) } else { argv.get(1).map(String::as_str) };
        let text = match name {
            Some("--markdown") => help::markdown(),
            Some(name) => help::page(help::find(name).ok_or(format!("there is no help for {name}; lw help lists the commands and topics"))?),
            None => help::overview(),
        };
        out.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
        return Ok(0);
    }
    let mut args = argv.clone().into_iter();
    let command = args.next().unwrap_or_default();
    let a = parse(args)?;
    let io = |e: io::Error| e.to_string();
    let pick = linework_search::pick::Pick::new(&a.globs, &a.types, &a.not_types)?;
    // Set for every command, since a server runs many in one process.
    linework_search::sweep::set_clipping(!a.whole_lines);
    let profile = profile();
    linework_storage::set_index_cache(Some(&profile.join("line-index")));
    // A command that only searches turns this off for itself; a server runs
    // many commands in one process, so each starts from the default.
    linework_storage::set_line_indexing(true);
    let cancel = AtomicBool::new(false);
    let code = match (command.as_str(), a.rest.as_slice()) {
        // A folder: every file under it that the ignore rules leave in.
        ("count" | "find", [_, _]) if (a.since.is_some() || a.until.is_some()) && Path::new(&a.rest[1]).is_dir() => {
            return Err("--since and --until search one time-ordered log, not a folder".into());
        }
        ("count" | "find", [pattern, folder]) if Path::new(folder).is_dir() => {
            use linework_search::sweep::{self, Query, Report};
            // A few files at a time, so a large folder is shared between threads.
            let mut options = linework_search::walk::Options { hidden: a.hidden, ignore: !a.no_ignore, pick: pick.as_ref(), batch: 8, ..Default::default() };
            if let Some(threads) = a.threads {
                options.threads = threads;
            }
            let query: Query = a.query(pattern)?;
            let filters = a.filters()?;
            let report = if command == "count" { Report::Counts } else { Report::Lines };
            let shown = folder.trim_end_matches(['/', '\\']);
            if a.account || a.samples.is_some() {
                linework_search::account::switch_on();
            }
            let sampler = a.samples.as_ref().map(|_| sampler::Sampler::start());
            let style = a.style();
            let searching = std::time::Instant::now();
            let root = Path::new(folder);
            // With --changed, only the files added or changed since the folder's
            // snapshot are read; the snapshot itself is left as it is.
            let only = if a.changed {
                let name = a.mark.as_deref().unwrap_or("last");
                let before = marks::load(&profile, root, name).map_err(io)?
                    .ok_or(format!("no snapshot named {name} for this folder yet; lw changes takes one"))?;
                let now = marks::list(root, options).map_err(io)?;
                let changes = marks::compare(&before, &now);
                Some(changes.added.into_iter().chain(changes.changed).map(|path| { let size = now[&path].0; (path, size) }).collect::<Vec<_>>())
            } else {
                None
            };
            let gathered = if !filters.is_empty() || style.json || (command == "find" && style.heading) || only.is_some() || a.most != usize::MAX || a.names_only {
                // Every file with a match passes the conditions first; then its kept lines are shown.
                let format = |path: &str, bytes: &[u8], hits: &[sweep::LineHit], out: &mut Vec<u8>| -> u64 {
                    if !filters.file_passes(bytes) {
                        return 0;
                    }
                    let mut kept = if filters.near.is_empty() { hits.to_vec() } else { filters.near(bytes, hits) };
                    // -m N: at most N lines from each file, as in ripgrep.
                    kept.truncate(a.most);
                    if kept.is_empty() {
                        return 0;
                    }
                    let name = format!("{shown}/{path}");
                    match (command.as_str(), style.json) {
                        // -l: each file with matching lines, once.
                        (_, true) if a.names_only => {
                            serde_json::to_writer(&mut *out, &serde_json::json!({"path": name})).expect("writing to memory");
                            out.push(b'\n');
                        }
                        _ if a.names_only => {
                            out.extend_from_slice(name.as_bytes());
                            out.push(b'\n');
                        }
                        ("count", true) => report::json_count(out, &name, kept.len() as u64),
                        ("count", false) => out.extend_from_slice(format!("{name}:{}\n", kept.len()).as_bytes()),
                        _ if style.json || style.heading => {
                            report::folder_file(out, style, &query, shown, path, bytes, &kept);
                        }
                        _ => {
                            report::plain(out, &query, &name, bytes, &kept);
                        }
                    }
                    kept.len() as u64
                };
                match &only {
                    Some(files) => sweep::listed_with(root, files, options.threads, &query, &format),
                    None => sweep::tree_with(root, options, &query, &format),
                }
            } else {
                sweep::tree(root, shown, options, &query, report)
            }
            .map_err(io)?;
            let searched = searching.elapsed();
            let writing = std::time::Instant::now();
            let parts = sweep::in_order(&gathered);
            let mut budget = report::Budget::new(a.budget);
            for (_, part) in &parts {
                if !budget.write(&mut out, part).map_err(io)? {
                    break;
                }
            }
            let totals = report::Totals {
                files_searched: gathered.iter().map(|g| g.files).sum(),
                files_matched: parts.len() as u64,
                lines: Some(gathered.iter().map(|g| g.matched_lines).sum()),
                matches: None,
                bytes_searched: gathered.iter().map(|g| g.bytes).sum(),
                seconds: began.elapsed().as_secs_f64(),
            };
            a.finish(&mut out, &totals, &budget).map_err(io)?;
            let any = totals.lines != Some(0);
            if let (Some(sampler), Some(path)) = (sampler, &a.samples) {
                let taken = sampler.finish(path).map_err(io)?;
                note!("{taken} samples written to {path}");
            }
            if a.account {
                account::print(began, searched, writing.elapsed(), &gathered, options.threads);
            }
            i32::from(!any)
        }
        ("count", [pattern, file]) => {
            // Counting matches needs no line numbers; a background line count
            // would read the whole file a second time beside the search.
            linework_storage::set_line_indexing(false);
            let doc = open(file).map_err(io)?;
            let sampler = a.samples.as_ref().map(|_| sampler::Sampler::start());
            // With conditions or a time span, the count is of matching lines that pass them.
            let conditions = !(a.near.is_empty() && a.and.is_empty() && a.not.is_empty() && a.since.is_none() && a.until.is_none());
            let mut searched = doc.len();
            let (count, lines) = if conditions {
                let scope = span(&doc, &a, &cancel)?;
                if a.since.is_some() || a.until.is_some() {
                    searched = scope.0.end - scope.0.start;
                }
                let kept = file_lines(&doc, &a, pattern, &scope, &cancel).map_err(io)?.len() as u64;
                (kept, Some(kept))
            } else {
                let mut found = doc.find_all_bounded(pattern, options(&a, pattern), MOST, &cancel).map_err(io)?;
                if found.last().is_some_and(|hit| hit.is_empty() && change::after_last_line(&doc, hit.start).unwrap_or(false)) {
                    found.pop();
                }
                (found.len() as u64, None)
            };
            if let (Some(sampler), Some(path)) = (sampler, &a.samples) {
                let taken = sampler.finish(path).map_err(io)?;
                note!("{taken} samples written to {path}");
            }
            let mut budget = report::Budget::new(a.budget);
            if a.style().json {
                let mut line = Vec::new();
                report::json_count(&mut line, file, count);
                budget.write(&mut out, &line).map_err(io)?;
            } else {
                budget.write(&mut out, format!("{count}\n").as_bytes()).map_err(io)?;
            }
            let totals = report::Totals {
                files_searched: 1,
                files_matched: u64::from(count != 0),
                lines,
                matches: if lines.is_none() { Some(count) } else { None },
                bytes_searched: searched,
                seconds: began.elapsed().as_secs_f64(),
            };
            a.finish(&mut out, &totals, &budget).map_err(io)?;
            i32::from(count == 0)
        }
        ("find", [pattern, file]) => {
            // Line numbers are counted in the search's own pass, so the
            // background line count is not needed.
            linework_storage::set_line_indexing(false);
            let doc = open(file).map_err(io)?;
            let style = a.style();
            let mut budget = report::Budget::new(a.budget);
            let mut printed = 0usize;
            let conditions = !(a.near.is_empty() && a.and.is_empty() && a.not.is_empty());
            let scope = span(&doc, &a, &cancel)?;
            if a.names_only {
                // -l: the file's name if any line matches and passes the conditions.
                let any = if conditions {
                    !file_lines(&doc, &a, pattern, &scope, &cancel).map_err(io)?.is_empty()
                } else {
                    doc.find_lines_in(pattern, options(&a, pattern), scope.0.clone(), scope.1, 1, &cancel, &mut |_| Ok(true)).map_err(io)?
                };
                if any {
                    printed = 1;
                    let line = if style.json { format!("{}\n", serde_json::json!({"path": file})) } else { format!("{file}\n") };
                    budget.write(&mut out, line.as_bytes()).map_err(io)?;
                }
            } else if !style.json && !style.heading && !conditions {
                if a.most != 0 {
                    doc.find_lines_in(pattern, options(&a, pattern), scope.0.clone(), scope.1, a.widest(), &cancel, &mut |found| {
                        let mut line = Vec::new();
                        report::numbered_found(&mut line, found);
                        printed += 1;
                        Ok(!budget.write(&mut out, &line)? || printed == a.most)
                    })
                    .map_err(io)?;
                }
            } else if !style.json && !style.heading {
                let found = file_lines(&doc, &a, pattern, &scope, &cancel).map_err(io)?;
                printed = found.len();
                for f in &found {
                    let mut line = Vec::new();
                    report::numbered_found(&mut line, f);
                    if !budget.write(&mut out, &line).map_err(io)? {
                        break;
                    }
                }
            } else {
                let query = a.query(pattern)?;
                let found = file_lines(&doc, &a, pattern, &scope, &cancel).map_err(io)?;
                printed = found.len();
                // The definitions around the lines, for a file the outline can read.
                let whole = if style.scope && doc.len() <= linework_project::MAX_SYMBOL_SOURCE as u64 {
                    doc.read(0, doc.len() as usize).map_err(io)?
                } else {
                    Vec::new()
                };
                let starts: Vec<usize> = found.iter().map(|f| f.first.start as usize).collect();
                let scopes = report::scopes(style.scope && !whole.is_empty(), file, &whole, &starts);
                let lines: Vec<report::Line> = found
                    .iter()
                    .zip(&scopes)
                    .map(|(f, scope)| report::Line::found(f, scope.as_ref()))
                    .collect();
                let mut text = Vec::new();
                if !lines.is_empty() {
                    report::file(&mut text, style, &query, file, &lines);
                }
                budget.write(&mut out, &text).map_err(io)?;
            }
            let totals = report::Totals {
                files_searched: 1,
                files_matched: u64::from(printed != 0),
                lines: Some(printed as u64),
                matches: None,
                bytes_searched: scope.0.end - scope.0.start,
                seconds: began.elapsed().as_secs_f64(),
            };
            a.finish(&mut out, &totals, &budget).map_err(io)?;
            i32::from(printed == 0)
        }
        // The span of a time-ordered log, found by bisecting its timestamps.
        ("lines", [file]) if a.since.is_some() || a.until.is_some() => {
            let doc = open(file).map_err(io)?;
            let range = time::span(&doc, a.since.as_deref(), a.until.as_deref())?;
            let mut at = range.start;
            let mut block = vec![0; 1 << 20];
            let mut budget = report::Budget::new(a.budget);
            while at < range.end {
                let n = (range.end - at).min(block.len() as u64) as usize;
                doc.read_into(at, &mut block[..n]).map_err(io)?;
                if !budget.write(&mut out, &block[..n]).map_err(io)? {
                    break;
                }
                at += n as u64;
            }
            out.flush().map_err(io)?;
            if a.stats {
                note!("bytes {} to {} of {} ({:.1} MB) in {:.3} s", range.start, range.end, doc.len(), (range.end - range.start) as f64 / 1e6, began.elapsed().as_secs_f64());
            }
            i32::from(range.is_empty())
        }
        ("lines", [file, from, rest @ ..]) if rest.len() <= 1 => {
            let first: u64 = from.parse().map_err(|_| format!("FROM must be a line number, not {from}"))?;
            let last: u64 = match rest.first() {
                Some(to) => to.parse().map_err(|_| format!("TO must be a line number, not {to}"))?,
                None => first,
            };
            if first == 0 || last < first {
                return Err("lines are counted from 1, and TO must not be before FROM".into());
            }
            let doc = open(file).map_err(io)?;
            counted(&doc).map_err(io)?;
            let lines = doc.line_count();
            if first > lines {
                return Ok(1);
            }
            let mut at = doc.line_start(first - 1);
            let end = if last < lines { doc.line_start(last) } else { doc.len() };
            let mut block = vec![0; 1 << 20];
            while at < end {
                let n = (end - at).min(block.len() as u64) as usize;
                doc.read_into(at, &mut block[..n]).map_err(io)?;
                out.write_all(&block[..n]).map_err(io)?;
                at += n as u64;
            }
            0
        }
        ("info", [file]) => {
            let doc = open(file).map_err(io)?;
            counted(&doc).map_err(io)?;
            let ends = match doc.newline() {
                "\r\n" => "CRLF",
                "\r" => "CR",
                _ => "LF",
            };
            writeln!(out, "bytes {}\nlines {}\nencoding {}\nline ends {}", doc.len(), doc.line_count(), doc.encoding_name(), ends).map_err(io)?;
            0
        }
        ("replace", [old, new, target]) => {
            // Records that let a small change be written straight into a large
            // file; one left by a crash is settled here before the file is opened.
            linework_storage::set_save_journal(Some(&profile.join("save-records")));
            let searched = options(&a, old);
            let style = a.style();
            // In a folder, the files a search for OLD would show.
            let files: Vec<String> = if Path::new(target).is_dir() {
                use linework_search::sweep;
                let mut walk = linework_search::walk::Options { hidden: a.hidden, ignore: !a.no_ignore, pick: pick.as_ref(), batch: 8, ..Default::default() };
                if let Some(threads) = a.threads {
                    walk.threads = threads;
                }
                let shown = target.trim_end_matches(['/', '\\']);
                let query = a.query(old)?;
                let name = |path: &str, _: &[u8], _: &[sweep::LineHit], out: &mut Vec<u8>| -> u64 {
                    out.extend_from_slice(format!("{shown}/{path}").as_bytes());
                    1
                };
                let gathered = sweep::tree_with(Path::new(target), walk, &query, &name).map_err(io)?;
                sweep::in_order(&gathered).into_iter().map(|(_, part)| String::from_utf8_lossy(part).into_owned()).collect()
            } else {
                vec![target.clone()]
            };
            let changes = profile.join("changes");
            let mut record = if a.dry_run { None } else { Some(change::Record::start(&changes, &std::iter::once("lw".to_owned()).chain(argv.iter().cloned()).collect::<Vec<_>>()).map_err(io)?) };
            let mut budget = report::Budget::new(a.budget);
            let (mut replaced, mut changed, mut failed) = (0usize, 0usize, 0usize);
            // In batches: plan on several threads, keep the batch's record on disk,
            // then save its files on several threads, so their disk flushes overlap.
            for batch in files.chunks(256) {
                let plans = change::each(batch, |path| change::plan(Path::new(path), old, new, searched, a.dry_run, a.widest(), &cancel));
                let mut ready = Vec::new();
                for (path, plan) in batch.iter().zip(plans) {
                    match plan {
                        Ok(Some(planned)) => ready.push((path, planned)),
                        Ok(None) => {}
                        Err(e) => {
                            note!("lw: {path}: {e}");
                            failed += 1;
                        }
                    }
                }
                let ready: Vec<(&String, change::Planned, bool)> = if let Some(record) = &mut record {
                    record.keep(&ready.iter().map(|(_, p)| p).collect::<Vec<_>>()).map_err(io)?;
                    let saved = change::each_owned(ready, |(path, mut planned)| {
                        let result = planned.doc.save(&planned.path);
                        (path, planned, result)
                    });
                    let mut kept = Vec::new();
                    for (path, planned, result) in saved {
                        match result {
                            Ok(()) => {
                                record.saved(&planned.path).map_err(io)?;
                                kept.push((path, planned, true));
                            }
                            Err(e) => {
                                note!("lw: {path}: not changed: {e}");
                                failed += 1;
                            }
                        }
                    }
                    kept
                } else {
                    ready.into_iter().map(|(path, planned)| (path, planned, false)).collect()
                };
                for (path, planned, _) in ready {
                    replaced += planned.edits.len();
                    changed += 1;
                    let mut text = Vec::new();
                    match (a.dry_run, style.json) {
                        // A long line shows in part around its first change, the same part before and after.
                        (true, true) => {
                            for (line, before, after, at) in &planned.lines {
                                let mut value = serde_json::json!({"path": path, "line": line});
                                for (name, side) in [("before", before), ("after", after)] {
                                    let shown = linework_search::sweep::clip(side, *at);
                                    value[name] = serde_json::json!(String::from_utf8_lossy(&side[shown.clone()]));
                                    if shown != (0..side.len()) {
                                        // Bytes left out of this side: before the part shown, and after it.
                                        value[format!("{name}_omitted")] = serde_json::json!([shown.start, side.len() - shown.end]);
                                    }
                                }
                                text.extend_from_slice(value.to_string().as_bytes());
                                text.push(b'\n');
                            }
                        }
                        (true, false) => {
                            text.extend_from_slice(format!("{path}\n").as_bytes());
                            for (line, before, after, at) in &planned.lines {
                                text.extend_from_slice(format!("  {line} - ").as_bytes());
                                linework_search::sweep::write_clipped(&mut text, before, *at);
                                text.extend_from_slice(format!("\n  {line} + ").as_bytes());
                                linework_search::sweep::write_clipped(&mut text, after, *at);
                                text.push(b'\n');
                            }
                            text.push(b'\n');
                        }
                        (false, true) => {
                            text.extend_from_slice(serde_json::json!({"path": path, "replacements": planned.edits.len()}).to_string().as_bytes());
                            text.push(b'\n');
                        }
                        (false, false) => text.extend_from_slice(format!("{path}: {} replaced\n", planned.edits.len()).as_bytes()),
                    }
                    budget.write(&mut out, &text).map_err(io)?;
                }
            }
            if let Some(record) = &mut record {
                record.finish().map_err(io)?;
            }
            let id = record.as_ref().map(|r| r.id.clone());
            if let (Some(id), 0) = (&id, changed) {
                // Nothing was changed: no record to keep.
                let _ = std::fs::remove_file(changes.join(format!("{id}.jsonl")));
            }
            if a.json {
                let value = serde_json::json!({"summary": {
                    "files_changed": changed, "replacements": replaced, "saved": !a.dry_run,
                    "change": if changed == 0 { None } else { id.clone() }, "failed": failed,
                    "output_bytes": budget.written, "cut_short": budget.cut,
                    "seconds": (began.elapsed().as_secs_f64() * 1000.0).round() / 1000.0,
                }});
                writeln!(out, "{value}").map_err(io)?;
            } else if a.dry_run {
                writeln!(out, "{replaced} replacements in {changed} files would be made; nothing was saved").map_err(io)?;
            } else if let (Some(id), true) = (&id, changed != 0) {
                writeln!(out, "{replaced} replacements in {changed} files saved; undo with: lw undo {id}").map_err(io)?;
            }
            out.flush().map_err(io)?;
            if failed != 0 {
                note!("lw: {failed} files could not be changed");
                2
            } else {
                i32::from(changed == 0)
            }
        }
        ("undo", rest) if rest.len() <= 1 => {
            let changes = profile.join("changes");
            let records = change::records(&changes).map_err(io)?;
            if a.list {
                for r in records.iter().rev().take(20) {
                    let edits: usize = r.files.iter().map(|f| f.edits.len()).sum();
                    let state = if r.undone { "  undone" } else { "" };
                    writeln!(out, "{}  {} files, {edits} replacements{state}  {}", r.id, r.files.len(), r.command.iter().skip(1).cloned().collect::<Vec<_>>().join(" "))
                        .map_err(io)?;
                }
                return Ok(0);
            }
            let chosen = match rest.first() {
                Some(id) => records.iter().find(|r| &r.id == id).ok_or(format!("no change {id}; lw undo --list shows them"))?,
                None => records.iter().rev().find(|r| !r.undone && !r.files.is_empty()).ok_or("no change to undo")?,
            };
            if chosen.undone {
                return Err(format!("change {} was already undone", chosen.id));
            }
            linework_storage::set_save_journal(Some(&profile.join("save-records")));
            let (mut restored, mut refused) = (0usize, 0usize);
            // The files are restored on several threads; each is checked before it is touched.
            let files: Vec<&change::Changed> = chosen.files.iter().rev().collect();
            let results = change::each(&files, |file| change::revert(&file.path, file.length, &file.edits));
            for (file, result) in files.iter().zip(results) {
                match result {
                    Ok(()) => {
                        restored += 1;
                        writeln!(out, "{}: restored", file.path.display()).map_err(io)?;
                    }
                    // A save that was never marked done: the file holds the
                    // change only if the save got through; otherwise there is nothing to undo.
                    Err(_) if !file.saved => {}
                    Err(e) => {
                        refused += 1;
                        note!("lw: {}: not restored: {e}", file.path.display());
                    }
                }
            }
            if refused == 0 {
                change::mark_undone(&changes, &chosen.id).map_err(io)?;
            }
            writeln!(out, "{restored} files restored, {refused} left as they are").map_err(io)?;
            i32::from(refused != 0) * 2
        }
        ("files", [folder]) => {
            let mut options = linework_search::walk::Options { hidden: a.hidden, ignore: !a.no_ignore, pick: pick.as_ref(), ..Default::default() };
            if let Some(threads) = a.threads {
                options.threads = threads;
            }
            let root = folder.trim_end_matches(['/', '\\']);
            if a.account {
                linework_search::account::switch_on();
            }
            let walking = std::time::Instant::now();
            // Each thread fills its own list; nothing is shared while walking.
            let lists = linework_search::walk::walk(Path::new(folder), options, Vec::<u8>::new, |listed, found| {
                listed.extend_from_slice(root.as_bytes());
                listed.push(b'/');
                listed.extend_from_slice(found.path.as_bytes());
                listed.push(b'\n');
            })
            .map_err(io)?;
            let walked = walking.elapsed();
            let writing = std::time::Instant::now();
            // In path order, so the same folder lists the same way every time.
            let mut paths: Vec<&[u8]> = lists.iter().flat_map(|l| l.split_inclusive(|&b| b == b'\n')).collect();
            paths.sort_unstable_by(|a, b| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()).then_with(|| a.cmp(b)));
            let any = !paths.is_empty();
            let mut budget = report::Budget::new(a.budget);
            for path in &paths {
                if !budget.write(&mut out, path).map_err(io)? {
                    break;
                }
            }
            out.flush().map_err(io)?;
            if budget.cut {
                note!("lw: output cut at {} bytes (--budget)", budget.written);
            }
            if a.account {
                let found = lists.iter().map(|l| l.iter().filter(|&&b| b == b'\n').count()).sum::<usize>();
                note!("{found} files listed");
                account::print(began, walked, writing.elapsed(), &[], options.threads);
            }
            i32::from(!any)
        }
        // Where a name is defined, or used outside comments and strings, in code.
        ("defs" | "uses", [name, target]) => {
            use linework_search::sweep;
            let definitions = command == "defs";
            // Only files holding the name as a whole word are parsed.
            let query = sweep::Query::new(name, true, false, true)?;
            let style = a.style();
            let lines_of = |path: &str, bytes: &[u8]| -> Vec<sweep::LineHit> {
                let Ok(text) = std::str::from_utf8(bytes) else { return Vec::new() };
                let places: Vec<_> =
                    linework_project::places(Path::new(path), text, name).into_iter().filter(|p| p.definition.is_some() == definitions).collect();
                report::place_lines(bytes, &places)
            };
            let mut budget = report::Budget::new(a.budget);
            let totals = if Path::new(target).is_dir() {
                let mut options = linework_search::walk::Options { hidden: a.hidden, ignore: !a.no_ignore, pick: pick.as_ref(), batch: 8, ..Default::default() };
                if let Some(threads) = a.threads {
                    options.threads = threads;
                }
                let shown = target.trim_end_matches(['/', '\\']);
                let format = |path: &str, bytes: &[u8], _: &[sweep::LineHit], out: &mut Vec<u8>| -> u64 {
                    let mut lines = lines_of(path, bytes);
                    lines.truncate(a.most);
                    if lines.is_empty() {
                        return 0;
                    }
                    if style.json || style.heading {
                        report::folder_file(out, style, &query, shown, path, bytes, &lines)
                    } else {
                        report::plain(out, &query, &format!("{shown}/{path}"), bytes, &lines)
                    }
                };
                let gathered = sweep::tree_with(Path::new(target), options, &query, &format).map_err(io)?;
                let parts = sweep::in_order(&gathered);
                for (_, part) in &parts {
                    if !budget.write(&mut out, part).map_err(io)? {
                        break;
                    }
                }
                report::Totals {
                    files_searched: gathered.iter().map(|g| g.files).sum(),
                    files_matched: parts.len() as u64,
                    lines: Some(gathered.iter().map(|g| g.matched_lines).sum()),
                    matches: None,
                    bytes_searched: gathered.iter().map(|g| g.bytes).sum(),
                    seconds: began.elapsed().as_secs_f64(),
                }
            } else {
                let bytes = std::fs::read(target).map_err(io)?;
                if bytes.len() > linework_project::MAX_SYMBOL_SOURCE {
                    return Err(format!("{target} is over the {} MiB that code is parsed for", linework_project::MAX_SYMBOL_SOURCE >> 20));
                }
                let mut lines = lines_of(target, &bytes);
                lines.truncate(a.most);
                let mut text = Vec::new();
                if !lines.is_empty() {
                    if style.json || style.heading {
                        let starts: Vec<usize> = lines.iter().map(|l| l.start).collect();
                        let scopes = report::scopes(style.scope, target, &bytes, &starts);
                        let shown: Vec<report::Line> = lines
                            .iter()
                            .zip(&scopes)
                            .map(|(l, s)| report::Line::whole(l.number, l.start as u64, &bytes[l.start..l.end], s.as_ref()))
                            .collect();
                        report::file(&mut text, style, &query, target, &shown);
                    } else {
                        for l in &lines {
                            let line = &bytes[l.start..l.end];
                            report::numbered(&mut text, l.number, line, sweep::first_match(&query, line));
                        }
                    }
                }
                budget.write(&mut out, &text).map_err(io)?;
                report::Totals {
                    files_searched: 1,
                    files_matched: u64::from(!lines.is_empty()),
                    lines: Some(lines.len() as u64),
                    matches: None,
                    bytes_searched: bytes.len() as u64,
                    seconds: began.elapsed().as_secs_f64(),
                }
            };
            a.finish(&mut out, &totals, &budget).map_err(io)?;
            i32::from(totals.lines == Some(0))
        }
        // The files added, changed or removed in a folder since the last look, then a new snapshot.
        ("changes", [folder]) if Path::new(folder).is_dir() => {
            // A snapshot is of the whole folder, so the next look compares like with like.
            if pick.is_some() {
                return Err("lw changes looks at the whole folder; -g, -t and -T apply to the searches (lw find --changed -t rust)".into());
            }
            let options = linework_search::walk::Options { hidden: a.hidden, ignore: !a.no_ignore, ..Default::default() };
            let root = Path::new(folder);
            let name = a.mark.as_deref().unwrap_or("last");
            let shown = folder.trim_end_matches(['/', '\\']);
            let now = marks::list(root, options).map_err(io)?;
            let before = marks::load(&profile, root, name).map_err(io)?;
            let mut budget = report::Budget::new(a.budget);
            let found = match &before {
                None => {
                    note!("lw: {} files noted as snapshot {name}; the next lw changes shows what changed since", now.len());
                    marks::Changes::default()
                }
                Some(before) => marks::compare(before, &now),
            };
            let mut text = Vec::new();
            for (kind, paths) in [("added", &found.added), ("changed", &found.changed), ("removed", &found.removed)] {
                for path in paths {
                    if a.json {
                        text.extend_from_slice(serde_json::json!({"change": kind, "path": format!("{shown}/{path}")}).to_string().as_bytes());
                        text.push(b'\n');
                    } else {
                        text.extend_from_slice(format!("{kind:8}{shown}/{path}\n").as_bytes());
                    }
                }
            }
            budget.write(&mut out, &text).map_err(io)?;
            if a.json {
                let value = serde_json::json!({"summary": {
                    "snapshot": name, "first": before.is_none(), "files": now.len(),
                    "added": found.added.len(), "changed": found.changed.len(), "removed": found.removed.len(),
                    "cut_short": budget.cut, "seconds": (began.elapsed().as_secs_f64() * 1000.0).round() / 1000.0,
                }});
                writeln!(out, "{value}").map_err(io)?;
            }
            out.flush().map_err(io)?;
            marks::save(&profile, root, name, &now).map_err(io)?;
            i32::from(found.added.is_empty() && found.changed.is_empty() && found.removed.is_empty())
        }
        ("open", targets) if !targets.is_empty() => {
            // Installed, the editor sits next to lw; in a build tree it is
            // linework.exe next to lw.exe, the same name to Windows.
            let editor = env::current_exe().map_err(io)?.with_file_name("Linework.exe");
            if !editor.is_file() {
                return Err(format!("Linework.exe is not next to lw: {}", editor.display()));
            }
            let mut command = std::process::Command::new(&editor);
            command.arg("--reuse-window").arg("--").args(targets);
            if a.dry_run {
                let shown: Vec<String> = std::iter::once(editor.display().to_string())
                    .chain(command.get_args().map(|arg| arg.to_string_lossy().into_owned()))
                    .map(|arg| if arg.contains(' ') { format!("\"{arg}\"") } else { arg })
                    .collect();
                writeln!(out, "{}", shown.join(" ")).map_err(io)?;
            } else {
                command.spawn().map_err(io)?;
            }
            0
        }
        _ => {
            return Err(match help::find(&command).filter(|t| !t.usage.is_empty()) {
                Some(topic) => format!("wrong words for {command}; usage: {}\nlw help {command} explains it", topic.usage),
                None => format!("there is no command {command}; lw help lists them"),
            })
        }
    };
    out.flush().map_err(io)?;
    Ok(code)
}

mod account;
mod change;
mod help;
mod marks;
mod serve;
mod time;
mod report;
mod sampler;

fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(message) => {
            note!("lw: {message}");
            2
        }
    };
    std::process::exit(code);
}
