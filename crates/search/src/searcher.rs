//! Which lines a search shows: the matching ones (or with `-v` the others),
//! the lines of context around them, and breaks between groups that are not
//! next to each other.
//!
//! The search moves from match to match, so the lines between matches are
//! only counted, never searched one by one; with `-v` a whole run of lines
//! between two matching lines is selected at once. It works on text held
//! whole, or on a stream given piece by piece: each piece ends at a line end
//! (or is the last), and the caller keeps, at the front of the next piece,
//! the lines from `Searcher::keep_from` on, which the next piece may need as
//! context before its first match.
use crate::matcher::Query;

/// What is shown around the lines a search selects.
#[derive(Clone, Copy, Debug, Default)]
pub struct Config {
    /// Select the lines that do not match (`-v`).
    pub invert: bool,
    /// Lines of context before and after (`-B`, `-A`).
    pub before: usize,
    pub after: usize,
    /// Every line, selected or not, with no breaks (`--passthru`).
    pub passthru: bool,
    /// At most this many selected lines (`-m`); with `-U` the lines of one match count once.
    pub max_count: Option<u64>,
    /// Stop at the first line not selected after one that was (`--stop-on-nonmatch`).
    pub stop_on_nonmatch: bool,
    /// Count line numbers.
    pub numbered: bool,
}

/// Where the lines go. Each method returns false to stop the search.
pub trait Sink {
    /// A selected line, or with `-U` the lines one match covers: the number of
    /// its first line (0 when not counted), where it starts in the input, and
    /// its text without the last line end; `ended` says whether a line end
    /// follows it in the input (only the input's last line may lack one).
    fn matched(&mut self, number: u64, offset: u64, text: &[u8], ended: bool) -> bool;
    /// A line shown for context, before or after a selected one.
    fn context(&mut self, number: u64, offset: u64, text: &[u8], after: bool, ended: bool) -> bool;
    /// Between groups of lines that are not next to each other.
    fn context_break(&mut self) -> bool;
}

/// Which matching lines may be selected, by line number; lw's --near uses it.
pub type Keep<'q> = &'q (dyn Fn(u64) -> bool + Sync);

pub struct Searcher<'q> {
    query: &'q Query,
    config: Config,
    keep: Option<Keep<'q>>,
    line_end: u8,
    /// Where the current piece starts in the input.
    base: u64,
    /// Line ends counted before `counted_at` (an offset in the input).
    counted_at: u64,
    counted: u64,
    /// Where the line after the last line given to the sink starts.
    shown_to: u64,
    after_left: usize,
    selected: u64,
    any_shown: bool,
    done: bool,
}

impl<'q> Searcher<'q> {
    pub fn new(query: &'q Query, config: Config) -> Self {
        Self {
            query,
            config,
            keep: None,
            line_end: query.line_end(),
            base: 0,
            counted_at: 0,
            counted: 0,
            shown_to: 0,
            after_left: 0,
            selected: 0,
            any_shown: false,
            done: false,
        }
    }

    /// Starts the search `offset` bytes into the input, after `lines` line
    /// ends, so a part of a file (a time span) is numbered as the whole is,
    /// and no context is taken from before it.
    pub fn starting_at(mut self, offset: u64, lines: u64) -> Self {
        self.counted_at = offset;
        self.counted = lines;
        self.shown_to = offset;
        self.base = offset;
        self
    }

    /// Selects a matching line only when `keep` says yes for its number (lines must be counted).
    pub fn keeping(mut self, keep: Keep<'q>) -> Self {
        self.keep = Some(keep);
        self.config.numbered = true;
        self
    }

    /// How many lines (or with `-U` blocks) were selected.
    pub fn selected(&self) -> u64 {
        self.selected
    }

    /// Whether the search has nothing more to do: the sink stopped it, or
    /// `-m` was reached and the context after it shown.
    pub fn done(&self) -> bool {
        self.done
    }

    /// Searches text held whole.
    pub fn search(&mut self, text: &[u8], sink: &mut impl Sink) {
        self.piece(text, 0, 0, text.len(), true, sink);
    }

