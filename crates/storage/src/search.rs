use super::{Document, invalid};
use memchr::memmem;
use regex::bytes::{Regex, RegexBuilder};
#[path = "replace_stream.rs"]
mod replacing;
#[path = "search_cursor.rs"]
mod streaming;
use std::{
    io,
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

const CHUNK: u64 = 1024 * 1024;
const FULL_REGEX: u64 = 24 * 1024 * 1024;
#[derive(Clone, Copy, Debug)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub regex: bool,
    pub whole_word: bool,
    pub backwards: bool,
    pub wrap: bool,
}
impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            case_sensitive: false,
            regex: false,
            whole_word: false,
            backwards: false,
            wrap: true,
        }
    }
}
struct Search<'a> {
    d: &'a Document,
    query: &'a str,
    opt: SearchOptions,
    pattern: Option<Regex>,
    streamed: Option<streaming::Regex>,
    cancel: &'a AtomicBool,
    scope: Range<u64>,
}
type Groups<'a> = dyn Fn(usize) -> Option<Range<u64>> + 'a;
type Visitor<'a> = dyn FnMut(Range<u64>, Option<&Groups<'_>>) -> io::Result<bool> + 'a;
impl<'a> Search<'a> {
    fn new(
        d: &'a Document,
        q: &'a str,
        opt: SearchOptions,
        cancel: &'a AtomicBool,
        scope: Range<u64>,
    ) -> io::Result<Self> {
        if scope.start > scope.end || scope.end > d.len() {
            return Err(invalid("Search selection is outside the document"));
        }
        if q.len() > 1024 * 1024 {
            return Err(invalid("Search text exceeds 1 MiB"));
        }
        let mut streamed = None;
        let pattern = if opt.regex || !opt.case_sensitive || opt.whole_word {
            let raw = if opt.regex {
                q.into()
            } else {
                regex::escape(q)
            };
            let s = if opt.whole_word {
                format!(r"\b(?:{raw})\b")
            } else {
                raw
            };
            if opt.regex && (d.len() > FULL_REGEX || scope.start != 0 || scope.end != d.len()) {
                streamed = Some(streaming::compile(&s, opt.case_sensitive)?);
                None
            } else {
                Some(
                    RegexBuilder::new(&s)
                        .case_insensitive(!opt.case_sensitive)
                        .multi_line(true)
                        .crlf(true)
                        .size_limit(16 * 1024 * 1024)
                        .build()
                        .map_err(|e| invalid(e.to_string()))?,
                )
            }
        } else {
            None
        };
        Ok(Self {
            d,
            query: q,
            opt,
            pattern,
            streamed,
            cancel,
            scope,
        })
    }
    fn check(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Search cancelled",
            ))
        } else {
            Ok(())
        }
    }
    fn scan(&self, range: Range<u64>, captures: bool, visit: &mut Visitor<'_>) -> io::Result<bool> {
        if range.is_empty() {
            return Ok(false);
        }
        if let Some(re) = &self.streamed {
            return streaming::scan(self, re, range, captures, visit);
        }
        if self.opt.regex {
            self.check()?;
            return self.matches(
                &self.d.read(0, self.d.len() as usize)?,
                0,
                &range,
                captures,
                visit,
            );
        }
        let mut pos = if self.opt.backwards {
            range.end
        } else {
            range.start
        };
        let overlap = if self.opt.case_sensitive {
            self.query.len() as u64
        } else {
            self.query.chars().count() as u64 * 4
        };
        while if self.opt.backwards {
            pos > range.start
        } else {
            pos < range.end
        } {
            self.check()?;
            let left = if self.opt.backwards {
                pos.saturating_sub(CHUNK).max(range.start)
            } else {
                pos
            };
            let right = if self.opt.backwards {
                pos
            } else {
                (pos + CHUNK).min(range.end)
            };
            let origin = left.saturating_sub(4);
            let end = (right + overlap + 4).min(self.d.len());
            let bytes = self.d.read(origin, end.saturating_sub(origin) as usize)?;
            if self.matches(&bytes, origin, &(left..right), captures, visit)? {
                return Ok(true);
            }
            pos = if self.opt.backwards { left } else { right };
        }
        Ok(false)
    }
    fn matches(
        &self,
        buf: &[u8],
        origin: u64,
        range: &Range<u64>,
        captures: bool,
        visit: &mut Visitor<'_>,
    ) -> io::Result<bool> {
        if range.end <= origin || range.start > origin + buf.len() as u64 {
            return Ok(false);
        }
        let off = range.start.saturating_sub(origin).min(buf.len() as u64) as usize;
        let mut latest = None;
        if let Some(re) = &self.pattern {
            if captures && self.opt.regex {
                let mut at = off;
                let mut locations = re.capture_locations();
                while let Some(m) = re.captures_read_at(&mut locations, buf, at) {
                    self.check()?;
                    let start = origin + m.start() as u64;
                    if start >= range.end {
                        break;
                    }
                    let group = |i| {
                        locations
                            .get(i)
                            .map(|(a, b)| origin + a as u64..origin + b as u64)
                    };
                    if visit(start..origin + m.end() as u64, Some(&group))? {
                        return Ok(true);
                    }
                    if m.is_empty() && m.end() == buf.len() {
                        break;
                    }
                    at = m.end() + usize::from(m.is_empty());
                }
            } else {
                let mut at = off;
                while let Some(m) = re.find_at(buf, at) {
                    self.check()?;
                    let start = origin + m.start() as u64;
                    if start >= range.end {
                        break;
                    }
                    let hit = start..origin + m.end() as u64;
                    if hit.end > self.scope.end {
                        break;
                    }
                    if self.opt.backwards {
                        latest = Some(hit);
                    } else if visit(hit, None)? {
                        return Ok(true);
                    }
                    if m.is_empty() && m.end() == buf.len() {
                        break;
                    }
                    at = if self.opt.backwards && !self.opt.regex {
                        m.start() + 1
                    } else {
                        m.end() + usize::from(m.is_empty())
                    };
                }
            }
        } else {
            let needle = self.query.as_bytes();
            if self.opt.backwards {
                let max = (range.end - origin)
                    .saturating_add(needle.len() as u64 - 1)
                    .min(self.scope.end.saturating_sub(origin))
                    .min(buf.len() as u64) as usize;
                if max < off {
                    return Ok(false);
                }
                if let Some(i) = memmem::rfind(&buf[off..max], needle) {
                    let p = origin + (off + i) as u64;
                    return visit(p..p + needle.len() as u64, None);
                }
            } else {
                for i in memmem::find_iter(&buf[off..], needle) {
                    let p = origin + (off + i) as u64;
                    if p >= range.end {
                        break;
                    }
                    if visit(p..p + needle.len() as u64, None)? {
                        return Ok(true);
                    }
                }
            }
        }
        if let Some(x) = latest {
            return visit(x, None);
        }
        Ok(false)
    }
}
impl Document {
    pub fn replacement_for(
        &self,
        query: &str,
        hit: Range<u64>,
        options: SearchOptions,
        replacement: &str,
        cancel: &AtomicBool,
    ) -> io::Result<Vec<u8>> {
        self.replacement_in(query, hit, options, replacement, 0..self.len(), cancel)
    }
    pub fn replacement_in(
        &self,
        query: &str,
        hit: Range<u64>,
        mut options: SearchOptions,
        replacement: &str,
        scope: Range<u64>,
        cancel: &AtomicBool,
    ) -> io::Result<Vec<u8>> {
        if hit.start < scope.start || hit.end > scope.end {
            return Err(invalid("Match is outside the search selection"));
        }
        if !options.regex {
            return Ok(replacement.as_bytes().to_vec());
        }
        options.backwards = false;
        let search = Search::new(self, query, options, cancel, scope)?;
        let plan = replacing::Template::new(&search, replacement);
        let mut reader = replacing::Reader::new(self, cancel);
        let mut expanded = None;
        search.scan(
            hit.start..hit.start + 1,
            plan.captures(),
            &mut |matched, groups| {
                if matched == hit {
                    // The replace-one GUI inserts a contiguous UTF-8 string.
                    // Replace All uses the streaming writer without this bound.
                    if plan.length(groups)? > 64 * 1024 * 1024 {
                        return Err(invalid(
                            "Use Replace All for a replacement larger than 64 MiB",
                        ));
                    }
                    let mut text = Vec::new();
                    plan.write(groups, &mut reader, &mut text)?;
                    expanded = Some(text);
                }
                Ok(true)
            },
        )?;
        expanded.ok_or_else(|| invalid("The original match changed"))
    }
    pub fn find(
        &self,
        query: &str,
        start: u64,
        options: SearchOptions,
    ) -> io::Result<Option<Range<u64>>> {
        self.find_cancel(query, start, options, &AtomicBool::new(false))
    }
    /// Collect a bounded list in a single pass, e.g. for a caller that needs a materialized list of positions.
    pub fn find_all_bounded(
        &self,
        query: &str,
        options: SearchOptions,
        limit: usize,
        cancel: &AtomicBool,
    ) -> io::Result<Vec<Range<u64>>> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let search = Search::new(self, query, options, cancel, 0..self.len())?;
        let mut ranges = Vec::new();
        let mut next = 0;
        search.scan(0..self.len() + 1, false, &mut |r, _| {
            if r.start >= next {
                if ranges.len() >= limit {
                    return Err(invalid(
                        "The search exceeds the supported number of in-memory matches",
                    ));
                }
                next = r.end.max(r.start + 1);
                ranges.push(r);
            }
            Ok(false)
        })?;
        Ok(ranges)
    }
    pub fn find_cancel(
        &self,
        query: &str,
        start: u64,
        options: SearchOptions,
        cancel: &AtomicBool,
    ) -> io::Result<Option<Range<u64>>> {
        self.find_in(query, start, options, 0..self.len(), cancel)
    }
    pub fn find_in(
        &self,
        query: &str,
        start: u64,
        options: SearchOptions,
        scope: Range<u64>,
        cancel: &AtomicBool,
    ) -> io::Result<Option<Range<u64>>> {
        if query.is_empty() {
            return Ok(None);
        }
        let search = Search::new(self, query, options, cancel, scope.clone())?;
        let start = start.clamp(scope.start, scope.end + 1);
        let entire = scope.end + 1;
        let (first, second) = if options.backwards {
            (scope.start..start, start..entire)
        } else {
            (start..entire, scope.start..start)
        };
        let mut result = None;
        let mut visit = |hit: Range<u64>, _: Option<&Groups<'_>>| {
            if hit.end > scope.end {
                return Ok(false);
            }
            result = Some(hit);
            Ok(true)
        };
        if !search.scan(first, false, &mut visit)? && options.wrap {
            search.scan(second, false, &mut visit)?;
        }
        Ok(result)
    }
    pub fn replace_all(
        &mut self,
        query: &str,
        replacement: &str,
        options: SearchOptions,
    ) -> io::Result<usize> {
        self.replace_all_cancel(query, replacement, options, &AtomicBool::new(false))
    }
    pub fn replace_all_cancel(
        &mut self,
        query: &str,
        replacement: &str,
        options: SearchOptions,
        cancel: &AtomicBool,
    ) -> io::Result<usize> {
        self.replace_all_in(query, replacement, options, 0..self.len(), cancel)
    }
    pub fn replace_all_in(
        &mut self,
        query: &str,
        replacement: &str,
        options: SearchOptions,
        scope: Range<u64>,
        cancel: &AtomicBool,
    ) -> io::Result<usize> {
        self.replace_observed(query, replacement, options, scope, cancel, |_, _| {})
    }
    pub fn replace_observed(
        &mut self,
        query: &str,
        replacement: &str,
        mut options: SearchOptions,
        scope: Range<u64>,
        cancel: &AtomicBool,
        mut observe: impl FnMut(Range<u64>, u64),
    ) -> io::Result<usize> {
        if query.is_empty() {
            return Ok(0);
        }
        options.backwards = false;
        let search = Search::new(self, query, options, cancel, scope.clone())?;
        self.check_original()?;
        let template = replacing::Template::new(&search, replacement);
        let mut edits = replacing::Batch::new(self, cancel);
        search.scan(
            scope.start..scope.end + 1,
            template.captures(),
            &mut |hit, groups| {
                if hit.start >= edits.end() && hit.end <= scope.end {
                    observe(hit.clone(), template.length(groups)?);
                    edits.push(hit, &template, groups)?;
                }
                Ok(false)
            },
        )?;
        let (count, change) = edits.finish()?;
        search.check()?;
        self.check_original()?;
        match change {
            replacing::Change::Sparse(edits) => self.replace_many(&edits)?,
            replacing::Change::File(root) => self.commit(root)?,
        }
        Ok(count)
    }
}
impl Document {
    pub fn occurrences(&self, query: &str, cancel: &AtomicBool) -> io::Result<crate::SelectionSet> {
        if query.is_empty() {
            return Err(invalid("Select a word or phrase first"));
        }
        let opt = SearchOptions {
            case_sensitive: true,
            ..Default::default()
        };
        let search = Search::new(self, query, opt, cancel, 0..self.len())?;
        let mut next = 0;
        let mut out = crate::SelectionBuilder::default();
        search.scan(0..self.len() + 1, false, &mut |r, _| {
            if r.start >= next {
                next = r.end;
                out.push(r)?;
            }
            Ok(false)
        })?;
        out.finish()
    }
    /// All positions use the same snapshot. Sparse edits retain untouched
    /// pieces; dense edits stream to temporary storage before one history commit.
    pub fn edit_carets(
        &mut self,
        set: &crate::SelectionSet,
        primary: u64,
        cancel: &AtomicBool,
        mut edit: impl FnMut(u64, Range<u64>, &Document) -> io::Result<(Range<u64>, Vec<u8>)>,
    ) -> io::Result<(crate::SelectionSet, u64)> {
        self.check_original()?;
        let mut out = crate::SelectionBuilder::default();
        let mut batch = replacing::Batch::new(self, cancel);
        let (mut old, mut new, mut index, mut caret) = (0, 0, 0, 0);
        set.visit(0, |selection| {
            let (r, bytes) = edit(index, selection, self)?;
            if r.end > self.len() || r.start < old || r.end < r.start {
                return Err(invalid("Overlapping or invalid multi-cursor edit"));
            }
            new += r.start - old + bytes.len() as u64;
            old = r.end;
            if index == primary {
                caret = new;
            }
            out.push(new..new)?;
            if !r.is_empty() || !bytes.is_empty() {
                batch.push(r, &replacing::Template::bytes(&bytes), None)?;
            }
            index += 1;
            Ok(true)
        })?;
        let set = out.finish()?;
        let (_, changes) = batch.finish()?;
        self.check_original()?;
        match changes {
            replacing::Change::Sparse(v) => self.replace_many(&v)?,
            replacing::Change::File(root) => self.commit(root)?,
        }
        Ok((set, caret))
    }
}
