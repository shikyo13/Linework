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
  lw count   [-i] [-F] [-w] PATTERN FILE
  lw find    [-i] [-F] [-w] [-m N] PATTERN FILE
  lw lines   FILE FROM [TO]
  lw info    FILE
  lw replace [-i] [-F] [-w] [--dry-run] OLD NEW FILE
  lw files   [--hidden] [--no-ignore] FOLDER
  lw open    [--dry-run] FILE[:LINE[:COLUMN]]...

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
    rest: Vec<String>,
}

fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args { ignore_case: false, fixed: false, word: false, dry_run: false, hidden: false, no_ignore: false, account: false, samples: None, most: usize::MAX, threads: None, rest: Vec::new() };
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
            let searching = std::time::Instant::now();
            let gathered = sweep::tree(Path::new(folder), shown, options, &query, report).map_err(io)?;
            let searched = searching.elapsed();
            let writing = std::time::Instant::now();
            let mut any = false;
            for part in &gathered {
                any |= part.matched_lines != 0;
                out.write_all(&part.out).map_err(io)?;
            }
            out.flush().map_err(io)?;
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
            writeln!(out, "{}", found.len()).map_err(io)?;
            i32::from(found.is_empty())
        }
        ("find", [pattern, file]) => {
            // Line numbers are counted in the search's own pass, so the
            // background line count is not needed.
            linework_storage::set_line_indexing(false);
            let doc = open(file).map_err(io)?;
            let mut printed = 0usize;
            if a.most != 0 {
                doc.find_lines(pattern, options(&a, pattern), WIDEST as usize, &cancel, &mut |found| {
                    write!(out, "{}:", found.line + 1)?;
                    out.write_all(&found.text)?;
                    out.write_all(b"\n")?;
                    printed += 1;
                    Ok(printed == a.most)
                })
                .map_err(io)?;
            }
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
