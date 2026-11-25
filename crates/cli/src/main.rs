//! lw: Linework's large-file engine from the command line, for scripts and
//! for programs that drive an editor without a window.
//!
//!   lw count   [-i] [-F] [-w] PATTERN FILE     number of matches
//!   lw find    [-i] [-F] [-w] [-m N] PATTERN FILE   matching lines as line:text
//!   lw lines   FILE FROM [TO]                  lines FROM to TO, counted from 1
//!   lw info    FILE                            size, lines, encoding, line ends
//!   lw replace [-i] [-F] [-w] [--dry-run] OLD NEW FILE   replace all and save
//!   lw files   [--hidden] [--no-ignore] FOLDER    the files a search would read
//!
//! PATTERN is a regular expression unless it holds no special character or -F
//! is given. Exit code: 0 when something was found or done, 1 when nothing
//! matched, 2 on an error.
use linework_storage::{Document, SearchOptions};
use std::{
    env,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Duration,
};

const USAGE: &str = "usage:
  lw count   [-i] [-F] [-w] [OUTPUT] [CONDITIONS] PATTERN FILE
  lw find    [-i] [-F] [-w] [-m N] [OUTPUT] [CONDITIONS] PATTERN FILE
  lw lines   FILE FROM [TO]
  lw info    FILE
  lw replace [-i] [-F] [-w] [--dry-run] OLD NEW FILE
  lw files   [--hidden] [--no-ignore] FOLDER
  lw defs    [OUTPUT] NAME FILE    where NAME is defined in code
  lw uses    [OUTPUT] NAME FILE    where NAME is used in code, not in comments or strings
  lw open    [--dry-run] FILE[:LINE[:COLUMN]]...

OUTPUT: --json (one object per line, then a summary), --heading (each file
once, then its lines), --scope (the function, class or section around each
line), --budget BYTES (stop printing there), --stats (totals on stderr).

CONDITIONS, each may repeat: --near PATTERN N (keep a line only if PATTERN is
within N lines of it), --and PATTERN (only files that also hold PATTERN),
--not PATTERN (only files that do not).

A FOLDER in place of FILE searches every file under it that the ignore rules
leave in. `lw open` shows files in the Linework window, at a line and column
when given; with --dry-run it prints the command instead.";
/// Matches kept in memory for one command: 16 bytes each.
const MOST: usize = 50_000_000;
/// Bytes of one line printed by `find` before it is cut.
const WIDEST: u64 = 1 << 20;

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
    near: Vec<(String, u64)>,
    and: Vec<String>,
    not: Vec<String>,
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
            eprintln!("{}", totals.text(budget));
        } else if budget.cut && !self.json {
            eprintln!("lw: output cut at {} bytes (--budget)", budget.written);
        }
        Ok(())
    }
}

fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args { ignore_case: false, fixed: false, word: false, dry_run: false, hidden: false, no_ignore: false, account: false, samples: None, most: usize::MAX, threads: None, json: false, heading: false, scope: false, stats: false, budget: None, near: Vec::new(), and: Vec::new(), not: Vec::new(), rest: Vec::new() };
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
            "--budget" => {
                let n = args.next().ok_or("--budget needs a number of bytes")?;
                a.budget = Some(n.parse().map_err(|_| format!("--budget needs a number of bytes, not {n}"))?);
            }
            "--near" => {
                let pattern = args.next().ok_or("--near needs a pattern and a number of lines")?;
                let n = args.next().ok_or("--near needs a number of lines after its pattern")?;
                a.near.push((pattern, n.parse().map_err(|_| format!("--near needs a number of lines, not {n}"))?));
            }
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

