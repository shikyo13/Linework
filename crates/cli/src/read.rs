//! Reading that saves an agent a round trip.
//!
//! A program that works on code through a model pays for each command with a
//! turn of the model, which costs it seconds where the command costs
//! milliseconds. Three habits make turns that one richer command can spare:
//! reading a file a screenful at a time to find where something is (`lw
//! outline` says where everything is), finding a name and then reading the
//! lines around it (`lw show` prints the whole definition, of several names at once), and reading
//! several parts of a file or several files one after another (`lw lines`
//! takes many ranges and files at once, with line numbers on request).
use crate::report;
use linework_search::settings::Settings;
use linework_storage::Document;
use std::{io::Write, path::Path};

fn io(e: std::io::Error) -> String {
    e.to_string()
}

/// A file's text for the outline, or why it has none.
fn code(path: &str) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    if bytes.len() > linework_project::MAX_SYMBOL_SOURCE {
        return Err(format!("{path} is over the {} MiB that code is parsed for", linework_project::MAX_SYMBOL_SOURCE >> 20));
    }
    String::from_utf8(bytes).map_err(|_| format!("{path}: not UTF-8 text"))
}

fn line_count(text: &str) -> usize {
    text.split_inclusive('\n').count()
}

/// `lw outline FILE...`: every definition with the lines it spans.
pub fn outline(s: &Settings, files: &[String], out: &mut dyn Write) -> Result<i32, String> {
    let mut any = false;
    for (index, path) in files.iter().enumerate() {
        let text = code(path)?;
        let parts = linework_project::outline(Path::new(path), &text);
        any |= !parts.is_empty();
        if s.json {
            for (depth, part) in &parts {
                let value = serde_json::json!({"path": path, "line": part.line, "end_line": part.end_line, "kind": part.kind, "name": part.name, "depth": depth});
                writeln!(out, "{value}").map_err(io)?;
            }
            continue;
        }
        if index > 0 {
            writeln!(out).map_err(io)?;
        }
        writeln!(out, "{path}: {} lines", line_count(&text)).map_err(io)?;
        if parts.is_empty() {
            crate::note(format!("lw: {path}: no outline for this kind of file"));
        }
        let width = parts.iter().map(|(_, part)| format!("{}-{}", part.line, part.end_line).len()).max().unwrap_or(0);
        for (depth, part) in &parts {
            let span = format!("{}-{}", part.line, part.end_line);
            writeln!(out, "{span:>width$}  {:indent$}{} {}", "", part.kind, part.name, indent = depth * 2).map_err(io)?;
        }
    }
    Ok(i32::from(!any))
}

/// The definitions of `name` in one file's text: (kind, first line, last line), in order. With an `owner`, only
/// those inside a definition of that name, where the file has any such; all of them where it has none (the outline
/// does not name every enclosing block, a Rust `impl` among them).
fn definitions_of(path: &str, text: &str, name: &str, owner: Option<&str>) -> Vec<(String, u64, u64)> {
    let places: Vec<_> = linework_project::places(Path::new(path), text, name).into_iter().filter(|place| place.definition.is_some()).collect();
    let offsets: Vec<usize> = places.iter().map(|place| place.offset).collect();
    let mut found: Vec<(bool, String, u64, u64)> = Vec::new();
    for (place, chain) in places.iter().zip(linework_project::scope_chains(Path::new(path), text, &offsets)) {
        // The definition around the name where the outline knows its extent, else the line it is on.
        let (first, last) = match chain.last() {
            Some(scope) if scope.name == name => (scope.line, scope.end_line),
            _ => (place.line, place.line),
        };
        let owned = owner.is_some_and(|owner| chain.iter().rev().skip(1).any(|scope| scope.name == owner));
        if !found.iter().any(|known| known.2 == first && known.3 == last) {
            found.push((owned, place.definition.clone().unwrap_or_default(), first, last));
        }
    }
    let narrow = found.iter().any(|known| known.0);
    found.into_iter().filter(|known| known.0 || !narrow).map(|(_, kind, first, last)| (kind, first, last)).collect()
}