    /// Searches `buf[start..end]` of a stream. `buf` starts `base` bytes into
    /// the input; `buf[..start]` was searched before and is kept for context;
    /// `end` follows a line end, or with `last` is the input's end.
    pub fn piece(&mut self, buf: &[u8], base: u64, start: usize, end: usize, last: bool, sink: &mut impl Sink) {
        self.base = base;
        let text = &buf[..end];
        let mut at = start;
        while !self.done {
            let next = if self.limit_reached() { None } else { self.next_selected(text, at) };
            let limit = next.map_or(text.len(), |(first, _)| first);
            self.show_after(text, at, limit, sink);
            let Some((first, last_line_end)) = next else { break };
            if self.done {
                break;
            }
            if self.config.stop_on_nonmatch && self.selected > 0 && self.abs(first) > self.shown_to {
                self.done = true;
                break;
            }
            self.show_before(text, first, sink);
            if self.done {
                break;
            }
            let after_block = (last_line_end + 1).min(text.len());
            if self.query.multiline() && !self.config.invert {
                self.select(text, first, last_line_end, sink);
            } else {
                let mut line = first;
                while line < after_block && !self.done && !self.limit_reached() {
                    let line_end = self.line_end_of(text, line);
                    self.select(text, line, line_end, sink);
                    line = line_end + 1;
                }
            }
            at = after_block;
            if at >= text.len() {
                // Context after the last line may still come in the next piece.
                break;
            }
        }
        if self.limit_reached() && (self.after_left == 0 || last) {
            self.done = true;
        }
        // Count up to the piece's end, so the caller may drop what it does not keep.
        if self.config.numbered && self.abs(text.len()) > self.counted_at {
            let from = (self.counted_at - self.base) as usize;
            self.counted += memchr::memchr_iter(self.line_end, &text[from..]).count() as u64;
            self.counted_at = self.abs(text.len());
        }
    }

    /// Where the next piece must start in `buf`: the first of the lines
    /// before `end` that a match at the next piece's start could show as
    /// context, and never after a line not yet counted.
    pub fn keep_from(&self, buf: &[u8], end: usize) -> usize {
        let wanted = if self.config.passthru { 0 } else { self.config.before };
        let mut from = end;
        for _ in 0..wanted {
            if from == 0 {
                break;
            }
            from = self.line_start_of(buf, from - 1);
        }
        from
    }

    fn abs(&self, at: usize) -> u64 {
        self.base + at as u64
    }

    fn rel(&self, at: u64) -> usize {
        at.saturating_sub(self.base) as usize
    }

    fn limit_reached(&self) -> bool {
        self.config.max_count.is_some_and(|most| self.selected >= most)
    }

    /// The position of the line end that ends the line holding `at`, or the text's end.
    fn line_end_of(&self, text: &[u8], at: usize) -> usize {
        if at >= text.len() {
            return text.len();
        }
        memchr::memchr(self.line_end, &text[at..]).map_or(text.len(), |e| at + e)
    }

    fn line_start_of(&self, text: &[u8], at: usize) -> usize {
        memchr::memrchr(self.line_end, &text[..at]).map_or(0, |e| e + 1)
    }

    /// The next selected lines at or after the line start `at`: the first
    /// one's start and the position of the last one's line end.
    fn next_selected(&mut self, text: &[u8], at: usize) -> Option<(usize, usize)> {
        if at >= text.len() {
            return None;
        }
        if self.config.invert {
            // The run of lines from the first non-matching one up to the next matching line.
            let mut line = at;
            loop {
                if line >= text.len() {
                    return None;
                }
                match self.next_match(text, line) {
                    None => return Some((line, self.end_of_text_line(text))),
                    Some((first, _)) if first > line => return Some((line, first - 1)),
                    Some((_, last_line_end)) => line = last_line_end + 1,
                }
            }
        }
        self.next_match(text, at)
    }

    /// The position of the last line end of the text, or the text's end if it has none at the end.
    fn end_of_text_line(&self, text: &[u8]) -> usize {
        if text.last() == Some(&self.line_end) { text.len() - 1 } else { text.len() }
    }

