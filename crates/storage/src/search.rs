use super::{parallel, source, Document, invalid};
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
/// One line that holds a match, from `Document::find_lines`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundLine {
    /// Counted from 0, as `Document::line_of` counts.
    pub line: u64,
    /// Where the line starts in the document.
    pub start: u64,
    /// The first match on the line.
    pub first: Range<u64>,
    /// The line's text without its line break, at most the `widest` bytes
    /// asked for. A longer line gives the part around its first match.
    pub text: Vec<u8>,
    /// Where `text` starts in the document: `start`, unless the line was longer than `widest`.
    pub text_start: u64,
    /// Whether the line goes on past the end of `text`.
    pub cut: bool,
}
/// The text kept of a matching line: where the line starts, where the kept
/// text starts, the text, and whether the line goes on past it.
type Kept = (u64, u64, Vec<u8>, bool);
/// A matching line as one thread found it inside its piece of the document.
struct PieceLine {
    /// Line breaks between the piece's first byte and the match.
    breaks: u64,
    first: Range<u64>,
    /// The line's text when the bytes the thread held contained the whole line.
    found: Option<Kept>,
}

/// Of a line `line_start..line_end` with a match at `at`, the part to keep in
/// at most `widest` bytes: all of it, or a window that starts a quarter of
/// `widest` before the match.
fn window(line_start: u64, line_end: u64, at: u64, widest: usize) -> Range<u64> {
    if line_end - line_start <= widest as u64 {
        return line_start..line_end;
    }
    let from = at.saturating_sub(widest as u64 / 4).max(line_start);
    from..(from + widest as u64).min(line_end)
}
struct Piece {
    /// Line breaks in the whole piece, counted as the line index counts them.
    breaks: u64,
    lines: Vec<PieceLine>,
    /// The end of the last match, for the rule that matches do not overlap.
    last_end: Option<u64>,
}
/// Which bytes end a line when matching lines are found and numbered.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LineEnds {
    /// The editor's rule: CR, LF and CRLF each end one line.
    #[default]
    Editor,
    /// grep's rule: only LF ends a line, and a CR is part of the line's text.
    LineFeed,
}

impl LineEnds {
    /// Line ends in `buf`; `next` is the byte after it, to tell a lone CR from a CRLF's.
    fn count(self, buf: &[u8], next: Option<u8>) -> u64 {
        match self {
            LineEnds::Editor => source::count(buf, next),
            LineEnds::LineFeed => memchr::memchr_iter(b'\n', buf).count() as u64,
        }
    }

    /// Where the line holding `buf[upto]` starts in `buf`; None when no line end before it is in `buf`.
    fn begin(self, buf: &[u8], upto: usize) -> Option<usize> {
        match self {
            LineEnds::Editor => line_begin(buf, upto),
            LineEnds::LineFeed => memchr::memrchr(b'\n', &buf[..upto]).map(|i| i + 1),
        }
    }

    /// Where the text of the line holding `buf[at]` ends: at the first CR or
    /// LF for the editor; at the LF for grep, whose line keeps a CRLF's CR as
    /// it prints it. None when that cannot be told from `buf` alone.
    fn end(self, buf: &[u8], at: usize, whole: bool) -> Option<usize> {
        let found = match self {
            LineEnds::LineFeed => memchr::memchr(b'\n', &buf[at..]),
            LineEnds::Editor => memchr::memchr2(b'\n', b'\r', &buf[at..]),
        };
        found.map(|e| at + e).or(whole.then_some(buf.len()))
    }
}