/// The lines of one file that match and pass the conditions, at most `a.most`.
/// Each --and, --not and --near pattern is searched in the file as well.
fn file_lines(doc: &Document, a: &Args, pattern: &str, cancel: &AtomicBool) -> io::Result<Vec<linework_storage::FoundLine>> {
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
    doc.find_lines(pattern, options(a, pattern), WIDEST as usize, cancel, &mut |line| {
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
    let began = std::time::Instant::now();
    let mut args = env::args().skip(1);
    let command = args.next().ok_or(USAGE)?;
    let a = parse(args)?;
    let io = |e: io::Error| e.to_string();
    let profile = profile();
    linework_storage::set_index_cache(Some(&profile.join("line-index")));
    let out = io::stdout();
    let mut out = io::BufWriter::with_capacity(1 << 16, out.lock());
    let cancel = AtomicBool::new(false);
    let code = match (command.as_str(), a.rest.as_slice()) {
        // A folder: every file under it that the ignore rules leave in.
        ("count" | "find", [pattern, folder]) if Path::new(folder).is_dir() => {
            use linework_search::sweep::{self, Query, Report};
            // A few files at a time, so a large folder is shared between threads.
            let mut options = linework_search::walk::Options { hidden: a.hidden, ignore: !a.no_ignore, batch: 8, ..Default::default() };
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
            let gathered = if !filters.is_empty() || style.json || (command == "find" && style.heading) {
                // Every file with a match passes the conditions first; then its kept lines are shown.
                let format = |path: &str, bytes: &[u8], hits: &[sweep::LineHit], out: &mut Vec<u8>| -> u64 {
                    if !filters.file_passes(bytes) {
                        return 0;
                    }
                    let kept = if filters.near.is_empty() { hits.to_vec() } else { filters.near(bytes, hits) };
                    if kept.is_empty() {
                        return 0;
                    }
                    let name = format!("{shown}/{path}");
                    match (command.as_str(), style.json) {
                        ("count", true) => report::json_count(out, &name, kept.len() as u64),
                        ("count", false) => out.extend_from_slice(format!("{name}:{}\n", kept.len()).as_bytes()),
                        _ if style.json || style.heading => {
                            report::folder_file(out, style, &query, shown, path, bytes, &kept);
                        }
                        _ => {
                            report::plain(out, &name, bytes, &kept);
                        }
                    }
                    kept.len() as u64
                };
                sweep::tree_with(root, options, &query, &format)
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
                eprintln!("{taken} samples written to {path}");
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
            // With conditions, the count is of matching lines that pass them.
            let conditions = !(a.near.is_empty() && a.and.is_empty() && a.not.is_empty());
            let (count, lines) = if conditions {
                let kept = file_lines(&doc, &a, pattern, &cancel).map_err(io)?.len() as u64;
                (kept, Some(kept))
            } else {
                (doc.find_all_bounded(pattern, options(&a, pattern), MOST, &cancel).map_err(io)?.len() as u64, None)
            };
            if let (Some(sampler), Some(path)) = (sampler, &a.samples) {
                let taken = sampler.finish(path).map_err(io)?;
                eprintln!("{taken} samples written to {path}");
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
                bytes_searched: doc.len(),
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
            if !style.json && !style.heading && !conditions {
                if a.most != 0 {
                    doc.find_lines(pattern, options(&a, pattern), WIDEST as usize, &cancel, &mut |found| {
                        let mut line = format!("{}:", found.line + 1).into_bytes();
                        line.extend_from_slice(&found.text);
                        line.push(b'\n');
                        printed += 1;
                        Ok(!budget.write(&mut out, &line)? || printed == a.most)
                    })
                    .map_err(io)?;
                }
            } else if !style.json && !style.heading {
                let found = file_lines(&doc, &a, pattern, &cancel).map_err(io)?;
                printed = found.len();
                for f in &found {
                    let mut line = format!("{}:", f.line + 1).into_bytes();
                    line.extend_from_slice(&f.text);
                    line.push(b'\n');
                    if !budget.write(&mut out, &line).map_err(io)? {
                        break;
                    }
                }
            } else {
                let query = a.query(pattern)?;
                let found = file_lines(&doc, &a, pattern, &cancel).map_err(io)?;
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
                    .map(|(f, scope)| report::Line { number: f.line + 1, start: f.start, text: &f.text, scope: scope.as_ref() })
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
                bytes_searched: doc.len(),
                seconds: began.elapsed().as_secs_f64(),
            };
            a.finish(&mut out, &totals, &budget).map_err(io)?;
            i32::from(printed == 0)
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
        ("replace", [old, new, file]) => {
            // Records that let a small change be written straight into a large
            // file; one left by a crash is settled here before the file is opened.
            linework_storage::set_save_journal(Some(&profile.join("save-records")));
            let mut doc = open(file).map_err(io)?;
            let count = doc.replace_all(old, new, options(&a, old)).map_err(io)?;
            if count != 0 && !a.dry_run {
                let path = doc.path().map(Path::to_owned).ok_or("the file has no path")?;
                doc.save(&path).map_err(io)?;
            }
            writeln!(out, "{count}").map_err(io)?;
            i32::from(count == 0)
        }
        ("files", [folder]) => {
            let mut options = linework_search::walk::Options { hidden: a.hidden, ignore: !a.no_ignore, ..Default::default() };
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
            let mut any = false;
            for listed in &lists {
                any |= !listed.is_empty();
                out.write_all(listed).map_err(io)?;
            }
            out.flush().map_err(io)?;
            if a.account {
                let found = lists.iter().map(|l| l.iter().filter(|&&b| b == b'\n').count()).sum::<usize>();
                eprintln!("{found} files listed");
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
            let pick = |path: &str, bytes: &[u8]| -> Vec<sweep::LineHit> {
                let Ok(text) = std::str::from_utf8(bytes) else { return Vec::new() };
                let places: Vec<_> =
                    linework_project::places(Path::new(path), text, name).into_iter().filter(|p| p.definition.is_some() == definitions).collect();
                report::place_lines(bytes, &places)
            };
            let mut budget = report::Budget::new(a.budget);
            let totals = if Path::new(target).is_dir() {
                let mut options = linework_search::walk::Options { hidden: a.hidden, ignore: !a.no_ignore, batch: 8, ..Default::default() };
                if let Some(threads) = a.threads {
                    options.threads = threads;
                }
                let shown = target.trim_end_matches(['/', '\\']);
                let format = |path: &str, bytes: &[u8], _: &[sweep::LineHit], out: &mut Vec<u8>| -> u64 {
                    let lines = pick(path, bytes);
                    if lines.is_empty() {
                        return 0;
                    }
                    if style.json || style.heading {
                        report::folder_file(out, style, &query, shown, path, bytes, &lines)
                    } else {
                        report::plain(out, &format!("{shown}/{path}"), bytes, &lines)
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
                let lines = pick(target, &bytes);
                let mut text = Vec::new();
                if !lines.is_empty() {
                    if style.json || style.heading {
                        let starts: Vec<usize> = lines.iter().map(|l| l.start).collect();
                        let scopes = report::scopes(style.scope, target, &bytes, &starts);
                        let shown: Vec<report::Line> = lines
                            .iter()
                            .zip(&scopes)
                            .map(|(l, s)| report::Line { number: l.number, start: l.start as u64, text: &bytes[l.start..l.end], scope: s.as_ref() })
                            .collect();
                        report::file(&mut text, style, &query, target, &shown);
                    } else {
                        for l in &lines {
                            text.extend_from_slice(format!("{}:", l.number).as_bytes());
                            text.extend_from_slice(&bytes[l.start..l.end]);
                            text.push(b'\n');
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
        _ => return Err(USAGE.into()),
    };
    out.flush().map_err(io)?;
    Ok(code)
}

mod account;
mod report;
mod sampler;

fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("lw: {message}");
            2
        }
    };
    std::process::exit(code);
}
