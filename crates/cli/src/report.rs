//! Output beyond the plain `path:line:text`: one JSON object per line for
//! programs, a heading per file and the enclosing definition for people, a
//! limit on how much is printed, and the totals of a search.
use linework_project::Scope;
use linework_search::sweep::{clip, first_match, LineHit, Query};
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
    /// The line, or of a very long line the part read around its match.
    pub text: &'a [u8],
    /// Bytes of the line before `text`, and whether the line goes on past it.
    pub offset: u64,
    pub cut: bool,
    pub scope: Option<&'a Scope>,
}

impl<'a> Line<'a> {
    /// A whole line.
    pub fn whole(number: u64, start: u64, text: &'a [u8], scope: Option<&'a Scope>) -> Self {
        Self { number, start, text, offset: 0, cut: false, scope }
    }
    /// A line a single-file search found, which may be part of a very long line.
    pub fn found(f: &'a linework_storage::FoundLine, scope: Option<&'a Scope>) -> Self {
        Self { number: f.line + 1, start: f.start, text: &f.text, offset: f.text_start - f.start, cut: f.cut, scope }
    }
}

/// Appends a line's text, or of a long line the part around `at` (where its
/// first match starts in `text`), saying how many bytes were left out.
/// `offset` bytes of the line come before `text`; with `cut`, more follow it.
pub fn write_line(out: &mut Vec<u8>, text: &[u8], at: usize, offset: u64, cut: bool) {
    let shown = clip(text, at);
    let before = offset + shown.start as u64;
    if before != 0 {
        out.extend_from_slice(format!("[{before} bytes omitted] ").as_bytes());
    }
    out.extend_from_slice(&text[shown.clone()]);
    if cut {
        out.extend_from_slice(b" [rest of line omitted]");
    } else if shown.end != text.len() {
        out.extend_from_slice(format!(" [{} bytes omitted]", text.len() - shown.end).as_bytes());
    }
}

fn json_line(out: &mut Vec<u8>, query: &Query, path: &str, line: &Line) {
    let text = bare(line.text);
    let found = matches(query, text);
    let first = found.first().map_or(0, |m| m[0]);
    // A long line is given in part: "text" starts "omitted_before" bytes into
    // the line, and "matches" are positions in "text".
    let shown = clip(text, first);
    let inside: Vec<[usize; 2]> =
        found.iter().filter(|m| m[0] >= shown.start && m[1] <= shown.end).map(|m| [m[0] - shown.start, m[1] - shown.start]).collect();
    let mut value = json!({
        "path": path,
        "line": line.number,
        // Characters from the line's start: unknown when only part of a very long line was read.
        "column": if line.offset == 0 { json!(String::from_utf8_lossy(&text[..first]).chars().count() + 1) } else { json!(null) },
        "byte": line.start + line.offset + first as u64,
        "text": String::from_utf8_lossy(&text[shown.clone()]),
        "matches": inside,
    });
    if shown != (0..text.len()) || line.offset != 0 || line.cut {
        value["omitted_before"] = json!(line.offset + shown.start as u64);
        // Null when the line goes on past what was read, so its length is not known.
        value["omitted_after"] = if line.cut { json!(null) } else { json!(text.len() - shown.end) };
    }
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
        let text = bare(line.text);
        write_line(out, text, first_match(query, text), line.offset, line.cut);
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
pub fn folder_file(out: &mut Vec<u8>, style: Style, query: &Query, shown: &str, path: &str, bytes: &[u8], hits: &[LineHit]) -> u64 {
    let starts: Vec<usize> = hits.iter().map(|h| h.start).collect();
    let scopes = scopes(style.scope, path, bytes, &starts);
    let lines: Vec<Line> = hits
        .iter()
        .zip(&scopes)
        .map(|(hit, scope)| Line::whole(hit.number, hit.start as u64, &bytes[hit.start..hit.end], scope.as_ref()))
        .collect();
    file(out, style, query, &format!("{shown}/{path}"), &lines);
    hits.len() as u64
}

/// The lines that hold places in code, once each, in order.
pub fn place_lines(bytes: &[u8], places: &[linework_project::Place]) -> Vec<LineHit> {
    let mut lines: Vec<LineHit> = Vec::new();
    for place in places {
        if lines.last().is_some_and(|l| l.number == place.line) {
            continue;
        }
        let start = memchr::memrchr(b'\n', &bytes[..place.offset]).map_or(0, |at| at + 1);
        let end = memchr::memchr(b'\n', &bytes[place.offset..]).map_or(bytes.len(), |at| place.offset + at);
        lines.push(LineHit { number: place.line, start, end });
    }
    lines
}

/// Matching lines in the plain form, `path:line:text`, a long line in part around `query`'s first match.
pub fn plain(out: &mut Vec<u8>, query: &Query, path: &str, bytes: &[u8], hits: &[LineHit]) -> u64 {
    for hit in hits {
        out.extend_from_slice(format!("{path}:{}:", hit.number).as_bytes());
        let line = &bytes[hit.start..hit.end];
        write_line(out, line, first_match(query, line), 0, false);
        out.push(b'\n');
    }
    hits.len() as u64
}

/// One numbered line in the plain form, `line:text`, a long line in part around `at`.
pub fn numbered(out: &mut Vec<u8>, number: u64, line: &[u8], at: usize) {
    out.extend_from_slice(format!("{number}:").as_bytes());
    write_line(out, line, at, 0, false);
    out.push(b'\n');
}

/// A line a single-file search found, in the plain form `line:text`.
pub fn numbered_found(out: &mut Vec<u8>, f: &linework_storage::FoundLine) {
    out.extend_from_slice(format!("{}:", f.line + 1).as_bytes());
    let at = f.first.start.saturating_sub(f.text_start) as usize;
    write_line(out, &f.text, at, f.text_start - f.start, f.cut);
    out.push(b'\n');
}

/// Conditions beyond the main pattern: other patterns a file must or must not
/// hold anywhere, and patterns that must appear within some lines of a match.
#[derive(Default)]
pub struct Filters {
    pub near: Vec<(Query, u64)>,
    pub and: Vec<Query>,
    pub not: Vec<Query>,
}
impl Filters {
    pub fn is_empty(&self) -> bool {
        self.near.is_empty() && self.and.is_empty() && self.not.is_empty()
    }
    /// Whether the whole file passes --and and --not.
    pub fn file_passes(&self, bytes: &[u8]) -> bool {
        self.and.iter().all(|q| q.within(bytes)) && !self.not.iter().any(|q| q.within(bytes))
    }
    /// The lines that have each --near pattern within its number of lines, before or after.
    pub fn near(&self, bytes: &[u8], hits: &[LineHit]) -> Vec<LineHit> {
        let mut kept = hits.to_vec();
        for (query, reach) in &self.near {
            let mut numbers = Vec::new();
            linework_search::sweep::matching_lines(query, bytes, true, |hit| numbers.push(hit.number));
            kept.retain(|hit| close(&numbers, hit.number, *reach));
        }
        kept
    }
}

/// Whether any of the ascending `numbers` lies within `reach` of `line`.
pub fn close(numbers: &[u64], line: u64, reach: u64) -> bool {
    let from = numbers.partition_point(|&n| n + reach < line);
    numbers.get(from).is_some_and(|&n| n <= line + reach)
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