/// `Type::name`, `Type.name` and `Type#name` as (the owner, the name): how code and people write a method.
fn qualified(name: &str) -> Option<(&str, &str)> {
    let at = name.rfind(|c| matches!(c, ':' | '.' | '#'))?;
    let owner = name[..at].trim_end_matches(':').rsplit(|c| matches!(c, ':' | '.' | '#')).next()?;
    (!owner.is_empty() && at + 1 < name.len()).then(|| (owner, &name[at + 1..]))
}

/// One file's definitions of `name`, written whole with their line numbers. Returns how many.
fn show_file(out: &mut Vec<u8>, json: bool, shown: &str, text: &str, name: &str, owner: Option<&str>) -> u64 {
    let found = definitions_of(shown, text, name, owner);
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    for (kind, first, last) in &found {
        let body = &lines[(*first as usize - 1).min(lines.len())..(*last as usize).min(lines.len())];
        if json {
            let value = serde_json::json!({"path": shown, "name": name, "kind": kind, "line": first, "end_line": last, "text": body.concat()});
            out.extend_from_slice(format!("{value}\n").as_bytes());
            continue;
        }
        if !out.is_empty() {
            out.extend_from_slice(b"--\n");
        }
        out.extend_from_slice(format!("{shown}:{first}-{last}  {kind} {name}\n").as_bytes());
        for (number, line) in (*first..).zip(body) {
            out.extend_from_slice(format!("{number}:{line}").as_bytes());
            if !line.ends_with('\n') {
                out.push(b'\n');
            }
        }
    }
    found.len() as u64
}

/// One name's definitions under `target`, each whole, appended to `part`. Returns how many.
fn show_name(json: bool, name: &str, owner: Option<&str>, target: &str, walk: linework_search::walk::Options<'_>, part: &mut Vec<u8>) -> Result<u64, String> {
    use linework_search::sweep;
    if !Path::new(target).is_dir() {
        return Ok(show_file(part, json, target, &code(target)?, name, owner));
    }
    // Only files holding the name as a whole word are parsed.
    let query = sweep::Query::new(name, true, false, true)?;
    let folder = target.trim_end_matches(['/', '\\']);
    let format = |path: &str, bytes: &[u8], _: &[sweep::LineHit], out: &mut Vec<u8>| -> u64 {
        let Ok(text) = std::str::from_utf8(bytes) else { return 0 };
        let mut file = Vec::new();
        let count = show_file(&mut file, json, &format!("{folder}/{path}"), text, name, owner);
        out.extend_from_slice(&file);
        count
    };
    let gathered = sweep::tree_with(Path::new(target), walk, &query, &format).map_err(io)?;
    let mut shown = 0;
    for (_, file) in sweep::in_order(&gathered) {
        if shown > 0 && !json {
            part.extend_from_slice(b"--\n");
        }
        shown += 1;
        part.extend_from_slice(file);
    }
    Ok(shown)
}

/// How many of the lines that name a name are printed when nothing defines it.
const MENTIONS: usize = 20;

/// For a name nothing in `targets` defines (a constant, a field, a name in another language): the lines that hold
/// it as a whole word, so that the call is not wasted and the next one can ask for the right lines. Returns whether
/// there were any.
fn mentions(name: &str, targets: &[String], walk: linework_search::walk::Options<'_>, part: &mut Vec<u8>) -> Result<bool, String> {
    use linework_search::sweep;
    let query = sweep::Query::new(name, true, false, true)?;
    let line = |path: &str, bytes: &[u8], hit: &sweep::LineHit, out: &mut Vec<u8>| {
        out.extend_from_slice(format!("{path}:{}:", hit.number).as_bytes());
        out.extend_from_slice(&bytes[hit.start..hit.end]);
        out.push(b'\n');
    };
    let mut found = Vec::new();
    for target in targets {
        if Path::new(target).is_dir() {
            let folder = target.trim_end_matches(['/', '\\']);
            let format = |path: &str, bytes: &[u8], hits: &[sweep::LineHit], out: &mut Vec<u8>| -> u64 {
                hits.iter().for_each(|hit| line(&format!("{folder}/{path}"), bytes, hit, out));
                hits.len() as u64
            };
            let gathered = sweep::tree_with(Path::new(target), walk, &query, &format).map_err(io)?;
            sweep::in_order(&gathered).iter().for_each(|(_, file)| found.extend_from_slice(file));
        } else {
            let bytes = std::fs::read(target).map_err(|e| format!("{target}: {e}"))?;
            sweep::matching_lines(&query, &bytes, true, |hit| line(target, &bytes, &hit, &mut found));
        }
    }
    let lines: Vec<&[u8]> = found.split_inclusive(|byte| *byte == b'\n').collect();
    let places = targets.join(" ");
    if lines.is_empty() {
        part.extend_from_slice(format!("{name}: not found in {places}\n").as_bytes());
        return Ok(false);
    }
    part.extend_from_slice(format!("{name}: no definition found; the lines that name it:\n").as_bytes());
    lines.iter().take(MENTIONS).for_each(|line| part.extend_from_slice(line));
    if lines.len() > MENTIONS {
        part.extend_from_slice(format!("... and {} more lines (lw find -w {name} {places})\n", lines.len() - MENTIONS).as_bytes());
    }
    Ok(true)
}

