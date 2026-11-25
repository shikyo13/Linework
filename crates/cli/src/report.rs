//! Output beyond the plain `path:line:text`: one JSON object per line for
//! programs, a heading per file and the enclosing definition for people, a
//! limit on how much is printed, and the totals of a search.
use linework_project::Scope;
use linework_search::sweep::{LineHit, Query};
use serde_json::json;
use std::{io, path::Path};

/// How matching lines are shown.
#[derive(Clone, Copy)]
pub struct Style {
    pub json: bool,
    pub heading: bool,
    pub scope: bool,
}

/// A line's text without the CR of a CRLF break.
fn bare(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Every match inside one line, as byte ranges within it.
fn matches(query: &Query, line: &[u8]) -> Vec<[usize; 2]> {
    let mut found = Vec::new();
    let mut at = 0;
    while at <= line.len() && found.len() < 1000 {
        let Some((start, end)) = query.next(line, at) else { break };
        if start >= line.len() && !found.is_empty() {
            break;
        }
        found.push([start, end.min(line.len())]);
        at = if end > start { end } else { start + 1 };
    }
    found
}

/// One matching line, wherever it was found.
pub struct Line<'a> {
    /// Counted from 1.
    pub number: u64,
    /// Where the line starts in its file.
    pub start: u64,
    pub text: &'a [u8],
    pub scope: Option<&'a Scope>,
}

fn json_line(out: &mut Vec<u8>, query: &Query, path: &str, line: &Line) {
    let text = bare(line.text);
    let found = matches(query, text);
    let first = found.first().map_or(0, |m| m[0]);
    let mut value = json!({
        "path": path,
        "line": line.number,
        "column": String::from_utf8_lossy(&text[..first]).chars().count() + 1,
        "byte": line.start + first as u64,
        "text": String::from_utf8_lossy(text),
        "matches": found,
    });
    if let Some(scope) = line.scope {
        value["scope"] = json!({"name": scope.name, "kind": scope.kind, "line": scope.line, "end_line": scope.end_line});
    }
    serde_json::to_writer(&mut *out, &value).expect("writing to memory");
    out.push(b'\n');
}

/// One file's matching lines in the chosen style.
pub fn file(out: &mut Vec<u8>, style: Style, query: &Query, path: &str, lines: &[Line]) {
    if style.json {
        for line in lines {
            json_line(out, query, path, line);
        }
        return;
    }
    out.extend_from_slice(path.as_bytes());
    out.push(b'\n');
    let mut shown: Option<&Scope> = None;
    for line in lines {
        if style.scope && line.scope.is_some() && line.scope != shown {
            let scope = line.scope.unwrap();
            let span = if scope.line == scope.end_line { format!("line {}", scope.line) } else { format!("lines {}-{}", scope.line, scope.end_line) };
            out.extend_from_slice(format!("  {} {} ({span})\n", scope.kind, scope.name).as_bytes());
        }
        shown = line.scope;
        out.extend_from_slice(format!("{}:", line.number).as_bytes());
        out.extend_from_slice(bare(line.text));
        out.push(b'\n');
    }
    out.push(b'\n');
}

/// The definitions around `starts` in a file's bytes, when asked for and the
/// file is text the outline can read; otherwise none.
pub fn scopes(wanted: bool, path: &str, bytes: &[u8], starts: &[usize]) -> Vec<Option<Scope>> {
    match std::str::from_utf8(bytes) {
        Ok(text) if wanted => linework_project::scopes(Path::new(path), text, starts),
        _ => vec![None; starts.len()],
    }
}

/// One file of a folder search: its numbered matching lines, formatted.
pub fn folder_file(out: &mut Vec<u8>, style: Style, query: &Query, shown: &str, path: &str, bytes: &[u8], hits: &[LineHit]) {
    let starts: Vec<usize> = hits.iter().map(|h| h.start).collect();
    let scopes = scopes(style.scope, path, bytes, &starts);
    let lines: Vec<Line> = hits
        .iter()
        .zip(&scopes)
        .map(|(hit, scope)| Line { number: hit.number, start: hit.start as u64, text: &bytes[hit.start..hit.end], scope: scope.as_ref() })
        .collect();
    file(out, style, query, &format!("{shown}/{path}"), &lines);
}

/// A file's match count as one JSON object.
pub fn json_count(out: &mut Vec<u8>, path: &str, count: u64) {
    serde_json::to_writer(&mut *out, &json!({"path": path, "count": count})).expect("writing to memory");
    out.push(b'\n');
}

/// Writes output up to a number of bytes, cutting only at a line break.
pub struct Budget {
    left: Option<usize>,
    pub cut: bool,
    pub written: usize,
}
impl Budget {
    pub fn new(most: Option<usize>) -> Self {
        Self { left: most, cut: false, written: 0 }
    }
    /// False once the budget is spent.
    pub fn write(&mut self, out: &mut impl io::Write, bytes: &[u8]) -> io::Result<bool> {
        if self.cut {
            return Ok(false);
        }
        let take = match self.left {
            Some(left) if bytes.len() > left => {
                self.cut = true;
                memchr::memrchr(b'\n', &bytes[..left]).map_or(0, |at| at + 1)
            }
            _ => bytes.len(),
        };
        out.write_all(&bytes[..take])?;
        self.written += take;
        if let Some(left) = &mut self.left {
            *left -= take;
        }
        Ok(!self.cut)
    }
}

/// What a search did, for the end of JSON output and for --stats.
pub struct Totals {
    pub files_searched: u64,
    pub files_matched: u64,
    /// Matching lines, when known; a count of a single file counts matches instead.
    pub lines: Option<u64>,
    pub matches: Option<u64>,
    pub bytes_searched: u64,
    pub seconds: f64,
}
impl Totals {
    pub fn json(&self, out: &mut impl io::Write, budget: &Budget) -> io::Result<()> {
        let value = json!({"summary": {
            "files_searched": self.files_searched,
            "files_matched": self.files_matched,
            "lines": self.lines,
            "matches": self.matches,
            "bytes_searched": self.bytes_searched,
            "output_bytes": budget.written,
            "cut_short": budget.cut,
            "seconds": (self.seconds * 1000.0).round() / 1000.0,
        }});
        serde_json::to_writer(&mut *out, &value)?;
        out.write_all(b"\n")
    }
    pub fn text(&self, budget: &Budget) -> String {
        let cut = if budget.cut { format!("; output cut at {} bytes", budget.written) } else { String::new() };
        let found = match (self.lines, self.matches) {
            (Some(lines), _) => format!("{lines} matching lines"),
            (None, Some(matches)) => format!("{matches} matches"),
            _ => String::from("nothing counted"),
        };
        format!(
            "{found} in {} of {} files ({:.1} MB searched) in {:.3} s{cut}",
            self.files_matched,
            self.files_searched,
            self.bytes_searched as f64 / 1e6,
            self.seconds
        )
    }
}
