//! Highlighting for log files: severity levels, timestamps, quoted strings,
//! numbers and links, styled line by line as Scintilla asks for them. Each
//! line is styled on its own, so a view into a file of any size costs only
//! the lines on screen.
use super::{Editor, sci::*};

pub(super) const DEFAULT: u8 = 0;
pub(super) const TIME: u8 = 1;
pub(super) const ERROR: u8 = 2;
pub(super) const WARN: u8 = 3;
pub(super) const INFO: u8 = 4;
pub(super) const DEBUG: u8 = 5;
pub(super) const STRING: u8 = 6;
pub(super) const NUMBER: u8 = 7;
pub(super) const LINK: u8 = 8;
/// Names for each style, for copying as rich text.
pub(super) const NAMES: [&str; 9] = [
    "",
    "comment",
    "invalid",
    "constant.numeric",
    "entity.name",
    "comment",
    "string.quoted",
    "keyword",
    "markup.underline.link",
];
/// Lines longer than this are styled up to here; the rest stays plain.
const LONGEST: usize = 16 * 1024;

fn level(word: &[u8]) -> Option<u8> {
    let upper: Vec<u8> = word.to_ascii_uppercase();
    Some(match upper.as_slice() {
        b"ERROR" | b"ERR" | b"FATAL" | b"FAIL" | b"FAILED" | b"FAILURE" | b"CRITICAL" | b"CRIT" | b"SEVERE"
        | b"ALERT" | b"EMERG" | b"EMERGENCY" | b"PANIC" => ERROR,
        b"WARN" | b"WARNING" | b"WRN" => WARN,
        b"INFO" | b"INF" | b"NOTICE" => INFO,
        b"DEBUG" | b"DBG" | b"TRACE" | b"TRC" | b"VERBOSE" | b"VRB" => DEBUG,
        _ => return None,
    })
}

/// The style of a word: a level written as logs write one (all capitals,
/// capitalised, or any case right after `[`, `<`, `=`, `"` or `(`), or an
/// exception's name.
fn word_style(word: &[u8], before: Option<u8>) -> u8 {
    if let Some(l) = level(word) {
        let capitals = word.iter().all(|b| !b.is_ascii_lowercase());
        let capitalised = word[0].is_ascii_uppercase() && word[1..].iter().all(|b| b.is_ascii_lowercase());
        if capitals || capitalised || matches!(before, Some(b'[' | b'<' | b'=' | b'"' | b'(')) {
            return l;
        }
    }
    let exception = word.len() > 5
        && word[0].is_ascii_uppercase()
        && (word.ends_with(b"Exception") || (word.ends_with(b"Error") && word.len() > 5));
    if exception {
        return ERROR;
    }
    // Hexadecimal identifiers: trace ids, hashes, addresses.
    let hex = word.len() >= 6
        && word.iter().all(u8::is_ascii_hexdigit)
        && word.iter().any(u8::is_ascii_digit)
        && word.iter().any(u8::is_ascii_alphabetic);
    if hex || (word.len() > 2 && (word.starts_with(b"0x") || word.starts_with(b"0X")) && word[2..].iter().all(u8::is_ascii_hexdigit)) {
        return NUMBER;
    }
    DEFAULT
}

/// A run of digits and the separators of dates and times, from `at`: its end
/// and whether it is a date or a time rather than a plain number.
fn numeric(line: &[u8], at: usize) -> (usize, bool) {
    let mut end = at;
    while end < line.len() && matches!(line[end], b'0'..=b'9' | b'-' | b':' | b'.' | b',' | b'/') {
        end += 1;
    }
    // Trim separators that end the run ("12." at the end of a sentence).
    while end > at + 1 && !line[end - 1].is_ascii_digit() {
        end -= 1;
    }
    let run = &line[at..end];
    let digits = |s: &[u8]| s.iter().all(u8::is_ascii_digit);
    let date = run.len() >= 8
        && ((run.len() >= 10 && digits(&run[..4]) && run[4] == b'-' && digits(&run[5..7]) && run[7] == b'-')
            || (run.len() >= 10 && digits(&run[..2]) && run[2] == b'/' && digits(&run[3..5]) && run[5] == b'/'));
    let time = run.len() >= 5 && run.iter().filter(|&&b| b == b':').count() >= 1 && {
        let c = run.iter().position(|&b| b == b':').unwrap();
        c >= 1 && c <= 2 && run.len() >= c + 3 && digits(&run[c + 1..c + 3])
    };
    (end, date || time)
}