/// `lw show NAME... PATH...`: each definition of each name, whole.
pub fn show(s: &Settings, names: &[String], targets: &[String], walk: linework_search::walk::Options<'_>, mut out: &mut dyn Write) -> Result<i32, String> {
    let mut budget = report::Budget::new(s.budget);
    // One name's definitions in every target, in the order the targets were given.
    let across = |name: &str, owner: Option<&str>, part: &mut Vec<u8>| -> Result<u64, String> {
        let mut shown = 0;
        for target in targets {
            let mut one = Vec::new();
            let count = show_name(s.json, name, owner, target, walk, &mut one)?;
            if count > 0 && shown > 0 && !s.json {
                part.extend_from_slice(b"--\n");
            }
            part.extend_from_slice(&one);
            shown += count;
        }
        Ok(shown)
    };
    // Whether any name was answered with a definition or with lines that hold it.
    let (mut answered, mut printed) = (false, false);
    for name in names {
        let mut part = Vec::new();
        // The name as written first: some languages have names with a dot or colons in them.
        let mut shown = across(name, None, &mut part)?;
        let (owner, bare) = qualified(name).map_or((None, name.as_str()), |(owner, bare)| (Some(owner), bare));
        if shown == 0 && owner.is_some() {
            shown = across(bare, owner, &mut part)?;
        }
        answered |= shown > 0;
        if shown == 0 && !s.json {
            answered |= mentions(bare, targets, walk, &mut part)?;
        }
        if part.is_empty() {
            continue;
        }
        if printed && !s.json {
            budget.write(&mut out, b"--\n").map_err(io)?;
        }
        printed = true;
        if !budget.write(&mut out, &part).map_err(io)? {
            break;
        }
    }
    if budget.cut {
        crate::note(format!("lw: output cut at {} bytes (--budget)", budget.written));
    }
    Ok(i32::from(!answered))
}

/// The words after `lw show` as names and paths. The last word is a path; so is each word before it that is a
/// file or folder that exists, as long as a name is left.
pub fn names_and_paths(words: &[String]) -> (&[String], &[String]) {
    let mut names = words.len() - 1;
    while names > 1 && Path::new(&words[names - 1]).exists() {
        names -= 1;
    }
    words.split_at(names)
}

/// One run of lines asked of `lw lines`: first and last, counted from 1; `None` for the last means to the end.
type Span = (u64, Option<u64>);

/// `12`, `12-40` or `12-` as a run of lines.
fn span(word: &str) -> Option<Span> {
    let (from, to) = match word.split_once('-') {
        Some((from, to)) => (from, Some(to)),
        None => (word, None),
    };
    let first: u64 = from.parse().ok().filter(|n| *n > 0)?;
    match to {
        None => Some((first, Some(first))),
        Some("") => Some((first, None)),
        Some(to) => to.parse().ok().filter(|last| *last >= first).map(|last| (first, Some(last))),
    }
}

/// Writes the text between two places, as far as the budget allows.
fn copy(doc: &Document, mut from: u64, to: u64, budget: &mut report::Budget, mut out: &mut dyn Write, block: &mut [u8]) -> std::io::Result<()> {
    while from < to {
        let n = (to - from).min(block.len() as u64) as usize;
        doc.read_into(from, &mut block[..n])?;
        if !budget.write(&mut out, &block[..n])? {
            break;
        }
        from += n as u64;
    }
    Ok(())
}