    /// The lines of the next match at or after the line start `at`: the first
    /// line's start and the position of the last line's end. With `-U`, a
    /// further match that starts before that line end joins the block.
    /// Only lines the keep test allows count; a match on another line is passed over.
    fn next_match(&mut self, text: &[u8], mut at: usize) -> Option<(usize, usize)> {
        loop {
            let (first, last) = self.next_raw_match(text, at)?;
            let Some(keep) = self.keep else { return Some((first, last)) };
            let number = self.number(text, first);
            if keep(number) {
                return Some((first, last));
            }
            at = (last + 1).min(text.len());
            if at >= text.len() {
                return None;
            }
        }
    }

    fn next_raw_match(&self, text: &[u8], at: usize) -> Option<(usize, usize)> {
        let mut from = at;
        loop {
            let (start, end) = self.query.next(text, from)?;
            if start >= text.len() {
                // An empty match after the last line end is in no line.
                return None;
            }
            let line = self.line_start_of(text, start);
            if self.query.multiline() {
                let mut last = self.line_end_of(text, end.saturating_sub(1).max(start));
                let mut after = end.max(start + 1);
                while after <= last {
                    match self.query.next(text, after) {
                        Some((s, e)) if s <= last && s < text.len() => {
                            last = last.max(self.line_end_of(text, e.saturating_sub(1).max(s)));
                            after = e.max(s + 1);
                        }
                        _ => break,
                    }
                }
                return Some((line, last));
            }
            let line_end = self.line_end_of(text, start);
            // PCRE2 cannot keep a match within a line; there the line is judged by itself.
            if !self.query.is_pcre() || self.query.within(&text[line..line_end]) {
                return Some((line, line_end));
            }
            from = line_end + 1;
            if from >= text.len() {
                return None;
            }
        }
    }

    /// The number of the line starting at `at`, counting line ends up to it.
    /// A line kept from an earlier piece lies before what was counted; its
    /// number is counted back from there.
    fn number(&mut self, text: &[u8], at: usize) -> u64 {
        if !self.config.numbered {
            return 0;
        }
        let target = self.abs(at);
        if target < self.counted_at {
            let upto = self.rel(self.counted_at).min(text.len());
            return self.counted + 1 - memchr::memchr_iter(self.line_end, &text[at..upto]).count() as u64;
        }
        if target > self.counted_at {
            let from = self.rel(self.counted_at);
            self.counted += memchr::memchr_iter(self.line_end, &text[from..at]).count() as u64;
            self.counted_at = target;
        }
        self.counted + 1
    }

    fn select(&mut self, text: &[u8], start: usize, end: usize, sink: &mut impl Sink) {
        let number = self.number(text, start);
        let offset = self.abs(start);
        if !sink.matched(number, offset, &text[start..end], end < text.len()) {
            self.done = true;
        }
        self.selected += 1;
        self.any_shown = true;
        self.shown_to = self.abs((end + 1).min(text.len()));
        self.after_left = if self.config.passthru { usize::MAX } else { self.config.after };
    }

    fn context_line(&mut self, text: &[u8], start: usize, end: usize, after: bool, sink: &mut impl Sink) {
        let number = self.number(text, start);
        if !sink.context(number, self.abs(start), &text[start..end], after, end < text.len()) {
            self.done = true;
        }
        self.any_shown = true;
        self.shown_to = self.abs((end + 1).min(text.len()));
    }

