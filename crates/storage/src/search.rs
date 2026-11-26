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
    borrow::Cow,
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
    /// A pattern on a large text that never matches a line feed, so every match
    /// lies within one line: pieces of the text are searched on several
    /// threads with `pattern`, each read on to the end of its last line.
    lined: bool,
    /// A `lined` pattern that never matches empty text, so Replace All can
    /// splice its matches as it does plain text's.
    solid: bool,
    cancel: &'a AtomicBool,
    scope: Range<u64>,
}
/// How far past its piece a thread reads to finish the piece's last line.
/// A line longer than this is searched by the streaming engine instead.
const LINE_REACH: u64 = 4 * CHUNK;
/// Whether every match of the pattern is at least one byte long, as parsed
/// with the options the search compiles it with.
fn never_empty(pattern: &str, case_insensitive: bool) -> bool {
    regex_syntax::ParserBuilder::new()
        .case_insensitive(case_insensitive)
        .multi_line(true)
        .crlf(true)
        .utf8(false)
        .build()
        .parse(pattern)
        .is_ok_and(|h| h.properties().minimum_len().is_some_and(|n| n > 0))
}
/// Whether the pattern can match a line feed anywhere (a literal, a class
/// holding it, or anything that contains one), as parsed with the options
/// the search compiles it with. A pattern that fails to parse here is taken
/// to cross lines, so it keeps the streaming engine.
fn crosses_lines(pattern: &str, case_insensitive: bool) -> bool {
    use regex_syntax::hir::{Class, Hir, HirKind};
    fn feed(h: &Hir) -> bool {
        match h.kind() {
            HirKind::Empty | HirKind::Look(_) => false,
            HirKind::Literal(l) => l.0.contains(&b'\n'),
            HirKind::Class(Class::Unicode(c)) => c.ranges().iter().any(|r| r.start() <= '\n' && '\n' <= r.end()),
            HirKind::Class(Class::Bytes(c)) => c.ranges().iter().any(|r| r.start() <= b'\n' && b'\n' <= r.end()),
            HirKind::Repetition(r) => feed(&r.sub),
            HirKind::Capture(c) => feed(&c.sub),
            HirKind::Concat(v) | HirKind::Alternation(v) => v.iter().any(feed),
        }
    }
    regex_syntax::ParserBuilder::new()
        .case_insensitive(case_insensitive)
        .multi_line(true)
        .crlf(true)
        .utf8(false)
        .build()
        .parse(pattern)
        .map_or(true, |h| feed(&h))
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
/// Its text is lent for the call that receives it; `owned` keeps a copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundLine<'a> {
    /// Counted from 0, as `Document::line_of` counts.
    pub line: u64,
    /// Where the line starts in the document.
    pub start: u64,
    /// The first match on the line.
    pub first: Range<u64>,
    /// The line's text without its line break, at most the `widest` bytes
    /// asked for. A longer line gives the part around its first match.
    pub text: Cow<'a, [u8]>,
    /// Where `text` starts in the document: `start`, unless the line was longer than `widest`.
    pub text_start: u64,
    /// Whether the line goes on past the end of `text`.
    pub cut: bool,
}
impl FoundLine<'_> {
    /// The same line with its own copy of the text.
    pub fn owned(&self) -> FoundLine<'static> {
        FoundLine {
            line: self.line,
            start: self.start,
            first: self.first.clone(),
            text: Cow::Owned(self.text.to_vec()),
            text_start: self.text_start,
            cut: self.cut,
        }
    }
}
/// The text kept of a matching line: where the line starts, where the kept
/// text starts, the text, and whether the line goes on past it.
type Kept = (u64, u64, Vec<u8>, bool);
/// A matching line as one thread found it inside its piece of the document.
struct PieceLine {
    /// Line breaks between the piece's first byte and the match.
    breaks: u64,
    first: Range<u64>,
    /// When the bytes the thread held contained the whole line: where the line
    /// starts, where its kept text starts, that text's place in the piece's
    /// `text`, and whether the line goes on past it.
    found: Option<(u64, u64, Range<u32>, bool)>,
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
    /// The kept text of every line in `lines`, one after another, so a piece
    /// of many matching lines makes one allocation rather than one a line.
    text: Vec<u8>,
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

/// The lines of matches that arrive in order, read forward through the
/// document once: each match's line number, where its line starts, and its
/// text. It holds the bytes from the current line's start (or, on a line longer
/// than `widest`, from a quarter of `widest` before the match) to as far as it
/// has read, so many matches cost one pass over the text, whatever their number.
struct Lines<'a> {
    d: &'a Document,
    ends: LineEnds,
    widest: usize,
    /// Bytes of the document from `at`.
    buf: Vec<u8>,
    at: u64,
    /// Line ends are counted up to here, and `breaks` is their number.
    counted: u64,
    breaks: u64,
    /// Where the line holding `counted` starts.
    start: u64,
}
impl<'a> Lines<'a> {
    /// From `from`, which lies on the line starting at `start`, with `breaks` line ends before it.
    fn new(d: &'a Document, ends: LineEnds, widest: usize, from: u64, start: u64, breaks: u64) -> Self {
        Self { d, ends, widest, buf: Vec::new(), at: start, counted: from, breaks, start }
    }
    /// Reads forward until the bytes reach `upto`, and one byte past it to tell a
    /// lone CR from the CR of a CRLF.
    fn fill(&mut self, upto: u64) -> io::Result<()> {
        let want = (upto + 1).min(self.d.len());
        while self.at + (self.buf.len() as u64) < want {
            let from = self.at + self.buf.len() as u64;
            let n = (want - from).max(CHUNK.min(self.d.len() - from)) as usize;
            let old = self.buf.len();
            self.buf.resize(old + n, 0);
            self.d.read_into(from, &mut self.buf[old..])?;
        }
        Ok(())
    }
    /// The number of line ends before `hit` (never before an earlier one), and
    /// where its line starts.
    fn number(&mut self, hit: u64) -> io::Result<u64> {
        if hit > self.counted {
            self.fill(hit)?;
            let (from, to) = ((self.counted - self.at) as usize, (hit - self.at) as usize);
            self.breaks += self.ends.count(&self.buf[from..to], self.buf.get(to).copied());
            if let Some(b) = self.ends.begin(&self.buf[..to], to).filter(|&b| b > from || self.at + b as u64 > self.start) {
                self.start = self.at + b as u64;
            }
            self.counted = hit;
        }
        Ok(self.breaks)
    }
    /// The line holding `hit`, numbered by `number` just before: its start,
    /// where its text starts, its text without its line end (at most `widest`
    /// bytes), and whether the text was cut there.
    fn text(&mut self, hit: u64) -> io::Result<Kept> {
        let widest = self.widest as u64;
        let len = self.d.len();
        let from = if hit - self.start < widest / 2 { self.start } else { hit - widest / 4 };
        let limit = (from + widest).min(len);
        // Read forward only as far as the line's end, or the limit.
        let mut end = None;
        let mut look = hit;
        while end.is_none() && look < limit {
            // Twice as far from the text's start each time, at least 4 KiB.
            let next = (look + (look - from).max(4096)).min(limit);
            self.fill(next)?;
            let (a, b) = ((look - self.at) as usize, (next - self.at) as usize);
            end = self.ends.end(&self.buf[a..b], 0, false).map(|e| look + e as u64);
            look = next;
        }
        let stop = end.unwrap_or(limit);
        let text = self.buf[(from - self.at) as usize..(stop - self.at) as usize].to_vec();
        let cut = end.is_none() && limit < len;
        // Let go of what no later match can need, once it is most of the
        // buffer, so the bytes still needed move rarely.
        let keep = if hit - self.start > widest { hit - widest / 4 } else { self.start };
        let dead = keep.saturating_sub(self.at) as usize;
        if dead >= CHUNK as usize && dead * 2 >= self.buf.len() {
            self.buf.drain(..dead);
            self.at = keep;
        }
        Ok((self.start, from, text, cut))
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
        let (mut lined, mut solid) = (false, false);
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
            let whole = || {
                RegexBuilder::new(&s)
                    .case_insensitive(!opt.case_sensitive)
                    .multi_line(true)
                    .crlf(true)
                    .size_limit(16 * 1024 * 1024)
                    .build()
                    .map_err(|e| invalid(e.to_string()))
            };
            if opt.regex && (d.len() > FULL_REGEX || scope.start != 0 || scope.end != d.len()) {
                streamed = Some(streaming::compile(&s, opt.case_sensitive)?);
                // Pieces may be searched apart only when no match can cross the
                // scope's end either: it is the text's end or follows a line feed.
                let mut before = [0u8];
                let ends_a_line = scope.end == d.len()
                    || (scope.end > 0 && d.read_into(scope.end - 1, &mut before).is_ok() && before[0] == b'\n');
                lined = !opt.backwards && ends_a_line && !crosses_lines(&s, !opt.case_sensitive);
                solid = lined && never_empty(&s, !opt.case_sensitive);
                if lined {
                    Some(whole()?)
                } else {
                    None
                }
            } else {
                Some(whole()?)
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
            lined,
            solid,
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
    fn scan(&self, range: Range<u64>, captures: bool, outer: &mut Visitor<'_>) -> io::Result<bool> {
        if range.is_empty() {
            return Ok(false);
        }
        // The streaming engine passes over an empty match that starts where the
        // match before it ended. A pattern searched in pieces does the same,
        // across pieces too, so a text finds the same matches either way.
        let mut prior = None;
        let mut adjacent = |hit: Range<u64>, groups: Option<&Groups<'_>>| {
            if hit.is_empty() && prior == Some(hit.start) {
                return Ok(false);
            }
            prior = Some(hit.end);
            outer(hit, groups)
        };
        let visit: &mut Visitor<'_> = if self.lined { &mut adjacent } else { outer };
        if let Some(re) = &self.streamed {
            if !self.lined || self.opt.backwards {
                return streaming::scan(self, re, range, captures, visit);
            }
        }
        if self.opt.regex && !self.lined {
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
            let mut step = |hit: Range<u64>, groups: Option<&Groups<'_>>| {
                floor = hit.end;
                visit(hit, groups)
            };
            let read = |at, out: &mut [u8]| self.d.read_into(at, out);
            let stop = if self.lined {
                self.lined_part(left, right, from, &mut bytes, read, captures, &mut step)?
            } else {
                let origin = self.load(left, right, &mut bytes, read)?;
                self.matches(&bytes, origin, &(from..right), captures, &mut step)?
            };
            if stop {
                return Ok(true);
            }
        }
        if direct == count {
            return Ok(false);
        }
        // The rest is searched by several threads, each with its own handle to
        // the file. Their matches are replayed here in document order. A
        // forward scan reads with a lead (`lead.rs`).
        let rest = (count - direct) as usize;
        let lead = if self.opt.backwards { None } else { self.d.lead(self.bounds(&range, direct).0) };
        // A pattern's capture groups travel with its matches, as (start, length)
        // from the chunk's start; an unmatched group is `NO_GROUP`.
        const NO_GROUP: (u32, u32) = (u32::MAX, 0);
        let groups_per_hit = if captures && self.opt.regex {
            self.pattern.as_ref().map_or(0, |p| p.captures_len())
        } else {
            0
        };
        let found = |k: usize, (reader, bytes, search): &mut (parallel::Reader<'_>, Vec<u8>, Search<'_>)| {
            search.check()?;
            let (left, right) = search.bounds(&range, direct + k as u64);
            reader.ahead(left)?;
            let (mut hits, mut groups) = (Vec::new(), Vec::new());
            let mut keep = |hit: Range<u64>, g: Option<&Groups<'_>>| {
                hits.push(((hit.start - left) as u32, (hit.end - hit.start) as u32));
                if let Some(g) = g.filter(|_| groups_per_hit > 0) {
                    for i in 0..groups_per_hit {
                        groups.push(g(i).map_or(NO_GROUP, |r| ((r.start - left) as u32, (r.end - r.start) as u32)));
                    }
                }
                Ok(false)
            };
            let read = |at, out: &mut [u8]| reader.read_into(at, out);
            if search.lined {
                search.lined_part(left, right, left, bytes, read, captures, &mut keep)?;
            } else {
                let origin = search.load(left, right, bytes, read)?;
                search.matches(bytes, origin, &(left..right), false, &mut keep)?;
            }
            Ok((hits, groups))
        };
        parallel::ordered(
            rest,
            workers.min(rest),
            || (parallel::Reader::led(self.d, lead.as_ref()), Vec::new(), self.fork()),
            found,
            |k, (hits, groups): (Vec<(u32, u32)>, Vec<(u32, u32)>)| {
                let (left, right) = self.bounds(&range, direct + k as u64);
                let from = floor;
                if !self.opt.backwards && hits.first().is_some_and(|h| left + u64::from(h.0) < from) {
                    // The thread started at the chunk's first byte, inside the
                    // previous match. Search this chunk again from its end.
                    if from >= right {
                        return Ok(false);
                    }
                    let mut step = |hit: Range<u64>, groups: Option<&Groups<'_>>| {
                        floor = hit.end;
                        visit(hit, groups)
                    };
                    let read = |at, out: &mut [u8]| self.d.read_into(at, out);
                    if self.lined {
                        return self.lined_part(left, right, from, &mut bytes, read, captures, &mut step);
                    }
                    let origin = self.load(left, right, &mut bytes, read)?;
                    return self.matches(&bytes, origin, &(from..right), false, &mut step);
                }
                for (j, (start, len)) in hits.into_iter().enumerate() {
                    let start = left + u64::from(start);
                    floor = start + u64::from(len);
                    let own = groups.get(j * groups_per_hit..(j + 1) * groups_per_hit).unwrap_or(&[]);
                    let group = |i: usize| {
                        own.get(i)
                            .filter(|g| **g != NO_GROUP)
                            .map(|g| left + u64::from(g.0)..left + u64::from(g.0) + u64::from(g.1))
                    };
                    let groups: Option<&Groups<'_>> = (groups_per_hit > 0).then_some(&group);
                    if visit(start..floor, groups)? {
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
            // A piece whose last line runs past `LINE_REACH` is searched by the streaming engine.
            streamed: if self.lined { self.streamed.clone() } else { None },
            lined: self.lined,
            solid: self.solid,
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
    /// `load` for a `lined` search: the piece and on to the line feed that ends
    /// its last line, so every match starting in it is whole in `bytes`.
    /// Returns where `bytes` starts and whether that line end was reached
    /// within `LINE_REACH`; when it was not, `bytes` still holds the piece and
    /// the byte after it.
    fn load_lines(
        &self,
        left: u64,
        right: u64,
        bytes: &mut Vec<u8>,
        mut read: impl FnMut(u64, &mut [u8]) -> io::Result<()>,
    ) -> io::Result<(u64, bool)> {
        let len = self.d.len();
        let origin = left.saturating_sub(4);
        // The line of the piece's last byte ends at the first line feed from there.
        let last = (right.min(len).max(left + 1) - 1 - origin) as usize;
        let mut end = (right + 64 * 1024).min(len);
        bytes.clear();
        loop {
            let old = bytes.len();
            bytes.resize((end - origin) as usize, 0);
            read(origin + old as u64, &mut bytes[old..])?;
            let from = last.max(old.saturating_sub(1)).min(bytes.len());
            if end == len || memchr::memchr(b'\n', &bytes[from..]).is_some() {
                return Ok((origin, true));
            }
            if end >= right + LINE_REACH {
                return Ok((origin, false));
            }
            end = (end + (end - right)).min(right + LINE_REACH).min(len);
        }
    }
    /// The matches starting in `range`, found by the streaming engine.
    fn streamed_part(&self, range: Range<u64>, visit: &mut Visitor<'_>) -> io::Result<bool> {
        let re = self.streamed.as_ref().ok_or_else(|| invalid("No streaming pattern"))?;
        streaming::scan(self, re, range, false, visit)
    }
    /// Search the piece `left..right` from `from` for a `lined` search on this
    /// thread: in the bytes read on to its last line's end, or by the
    /// streaming engine when that line is longer than `LINE_REACH`.
    #[allow(clippy::too_many_arguments)]
    fn lined_part(
        &self,
        left: u64,
        right: u64,
        from: u64,
        bytes: &mut Vec<u8>,
        read: impl FnMut(u64, &mut [u8]) -> io::Result<()>,
        captures: bool,
        visit: &mut Visitor<'_>,
    ) -> io::Result<bool> {
        let (origin, whole) = self.load_lines(left, right, bytes, read)?;
        if whole {
            self.matches(bytes, origin, &(from..right), captures, visit)
        } else {
            let re = self.streamed.as_ref().ok_or_else(|| invalid("No streaming pattern"))?;
            streaming::scan(self, re, from..right, captures, visit)
        }
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
    /// Read the piece `left..right` with `read` and search it from `from` as
    /// `piece` does. A `lined` search reads on to its last line's end; when that
    /// line is longer than `LINE_REACH`, the streaming engine finds the piece's
    /// matches and their lines' text is read later, around each match.
    #[allow(clippy::too_many_arguments)]
    fn lined_piece(
        &self,
        left: u64,
        right: u64,
        from: u64,
        widest: usize,
        ends: LineEnds,
        bytes: &mut Vec<u8>,
        read: impl FnMut(u64, &mut [u8]) -> io::Result<()>,
    ) -> io::Result<Piece> {
        if !self.lined {
            let origin = self.load(left, right, bytes, read)?;
            return self.piece(bytes, origin, left, right, from, widest, ends);
        }
        let (origin, whole) = self.load_lines(left, right, bytes, read)?;
        if whole {
            return self.piece(bytes, origin, left, right, from, widest, ends);
        }
        let re = self.streamed.as_ref().ok_or_else(|| invalid("No streaming pattern"))?;
        let rel = |at: u64| (at - origin) as usize;
        let (mut lines, mut last_end): (Vec<PieceLine>, _) = (Vec::new(), None);
        let (mut counted, mut breaks) = (rel(left), 0u64);
        let buf = &bytes[..];
        streaming::scan(self, re, from..right, false, &mut |hit, _| {
            let s = rel(hit.start);
            breaks += ends.count(&buf[counted..s], buf.get(s).copied());
            counted = s;
            last_end = Some(hit.end);
            if !lines.last().is_some_and(|l| l.breaks == breaks) {
                lines.push(PieceLine { breaks, first: hit, found: None });
            }
            Ok(false)
        })?;
        let end = rel(right);
        breaks += ends.count(&buf[counted..end], buf.get(end).copied());
        Ok(Piece { breaks, lines, text: Vec::new(), last_end })
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
        let mut text = Vec::new();
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
                    let at = text.len() as u32;
                    text.extend_from_slice(&buf[kept.start as usize..kept.end as usize]);
                    Some((origin + b as u64, origin + kept.start, at..text.len() as u32, kept.end < e as u64))
                }
                _ => None,
            };
            lines.push(PieceLine { breaks, first: hit, found });
            Ok(false)
        })?;
        let end = rel(right);
        breaks += ends.count(&buf[counted..end], buf.get(end).copied());
        Ok(Piece { breaks, lines, text, last_end })
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
        let mut emit = |line: u64, first: Range<u64>, found: Option<(u64, u64, Cow<'_, [u8]>, bool)>| -> io::Result<bool> {
            if last == Some(line) {
                return Ok(false);
            }
            let (start, text_start, text, cut) = match found {
                Some(found) => found,
                None => {
                    let (start, text_start, text, cut) = self.line_text(first.start, widest, ends)?;
                    (start, text_start, Cow::Owned(text), cut)
                }
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
        if search.opt.regex && !search.lined {
            // A pattern searches the whole text or streams it: take its matches
            // in order and count the lines up to each in one forward pass.
            let start = self.line_text(scope.start, 0, ends)?.0;
            let mut lines = Lines::new(self, ends, widest, scope.start, start, breaks_before);
            let mut shown = None;
            return search.scan(scope.start..scope.end + 1, false, &mut |hit, _| {
                let line = lines.number(hit.start)?;
                // A line is shown once, at its first match.
                if shown == Some(line) {
                    return Ok(false);
                }
                shown = Some(line);
                let (start, text_start, text, cut) = lines.text(hit.start)?;
                emit(line, hit, Some((start, text_start, Cow::Owned(text), cut)))
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
                let read = |at, out: &mut [u8]| self.read_into(at, out);
                search.lined_piece(left, right, floor.min(right), widest, ends, &mut bytes, read)?
            } else {
                piece
            };
            for l in piece.lines {
                let found = l.found.map(|(start, text_start, at, cut)| {
                    (start, text_start, Cow::Borrowed(&piece.text[at.start as usize..at.end as usize]), cut)
                });
                if emit(base + l.breaks, l.first, found)? {
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
                let read = |at, out: &mut [u8]| self.read_into(at, out);
                let piece = search.lined_piece(left, right, left, widest, ends, &mut own, read)?;
                if take(k, piece)? {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        let lead = self.lead(range.start);
        parallel::ordered(
            count,
            workers,
            || (parallel::Reader::led(self, lead.as_ref()), Vec::new(), search.fork()),
            |k, (reader, own, search): &mut (parallel::Reader<'_>, Vec<u8>, Search<'_>)| {
                search.check()?;
                let (left, right) = search.bounds(&range, k as u64);
                reader.ahead(left)?;
                search.lined_piece(left, right, left, widest, ends, own, |at, out| reader.read_into(at, out))
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
        let lead = self.lead(0);
        parallel::ordered(
            chunks,
            parallel::threads().min(chunks),
            || (parallel::Reader::led(self, lead.as_ref()), Vec::new()),
            |k, (reader, bytes): &mut (parallel::Reader<'_>, Vec<u8>)| {
                if cancel.load(Ordering::Relaxed) {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "Count cancelled"));
                }
                let left = k as u64 * CHUNK;
                let right = (left + CHUNK).min(upto);
                reader.ahead(left)?;
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
        let template = replacing::Template::new(&search, replacement);
        // A pattern searched in pieces whose replacement uses no group is
        // spliced as plain text is: the new text shares the old one's pieces.
        let spliced = if options.regex {
            template.literal().filter(|_| search.solid)
        } else {
            Some(replacement.as_bytes().to_vec())
        };
        let (count, change) = if let Some(literal) = spliced {
            splicing::replace(self, &search, &literal, &mut observe)?
        } else {
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
            whole.push(l.owned());
            Ok(false)
        })?;
        for at in [0, 1, 3, 1_000_001, d.len() / 2 + 5, d.len() - 3, d.len()] {
            assert_eq!(d.count_breaks(at, &cancel)?, d.line_of(at), "breaks before {at}");
        }
        let scope = d.len() / 3 + 2..2 * d.len() / 3;
        let mut part = Vec::new();
        d.find_lines_in("needle", options, scope.clone(), d.count_breaks(scope.start, &cancel)?, 1 << 20, LineEnds::Editor, &cancel, &mut |l| {
            part.push(l.owned());
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

    /// A pattern that cannot match a line feed is searched in pieces on several
    /// threads; it finds, numbers and replaces exactly what one pass over the
    /// whole text does. The text crosses the size where patterns stream, has
    /// CR, LF and CRLF line ends, lines ending at piece edges, and a line
    /// longer than a thread reads past its piece.
    #[test]
    fn lined_patterns_match_one_pass() -> io::Result<()> {
        let piece = CHUNK as usize;
        let breaks: [&[u8]; 3] = [b"\n", b"\r\n", b"\r"];
        let mut text = Vec::new();
        let mut n = 0usize;
        while text.len() < FULL_REGEX as usize + 2 * piece {
            if n == 4000 {
                // One line of 6 MiB with matches spread along it.
                for k in 0..(6 * piece / 64) {
                    text.extend_from_slice(if k % 7 == 0 { b"key=12 needle 2026 " } else { b"zzzzzzzzzzzzzzzzzzz " });
                    text.resize(text.len() + 44, b'q');
                }
                text.push(b'\n');
            }
            let line: &[u8] = match n % 5 {
                0 => b"2026-11-21 GET key=503 Needle",
                1 => b"plain words needle x",
                2 => b"",
                3 => b"x2026 id=7 NEEDLE end",
                _ => b"word, word; words",
            };
            text.extend_from_slice(line);
            text.extend_from_slice(breaks[n % 3]);
            n += 1;
            let edge = (text.len() / piece + 1) * piece;
            if edge - text.len() < 40 {
                text.resize(edge - 1, b'y');
                text.push(b'\n');
            }
        }
        let mut d = Document::empty();
        d.replace(0, 0, &text)?;
        let cancel = AtomicBool::new(false);
        let patterns = ["^2026", "ne+dle$", r"(\w+)=(\d+)", r"\bword\b", "x*", "$", r"(?i)needle", r"\d{4}-\d\d"];
        for (pattern, case_sensitive) in patterns.iter().flat_map(|p| [(*p, true), (*p, false)]) {
            let options = SearchOptions { regex: true, case_sensitive, wrap: false, ..Default::default() };
            let search = Search::new(&d, pattern, options, &cancel, 0..d.len())?;
            assert!(search.lined, "{pattern} should be searched in pieces");
            let re = RegexBuilder::new(pattern).case_insensitive(!case_sensitive).multi_line(true).crlf(true).build().unwrap();
            let want: Vec<_> = re.find_iter(&text).map(|m| m.start() as u64..m.end() as u64).collect();
            let got = d.find_all_bounded(pattern, options, usize::MAX, &cancel)?;
            assert_eq!(got.len(), want.len(), "{pattern} {case_sensitive}");
            assert!(got == want, "{pattern} {case_sensitive}: matches differ");
            // Lines: each numbered as the line index numbers it, once.
            let mut lines = Vec::new();
            d.find_lines(pattern, options, 64, LineEnds::Editor, &cancel, &mut |l| {
                lines.push((l.line, l.first.clone()));
                Ok(false)
            })?;
            let mut expected = Vec::new();
            for hit in &want {
                let line = d.line_of(hit.start);
                // An empty match after the last line break is in no line.
                if hit.start == d.len() && d.line_start(line) == d.len() {
                    continue;
                }
                if expected.last().is_none_or(|(l, _)| *l != line) {
                    expected.push((line, hit.clone()));
                }
            }
            assert!(lines == expected, "{pattern} {case_sensitive}: lines differ");
        }
        // Replace All with groups gives what one pass gives.
        let re = RegexBuilder::new(r"(\w+)=(\d+)").multi_line(true).crlf(true).build().unwrap();
        let want = re.replace_all(&text, &b"${2}:${1}"[..]).into_owned();
        let options = SearchOptions { regex: true, case_sensitive: true, wrap: false, ..Default::default() };
        let count = d.replace_all(r"(\w+)=(\d+)", "${2}:${1}", options)?;
        assert_eq!(count, re.find_iter(&text).count());
        assert!(d.read(0, d.len() as usize)? == want, "replaced text differs");
        // Without groups, a pattern's matches are spliced as plain text's are.
        for (pattern, with) in [("ne+dle", "N$$"), (r"\d{4}-\d\d", "")] {
            let search = Search::new(&d, pattern, options, &cancel, 0..d.len())?;
            assert!(search.solid, "{pattern} should be spliced");
            let re = RegexBuilder::new(pattern).multi_line(true).crlf(true).build().unwrap();
            let before = d.read(0, d.len() as usize)?;
            let want = re.replace_all(&before, with.as_bytes()).into_owned();
            let count = d.replace_all(pattern, with, options)?;
            assert_eq!(count, re.find_iter(&before).count(), "{pattern}");
            assert!(d.read(0, d.len() as usize)? == want, "{pattern}: spliced text differs");
            d.undo();
            assert!(d.read(0, d.len() as usize)? == before, "{pattern}: undo differs");
        }
        // A pattern that can match a line feed keeps the streaming engine.
        for p in [r"a\sb", "[^x]", r"(?s)a.b", "a\nb"] {
            assert!(!Search::new(&d, p, options, &cancel, 0..d.len())?.lined, "{p}");
        }
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
                found.push(l.owned());
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