/// Waits until the lines are counted as far as line `furthest`, or to the end of the text when it has fewer or none
/// is named. A count that is already past the middle of the text is waited for to its end: a finished count of a
/// large file is kept for the next call, and an unfinished one is lost.
fn counted_to(doc: &Document, furthest: Option<u64>) -> std::io::Result<()> {
    let enough = || {
        let counted = doc.indexed_bytes();
        counted == doc.len() || (counted < doc.len() / 2 && furthest.is_some_and(|line| doc.line_of(counted) >= line))
    };
    // The first lines are counted within microseconds of opening, less than the shortest sleep.
    let mut turns = 0;
    while !enough() {
        if let Some(error) = doc.index_error() {
            return Err(std::io::Error::other(error));
        }
        turns += 1;
        if turns < 2000 { std::thread::yield_now() } else { std::thread::sleep(std::time::Duration::from_millis(1)) }
    }
    Ok(())
}

/// Where the last `count` lines start, found by reading back from the end of the text. A line ends as the line index
/// has it (CR, LF or CRLF), and a text that ends with a line break has no line after it.
fn tail_start(doc: &Document, count: u64) -> std::io::Result<u64> {
    let len = doc.len();
    if count == 0 {
        return Ok(len);
    }
    // A few lines are in the last 64 KiB. Many lines are read in larger steps, which a drive reads faster going back.
    let mut block = vec![0; 4 << 20];
    let (mut at, mut found, mut step) = (len, 0, 1 << 16);
    // The byte that follows the block being read, to tell a CR that is half of CRLF.
    let mut after = None;
    while at > 0 {
        let n = at.min(step) as usize;
        step = (step * 4).min(block.len() as u64);
        at -= n as u64;
        doc.read_into(at, &mut block[..n])?;
        for i in memchr::memrchr2_iter(b'\n', b'\r', &block[..n]) {
            let start = at + i as u64 + 1;
            let next = block[..n].get(i + 1).copied().or(after);
            if start == len || (block[i] == b'\r' && next == Some(b'\n')) {
                continue;
            }
            found += 1;
            if found == count {
                return Ok(start);
            }
        }
        after = Some(block[0]);
    }
    Ok(0)
}

