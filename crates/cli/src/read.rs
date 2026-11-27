//! Reading that saves an agent a round trip.
//!
//! A program that works on code through a model pays for each command with a
//! turn of the model, which costs it seconds where the command costs
//! milliseconds. Three habits make turns that one richer command can spare:
//! reading a file a screenful at a time to find where something is (`lw
//! outline` says where everything is), finding a name and then reading the
//! lines around it (`lw show` prints the whole definition), and reading
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

/// The definitions of `name` in one file's text: (kind, first line, last line), in order.
fn definitions_of(path: &str, text: &str, name: &str) -> Vec<(String, u64, u64)> {
    let places: Vec<_> = linework_project::places(Path::new(path), text, name).into_iter().filter(|place| place.definition.is_some()).collect();
    let offsets: Vec<usize> = places.iter().map(|place| place.offset).collect();
    let mut found: Vec<(String, u64, u64)> = Vec::new();
    for (place, scope) in places.iter().zip(linework_project::scopes(Path::new(path), text, &offsets)) {
        // The definition around the name where the outline knows its extent, else the line it is on.
        let (first, last) = match scope {
            Some(scope) if scope.name == name => (scope.line, scope.end_line),
            _ => (place.line, place.line),
        };
        if !found.iter().any(|known| known.1 == first && known.2 == last) {
            found.push((place.definition.clone().unwrap_or_default(), first, last));
        }
    }
    found
}

/// One file's definitions of `name`, written whole with their line numbers. Returns how many.
fn show_file(out: &mut Vec<u8>, json: bool, shown: &str, text: &str, name: &str) -> u64 {
    let found = definitions_of(shown, text, name);
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

/// `lw show NAME PATH`: each definition of a name, whole.
pub fn show(s: &Settings, name: &str, target: &str, walk: linework_search::walk::Options<'_>, mut out: &mut dyn Write) -> Result<i32, String> {
    use linework_search::sweep;
    let mut budget = report::Budget::new(s.budget);
    let mut shown = 0u64;
    if Path::new(target).is_dir() {
        // Only files holding the name as a whole word are parsed.
        let query = sweep::Query::new(name, true, false, true)?;
        let folder = target.trim_end_matches(['/', '\\']);
        let json = s.json;
        let format = |path: &str, bytes: &[u8], _: &[sweep::LineHit], out: &mut Vec<u8>| -> u64 {
            let Ok(text) = std::str::from_utf8(bytes) else { return 0 };
            let mut part = Vec::new();
            let count = show_file(&mut part, json, &format!("{folder}/{path}"), text, name);
            out.extend_from_slice(&part);
            count
        };
        let gathered = sweep::tree_with(Path::new(target), walk, &query, &format).map_err(io)?;
        let mut first = true;
        for (_, part) in sweep::in_order(&gathered) {
            if !first && !json {
                budget.write(&mut out, b"--\n").map_err(io)?;
            }
            first = false;
            shown += 1;
            if !budget.write(&mut out, part).map_err(io)? {
                break;
            }
        }
    } else {
        let text = code(target)?;
        let mut part = Vec::new();
        shown = show_file(&mut part, s.json, target, &text, name);
        budget.write(&mut out, &part).map_err(io)?;
    }
    if budget.cut {
        crate::note(format!("lw: output cut at {} bytes (--budget)", budget.written));
    }
    Ok(i32::from(shown == 0))
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
        crate::counted(&doc).map_err(io)?;
        let total = doc.line_count();
        // A text that ends with a line break has no line after it.
        let total = if total > 0 && doc.line_start(total - 1) == doc.len() { total - 1 } else { total };
        let mut runs: Vec<Span> = runs.clone();
        if let Some(last) = s.last {
            runs.push((total.saturating_sub(last.saturating_sub(1)).max(1), None));
        }
        if runs.is_empty() {
            runs.push((1, None));
        }
        if several {
            let head = format!("{}==> {path} <==\n", if index > 0 { "\n" } else { "" });
            budget.write(&mut out, head.as_bytes()).map_err(io)?;
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
                let mut from = doc.line_start(first - 1);
                let to = if last < doc.line_count() { doc.line_start(last) } else { doc.len() };
                while from < to {
                    let n = (to - from).min(block.len() as u64) as usize;
                    doc.read_into(from, &mut block[..n]).map_err(io)?;
                    if !budget.write(&mut out, &block[..n]).map_err(io)? {
                        break;
                    }
                    from += n as u64;
                }
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
        assert_eq!(run(&["show", "nowhere", &code]).0, 1);
        assert_eq!(run(&["lines", &notes, "2", "3"]), (0, "two\nthree\n".to_owned()));
        assert_eq!(run(&["lines", "-n", &notes, "1-2", "4-"]), (0, "1:one\n2:two\n--\n4:four\n5:five\n".to_owned()));
        assert_eq!(run(&["lines", "--last", "2", &notes]), (0, "four\nfive".to_owned()));
        assert_eq!(run(&["lines", &notes, "2", &code, "11-11"]), (0, format!("==> {notes} <==\ntwo\n\n==> {code} <==\nfn main() {{\n")));
        assert_eq!(run(&["lines", &notes, "9"]).0, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