/// Counts line breaks forward from the start of a document, for matches that
/// arrive in order.
struct LineCounter<'a> {
    d: &'a Document,
    pos: u64,
    breaks: u64,
    block: Vec<u8>,
    ends: LineEnds,
}
impl LineCounter<'_> {
    /// The line, counted from 0, that holds `at`. `at` never goes backwards.
    fn line_of(&mut self, at: u64) -> io::Result<u64> {
        while self.pos < at {
            let n = (at - self.pos).min(CHUNK) as usize;
            // One byte more, to tell a lone CR from the CR of a CRLF.
            self.block.resize((n as u64 + 1).min(self.d.len() - self.pos) as usize, 0);
            self.d.read_into(self.pos, &mut self.block)?;
            self.breaks += self.ends.count(&self.block[..n], self.block.get(n).copied());
            self.pos += n as u64;
        }
        Ok(self.breaks)
    }
}
/// Where the line holding `buf[upto]` starts in `buf`, or `None` when no line
/// break before it is in `buf`. CR, LF and CRLF each end one line.
fn line_begin(buf: &[u8], upto: usize) -> Option<usize> {
    let mut end = upto;
    while let Some(i) = memchr::memrchr2(b'\n', b'\r', &buf[..end]) {
        // A CR directly before an LF is part of one CRLF break, which ends with the LF.
        if buf[i] == b'\n' || buf.get(i + 1) != Some(&b'\n') {
            return Some(i + 1);
        }
        end = i;
    }
    None
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
    /// Search the piece `left..right` of a forward scan from `from`, in `buf`
    /// read from `origin`, and count its line breaks in the same pass: for each
    /// matching line, the breaks before its first match, and its text when the
    /// whole line is in `buf`. Breaks are counted from `left` whatever `from` is.
    #[allow(clippy::too_many_arguments)]
    fn piece(&self, buf: &[u8], origin: u64, left: u64, right: u64, from: u64, widest: usize, ends: LineEnds) -> io::Result<Piece> {
        let rel = |at: u64| (at - origin) as usize;
        let mut lines: Vec<PieceLine> = Vec::new();
        let (mut counted, mut breaks) = (rel(left), 0u64);
        let mut last_end = None;
        let whole = origin + buf.len() as u64 == self.d.len();
        self.matches(buf, origin, &(from..right), false, &mut |hit, _| {
            let s = rel(hit.start);
            breaks += ends.count(&buf[counted..s], buf.get(s).copied());
            counted = s;
            last_end = Some(hit.end);
            if lines.last().is_some_and(|l| l.breaks == breaks) {
                return Ok(false);
            }
            let begin = ends.begin(buf, s).or((origin == 0).then_some(0));
            // A line end at the buffer's edge is settled by reading the line on its own.
            let end = ends.end(buf, s, whole);
            let found = match (begin, end) {
                (Some(b), Some(e)) => {
                    let kept = window(b as u64, e as u64, s as u64, widest);
                    let text = buf[kept.start as usize..kept.end as usize].to_vec();
                    Some((origin + b as u64, origin + kept.start, text, kept.end < e as u64))
                }
                _ => None,
            };
            lines.push(PieceLine { breaks, first: hit, found });
            Ok(false)
        })?;
        let end = rel(right);
        breaks += ends.count(&buf[counted..end], buf.get(end).copied());
        Ok(Piece { breaks, lines, last_end })
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
    /// Every line that holds a match, in document order, with its number and
    /// text, each passed to `visit` as it is found; `visit` returns true to stop.
    /// Line numbers are counted in the same pass as the search, so this needs
    /// neither the line index nor a second read of the file. Returns true when
    /// `visit` stopped the search.
    pub fn find_lines(
        &self,
        query: &str,
        options: SearchOptions,
        widest: usize,
        ends: LineEnds,
        cancel: &AtomicBool,
        visit: &mut dyn FnMut(&FoundLine) -> io::Result<bool>,
    ) -> io::Result<bool> {
        self.find_lines_in(query, options, 0..self.len(), 0, widest, ends, cancel, visit)
    }
    /// `find_lines` for the matches that start in `scope`, numbering lines from
    /// `breaks_before`, the number of line breaks before `scope.start`
    /// (`count_breaks` gives it, by the same `ends`).
    #[allow(clippy::too_many_arguments)]
    pub fn find_lines_in(
        &self,
        query: &str,
        mut options: SearchOptions,
        scope: Range<u64>,
        breaks_before: u64,
        widest: usize,
        ends: LineEnds,
        cancel: &AtomicBool,
        visit: &mut dyn FnMut(&FoundLine) -> io::Result<bool>,
    ) -> io::Result<bool> {
        if query.is_empty() || self.is_empty() || scope.is_empty() {
            return Ok(false);
        }
        options.backwards = false;
        let search = Search::new(self, query, options, cancel, scope.clone())?;
        let mut last = None;
        let end = scope.end;
        let mut emit = |line: u64, first: Range<u64>, found: Option<Kept>| -> io::Result<bool> {
            if last == Some(line) {
                return Ok(false);
            }
            let (start, text_start, text, cut) = match found {
                Some(found) => found,
                None => self.line_text(first.start, widest, ends)?,
            };
            // An empty match at the end of the scope, where a line would begin
            // (after the text's last line break, or at the next line of a
            // span), is in no line of the scope: grep does not count it either.
            if first.start == end && start == end {
                return Ok(false);
            }
            last = Some(line);
            visit(&FoundLine { line, start, first, text, text_start, cut })
        };
        if search.opt.regex {
            // A pattern searches the whole text or streams it: take its matches
            // in order and count the lines up to each in one forward pass.
            let mut counter = LineCounter { d: self, pos: scope.start, breaks: breaks_before, block: Vec::new(), ends };
            return search.scan(scope.start..scope.end + 1, false, &mut |hit, _| {
                let line = counter.line_of(hit.start)?;
                emit(line, hit, None)
            });
        }
        let range = scope.clone();
        let count = (range.end - range.start).div_ceil(CHUNK) as usize;
        let (mut base, mut floor) = (breaks_before, scope.start);
        let mut bytes = Vec::new();
        // Matches never overlap the one before them, including one that ends in
        // the next piece. A piece whose first match starts inside the previous
        // match is searched again here from that match's end.
        let mut take = |k: usize, piece: Piece| -> io::Result<bool> {
            let (left, right) = search.bounds(&range, k as u64);
            let piece = if piece.lines.first().is_some_and(|l| l.first.start < floor) {
                let origin = search.load(left, right, &mut bytes, |at, out| self.read_into(at, out))?;
                search.piece(&bytes, origin, left, right, floor.min(right), widest, ends)?
            } else {
                piece
            };
            for l in piece.lines {
                if emit(base + l.breaks, l.first, l.found)? {
                    return Ok(true);
                }
            }
            base += piece.breaks;
            if let Some(end) = piece.last_end {
                floor = end;
            }
            Ok(false)
        };
        let workers = parallel::threads().min(count);
        if workers < 2 {
            let mut own = Vec::new();
            for k in 0..count {
                search.check()?;
                let (left, right) = search.bounds(&range, k as u64);
                let origin = search.load(left, right, &mut own, |at, out| self.read_into(at, out))?;
                let piece = search.piece(&own, origin, left, right, left, widest, ends)?;
                if take(k, piece)? {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        parallel::ordered(
            count,
            workers,
            || (parallel::Reader::new(self), Vec::new(), search.fork()),
            |k, (reader, own, search): &mut (parallel::Reader<'_>, Vec<u8>, Search<'_>)| {
                search.check()?;
                let (left, right) = search.bounds(&range, k as u64);
                let origin = search.load(left, right, own, |at, out| reader.read_into(at, out))?;
                search.piece(own, origin, left, right, left, widest, ends)
            },
            take,
        )
    }
    /// The number of line breaks before `upto`, by the line index's rule for CR,
    /// LF and CRLF: from the index when it covers `upto`, otherwise counted on
    /// several threads.
    pub fn count_breaks(&self, upto: u64, cancel: &AtomicBool) -> io::Result<u64> {
        self.count_ends(upto, LineEnds::Editor, cancel)
    }
    /// The number of line ends before `upto` by `ends`' rule: from the line
    /// index when it covers `upto` and counts by the same rule, otherwise
    /// counted on several threads.
    pub fn count_ends(&self, upto: u64, ends: LineEnds, cancel: &AtomicBool) -> io::Result<u64> {
        let upto = upto.min(self.len());
        if upto == 0 {
            return Ok(0);
        }
        if ends == LineEnds::Editor && self.indexed_bytes() >= upto {
            return Ok(self.line_of(upto));
        }
        let chunks = upto.div_ceil(CHUNK) as usize;
        let mut total = 0u64;
        parallel::ordered(
            chunks,
            parallel::threads().min(chunks),
            || (parallel::Reader::new(self), Vec::new()),
            |k, (reader, bytes): &mut (parallel::Reader<'_>, Vec<u8>)| {
                if cancel.load(Ordering::Relaxed) {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "Count cancelled"));
                }
                let left = k as u64 * CHUNK;
                let right = (left + CHUNK).min(upto);
                // One byte more, to tell a lone CR from the CR of a CRLF.
                bytes.resize(((right + 1).min(self.len()) - left) as usize, 0);
                reader.read_into(left, bytes)?;
                let n = (right - left) as usize;
                Ok(ends.count(&bytes[..n], bytes.get(n).copied()))
            },
            |_, n| {
                total += n;
                Ok(false)
            },
        )?;
        Ok(total)
    }
    /// Where the line holding `at` starts, and its text without its line break,
    /// at most `widest` bytes. Reads only around `at`.
    fn line_text(&self, at: u64, widest: usize, ends: LineEnds) -> io::Result<Kept> {
        const STEP: u64 = 64 * 1024;
        let len = self.len();
        let (mut start, mut hi) = (0, at);
        let mut block = Vec::new();
        while hi > 0 {
            let lo = hi.saturating_sub(STEP);
            // One byte past `hi`, to tell a lone CR from the CR of a CRLF.
            block.resize(((hi + 1).min(len) - lo) as usize, 0);
            self.read_into(lo, &mut block)?;
            if let Some(b) = ends.begin(&block, (hi - lo) as usize) {
                start = lo + b as u64;
                break;
            }
            hi = lo;
        }
        // The line from its start, or when the match lies past `widest` from
        // there, from a quarter of `widest` before the match; no line break
        // lies between the line's start and the match.
        let from = if at - start < widest as u64 / 2 { start } else { at - widest as u64 / 4 };
        let mut text = vec![0; (len - from).min(widest as u64) as usize];
        self.read_into(from, &mut text)?;
        let found = match ends {
            LineEnds::LineFeed => memchr::memchr(b'\n', &text),
            LineEnds::Editor => memchr::memchr2(b'\n', b'\r', &text),
        };
        let cut = match found {
            Some(e) => {
                text.truncate(e);
                false
            }
            None => from + (text.len() as u64) < len,
        };
        Ok((start, from, text, cut))
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

#[cfg(test)]
mod found_line_tests {
    use super::*;

    /// A search over part of the text numbers lines as the whole search does,
    /// and the break count before any place is the line index's.
    #[test]
    fn part_of_the_text_is_numbered_as_the_whole() -> io::Result<()> {
        let breaks: [&[u8]; 3] = [b"\n", b"\r\n", b"\r"];
        let mut text = Vec::new();
        for n in 0..300_000usize {
            text.extend_from_slice(if n % 7 == 0 { b"x needle y" } else { b"plain" });
            text.extend_from_slice(breaks[n % 3]);
        }
        let mut d = Document::empty();
        d.replace(0, 0, &text)?;
        let cancel = AtomicBool::new(false);
        let options = SearchOptions { case_sensitive: true, wrap: false, ..Default::default() };
        let mut whole = Vec::new();
        d.find_lines("needle", options, 1 << 20, LineEnds::Editor, &cancel, &mut |l| {
            whole.push(l.clone());
            Ok(false)
        })?;
        for at in [0, 1, 3, 1_000_001, d.len() / 2 + 5, d.len() - 3, d.len()] {
            assert_eq!(d.count_breaks(at, &cancel)?, d.line_of(at), "breaks before {at}");
        }
        let scope = d.len() / 3 + 2..2 * d.len() / 3;
        let mut part = Vec::new();
        d.find_lines_in("needle", options, scope.clone(), d.count_breaks(scope.start, &cancel)?, 1 << 20, LineEnds::Editor, &cancel, &mut |l| {
            part.push(l.clone());
            Ok(false)
        })?;
        let expected: Vec<_> = whole.into_iter().filter(|l| scope.contains(&l.first.start)).collect();
        assert!(expected.len() > 1000);
        assert_eq!(part, expected);
        Ok(())
    }

    #[test]
    fn an_empty_match_after_the_last_line_is_no_line() -> io::Result<()> {
        let cancel = AtomicBool::new(false);
        let options = SearchOptions { case_sensitive: true, regex: true, wrap: false, ..Default::default() };
        let lines = |text: &[u8], pattern: &str, scope: Option<Range<u64>>| -> io::Result<Vec<u64>> {
            let mut d = Document::empty();
            d.replace(0, 0, text)?;
            let scope = scope.unwrap_or(0..d.len());
            let before = d.count_breaks(scope.start, &cancel)?;
            let mut found = Vec::new();
            d.find_lines_in(pattern, options, scope, before, 1 << 20, LineEnds::Editor, &cancel, &mut |l| {
                found.push(l.line + 1);
                Ok(false)
            })?;
            Ok(found)
        };
        assert_eq!(lines(b"a\nb\n", "^", None)?, [1, 2]);
        assert_eq!(lines(b"a\r\nb\r\n", "^", None)?, [1, 2]);
        assert_eq!(lines(b"a\n\n", "^", None)?, [1, 2]);
        // Without a final line break the last line is real, and $ ends it.
        assert_eq!(lines(b"a\nb", "$", None)?, [1, 2]);
        // A span that ends where the next line begins.
        assert_eq!(lines(b"a\nb\nc\n", "^", Some(0..4))?, [1, 2]);
        Ok(())
    }

    /// Lines numbered in the search's own pass carry the numbers, starts and
    /// text the line index gives: across pieces, with CR, LF and CRLF breaks, a
    /// CRLF split between two pieces, and lines longer than a whole piece.
    #[test]
    fn found_lines_agree_with_the_line_index() -> io::Result<()> {
        let piece = CHUNK as usize;
        let breaks: [&[u8]; 3] = [b"\n", b"\r\n", b"\r"];
        let mut text = Vec::new();
        let mut n = 0usize;
        while text.len() < 6 * piece {
            if n % 997 == 5 {
                text.resize(text.len() + piece + 17, b'z');
            }
            text.extend_from_slice(if n % 3 == 0 { b"one needle two" } else { b"plain Needle words" });
            text.extend_from_slice(breaks[n % 3]);
            n += 1;
            let edge = (text.len() / piece + 1) * piece;
            if edge - text.len() < 60 {
                text.resize(edge - 1, b'y');
                text.extend_from_slice(b"\r\n");
            }
        }
        text.extend_from_slice(b"needle");
        let mut d = Document::empty();
        d.replace(0, 0, &text)?;
        let cancel = AtomicBool::new(false);
        for (query, options) in [
            ("needle", SearchOptions { case_sensitive: true, wrap: false, ..Default::default() }),
            ("needle", SearchOptions { wrap: false, ..Default::default() }),
            ("ne+dle", SearchOptions { regex: true, case_sensitive: true, wrap: false, ..Default::default() }),
        ] {
            let mut expected = Vec::new();
            for hit in d.find_all_bounded(query, options, usize::MAX, &cancel)? {
                let line = d.line_of(hit.start);
                if expected.last().is_none_or(|&(l, _)| l != line) {
                    expected.push((line, hit));
                }
            }
            let mut found = Vec::new();
            d.find_lines(query, options, 1 << 30, LineEnds::Editor, &cancel, &mut |l| {
                found.push(l.clone());
                Ok(false)
            })?;
            assert_eq!(found.len(), expected.len(), "{query}");
            assert!(found.len() > 1000, "{query}");
            for (f, (line, hit)) in found.iter().zip(&expected) {
                assert_eq!((f.line, &f.first), (*line, hit), "{query}");
                assert_eq!(f.start, d.line_start(*line), "{query}");
                let next = if line + 1 < d.line_count() { d.line_start(line + 1) } else { d.len() };
                let mut want = d.read(f.start, (next - f.start) as usize)?;
                while matches!(want.last(), Some(b'\n' | b'\r')) {
                    want.pop();
                }
                assert_eq!(f.text, want, "{query} line {line}");
            }
        }
        Ok(())
    }
}
