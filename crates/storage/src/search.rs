use super::{parallel, Document, invalid};
use memchr::memmem;
use regex::bytes::{Regex, RegexBuilder};
#[path = "replace_stream.rs"]
mod replacing;
#[path = "splice.rs"]
mod splicing;
#[path = "search_cursor.rs"]
mod streaming;
use std::{
    io,
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

const CHUNK: u64 = 1024 * 1024;
/// Chunks searched on the calling thread first, so a match close to the
/// starting point is found without starting any threads.
const DIRECT: u64 = 4;
/// Fewer remaining chunks than this are not worth starting threads for.
const PARALLEL_MIN: u64 = 12;
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
    fold: Option<Fold>,
    streamed: Option<streaming::Regex>,
    cancel: &'a AtomicBool,
    scope: Range<u64>,
}
/// A quicker way to find plain ASCII text in any letter case, when the text
/// has at least two characters in a row that have no case, such as the "=503"
/// of "status=503". Those are found exactly, and the letters around them are
/// compared as ASCII. Unicode has two characters beyond ASCII that match an
/// ASCII letter without regard to case: the long s matches "s" and the Kelvin
/// sign matches "k". On meeting a non-ASCII byte where an s or a k is due, the
/// caller searches that stretch with the general pattern, so the matches are
/// always the general pattern's.
#[derive(Clone)]
struct Fold {
    lower: Vec<u8>,
    run: Range<usize>,
    finder: memmem::Finder<'static>,
}
impl Fold {
    fn new(query: &str) -> Option<Self> {
        if !query.is_ascii() {
            return None;
        }
        let lower = query.to_ascii_lowercase().into_bytes();
        let mut run = 0..0;
        let mut start = 0;
        for i in 0..=lower.len() {
            if lower.get(i).is_none_or(u8::is_ascii_alphabetic) {
                if i - start > run.len() {
                    run = start..i;
                }
                start = i + 1;
            }
        }
        (run.len() >= 2).then(|| Self {
            finder: memmem::Finder::new(&lower[run.clone()]).into_owned(),
            lower,
            run,
        })
    }
    /// Does the text match with its caseless run at `buf[at..]`? `None` when
    /// only the general pattern can tell.
    fn check(&self, buf: &[u8], start: usize) -> Option<bool> {
        let before = (0..self.run.start).rev();
        for k in before.chain(self.run.end..self.lower.len()) {
            let (want, got) = (self.lower[k], buf[start + k]);
            if got.to_ascii_lowercase() != want {
                return if got >= 0x80 && matches!(want, b's' | b'k') {
                    None
                } else {
                    Some(false)
                };
            }
        }
        Some(true)
    }
    /// The matches that start in `from..stop`, leftmost first and not overlapping.
    fn scan(&self, buf: &[u8], from: usize, stop: usize) -> Option<Vec<Range<usize>>> {
        let mut hits = Vec::new();
        let mut floor = from;
        let mut at = from + self.run.start;
        while at < buf.len() {
            let Some(found) = self.finder.find(&buf[at..]) else {
                break;
            };
            let start = at + found - self.run.start;
            at += found + 1;
            if start >= stop {
                break;
            }
            let end = start + self.lower.len();
            if start < floor || end > buf.len() {
                continue;
            }
            if self.check(buf, start)? {
                hits.push(start..end);
                floor = end;
            }
        }
        Some(hits)
    }
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
        let plain = !opt.regex && !opt.case_sensitive && !opt.whole_word && !opt.backwards;
        Ok(Self {
            d,
            query: q,
            opt,
            pattern,
            fold: plain.then(|| Fold::new(q)).flatten(),
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
        let count = (range.end - range.start).div_ceil(CHUNK);
        let workers = parallel::threads();
        let direct = if workers < 2 || count < DIRECT + PARALLEL_MIN {
            count
        } else {
            DIRECT
        };
        // A forward match never overlaps the one before it, including one that
        // ends in the next chunk: each chunk is searched from that end.
        let mut floor = range.start;
        let mut bytes = Vec::new();
        for k in 0..direct {
            self.check()?;
            let (left, right) = self.bounds(&range, k);
            let from = if self.opt.backwards { left } else { left.max(floor) };
            if from >= right {
                continue;
            }
            let origin = self.load(left, right, &mut bytes, |at, out| self.d.read_into(at, out))?;
            let mut step = |hit: Range<u64>, groups: Option<&Groups<'_>>| {
                floor = hit.end;
                visit(hit, groups)
            };
            if self.matches(&bytes, origin, &(from..right), captures, &mut step)? {
                return Ok(true);
            }
        }
        if direct == count {
            return Ok(false);
        }
        // The rest is searched by several threads, each with its own handle to
        // the file. Their matches are replayed here in document order.
        let rest = (count - direct) as usize;
        let found = |k: usize, (reader, bytes, search): &mut (parallel::Reader<'_>, Vec<u8>, Search<'_>)| {
            search.check()?;
            let (left, right) = search.bounds(&range, direct + k as u64);
            let origin = search.load(left, right, bytes, |at, out| reader.read_into(at, out))?;
            let mut hits = Vec::new();
            search.matches(bytes, origin, &(left..right), false, &mut |hit, _| {
                hits.push(((hit.start - left) as u32, (hit.end - hit.start) as u32));
                Ok(false)
            })?;
            Ok(hits)
        };
        parallel::ordered(
            rest,
            workers.min(rest),
            || (parallel::Reader::new(self.d), Vec::new(), self.fork()),
            found,
            |k, hits: Vec<(u32, u32)>| {
                let (left, right) = self.bounds(&range, direct + k as u64);
                let from = floor;
                if !self.opt.backwards && hits.first().is_some_and(|h| left + u64::from(h.0) < from) {
                    // The thread started at the chunk's first byte, inside the
                    // previous match. Search this chunk again from its end.
                    if from >= right {
                        return Ok(false);
                    }
                    let origin =
                        self.load(left, right, &mut bytes, |at, out| self.d.read_into(at, out))?;
                    return self.matches(&bytes, origin, &(from..right), false, &mut |hit, groups| {
                        floor = hit.end;
                        visit(hit, groups)
                    });
                }
                for (start, len) in hits {
                    let start = left + u64::from(start);
                    floor = start + u64::from(len);
                    if visit(start..floor, None)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            },
        )
    }
    /// The same search for another thread. A copied pattern has its own scratch space.
    fn fork(&self) -> Search<'a> {
        Search {
            d: self.d,
            query: self.query,
            opt: self.opt,
            pattern: self.pattern.clone(),
            fold: self.fold.clone(),
            streamed: None,
            cancel: self.cancel,
            scope: self.scope.clone(),
        }
    }
    fn bounds(&self, range: &Range<u64>, k: u64) -> (u64, u64) {
        if self.opt.backwards {
            let right = range.end - k * CHUNK;
            (right.saturating_sub(CHUNK).max(range.start), right)
        } else {
            let left = range.start + k * CHUNK;
            (left, (left + CHUNK).min(range.end))
        }
    }
    /// Read a chunk with the bytes around it that a match starting inside it
    /// can touch. Returns the document offset of the first byte read.
    fn load(
        &self,
        left: u64,
        right: u64,
        bytes: &mut Vec<u8>,
        mut read: impl FnMut(u64, &mut [u8]) -> io::Result<()>,
    ) -> io::Result<u64> {
        let overlap = if self.opt.case_sensitive {
            self.query.len() as u64
        } else {
            self.query.chars().count() as u64 * 4
        };
        let origin = left.saturating_sub(4);
        let end = (right + overlap + 4).min(self.d.len());
        bytes.resize(end.saturating_sub(origin) as usize, 0);
        read(origin, bytes)?;
        Ok(origin)
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
                let stop = (range.end - origin).min(buf.len() as u64) as usize;
                if let Some(hits) = self.fold.as_ref().and_then(|fold| fold.scan(buf, off, stop)) {
                    for hit in hits {
                        let hit = origin + hit.start as u64..origin + hit.end as u64;
                        if hit.end > self.scope.end {
                            break;
                        }
                        if visit(hit, None)? {
                            return Ok(true);
                        }
                    }
                    return Ok(false);
                }
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
        let (count, change) = if options.regex {
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
            edits.finish()?
        } else {
            splicing::replace(self, &search, replacement.as_bytes(), &mut observe)?
        };
        search.check()?;
        self.check_original()?;
        match change {
            replacing::Change::Sparse(edits) => self.replace_many(&edits)?,
            replacing::Change::Whole(root) => self.commit(root)?,
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
            replacing::Change::Whole(root) => self.commit(root)?,
        }
        Ok((set, caret))
    }
}