/// `lw lines`: files, each followed by the runs of lines wanted of it.
pub fn lines(s: &Settings, words: &[String], mut out: &mut dyn Write) -> Result<i32, String> {
    // FILE FROM TO, the first form lw had, is one run.
    let old_form = matches!(words, [_, from, to] if from.parse::<u64>().is_ok() && to.parse::<u64>().is_ok());
    let mut asked: Vec<(&String, Vec<Span>)> = Vec::new();
    if old_form {
        let (first, last): (u64, u64) = (words[1].parse().unwrap_or(0), words[2].parse().unwrap_or(0));
        if first == 0 || last < first {
            return Err("lines are counted from 1, and TO must not be before FROM".into());
        }
        asked.push((&words[0], vec![(first, Some(last))]));
    } else {
        for word in words {
            match (span(word), asked.last_mut()) {
                (Some(run), Some(file)) => file.1.push(run),
                // A word that is not a run of lines is a file; so is a number where no file has been named.
                _ => asked.push((word, Vec::new())),
            }
        }
    }
    if asked.is_empty() {
        return Err("lw lines FILE FROM [TO], or FILE with runs such as 10-40 120-160; see lw help lines".into());
    }
    let numbered = s.line_number == Some(true);
    let several = asked.len() > 1;
    let mut budget = report::Budget::new(s.budget);
    let mut printed = false;
    let mut block = vec![0; 1 << 20];
    for (index, (path, runs)) in asked.iter().enumerate() {
        let doc = Document::open(Path::new(path.as_str())).map_err(|e| format!("{path}: {e}"))?;
        if several {
            let head = format!("{}==> {path} <==\n", if index > 0 { "\n" } else { "" });
            budget.write(&mut out, head.as_bytes()).map_err(io)?;
        }
        if let (Some(count), true, false) = (s.last, runs.is_empty(), numbered) {
            // The last lines alone are found from the end: nothing before them is read or counted.
            let from = tail_start(&doc, count).map_err(io)?;
            printed |= from < doc.len();
            copy(&doc, from, doc.len(), &mut budget, &mut out, &mut block).map_err(io)?;
            continue;
        }
        // Runs that all name their last line need the lines counted only as far as the furthest of them.
        let furthest = runs.iter().map(|run| run.1).try_fold(0, |most, last| last.map(|last| most.max(last)));
        counted_to(&doc, furthest.filter(|_| s.last.is_none() && !runs.is_empty())).map_err(io)?;
        let total = if doc.indexing_complete() {
            let total = doc.line_count();
            // A text that ends with a line break has no line after it.
            if total > 0 && doc.line_start(total - 1) == doc.len() { total - 1 } else { total }
        } else {
            doc.line_of(doc.indexed_bytes())
        };
        let mut runs: Vec<Span> = runs.clone();
        match s.last {
            Some(0) if runs.is_empty() => continue,
            Some(0) | None => {}
            Some(last) => runs.push((total.saturating_sub(last - 1).max(1), None)),
        }
        if runs.is_empty() {
            runs.push((1, None));
        }
        for (at, &(first, last)) in runs.iter().enumerate() {
            if first > total {
                continue;
            }
            let last = last.map_or(total, |last| last.min(total));
            if at > 0 {
                budget.write(&mut out, b"--\n").map_err(io)?;
            }
            printed = true;
            if numbered {
                for number in first..=last {
                    let (from, to) = (doc.line_start(number - 1), if number < doc.line_count() { doc.line_start(number) } else { doc.len() });
                    let mut line = format!("{number}:").into_bytes();
                    let text = doc.read(from, (to - from) as usize).map_err(io)?;
                    line.extend_from_slice(&text);
                    if !line.ends_with(b"\n") {
                        line.push(b'\n');
                    }
                    if !budget.write(&mut out, &line).map_err(io)? {
                        break;
                    }
                }
            } else {
                let to = if last < doc.line_count() { doc.line_start(last) } else { doc.len() };
                copy(&doc, doc.line_start(first - 1), to, &mut budget, &mut out, &mut block).map_err(io)?;
            }
        }
    }
    if budget.cut {
        crate::note(format!("lw: output cut at {} bytes (--budget)", budget.written));
    }
    Ok(i32::from(!printed))
}