    /// The context after the last shown line: lines from where showing left
    /// off (not before `at`... unless they belong to this piece's kept part)
    /// up to `limit`, the next selected line.
    fn show_after(&mut self, text: &[u8], at: usize, limit: usize, sink: &mut impl Sink) {
        if self.after_left == 0 || !self.any_shown {
            return;
        }
        let mut line = self.rel(self.shown_to).max(if self.shown_to < self.base { at } else { 0 });
        // Past -m's limit, a line that would have been selected still shows as
        // one, as in ripgrep, but neither counts nor extends the context.
        let past_limit = self.limit_reached() && !self.config.passthru;
        while self.after_left > 0 && line < limit && line < text.len() && !self.done {
            let end = self.line_end_of(text, line);
            if end == text.len() && end == line {
                break;
            }
            let matches = self.query.within(&text[line..end]) && self.keep.map_or(true, |keep| {
                let number = self.number(text, line);
                keep(number)
            });
            let selected = past_limit && matches != self.config.invert;
            if selected {
                let number = self.number(text, line);
                if !sink.matched(number, self.abs(line), &text[line..end], end < text.len()) {
                    self.done = true;
                }
                self.shown_to = self.abs((end + 1).min(text.len()));
            } else {
                self.context_line(text, line, end, true, sink);
            }
            self.after_left = self.after_left.saturating_sub(1);
            line = end + 1;
        }
    }