/// One style for each byte of `line`, without its line end.
pub(super) fn style_line(line: &[u8], out: &mut Vec<u8>) {
    let base = out.len();
    out.resize(base + line.len(), DEFAULT);
    let styles = &mut out[base..];
    let n = line.len().min(LONGEST);
    let mut i = 0;
    while i < n {
        let b = line[i];
        let before = line[..i].iter().rev().find(|c| !c.is_ascii_whitespace()).copied();
        if b == b'"' {
            // A quoted string, to its closing quote on the same line.
            let mut j = i + 1;
            while j < n && line[j] != b'"' {
                j += if line[j] == b'\\' { 2 } else { 1 };
            }
            let end = (j + 1).min(n);
            if end - i > 2 {
                // A level written as a JSON value keeps its level's style.
                let inner = &line[i + 1..end.saturating_sub(1)];
                let s = if inner.iter().all(u8::is_ascii_alphabetic) && !inner.is_empty() {
                    level(inner).unwrap_or(STRING)
                } else {
                    STRING
                };
                styles[i..end].fill(s);
            }
            i = end;
        } else if line[i..n].starts_with(b"http://") || line[i..n].starts_with(b"https://") || line[i..n].starts_with(b"file://") {
            let mut j = i;
            while j < n && !line[j].is_ascii_whitespace() && !matches!(line[j], b'"' | b'\'' | b'<' | b'>' | b')') {
                j += 1;
            }
            styles[i..j].fill(LINK);
            i = j;
        } else if b.is_ascii_digit() && !(i > 0 && (line[i - 1].is_ascii_alphanumeric() || line[i - 1] == b'_')) {
            let (mut end, dated) = numeric(line, i);
            // A date and a time joined by a space or a T read as one timestamp.
            if dated && end + 1 < n && matches!(line[end], b' ' | b'T') && line[end + 1].is_ascii_digit() {
                let (later, timed) = numeric(line, end + 1);
                if timed {
                    end = later;
                }
            }
            // A time zone or fraction directly after: Z, +01:00.
            if dated && end < n && line[end] == b'Z' {
                end += 1;
            }
            let word_end = line[end..n].iter().position(|c| !(c.is_ascii_alphanumeric() || *c == b'_')).map_or(n, |p| end + p);
            if !dated && word_end > end {
                // Digits that start a word (a hex id like 9e3779b1) style as the word does.
                let s = word_style(&line[i..word_end], before);
                styles[i..word_end].fill(if s == DEFAULT { DEFAULT } else { s });
                i = word_end;
                continue;
            }
            styles[i..end].fill(if dated { TIME } else { NUMBER });
            i = end;
        } else if b.is_ascii_alphabetic() || b == b'_' {
            let mut j = i;
            while j < n && (line[j].is_ascii_alphanumeric() || line[j] == b'_') {
                j += 1;
            }
            let s = word_style(&line[i..j], before);
            if s != DEFAULT {
                styles[i..j].fill(s);
            }
            i = j;
        } else {
            i += 1;
        }
    }
}

/// Whether text that has no other language looks like a log: most of its
/// first lines start with a date or a time and some carry a level.
pub(super) fn looks_like_log(sample: &str) -> bool {
    let lines: Vec<&str> = sample.lines().filter(|l| !l.trim().is_empty()).take(20).collect();
    if lines.len() < 3 {
        return false;
    }
    let stamped = lines
        .iter()
        .filter(|l| {
            let t = l.trim_start_matches(['[', '<', ' ']).as_bytes();
            t.first().is_some_and(u8::is_ascii_digit) && numeric(t, 0).1
        })
        .count();
    let leveled = lines.iter().any(|l| {
        let mut styles = Vec::new();
        style_line(l.as_bytes(), &mut styles);
        styles.iter().any(|&s| matches!(s, ERROR | WARN | INFO | DEBUG))
    });
    stamped * 4 >= lines.len() * 3 && leveled
}

thread_local! {
    /// Text views showing a log, by window: Scintilla asks them for styles.
    static VIEWS: std::cell::RefCell<std::collections::HashSet<isize>> = Default::default();
}

/// A Scintilla message to a view known by its window handle.
#[cfg(windows)]
unsafe fn message(hwnd: isize, m: u32, w: usize, l: isize) -> isize {
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW(hwnd as _, m, w, l) }
}
#[cfg(target_os = "macos")]
use super::mac::message;

impl Editor {
    pub(super) fn is_log(&self) -> bool {
        self.language.get().lexer == "log" && self.grammar.borrow().is_none()
    }
    /// Note whether this view now shows a log.
    pub(super) fn note_log(&self) {
        let on = self.is_log();
        VIEWS.with_borrow_mut(|v| if on { v.insert(self.hwnd()) } else { v.remove(&self.hwnd()) });
    }
}

/// Answer Scintilla's request to style a log view up to `upto`: whole lines
/// from where styling stopped. Call it while handling the request, before
/// Scintilla paints. False when `hwnd` is not a log view.
pub fn style_needed(hwnd: isize, upto: usize) -> bool {
    if !VIEWS.with_borrow(|v| v.contains(&hwnd)) {
        return false;
    }
    let send = |m: u32, w: usize, l: isize| unsafe { message(hwnd, m, w, l) };
    let from_line = send(SCI_LINEFROMPOSITION, send(SCI_GETENDSTYLED, 0, 0).max(0) as usize, 0).max(0) as usize;
    let last = send(SCI_LINEFROMPOSITION, upto, 0).max(0) as usize;
    style_lines(hwnd, from_line, last);
    true
}

