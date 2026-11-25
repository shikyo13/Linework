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
  lw count   [-i] [-F] [-w] [OUTPUT] PATTERN FILE
  lw find    [-i] [-F] [-w] [-m N] [OUTPUT] PATTERN FILE
  lw lines   FILE FROM [TO]
  lw info    FILE
  lw replace [-i] [-F] [-w] [--dry-run] OLD NEW FILE
  lw files   [--hidden] [--no-ignore] FOLDER
  lw open    [--dry-run] FILE[:LINE[:COLUMN]]...

OUTPUT: --json (one object per line, then a summary), --heading (each file
once, then its lines), --scope (the function, class or section around each
line), --budget BYTES (stop printing there), --stats (totals on stderr).

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
    rest: Vec<String>,
}

impl Args {
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
    let mut a = Args { ignore_case: false, fixed: false, word: false, dry_run: false, hidden: false, no_ignore: false, account: false, samples: None, most: usize::MAX, threads: None, json: false, heading: false, scope: false, stats: false, budget: None, rest: Vec::new() };
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
            let special = pattern.chars().any(|c| r"\^$.|?*+()[]{}".contains(c));
            let query = Query::new(pattern, a.fixed || !special, a.ignore_case, a.word)?;
            let report = if command == "count" { Report::Counts } else { Report::Lines };
            let shown = folder.trim_end_matches(['/', '\\']);
            if a.account || a.samples.is_some() {
                linework_search::account::switch_on();
            }
            let sampler = a.samples.as_ref().map(|_| sampler::Sampler::start());
            let style = a.style();
            let searching = std::time::Instant::now();
            let root = Path::new(folder);
            let gathered = if command == "find" && (style.json || style.heading) {
                let format = |path: &str, bytes: &[u8], hits: &[sweep::LineHit], out: &mut Vec<u8>| {
                    report::folder_file(out, style, &query, shown, path, bytes, hits)
                };
                sweep::tree_with(root, options, &query, &format)
            } else if command == "count" && style.json {
                let format = |path: &str, _: &[u8], hits: &[sweep::LineHit], out: &mut Vec<u8>| {
                    report::json_count(out, &format!("{shown}/{path}"), hits.len() as u64)
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
            let found = doc.find_all_bounded(pattern, options(&a, pattern), MOST, &cancel).map_err(io)?;
            if let (Some(sampler), Some(path)) = (sampler, &a.samples) {
                let taken = sampler.finish(path).map_err(io)?;
                eprintln!("{taken} samples written to {path}");
            }
            let mut budget = report::Budget::new(a.budget);
            if a.style().json {
                let mut line = Vec::new();
                report::json_count(&mut line, file, found.len() as u64);
                budget.write(&mut out, &line).map_err(io)?;
            } else {
                budget.write(&mut out, format!("{}\n", found.len()).as_bytes()).map_err(io)?;
            }
            let totals = report::Totals {
                files_searched: 1,
                files_matched: u64::from(!found.is_empty()),
                lines: None,
                matches: Some(found.len() as u64),
                bytes_searched: doc.len(),
                seconds: began.elapsed().as_secs_f64(),
            };
            a.finish(&mut out, &totals, &budget).map_err(io)?;
            i32::from(found.is_empty())
        }
        ("find", [pattern, file]) => {
            // Line numbers are counted in the search's own pass, so the
            // background line count is not needed.
            linework_storage::set_line_indexing(false);
            let doc = open(file).map_err(io)?;
            let style = a.style();
            let mut budget = report::Budget::new(a.budget);
            let mut printed = 0usize;
            if !style.json && !style.heading {
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
            } else if a.most != 0 {
                use linework_search::sweep::Query;
                let special = pattern.chars().any(|c| r"\^$.|?*+()[]{}".contains(c));
                let query = Query::new(pattern, a.fixed || !special, a.ignore_case, a.word)?;
                let mut found = Vec::new();
                doc.find_lines(pattern, options(&a, pattern), WIDEST as usize, &cancel, &mut |line| {
                    found.push(line.clone());
                    Ok(found.len() == a.most)
                })
                .map_err(io)?;
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
