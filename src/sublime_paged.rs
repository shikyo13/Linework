//! A package command owns a cheap snapshot of the piece tree on the IPC worker.
//! The UI can keep editing. Only an unchanged originating tab can accept the finished tree.
use linework_editor::{Editor, sci::SCI_COLOURISE};
use linework_storage::{ChangeMap, Document, SearchOptions};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    io,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

const TEXT_BUDGET: u64 = 8 * 1024 * 1024;
const MATCH_LIMIT: usize = 131_072;
fn invalid(s: impl ToString) -> io::Error {
    io::Error::other(s.to_string())
}
struct Scopes {
    base: u64,
    end: u64,
    runs: Vec<(usize, u8)>,
    names: Vec<String>,
}
thread_local! {static LEXER:RefCell<Option<Editor>>=const{RefCell::new(None)};}
pub(super) fn close_lexer() {
    if let Some(editor) = LEXER.take() {
        crate::win::destroy(editor.hwnd());
    }
}
pub(super) struct Transaction {
    doc: Document,
    mapping: ChangeMap,
    inserted: u64,
    write_error: Option<String>,
    scopes: Option<Scopes>,
    lang: String,
    cancelled: Arc<AtomicBool>,
}
pub(super) struct Commit {
    pub doc: Document,
    pub spans: Vec<(u64, u64, u64)>,
}
impl Transaction {
    pub fn new(mut doc: Document, lang: String, cancelled: Arc<AtomicBool>) -> Self {
        let mapping = ChangeMap::new(doc.len());
        doc.begin_undo_group();
        Self {
            doc,
            mapping,
            inserted: 0,
            write_error: None,
            scopes: None,
            lang,
            cancelled,
        }
    }
    fn index(&self, until: u64) -> io::Result<()> {
        while self.doc.indexed_bytes() < until {
            if self.cancelled.load(Ordering::Relaxed) {
                return Err(invalid("Package request cancelled"));
            }
            if let Some(e) = self.doc.index_error() {
                return Err(invalid(e));
            }
            thread::sleep(Duration::from_millis(3));
        }
        Ok(())
    }
    pub fn prepare(&self, request: &mut Value) -> io::Result<()> {
        let doc = &mut request["doc"];
        doc["size"] = json!(self.doc.char_len_cancel(&self.cancelled)?);
        for key in ["regions", "visible"] {
            fn convert(d: &Document, v: &mut Value) -> io::Result<()> {
                if let Some(arr) = v.as_array_mut() {
                    for value in arr {
                        convert(d, value)?;
                    }
                } else if let Some(byte) = v.as_u64() {
                    *v = json!(d.byte_to_char(byte)?);
                } else {
                    return Err(invalid("Invalid editor position"));
                }
                Ok(())
            }
            convert(&self.doc, &mut doc[key])?;
        }
        Ok(())
    }
    pub fn finish(mut self, reply: &mut super::Reply) -> io::Result<Option<Commit>> {
        if let Some(error) = self.write_error.take() {
            return Err(invalid(error));
        }
        // Several prior cursors may coincide after an erase. Return ordered,
        // disjoint regions that the editor can install before publishing any edit.
        reply.regions.sort_by_key(|&(a, b)| (a.min(b), a.max(b)));
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (a, b) in reply.regions.drain(..) {
            if let Some((x, y)) = merged.last_mut() {
                let (start, end) = ((*x).min(*y), (*x).max(*y));
                let (lo, hi) = (a.min(b), a.max(b));
                if (lo, hi) == (start, end) {
                    continue;
                }
                if lo < end || (start == end && start == lo) {
                    let combined_end = end.max(hi);
                    (*x, *y) = if *x > *y {
                        (combined_end, start)
                    } else {
                        (start, combined_end)
                    };
                    continue;
                }
            }
            merged.push((a, b));
        }
        if merged.len() > 65_536 {
            return Err(invalid(
                "The extension selection list exceeds 65,536 ranges",
            ));
        }
        reply.regions = merged;
        for (a, b) in &mut reply.regions {
            *a = self.doc.char_to_byte(*a as u64)? as usize;
            *b = self.doc.char_to_byte(*b as u64)? as usize;
        }
        if let Some(p) = reply.reveal.as_mut() {
            *p = self.doc.char_to_byte(*p as u64)? as usize;
        }
        self.doc.end_undo_group();
        if !self.doc.can_undo() {
            return Ok(None);
        }
        Ok(Some(Commit {
            doc: self.doc,
            spans: self.mapping.changes(),
        }))
    }
    fn bounds(&self, first: u64, last: u64) -> io::Result<Range<u64>> {
        if first > last {
            return Err(invalid("Invalid Unicode region"));
        }
        Ok(self.doc.char_to_byte(first)?..self.doc.char_to_byte(last)?)
    }
    fn trim_line(&self, start: u64, end: u64) -> io::Result<u64> {
        let tail = end.saturating_sub(2).max(start);
        let bytes = self.doc.read(tail, (end - tail) as usize)?;
        Ok(end
            - if bytes.ends_with(b"\r\n") {
                2
            } else if bytes.ends_with(b"\n") || bytes.ends_with(b"\r") {
                1
            } else {
                0
            })
    }
    fn replace(&mut self, range: Range<u64>, bytes: &[u8]) -> io::Result<()> {
        let removed = range.end - range.start;
        self.doc.prepare_edit(|_, _| {
            if self.cancelled.load(Ordering::Relaxed) {
                Err(invalid("Package request cancelled"))
            } else {
                Ok(())
            }
        })?;
        self.doc.replace(range.start, removed, bytes)?;
        self.mapping
            .replace(range.start, removed, bytes.len() as u64)?;
        self.inserted += bytes.len() as u64;
        if self.inserted >= TEXT_BUDGET {
            self.doc.spill_inserted()?;
            self.inserted = 0;
        }
        self.scopes = None;
        Ok(())
    }
    fn word(&self, first: u64, last: u64) -> io::Result<Value> {
        let mut range = self.bounds(first, last)?;
        let word_char = |ch: char| ch.is_alphanumeric() || ch == '_';
        // Expand through a long identifier using bounded UTF-8 windows in either direction.
        while range.start > 0 {
            if self.cancelled.load(Ordering::Relaxed) {
                return Err(invalid("Package request cancelled"));
            }
            let count = range.start.min(65536) as usize;
            let bytes = self.doc.read(range.start - count as u64, count)?;
            let start = bytes
                .iter()
                .position(|c| c & 0xc0 != 0x80)
                .unwrap_or(bytes.len());
            let window = std::str::from_utf8(&bytes[start..]).map_err(invalid)?;
            let n: usize = window
                .chars()
                .rev()
                .take_while(|c| word_char(*c))
                .map(char::len_utf8)
                .sum();
            range.start -= n as u64;
            if n < window.len() {
                break;
            }
        }
        while range.end < self.doc.len() {
            if self.cancelled.load(Ordering::Relaxed) {
                return Err(invalid("Package request cancelled"));
            }
            let count = (self.doc.len() - range.end).min(65536) as usize;
            let bytes = self.doc.read(range.end, count)?;
            let window = match std::str::from_utf8(&bytes) {
                Ok(s) => s,
                Err(e) if e.error_len().is_none() => {
                    std::str::from_utf8(&bytes[..e.valid_up_to()]).map_err(invalid)?
                }
                Err(e) => return Err(invalid(e)),
            };
            let n: usize = window
                .chars()
                .take_while(|c| word_char(*c))
                .map(char::len_utf8)
                .sum();
            range.end += n as u64;
            if n < window.len() {
                break;
            }
        }
        Ok(json!([
            self.doc.byte_to_char(range.start)?,
            self.doc.byte_to_char(range.end)?
        ]))
    }
    fn scope(&mut self, point: u64) -> io::Result<Value> {
        let byte = self.doc.char_to_byte(point)?;
        if byte == self.doc.len() {
            return Ok(json!([point, point, ""]));
        }
        if !self
            .scopes
            .as_ref()
            .is_some_and(|s| s.base <= byte && byte < s.end)
        {
            self.index(byte)?;
            let line = self.doc.line_start(self.doc.line_of(byte));
            let approx = if byte - line < 65536 {
                self.doc
                    .line_start(self.doc.line_of(byte.saturating_sub(32768)))
                    .max(byte.saturating_sub(65536))
            } else {
                byte.saturating_sub(65536)
            };
            let mut base = approx;
            while base < byte && self.doc.read(base, 1)?[0] & 0xc0 == 0x80 {
                base += 1;
            }
            let mut end = (byte + 65536).min(self.doc.len());
            while end < self.doc.len() && self.doc.read(end, 1)?[0] & 0xc0 == 0x80 {
                end += 1;
            }
            let content = self.doc.read(base, (end - base) as usize)?;
            let (runs, names) = LEXER.with_borrow_mut(|slot| -> io::Result<_> {
                if slot.is_none() {
                    *slot = Some(Editor::create(-3, 0).map_err(invalid)?);
                }
                let editor = slot.as_ref().unwrap();
                unsafe {
                    use windows_sys::Win32::UI::WindowsAndMessaging::*;
                    let mut message = std::mem::zeroed();
                    while PeekMessageW(&mut message, editor.hwnd() as _, 0, 0, PM_REMOVE) != 0 {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                if !editor.set_language(&self.lang) || editor.is_custom_grammar() {
                    return Err(invalid(
                        "A paged scope query requires a built-in syntax lexer",
                    ));
                }
                editor.set_bytes(&content);
                editor.send(SCI_COLOURISE, 0, -1);
                Ok(editor.package_scope_runs())
            })?;
            self.scopes = Some(Scopes {
                base,
                end,
                runs,
                names,
            });
        }
        let s = self.scopes.as_ref().unwrap();
        let i = s
            .runs
            .partition_point(|(p, _)| s.base + (*p as u64) <= byte)
            .saturating_sub(1);
        let (at, style) = s.runs.get(i).copied().unwrap_or((0, 0));
        let end = s.runs.get(i + 1).map_or(s.end, |(p, _)| s.base + *p as u64);
        Ok(json!([
            self.doc.byte_to_char(s.base + at as u64)?,
            self.doc.byte_to_char(end)?,
            s.names.get(style as usize).map_or("", String::as_str)
        ]))
    }
    pub fn call(&mut self, op: &Value) -> io::Result<Value> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(invalid("Package request cancelled"));
        }
        let args = &op["args"];
        let num = |i: usize| {
            args[i]
                .as_u64()
                .ok_or_else(|| invalid("Expected an unsigned text position"))
        };
        let text = |i: usize| args[i].as_str().ok_or_else(|| invalid("Expected text"));
        match op["method"].as_str().unwrap_or("") {
            "read" => {
                let r = self.bounds(num(0)?, num(1)?)?;
                if r.end - r.start > TEXT_BUDGET {
                    return Err(invalid(
                        "This package requested more than 8 MiB in a single Python string. Read the view in smaller regions.",
                    ));
                }
                Ok(json!(
                    String::from_utf8(self.doc.read(r.start, (r.end - r.start) as usize)?)
                        .map_err(invalid)?
                ))
            }
            "replace" => {
                let r = self.bounds(num(0)?, num(1)?)?;
                let bytes = text(2)?.as_bytes();
                if bytes.len() as u64 > TEXT_BUDGET {
                    return Err(invalid("A single package edit exceeds 8 MiB"));
                }
                if let Err(e) = self.replace(r, bytes) {
                    self.write_error = Some(e.to_string());
                    return Err(e);
                }
                Ok(Value::Null)
            }
            "rowcol" => {
                let p = num(0)?;
                let at = self.doc.char_to_byte(p)?;
                self.index(at)?;
                let row = self.doc.line_of(at);
                Ok(json!([
                    row,
                    p - self.doc.byte_to_char(self.doc.line_start(row))?
                ]))
            }
            "line" => {
                let p = num(0)?;
                let byte = self.doc.char_to_byte(p)?;
                self.index(self.doc.len())?;
                let row = self.doc.line_of(byte);
                let begin = self.doc.line_start(row);
                let end = self.doc.line_start(row + 1);
                Ok(json!([
                    self.doc.byte_to_char(begin)?,
                    self.doc.byte_to_char(self.trim_line(begin, end)?)?,
                    self.doc.byte_to_char(end)?
                ]))
            }
            "lines" => {
                let a = self.doc.char_to_byte(num(0)?)?;
                let b = self.doc.char_to_byte(num(1)?)?;
                self.index(self.doc.len())?;
                let first = self.doc.line_of(a);
                let last = self.doc.line_of(b);
                if last.saturating_sub(first) >= MATCH_LIMIT as u64 {
                    return Err(invalid("Requested line count exceeds 131,072 regions"));
                }
                let mut ranges = Vec::new();
                for row in first..=last {
                    let begin = self.doc.line_start(row);
                    let end = self.doc.line_start(row + 1);
                    ranges.push((
                        self.doc.byte_to_char(begin)?,
                        self.doc.byte_to_char(self.trim_line(begin, end)?)?,
                    ));
                }
                Ok(json!(ranges))
            }
            "point" => {
                self.index(self.doc.len())?;
                let row = num(0)?;
                let col = num(1)?;
                let begin = self.doc.line_start(row);
                let end = self.doc.line_start(row.saturating_add(1));
                let limit = if args[2].as_bool().unwrap_or(true) {
                    self.doc.byte_to_char(self.trim_line(begin, end)?)?
                } else {
                    self.doc.char_len_cancel(&self.cancelled)?
                };
                Ok(json!(
                    (self.doc.byte_to_char(begin)?.saturating_add(col)).min(limit)
                ))
            }
            "scope" => self.scope(num(0)?),
            "word" => self.word(num(0)?, num(1)?),
            "find" | "find_all" => {
                let pattern = text(0)?;
                let from = self.doc.char_to_byte(num(1)?)?;
                let flags = num(2)?;
                let options = SearchOptions {
                    regex: flags & 1 == 0,
                    case_sensitive: flags & 2 == 0,
                    wrap: false,
                    ..Default::default()
                };
                if op["method"] == "find" {
                    if let Some(r) =
                        self.doc
                            .find_cancel(pattern, from, options, &self.cancelled)?
                    {
                        return Ok(json!([
                            self.doc.byte_to_char(r.start)?,
                            self.doc.byte_to_char(r.end)?
                        ]));
                    }
                    return Ok(json!([-1, -1]));
                }
                let ranges =
                    self.doc
                        .find_all_bounded(pattern, options, MATCH_LIMIT, &self.cancelled)?;
                let points = ranges
                    .into_iter()
                    .map(|r| {
                        Ok((
                            self.doc.byte_to_char(r.start)?,
                            self.doc.byte_to_char(r.end)?,
                        ))
                    })
                    .collect::<io::Result<Vec<_>>>()?;
                Ok(json!(points))
            }
            _ => Err(invalid("Unknown paged view request")),
        }
    }
}