/// Style the lines on screen of a log view, and a few past them. Called just
/// before the view paints, so whatever styled the text before (another
/// view of the same document, a plain pass) is replaced where it is seen.
/// False when `hwnd` is not a log view.
pub fn style_visible(hwnd: isize) -> bool {
    if !VIEWS.with_borrow(|v| v.contains(&hwnd)) {
        return false;
    }
    let send = |m: u32, w: usize, l: isize| unsafe { message(hwnd, m, w, l) };
    let top = send(SCI_DOCLINEFROMVISIBLE, send(SCI_GETFIRSTVISIBLELINE, 0, 0).max(0) as usize, 0).max(0) as usize;
    let shown = send(SCI_LINESONSCREEN, 0, 0).clamp(1, 1000) as usize;
    style_lines(hwnd, top, top + shown + 2);
    true
}

/// Style lines `first..=last` (clamped to the document).
fn style_lines(hwnd: isize, first: usize, last: usize) {
    let send = |m: u32, w: usize, l: isize| unsafe { message(hwnd, m, w, l) };
    let count = send(SCI_GETLINECOUNT, 0, 0).max(1) as usize;
    let last = last.min(count - 1);
    if first > last {
        return;
    }
    let start = send(SCI_POSITIONFROMLINE, first, 0).max(0) as usize;
    let stop = if last + 1 < count {
        send(SCI_POSITIONFROMLINE, last + 1, 0).max(0) as usize
    } else {
        send(SCI_GETLENGTH, 0, 0).max(0) as usize
    };
    if stop <= start {
        return;
    }
    // Scintilla's own bytes, valid until the next change.
    let at = send(SCI_GETRANGEPOINTER, start, (stop - start) as isize) as *const u8;
    if at.is_null() {
        return;
    }
    let text = unsafe { std::slice::from_raw_parts(at, stop - start) };
    let mut styles = Vec::with_capacity(text.len());
    let mut done = 0;
    for piece in text.split_inclusive(|&b| b == b'\n') {
        let body = piece.strip_suffix(b"\n").unwrap_or(piece);
        let body = body.strip_suffix(b"\r").unwrap_or(body);
        style_line(body, &mut styles);
        done += piece.len();
        styles.resize(done, DEFAULT);
    }
    send(SCI_STARTSTYLING, start, 0);
    send(SCI_SETSTYLINGEX, styles.len(), styles.as_ptr() as isize);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styled(line: &str) -> Vec<(String, u8)> {
        let mut s = Vec::new();
        style_line(line.as_bytes(), &mut s);
        let mut runs: Vec<(String, u8)> = Vec::new();
        for (c, &st) in line.chars().zip(&s) {
            match runs.last_mut() {
                Some((text, last)) if *last == st => text.push(c),
                _ => runs.push((c.to_string(), st)),
            }
        }
        runs.into_iter().filter(|r| r.1 != DEFAULT).collect()
    }

    #[test]
    fn levels_times_and_values() {
        let r = styled("2026-11-21 00:00:00.003 WARN  req=000000002 GET /api status=503 trace=9e3779b1 \"bad input\" https://x.io/a?b=1");
        assert_eq!(r[0], ("2026-11-21 00:00:00.003".into(), TIME));
        assert_eq!(r[1], ("WARN".into(), WARN));
        assert!(r.contains(&("000000002".into(), NUMBER)));
        assert!(r.contains(&("503".into(), NUMBER)));
        assert!(r.contains(&("9e3779b1".into(), NUMBER)));
        assert!(r.contains(&("\"bad input\"".into(), STRING)));
        assert!(r.contains(&("https://x.io/a?b=1".into(), LINK)));
    }

    #[test]
    fn level_words_only_where_logs_write_them() {
        assert_eq!(styled("[error] disk full"), vec![("error".into(), ERROR)]);
        assert_eq!(styled("level=info msg=started"), vec![("info".into(), INFO)]);
        assert_eq!(styled("{\"level\":\"debug\"}"), vec![("\"level\"".into(), STRING), ("\"debug\"".into(), DEBUG)]);
        assert!(styled("no error was found").is_empty());
        assert_eq!(styled("Caused by: java.lang.NullPointerException"), vec![("NullPointerException".into(), ERROR)]);
        assert_eq!(styled("2026-11-21T08:00:00Z Fatal: out of memory")[1], ("Fatal".into(), ERROR));
    }

    #[test]
    fn logs_are_recognised_by_their_lines() {
        let log = "2026-11-21 00:00:00.000 INFO start\n2026-11-21 00:00:01.000 WARN slow\n2026-11-21 00:00:02.000 INFO done\n";
        assert!(looks_like_log(log));
        assert!(!looks_like_log("# Title\n\nSome prose about errors.\nMore text.\n"));
    }
}