    /// The context before the selected line starting at `first`, and a break
    /// first when it does not follow on from what was shown.
    fn show_before(&mut self, text: &[u8], first: usize, sink: &mut impl Sink) {
        let wanted = if self.config.passthru { usize::MAX } else { self.config.before };
        let floor = if self.shown_to >= self.base { self.rel(self.shown_to) } else { 0 };
        let mut from = first;
        let mut taken = 0;
        while taken < wanted && from > floor {
            from = self.line_start_of(text, from - 1).max(floor);
            taken += 1;
        }
        let context = self.config.before > 0 || self.config.after > 0;
        if self.any_shown && context && !self.config.passthru && self.abs(from) > self.shown_to {
            if !sink.context_break() {
                self.done = true;
                return;
            }
        }
        let mut line = from;
        while line < first && !self.done {
            let end = self.line_end_of(text, line);
            self.context_line(text, line, end, false, sink);
            line = end + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::{Query, Spec};

    struct Lines(Vec<String>);
    impl Sink for Lines {
        fn matched(&mut self, number: u64, offset: u64, text: &[u8], _ended: bool) -> bool {
            self.0.push(format!("{number}:{offset}:{}", String::from_utf8_lossy(text)));
            true
        }
        fn context(&mut self, number: u64, offset: u64, text: &[u8], _after: bool, _ended: bool) -> bool {
            self.0.push(format!("{number}-{offset}-{}", String::from_utf8_lossy(text)));
            true
        }
        fn context_break(&mut self) -> bool {
            self.0.push("--".into());
            true
        }
    }

    fn run(pattern: &str, text: &str, config: Config, multiline: bool) -> Vec<String> {
        let query = Query::build(&Spec { patterns: vec![pattern.into()], multiline, ..Spec::default() }).unwrap();
        let mut sink = Lines(Vec::new());
        Searcher::new(&query, Config { numbered: true, ..config }).search(text.as_bytes(), &mut sink);
        sink.0
    }

    /// The same search given in pieces of every possible size, as a stream would give it.
    fn streamed(pattern: &str, text: &str, config: Config) -> Vec<String> {
        let query = Query::build(&Spec { patterns: vec![pattern.into()], ..Spec::default() }).unwrap();
        let mut sink = Lines(Vec::new());
        let mut searcher = Searcher::new(&query, Config { numbered: true, ..config });
        let bytes = text.as_bytes();
        let mut buf: Vec<u8> = Vec::new();
        let (mut base, mut fed) = (0u64, 0usize);
        // Feed one line at a time, keeping what keep_from asks for.
        let mut start = 0;
        while fed < bytes.len() {
            let next = memchr::memchr(b'\n', &bytes[fed..]).map_or(bytes.len(), |e| fed + e + 1);
            buf.extend_from_slice(&bytes[fed..next]);
            fed = next;
            let last = fed == bytes.len();
            searcher.piece(&buf, base, start, buf.len(), last, &mut sink);
            let keep = searcher.keep_from(&buf, buf.len());
            buf.drain(..keep);
            base += keep as u64;
            start = buf.len();
        }
        sink.0
    }

    const TEXT: &str = "a\nb x\nc\nd\ne x\nf\ng\nh\ni\nj x\n";

    #[test]
    fn matching_lines_and_their_context() {
        let none = Config::default();
        assert_eq!(run("x", TEXT, none, false), ["2:2:b x", "5:10:e x", "10:22:j x"]);
        let around = Config { before: 1, after: 1, ..none };
        assert_eq!(
            run("x", TEXT, around, false),
            ["1-0-a", "2:2:b x", "3-6-c", "4-8-d", "5:10:e x", "6-14-f", "--", "9-20-i", "10:22:j x"]
        );
        assert_eq!(streamed("x", TEXT, around), run("x", TEXT, around, false));
        let after = Config { after: 2, ..none };
        assert_eq!(run("x", TEXT, after, false), ["2:2:b x", "3-6-c", "4-8-d", "5:10:e x", "6-14-f", "7-16-g", "--", "10:22:j x"]);
        assert_eq!(streamed("x", TEXT, after), run("x", TEXT, after, false));
    }

    #[test]
    fn inverted_limited_passed_through_and_stopped() {
        let invert = Config { invert: true, ..Config::default() };
        assert_eq!(run("[a-h]", TEXT, invert, false), ["9:20:i", "10:22:j x"]);
        assert_eq!(run("x", TEXT, Config { invert: true, after: 1, ..Config::default() }, false), [
            "1:0:a", "2-2-b x", "3:6:c", "4:8:d", "5-10-e x", "6:14:f", "7:16:g", "8:18:h", "9:20:i", "10-22-j x"
        ]);
        assert_eq!(streamed("[a-h]", TEXT, invert), run("[a-h]", TEXT, invert, false));
        // rg -m1 -A5: a match inside the trailing context prints as a match.
        let one = Config { max_count: Some(1), after: 5, ..Config::default() };
        assert_eq!(run("x", TEXT, one, false), ["2:2:b x", "3-6-c", "4-8-d", "5:10:e x", "6-14-f", "7-16-g"]);
        let most = Config { max_count: Some(2), after: 1, ..Config::default() };
        assert_eq!(run("x", TEXT, most, false), ["2:2:b x", "3-6-c", "--", "5:10:e x", "6-14-f"]);
        assert_eq!(streamed("x", TEXT, most), run("x", TEXT, most, false));
        let all = Config { passthru: true, ..Config::default() };
        assert_eq!(run("x", "a\nb x\nc", all, false), ["1-0-a", "2:2:b x", "3-6-c"]);
        let stop = Config { stop_on_nonmatch: true, ..Config::default() };
        assert_eq!(run("x", "a\nx1\nx2\nb\nx3\n", stop, false), ["2:2:x1", "3:5:x2"]);
        // The empty place after the last line end is no line.
        assert_eq!(run("^", "a\nb\n", Config::default(), false), ["1:0:a", "2:2:b"]);
        assert_eq!(run("zz", "a\nb\n", invert, false), ["1:0:a", "2:2:b"]);
    }

    #[test]
    fn only_kept_lines_are_selected_and_get_context() {
        let query = Query::build(&Spec { patterns: vec!["x".into()], ..Spec::default() }).unwrap();
        let mut sink = Lines(Vec::new());
        let keep = |number: u64| number == 5;
        Searcher::new(&query, Config { before: 1, ..Config::default() }).keeping(&keep).search(TEXT.as_bytes(), &mut sink);
        assert_eq!(sink.0, ["4-8-d", "5:10:e x"]);
    }

    #[test]
    fn a_multiline_match_is_one_block() {
        let none = Config::default();
        assert_eq!(run(r"b\nc", "a\nb\nc\nd\n", none, true), ["2:2:b\nc"]);
        // A second match starting on the block's last line joins it.
        assert_eq!(run(r"b\nc1|c2\nd", "a\nb\nc1 c2\nd\ne\n", none, true), ["2:2:b\nc1 c2\nd"]);
        // Matches never overlap, as in ripgrep: "c\nd" starts inside "b\nc".
        assert_eq!(run(r"b\nc|c\nd", "a\nb\nc\nd\ne\n", none, true), ["2:2:b\nc"]);
    }
}
