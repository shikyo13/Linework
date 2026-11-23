use crate::Document;
use std::{
    io,
    sync::atomic::{AtomicBool, Ordering},
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fold {
    pub header: u64,
    pub start: u64,
    pub end: u64,
}
/// Prefix sums convert original byte positions to the collapsed document and
/// back. At a seam, a right-biased caret lands on the next visible line.
#[derive(Clone, Default, Debug)]
pub struct FoldMap {
    entries: Vec<(Fold, u64)>,
    removed: u64,
}
impl FoldMap {
    pub fn new(mut folds: Vec<Fold>) -> Self {
        folds.sort_unstable_by_key(|f| (f.start, std::cmp::Reverse(f.end)));
        let mut merged: Vec<Fold> = Vec::new();
        for f in folds.into_iter().filter(|f| f.start < f.end) {
            if let Some(last) = merged.last_mut().filter(|x| f.start <= x.end) {
                last.end = last.end.max(f.end);
            } else {
                merged.push(f);
            }
        }
        let mut map = Self::default();
        for f in merged {
            map.entries.push((f, map.removed));
            map.removed += f.end - f.start;
        }
        map
    }
    pub fn headers(&self, start: u64, end: u64) -> impl Iterator<Item = Fold> + '_ {
        let first = self.entries.partition_point(|(f, _)| f.header < start);
        self.entries[first..]
            .iter()
            .take_while(move |(f, _)| f.header <= end)
            .map(|(f, _)| *f)
    }
    pub fn removed(&self) -> u64 {
        self.removed
    }
    pub fn visible(&self, p: u64) -> u64 {
        let i = self.entries.partition_point(|(f, _)| f.start <= p);
        if i == 0 {
            return p;
        }
        let (f, before) = self.entries[i - 1];
        p - before - (p.min(f.end) - f.start)
    }
    pub fn physical(&self, p: u64) -> u64 {
        let i = self
            .entries
            .partition_point(|(f, before)| f.start - before <= p);
        if i == 0 {
            return p;
        }
        let (f, before) = self.entries[i - 1];
        p + before + f.end - f.start
    }
    pub fn physical_end(&self, p: u64) -> u64 {
        if p == 0 {
            0
        } else {
            self.physical(p - 1) + 1
        }
    }
    pub fn hidden(&self, p: u64) -> Option<Fold> {
        let i = self.entries.partition_point(|(f, _)| f.start <= p);
        i.checked_sub(1)
            .map(|j| self.entries[j].0)
            .filter(|f| p < f.end)
    }
    pub fn project(&self, doc: &Document) -> io::Result<Document> {
        let mut projected = doc.clone();
        let edits = self
            .entries
            .iter()
            .map(|(f, _)| (f.start, f.end - f.start, Vec::new()))
            .collect::<Vec<_>>();
        projected.replace_many(&edits)?;
        Ok(projected)
    }
}
enum Mode {
    Braces,
    Indentation,
    Markdown,
    Markup,
}
struct Parser {
    mode: Mode,
    openings: Vec<(usize, u64, u64)>,
    regions: Vec<Fold>,
    target: Option<u64>,
    line: u64,
    first: Vec<u8>,
    quote: u8,
    escaped: bool,
    block: bool,
    comment: bool,
    previous: u8,
    indent: usize,
    leading: bool,
    nonblank: bool,
    prior: Option<(usize, u64, u64)>,
}
impl Parser {
    fn region(&mut self, header: u64, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let f = Fold { header, start, end };
        if let Some(p) = self.target {
            if header <= p
                && p < end
                && self
                    .regions
                    .first()
                    .is_none_or(|old| header > old.header || header == old.header && end > old.end)
            {
                self.regions.clear();
                self.regions.push(f);
            }
        } else if self.openings.is_empty() {
            self.regions.push(f);
        }
    }
    fn pop(&mut self, end: u64) {
        if let Some((_, header, start)) = self.openings.pop() {
            self.region(header, start, end);
        }
    }
    fn data(&mut self, b: &[u8]) {
        if self.first.len() < 1024 {
            self.first
                .extend_from_slice(&b[..(1024 - self.first.len()).min(b.len())]);
        }
        if self.leading {
            for &c in b {
                match c {
                    b' ' => self.indent += 1,
                    b'\t' => self.indent = (self.indent / 4 + 1) * 4,
                    _ => {
                        self.nonblank = true;
                        self.leading = false;
                        break;
                    }
                }
            }
        }
        if !matches!(self.mode, Mode::Braces) || self.comment {
            return;
        }
        static TOKEN: std::sync::OnceLock<regex::bytes::Regex> = std::sync::OnceLock::new();
        let tokens = TOKEN.get_or_init(|| {
            regex::bytes::Regex::new(r#"[\[\](){}/*"'\\\x60]"#).expect("fixed token class")
        });
        for m in tokens.find_iter(b) {
            let i = m.start();
            let c = b[i];
            let previous = if i == 0 { self.previous } else { b[i - 1] };
            if self.block {
                if previous == b'*' && c == b'/' {
                    self.block = false;
                }
                continue;
            }
            if self.quote != 0 {
                if self.escaped && previous == b'\\' {
                    self.escaped = false;
                    continue;
                }
                self.escaped = c == b'\\';
                if c == self.quote {
                    self.quote = 0;
                }
                continue;
            }
            if previous == b'/' && c == b'/' {
                self.comment = true;
                break;
            }
            if previous == b'/' && c == b'*' {
                self.block = true;
                continue;
            }
            match c {
                b'"' | b'\'' | b'\x60' => {
                    self.quote = c;
                    self.escaped = false
                }
                b'{' | b'[' | b'(' => self.openings.push((c as usize, self.line, u64::MAX)),
                b'}' | b']' | b')' => {
                    let mate = match c {
                        b'}' => b'{',
                        b']' => b'[',
                        _ => b'(',
                    };
                    if self
                        .openings
                        .last()
                        .is_some_and(|entry| entry.0 == mate as usize)
                    {
                        self.pop(self.line);
                    }
                }
                _ => {}
            }
        }
        if let Some(&last) = b.last() {
            self.previous = last;
        }
    }
    fn line_end(&mut self, next: u64) {
        let begin = self.line;
        match self.mode {
            Mode::Braces => {
                for opening in self
                    .openings
                    .iter_mut()
                    .rev()
                    .take_while(|entry| entry.2 == u64::MAX)
                {
                    opening.2 = next;
                }
                self.comment = false;
                self.previous = 0;
                self.escaped = false;
                if self.quote != b'\x60' {
                    self.quote = 0;
                }
            }
            Mode::Indentation if self.nonblank => {
                while self.openings.last().is_some_and(|x| x.0 >= self.indent) {
                    self.pop(begin);
                }
                if let Some((indent, head, start)) = self.prior {
                    if self.indent > indent {
                        self.openings.push((indent, head, start));
                    }
                }
                self.prior = Some((self.indent, begin, next));
            }
            Mode::Markdown => {
                let level = self.first.iter().take_while(|&&b| b == b'#').count();
                if (1..=6).contains(&level)
                    && self.first.get(level).is_some_and(u8::is_ascii_whitespace)
                {
                    while self.openings.last().is_some_and(|x| x.0 >= level) {
                        self.pop(begin);
                    }
                    self.openings.push((level, begin, next));
                }
            }
            Mode::Markup => {
                let s = String::from_utf8_lossy(&self.first);
                let s = s.trim();
                if s.starts_with("</") {
                    self.pop(begin);
                } else if s.starts_with('<')
                    && s.as_bytes().get(1).is_some_and(u8::is_ascii_alphabetic)
                    && !s.contains("</")
                    && !s.ends_with("/>")
                {
                    let tag = s[1..]
                        .split(|c: char| c.is_whitespace() || c == '>')
                        .next()
                        .unwrap_or("");
                    if ![
                        "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta",
                        "param", "source", "track", "wbr",
                    ]
                    .iter()
                    .any(|v| tag.eq_ignore_ascii_case(v))
                    {
                        self.openings.push((0, begin, next));
                    }
                }
            }
            _ => {}
        }
        self.first.clear();
        self.line = next;
        self.indent = 0;
        self.leading = true;
        self.nonblank = false;
    }
}
impl Document {
    /// Locate the enclosing region at a physical offset, or the outermost
    /// regions for Fold All. Work is cancellable and uses bounded input blocks.
    pub fn structural_folds(
        &self,
        lang: &str,
        target: Option<u64>,
        cancel: &AtomicBool,
    ) -> io::Result<Vec<Fold>> {
        let name = lang.to_ascii_lowercase();
        let mode = if name.contains("markdown") {
            Mode::Markdown
        } else if ["xml", "html", "xhtml"].contains(&name.as_str()) {
            Mode::Markup
        } else if [
            "python",
            "yaml",
            "text",
            "plain text",
            "coffee",
            "nim",
            "haskell",
        ]
        .iter()
        .any(|s| name.contains(s))
        {
            Mode::Indentation
        } else {
            Mode::Braces
        };
        let mut parser = Parser {
            mode,
            openings: vec![],
            regions: vec![],
            target,
            line: 0,
            first: vec![],
            quote: 0,
            escaped: false,
            block: false,
            comment: false,
            previous: 0,
            indent: 0,
            leading: true,
            nonblank: false,
            prior: None,
        };
        let mut at = 0;
        let mut cr = false;
        while at < self.len() {
            if cancel.load(Ordering::Relaxed) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let bytes = self.read(at, (self.len() - at).min(1024 * 1024) as usize)?;
            let mut start = 0;
            for split in memchr::memchr2_iter(b'\r', b'\n', &bytes) {
                if cr {
                    let pos = at + start as u64;
                    if start == split && bytes[split] == b'\n' {
                        parser.line_end(pos + 1);
                        start = split + 1;
                        cr = false;
                        continue;
                    }
                    parser.line_end(pos);
                    cr = false;
                }
                parser.data(&bytes[start..split]);
                start = split + 1;
                if bytes[split] == b'\r' {
                    cr = true;
                } else {
                    parser.line_end(at + start as u64);
                }
            }
            if start < bytes.len() {
                if cr {
                    parser.line_end(at + start as u64);
                    cr = false;
                }
                parser.data(&bytes[start..]);
            }
            at += bytes.len() as u64;
            if target.is_some_and(|p| p < at)
                && !parser.regions.is_empty()
                && parser.line > target.unwrap()
            {
                break;
            }
        }
        if cr || parser.nonblank {
            parser.line_end(self.len());
        }
        if at == self.len() {
            while !parser.openings.is_empty() {
                parser.pop(self.len());
            }
        }
        Ok(parser.regions)
    }
}
