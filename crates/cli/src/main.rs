//! lw: Linework's file engine from the command line, for people, scripts and
//! AI agents. The commands and their help are in `help.rs`; `lw help` shows them.
//! `lw PATTERN [PATH...]` is `lw find`, with ripgrep's and grep's options.
use linework_storage::{Document, SearchOptions};
use linework_project::change;
use linework_search::{options, settings};
use settings::{Mode, Settings};
use std::{
    env,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Duration,
};

/// The commands lw has; any other first word is a pattern for `lw find`.
const COMMANDS: &[&str] = &["find", "count", "lines", "info", "replace", "undo", "changes", "files", "defs", "uses", "open", "serve", "help"];

/// Messages for whoever runs lw (an output cut short, a file that could not be
/// read): on the error stream, or while `lw serve` runs a tool, kept to go
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

/// The editor engine's options for a pattern, by lw's automatic rule and the case and word choices.
fn engine_options(s: &Settings, pattern: &str) -> SearchOptions {
    use linework_search::matcher::{Case, Syntax};
    let regex = match s.matcher.syntax {
        Syntax::Fixed => false,
        Syntax::Auto => linework_search::matcher::looks_like_regex(pattern),
        Syntax::Regex | Syntax::Basic => true,
    };
    let insensitive = match s.matcher.case {
        Case::Insensitive => true,
        Case::Sensitive => false,
        Case::Smart => !pattern.chars().any(char::is_uppercase),
    };
    SearchOptions { case_sensitive: !insensitive, regex, whole_word: s.matcher.word, backwards: false, wrap: false }
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
    let io = |e: io::Error| e.to_string();
    // Help: lw, lw help [NAME | --markdown], lw COMMAND --help, lw -h.
    let first = argv.first().map(String::as_str).unwrap_or("help");
    let asks = argv.iter().take_while(|w| *w != "--").any(|w| w == "--help" || w == "-h");
    if matches!(first, "help" | "--help" | "-h") || asks {
        let name = if asks && help::find(first).is_some() { Some(first) } else { argv.get(1).map(String::as_str).filter(|n| *n != "--help" && *n != "-h") };
        let text = match name {
            Some("--markdown") => help::markdown(),
            Some(name) => help::page(help::find(name).ok_or(format!("there is no help for {name}; lw help lists the commands and topics"))?),
            None => help::overview(),
        };
        out.write_all(text.as_bytes()).map_err(io)?;
        out.flush().map_err(io)?;
        return Ok(0);
    }
    let (command, rest): (&str, Vec<String>) = if COMMANDS.contains(&first) { (first, argv[1..].to_vec()) } else { ("find", argv.clone()) };
    let mut s = Settings::read(rest, |e| note!("lw: {e}"))?;
    if s.version {
        writeln!(out, "lw {}", env!("CARGO_PKG_VERSION")).map_err(io)?;
        return Ok(0);
    }
    if let Some(kind) = &s.generate {
        out.write_all(generate::generate(kind).as_bytes()).map_err(io)?;
        return Ok(0);
    }
    // Set for every command, since a server runs many in one process.
    linework_search::sweep::set_clipping(!s.whole_lines);
    let profile = profile();
    linework_storage::set_index_cache(Some(&profile.join("line-index")));
    // A command that only searches turns this off for itself; a server runs
    // many commands in one process, so each starts from the default.
    linework_storage::set_line_indexing(true);
    let cancel = AtomicBool::new(false);
    let words = s.words.clone();
    let code = match (command, words.as_slice()) {
        ("find" | "count" | "files", _) => {
            match command {
                // lw count is -c, or with --count-matches the number of matches;
                // with --json it gives one object per file.
                "count" => {
                    if s.mode != Mode::CountMatches {
                        s.mode = Mode::Count;
                    }
                }
                "files" => s.list_files = true,
                _ => {}
            }
            if (s.since.is_some() || s.until.is_some()) && s.words.iter().skip(usize::from(!s.explicit_patterns && !s.list_files)).any(|w| Path::new(w).is_dir()) {
                return Err("--since and --until search one time-ordered log, not a folder".into());
            }
            if s.account {
                linework_search::account::switch_on();
            }
            let sampler = s.samples.as_ref().map(|_| sampler::Sampler::start());
            let code = search::run(&s, out)?;
            if let (Some(sampler), Some(path)) = (sampler, &s.samples) {
                let taken = sampler.finish(path).map_err(io)?;
                note!("{taken} samples written to {path}");
            }
            if s.account {
                account::print(began, began.elapsed(), Duration::ZERO, &[], s.threads.unwrap_or(0));
            }
            code
        }
        // The span of a time-ordered log, found by bisecting its timestamps.
        ("lines", [file]) if s.since.is_some() || s.until.is_some() => {
            let doc = open(file).map_err(io)?;
            let range = time::span(&doc, s.since.as_deref(), s.until.as_deref())?;
            let mut at = range.start;
            let mut block = vec![0; 1 << 20];
            let mut budget = report::Budget::new(s.budget);
            while at < range.end {
                let n = (range.end - at).min(block.len() as u64) as usize;
                doc.read_into(at, &mut block[..n]).map_err(io)?;
                if !budget.write(&mut out, &block[..n]).map_err(io)? {
                    break;
                }
                at += n as u64;
            }
            out.flush().map_err(io)?;
            if budget.cut {
                note!("lw: output cut at {} bytes (--budget)", budget.written);
            }
            if s.stats {
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
            let mut budget = report::Budget::new(s.budget);
            while at < end {
                let n = (end - at).min(block.len() as u64) as usize;
                doc.read_into(at, &mut block[..n]).map_err(io)?;
                if !budget.write(&mut out, &block[..n]).map_err(io)? {
                    break;
                }
                at += n as u64;
            }
            if budget.cut {
                note!("lw: output cut at {} bytes (--budget)", budget.written);
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
        ("replace", [old, new, target]) => replace(&s, &argv, old, new, target, &profile, began, &cancel, out)?,
        ("undo", rest) if rest.len() <= 1 => {
            let changes = profile.join("changes");
            let records = change::records(&changes).map_err(io)?;
            if s.list {
                for r in records.iter().rev().take(20) {
                    let edits: usize = r.files.iter().map(|f| f.edits.len()).sum();
                    let state = if r.undone { "  undone" } else { "" };
                    writeln!(out, "{}  {} files, {edits} replacements{state}  {}", r.id, r.files.len(), r.command.iter().skip(1).cloned().collect::<Vec<_>>().join(" "))
                        .map_err(io)?;
                }
                return Ok(0);
            }
            drop(records);
            linework_storage::set_save_journal(Some(&profile.join("save-records")));
            // The files are restored on several threads; each is checked before it is touched.
            let undone = change::undo(&changes, rest.first().map(String::as_str), &|_| false)?;
            for path in &undone.restored {
                writeln!(out, "{}: restored", path.display()).map_err(io)?;
            }
            for (path, why) in &undone.refused {
                note!("lw: {}: not restored: {why}", path.display());
            }
            writeln!(out, "{} files restored, {} left as they are", undone.restored.len(), undone.refused.len()).map_err(io)?;
            i32::from(!undone.refused.is_empty()) * 2
        }
        ("defs" | "uses", [name, target]) => code_places(&s, command == "defs", name, target, began, out)?,
        // The files added, changed or removed in a folder since the last look, then a new snapshot.
        ("changes", [folder]) if Path::new(folder).is_dir() => {
            // A snapshot is of the whole folder, so the next look compares like with like.
            if !s.choices.is_empty() {
                return Err("lw changes looks at the whole folder; -g, -t and -T apply to the searches (lw find --changed -t rust)".into());
            }
            let options = walk_options(&s, None);
            let root = Path::new(folder);
            let name = s.mark.as_deref().unwrap_or("last");
            let shown = folder.trim_end_matches(['/', '\\']);
            let now = marks::list(root, options).map_err(io)?;
            let before = marks::load(&profile, root, name).map_err(io)?;
            let mut budget = report::Budget::new(s.budget);
            let found = match &before {
                None => {
                    note!("lw: {} files noted as snapshot {name}; the next lw changes shows what changed since", now.len());
                    marks::Changes::default()
                }
                Some(before) => marks::compare(before, &now),
            };
            let json = s.json;
            let mut text = Vec::new();
            for (kind, paths) in [("added", &found.added), ("changed", &found.changed), ("removed", &found.removed)] {
                for path in paths {
                    if json {
                        text.extend_from_slice(serde_json::json!({"change": kind, "path": format!("{shown}/{path}")}).to_string().as_bytes());
                        text.push(b'\n');
                    } else {
                        text.extend_from_slice(format!("{kind:8}{shown}/{path}\n").as_bytes());
                    }
                }
            }
            budget.write(&mut out, &text).map_err(io)?;
            if json {
                let value = serde_json::json!({"summary": {
                    "snapshot": name, "first": before.is_none(), "files": now.len(),
                    "added": found.added.len(), "changed": found.changed.len(), "removed": found.removed.len(),
                    "cut_short": budget.cut, "seconds": (began.elapsed().as_secs_f64() * 1000.0).round() / 1000.0,
                }});
                writeln!(out, "{value}").map_err(io)?;
            } else if budget.cut {
                note!("lw: output cut at {} bytes (--budget)", budget.written);
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
            if s.dry_run {
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
            return Err(match help::find(command).filter(|t| !t.usage.is_empty()) {
                Some(topic) => format!("wrong words for {command}; usage: {}\nlw help {command} explains it", topic.usage),
                None => format!("there is no command {command}; lw help lists them"),
            })
        }
    };
    out.flush().map_err(io)?;
    Ok(code)
}

/// The walk's options from the settings, for the commands that walk folders.
fn walk_options<'a>(s: &'a Settings, pick: Option<&'a linework_search::pick::Pick>) -> linework_search::walk::Options<'a> {
    let mut options = linework_search::walk::Options {
        hidden: s.hidden,
        ignore: s.ignore,
        ignore_dot: s.ignore_dot,
        ignore_vcs: s.ignore_vcs,
        ignore_exclude: s.ignore_exclude,
        ignore_global: s.ignore_global,
        ignore_parent: s.ignore_parent,
        extra: &s.ignore_files,
        ignore_fold: s.ignore_fold,
        gitignore_anywhere: !s.require_git,
        pick,
        max_depth: s.max_depth,
        max_filesize: s.max_filesize,
        follow: s.follow,
        one_file_system: s.one_file_system,
        batch: 8,
        ..linework_search::walk::Options::default()
    };
    if let Some(threads) = s.threads {
        options.threads = threads;
    }
    options
}

/// lw replace: every match of OLD replaced with NEW, in a file or the files of a folder.
#[allow(clippy::too_many_arguments)]
fn replace(
    s: &Settings, argv: &[String], old: &str, new: &str, target: &str, profile: &Path, began: std::time::Instant, cancel: &AtomicBool, mut out: &mut dyn Write,
) -> Result<i32, String> {
    let io = |e: io::Error| e.to_string();
    // Records that let a small change be written straight into a large
    // file; one left by a crash is settled here before the file is opened.
    linework_storage::set_save_journal(Some(&profile.join("save-records")));
    let searched = engine_options(s, old);
    let json = s.json;
    let widest = if s.whole_lines { 256 << 20 } else { 1 << 20 };
    // In a folder, the files a search for OLD would show.
    let files: Vec<String> = if Path::new(target).is_dir() {
        use linework_search::sweep;
        let pick = linework_search::pick::Pick::new(&s.choices, &s.types, s.glob_fold)?;
        let walk = walk_options(s, pick.as_ref());
        let shown = target.trim_end_matches(['/', '\\']);
        let mut spec = s.matcher.clone();
        spec.patterns = vec![old.to_owned()];
        let query = linework_search::matcher::Query::build(&spec)?;
        let name = |path: &str, _: &[u8], _: &[sweep::LineHit], out: &mut Vec<u8>| -> u64 {
            out.extend_from_slice(format!("{shown}/{path}").as_bytes());
            1
        };
        let gathered = sweep::tree_with(Path::new(target), walk, &query, &name).map_err(io)?;
        sweep::in_order(&gathered).into_iter().map(|(_, part)| String::from_utf8_lossy(part).into_owned()).collect()
    } else {
        vec![target.to_owned()]
    };
    let changes = profile.join("changes");
    let mut record = if s.dry_run { None } else { Some(change::Record::start(&changes, &std::iter::once("lw".to_owned()).chain(argv.iter().cloned()).collect::<Vec<_>>()).map_err(io)?) };
    let mut budget = report::Budget::new(s.budget);
    let (mut replaced, mut changed, mut failed) = (0usize, 0usize, 0usize);
    // In batches: plan on several threads, keep the batch's record on disk,
    // then save its files on several threads, so their disk flushes overlap.
    for batch in files.chunks(256) {
        let plans = change::each(batch, |path| change::plan(Path::new(path), old, new, searched, s.dry_run, widest, cancel));
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
        let ready: Vec<(&String, change::Planned)> = if let Some(record) = &mut record {
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
                        kept.push((path, planned));
                    }
                    Err(e) => {
                        note!("lw: {path}: not changed: {e}");
                        failed += 1;
                    }
                }
            }
            kept
        } else {
            ready
        };
        for (path, planned) in ready {
            replaced += planned.edits.len();
            changed += 1;
            let mut text = Vec::new();
            match (s.dry_run, json) {
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
    if json {
        let value = serde_json::json!({"summary": {
            "files_changed": changed, "replacements": replaced, "saved": !s.dry_run,
            "change": if changed == 0 { None } else { id.clone() }, "failed": failed,
            "output_bytes": budget.written, "cut_short": budget.cut,
            "seconds": (began.elapsed().as_secs_f64() * 1000.0).round() / 1000.0,
        }});
        writeln!(out, "{value}").map_err(io)?;
    } else {
        if budget.cut {
            note!("lw: output cut at {} bytes (--budget)", budget.written);
        }
        if s.dry_run {
            writeln!(out, "{replaced} replacements in {changed} files would be made; nothing was saved").map_err(io)?;
        } else if let (Some(id), true) = (&id, changed != 0) {
            writeln!(out, "{replaced} replacements in {changed} files saved; undo with: lw undo {id}").map_err(io)?;
        }
    }
    out.flush().map_err(io)?;
    Ok(if failed != 0 {
        note!("lw: {failed} files could not be changed");
        2
    } else {
        i32::from(changed == 0)
    })
}

/// lw defs and lw uses: where a name is defined, or used outside comments and strings, in code.
fn code_places(s: &Settings, definitions: bool, name: &str, target: &str, began: std::time::Instant, mut out: &mut dyn Write) -> Result<i32, String> {
    use linework_search::sweep;
    let io = |e: io::Error| e.to_string();
    // Only files holding the name as a whole word are parsed.
    let query = sweep::Query::new(name, true, false, true)?;
    let json = s.json;
    let style = report::Style { json, heading: s.heading.unwrap_or(false) || (s.scope && !json), scope: s.scope };
    let most = s.max_count.map_or(usize::MAX, |m| m as usize);
    let lines_of = |path: &str, bytes: &[u8]| -> Vec<sweep::LineHit> {
        let Ok(text) = std::str::from_utf8(bytes) else { return Vec::new() };
        let places: Vec<_> = linework_project::places(Path::new(path), text, name).into_iter().filter(|p| p.definition.is_some() == definitions).collect();
        report::place_lines(bytes, &places)
    };
    let mut budget = report::Budget::new(s.budget);
    let totals = if Path::new(target).is_dir() {
        let pick = linework_search::pick::Pick::new(&s.choices, &s.types, s.glob_fold)?;
        let options = walk_options(s, pick.as_ref());
        let shown = target.trim_end_matches(['/', '\\']);
        let format = |path: &str, bytes: &[u8], _: &[sweep::LineHit], out: &mut Vec<u8>| -> u64 {
            let mut lines = lines_of(path, bytes);
            lines.truncate(most);
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
        lines.truncate(most);
        let mut text = Vec::new();
        if !lines.is_empty() {
            if style.json || style.heading {
                let starts: Vec<usize> = lines.iter().map(|l| l.start).collect();
                let scopes = report::scopes(style.scope, target, &bytes, &starts);
                let shown: Vec<report::Line> = lines.iter().zip(&scopes).map(|(l, sc)| report::Line::whole(l.number, l.start as u64, &bytes[l.start..l.end], sc.as_ref())).collect();
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
    if json {
        totals.json(&mut out, &budget).map_err(io)?;
    } else if s.stats {
        note!("{}", totals.text(&budget));
    } else if budget.cut {
        note!("lw: output cut at {} bytes (--budget)", budget.written);
    }
    out.flush().map_err(io)?;
    Ok(i32::from(totals.lines == Some(0)))
}

mod account;
mod generate;
mod help;
mod marks;
mod printer;
mod report;
mod sampler;
mod search;
mod serve;
mod time;

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