#[cfg(test)]
mod tests {
    /// What each of the three commands prints for a small file whose every line is known.
    #[test]
    fn outline_show_and_lines_print_what_the_file_holds() {
        let dir = std::env::temp_dir().join(format!("lw-read-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (code, notes) = (dir.join("shapes.rs"), dir.join("notes.txt"));
        let text = "struct Circle {\n    r: f64,\n}\n\nimpl Circle {\n    fn area(&self) -> f64 {\n        3.14 * self.r * self.r\n    }\n}\n\nfn main() {\n    println!(\"{}\", Circle { r: 1.0 }.area());\n}\n";
        std::fs::write(&code, text).unwrap();
        std::fs::write(&notes, "one\ntwo\nthree\nfour\nfive").unwrap();
        let (code, notes) = (code.to_str().unwrap().to_owned(), notes.to_str().unwrap().to_owned());
        let run = |words: &[&str]| {
            let mut out = Vec::new();
            let code = crate::execute(words.iter().map(|w| w.to_string()).collect(), &mut out).unwrap();
            (code, String::from_utf8(out).unwrap())
        };
        // The struct, the method in the impl block (which the outline does not name) and the function.
        assert_eq!(run(&["outline", &code]), (0, format!("{code}: 13 lines\n  1-3  class Circle\n  6-8  method area\n11-13  function main\n")));
        assert_eq!(run(&["show", "area", &code]), (0, format!("{code}:6-8  method area\n6:    fn area(&self) -> f64 {{\n7:        3.14 * self.r * self.r\n8:    }}\n")));
        assert_eq!(run(&["show", "nowhere", &code]), (1, format!("nowhere: not found in {code}\n")));
        // Several names in one call; a name that nothing defines gives the lines that hold it.
        assert_eq!(
            run(&["show", "main", "r", "area", &code]),
            (0, format!("{code}:11-13  function main\n11:fn main() {{\n12:    println!(\"{{}}\", Circle {{ r: 1.0 }}.area());\n13:}}\n--\nr: no definition found; the lines that name it:\n{code}:2:    r: f64,\n{code}:7:        3.14 * self.r * self.r\n{code}:12:    println!(\"{{}}\", Circle {{ r: 1.0 }}.area());\n--\n{code}:6-8  method area\n6:    fn area(&self) -> f64 {{\n7:        3.14 * self.r * self.r\n8:    }}\n"))
        );
        // A name written with its owner, as code writes it. Rust's impl blocks are not in the outline, so the owner
        // cannot narrow there; in Python it picks the right class's method of two.
        assert_eq!(run(&["show", "Circle::area", &code]), run(&["show", "area", &code]));
        let python = dir.join("jobs.py");
        std::fs::write(&python, "class A:\n    def run(self):\n        return 1\n\nclass B:\n    def run(self):\n        return 2\n").unwrap();
        let python = python.to_str().unwrap().to_owned();
        assert_eq!(run(&["show", "B.run", &python]), (0, format!("{python}:6-7  function run\n6:    def run(self):\n7:        return 2\n")));
        assert_eq!(run(&["show", "run", &python]).1.matches("function run").count(), 2);
        // Several files after the names: each name is looked for in every one of them.
        assert_eq!(
            run(&["show", "run", "main", &python, &code]),
            (0, format!("{python}:2-3  function run\n2:    def run(self):\n3:        return 1\n--\n{python}:6-7  function run\n6:    def run(self):\n7:        return 2\n--\n{code}:11-13  function main\n11:fn main() {{\n12:    println!(\"{{}}\", Circle {{ r: 1.0 }}.area());\n13:}}\n"))
        );
        assert_eq!(run(&["show", "nowhere", &python, &code]), (1, format!("nowhere: not found in {python} {code}\n")));
        assert_eq!(run(&["show", "area", dir.to_str().unwrap()]).1, format!("{code}:6-8  method area\n6:    fn area(&self) -> f64 {{\n7:        3.14 * self.r * self.r\n8:    }}\n"));
        assert_eq!(run(&["lines", &notes, "2", "3"]), (0, "two\nthree\n".to_owned()));
        assert_eq!(run(&["lines", "-n", &notes, "1-2", "4-"]), (0, "1:one\n2:two\n--\n4:four\n5:five\n".to_owned()));
        assert_eq!(run(&["lines", "--last", "2", &notes]), (0, "four\nfive".to_owned()));
        assert_eq!(run(&["lines", &notes, "2", &code, "11-11"]), (0, format!("==> {notes} <==\ntwo\n\n==> {code} <==\nfn main() {{\n")));
        assert_eq!(run(&["lines", &notes, "9"]).0, 1);
        // The last lines, found from the end: with a line break at the end and without, CRLF, a lone CR, more
        // lines asked for than there are, and a file longer than one block read.
        let ends = dir.join("ends.txt");
        let last = |text: &[u8], count: &str| {
            std::fs::write(&ends, text).unwrap();
            run(&["lines", "--last", count, ends.to_str().unwrap()])
        };
        assert_eq!(last(b"one\ntwo\nthree\n", "2"), (0, "two\nthree\n".to_owned()));
        assert_eq!(last(b"one\r\ntwo\r\nthree\r\n", "2"), (0, "two\r\nthree\r\n".to_owned()));
        assert_eq!(last(b"one\rtwo\rthree", "2"), (0, "two\rthree".to_owned()));
        assert_eq!(last(b"one\ntwo\n", "9"), (0, "one\ntwo\n".to_owned()));
        assert_eq!(last(b"one\ntwo\n", "0").0, 1);
        assert_eq!(last(b"", "3").0, 1);
        let long: String = (1..=30_000).map(|n| format!("line {n}\r\n")).collect();
        let tail: String = (29_001..=30_000).map(|n| format!("line {n}\r\n")).collect();
        assert_eq!(last(long.as_bytes(), "1000"), (0, tail));
        assert_eq!(run(&["lines", "-n", "--last", "2", ends.to_str().unwrap()]), (0, "29999:line 29999\r\n30000:line 30000\r\n".to_owned()));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
