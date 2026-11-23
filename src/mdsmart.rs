//! What Markdown typing help and pair glow need to know about one line of
//! text. Nothing here touches the editor, so all of it is tested directly.

/// A byte range inside the line.
pub type Span = (usize, usize);

fn blank(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}
fn wordy(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b >= 0x80
}

/// The opening and closing markup of the innermost emphasis, strikethrough,
/// code span or link that holds the caret. A caret touching either end counts.
pub fn pair(line: &[u8], caret: usize) -> Option<(Span, Span)> {
    let n = line.len();
    let mut found: Vec<(Span, Span)> = Vec::new();
    // Code spans first: nothing inside one is markup.
    let mut code = vec![false; n];
    let mut i = 0;
    while i < n {
        if line[i] == b'\\' {
            i += 2;
            continue;
        }
        if line[i] != b'`' {
            i += 1;
            continue;
        }
        let open = i;
        while i < n && line[i] == b'`' {
            i += 1;
        }
        let ticks = i - open;
        let mut j = i;
        let mut close = None;
        while j < n {
            if line[j] != b'`' {
                j += 1;
                continue;
            }
            let run = j;
            while j < n && line[j] == b'`' {
                j += 1;
            }
            if j - run == ticks {
                close = Some(run);
                break;
            }
        }
        if let Some(close) = close {
            found.push(((open, open + ticks), (close, close + ticks)));
            code[open..close + ticks].fill(true);
            i = close + ticks;
        }
    }
    let plain = |i: usize| !code[i] && (i == 0 || line[i - 1] != b'\\' || (i >= 2 && line[i - 2] == b'\\'));
    // Links and images: [text](destination).
    let mut opens: Vec<usize> = Vec::new();
    for i in 0..n {
        if !plain(i) {
            continue;
        }
        match line[i] {
            b'[' => opens.push(i),
            b']' => {
                let Some(open) = opens.pop() else { continue };
                if line.get(i + 1) != Some(&b'(') {
                    continue;
                }
                let mut depth = 0usize;
                let mut end = None;
                for (j, &b) in line.iter().enumerate().skip(i + 1) {
                    match b {
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                end = Some(j + 1);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(end) = end {
                    found.push(((open, open + 1), (i, end)));
                }
            }
            _ => {}
        }
    }
    // Emphasis and strikethrough: runs of one character that open or close.
    let mut stack: Vec<(u8, usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        let c = line[i];
        if !matches!(c, b'*' | b'_' | b'~') || !plain(i) {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && line[i] == c && !code[i] {
            i += 1;
        }
        let before = start.checked_sub(1).map(|k| line[k]);
        let after = line.get(i).copied();
        let mut can_open = after.is_some_and(|b| !blank(b));
        let mut can_close = before.is_some_and(|b| !blank(b));
        if c == b'_' && before.is_some_and(wordy) && after.is_some_and(wordy) {
            // snake_case is not emphasis.
            can_open = false;
            can_close = false;
        }
        let (mut from, to) = (start, i);
        if can_close {
            while from < to {
                let Some(at) = stack.iter().rposition(|open| open.0 == c) else { break };
                let (_, a, b) = stack[at];
                let take = (b - a).min(to - from);
                found.push(((b - take, b), (from, from + take)));
                from += take;
                stack.truncate(at);
                if b - take > a {
                    stack.push((c, a, b - take));
                }
            }
        }
        if can_open && from < to {
            stack.push((c, from, to));
        }
    }
    found
        .into_iter()
        .filter(|(open, close)| open.0 <= caret && caret <= close.1)
        .min_by_key(|(open, close)| close.1 - open.0)
}

/// The list or quote markup a line begins with.
#[derive(Debug, PartialEq, Eq)]
pub struct Lead {
    /// Bytes of leading white space.
    pub indent: usize,
    /// Where the quote markers end. Equal to `indent` when there are none.
    pub quote: usize,
    /// Where the bullet or number, its space and any task box end. Equal to
    /// `quote` when the line is not a list item.
    pub end: usize,
    /// The marker of the item after this one, with a fresh task box.
    pub next: String,
    /// How far Tab nests an item of this list.
    pub step: usize,
}

/// Three or more `-`, `*` or `_` and nothing else: a rule, not a list item.
fn rule(text: &[u8]) -> bool {
    let mut marks = text.iter().filter(|b| !blank(**b));
    let Some(&first) = marks.next() else { return false };
    matches!(first, b'-' | b'*' | b'_') && marks.clone().all(|b| *b == first) && marks.count() >= 2
}

/// Read the list and quote markup at the start of a line, if it has any.
pub fn lead(line: &[u8]) -> Option<Lead> {
    let indent = line.iter().take_while(|b| matches!(**b, b' ' | b'\t')).count();
    if rule(line) {
        return None;
    }
    let mut at = indent;
    while line.get(at) == Some(&b'>') {
        at += 1;
        if line.get(at) == Some(&b' ') {
            at += 1;
        }
    }
    let quote = at;
    let mut next = String::new();
    let mut step = 0;
    match line.get(at) {
        Some(&c @ (b'-' | b'*' | b'+')) if line.get(at + 1) == Some(&b' ') => {
            next.push(c as char);
            next.push(' ');
            at += 2;
            step = 2;
        }
        Some(b'0'..=b'9') => {
            let digits = line[at..].iter().take_while(|b| b.is_ascii_digit()).count();
            let close = line.get(at + digits).copied();
            if digits <= 9
                && matches!(close, Some(b'.' | b')'))
                && line.get(at + digits + 1) == Some(&b' ')
            {
                let number: u64 = std::str::from_utf8(&line[at..at + digits]).ok()?.parse().ok()?;
                next = format!("{}{} ", number + 1, close? as char);
                at += digits + 2;
                step = digits + 2;
            }
        }
        _ => {}
    }
    if step > 0 && line.len() >= at + 4 {
        let boxed = &line[at..at + 4];
        if boxed[0] == b'[' && matches!(boxed[1], b' ' | b'x' | b'X') && &boxed[2..] == b"] " {
            next.push_str("[ ] ");
            at += 4;
        }
    }
    (at > indent).then_some(Lead { indent, quote, end: at, next, step })
}

/// What Enter does on a list or quote line.
#[derive(Debug, PartialEq, Eq)]
pub enum Enter {
    /// Start a new line with this text after the line break.
    Continue(String),
    /// The item is empty: remove its markup from byte `from` to the end of
    /// the line, which ends the list or the quote.
    End { from: usize },
}

/// `line` is the caret's whole line without its line break and `caret` the
/// caret's byte offset in it.
pub fn enter(line: &[u8], caret: usize) -> Option<Enter> {
    let lead = lead(line)?;
    if caret < lead.end {
        return None;
    }
    if line[lead.end..].iter().all(|b| blank(*b)) {
        // An empty list item inside a quote leaves the quote. An empty quote
        // line, or an empty item outside one, leaves nothing.
        let from = if lead.end > lead.quote && lead.quote > lead.indent { lead.quote } else { 0 };
        return Some(Enter::End { from });
    }
    let mut text = String::from_utf8_lossy(&line[..lead.quote]).into_owned();
    text.push_str(&lead.next);
    Some(Enter::Continue(text))
}

/// How many `mark` characters sit directly before `start` and after `end`.
fn fence(text: &[u8], start: usize, end: usize, mark: u8) -> usize {
    let before = text[..start].iter().rev().take_while(|b| **b == mark).count();
    let after = text[end..].iter().take_while(|b| **b == mark).count();
    before.min(after)
}

/// Turn bold (`width` 2) or italic (`width` 1) on or off for `start..end` of
/// `text`. Returns the range to replace, its new content, and where the
/// same words are afterwards.
pub fn toggle(text: &[u8], start: usize, end: usize, width: usize) -> (Span, Vec<u8>, Span) {
    const MARK: u8 = b'*';
    let has = |count: usize| if width == 2 { count >= 2 } else { count % 2 == 1 };
    let outside = fence(text, start, end, MARK);
    if has(outside) {
        let inner = text[start..end].to_vec();
        return ((start - width, end + width), inner, (start - width, end - width));
    }
    // The selection may include the markup itself.
    let chosen = &text[start..end];
    let lead = chosen.iter().take_while(|b| **b == MARK).count();
    let tail = chosen.iter().rev().take_while(|b| **b == MARK).count();
    if chosen.len() > lead + tail && has(lead.min(tail)) {
        let inner = chosen[width..chosen.len() - width].to_vec();
        return ((start, end), inner, (start, end - 2 * width));
    }
    let mut out = vec![MARK; width];
    out.extend_from_slice(chosen);
    out.extend(std::iter::repeat_n(MARK, width));
    ((start, end), out, (start + width, end + width))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn glow(line: &str, caret: usize) -> Option<(&str, &str)> {
        pair(line.as_bytes(), caret).map(|(a, b)| (&line[a.0..a.1], &line[b.0..b.1]))
    }
    #[test]
    fn finds_the_innermost_pair_around_the_caret() {
        let line = "plain **bold and `code` here** then [a *link*](https://x.y/z(1)) end";
        assert_eq!(glow(line, 2), None);
        assert_eq!(glow(line, 10), Some(("**", "**")));
        assert_eq!(glow(line, 19), Some(("`", "`")));
        assert_eq!(glow(line, 38), Some(("[", "](https://x.y/z(1))")));
        assert_eq!(glow(line, 42), Some(("*", "*")));
        assert_eq!(glow(line, 55), Some(("[", "](https://x.y/z(1))")));
        assert_eq!(glow(line, line.len()), None);
    }
    #[test]
    fn ignores_what_is_not_markup() {
        assert_eq!(glow("a snake_case_name here", 10), None);
        assert_eq!(glow("2 * 3 * 4", 4), None);
        assert_eq!(glow(r"an \*escaped\* star", 6), None);
        assert_eq!(glow("`a * b` and * c", 3), Some(("`", "`")));
        assert_eq!(glow("~~gone~~ and ***both***", 3), Some(("~~", "~~")));
        assert_eq!(glow("~~gone~~ and ***both***", 18), Some(("***", "***")));
        assert_eq!(glow("naïve **émphasis** ok", 12), Some(("**", "**")));
    }
    #[test]
    fn reads_list_and_quote_markup() {
        assert_eq!(enter(b"- one", 5), Some(Enter::Continue("- ".into())));
        assert_eq!(enter(b"  * one", 7), Some(Enter::Continue("  * ".into())));
        assert_eq!(enter(b"9. nine", 7), Some(Enter::Continue("10. ".into())));
        assert_eq!(enter(b"3) three", 4), Some(Enter::Continue("4) ".into())));
        assert_eq!(enter(b"- [x] done", 10), Some(Enter::Continue("- [ ] ".into())));
        assert_eq!(enter(b"> quoted", 8), Some(Enter::Continue("> ".into())));
        assert_eq!(enter(b"> - item", 8), Some(Enter::Continue("> - ".into())));
        assert_eq!(enter(b"- ", 2), Some(Enter::End { from: 0 }));
        assert_eq!(enter(b"- [ ] ", 6), Some(Enter::End { from: 0 }));
        assert_eq!(enter(b"> ", 2), Some(Enter::End { from: 0 }));
        assert_eq!(enter(b"> - ", 4), Some(Enter::End { from: 2 }));
    }
    #[test]
    fn leaves_ordinary_lines_alone() {
        assert_eq!(enter(b"plain text", 5), None);
        assert_eq!(enter(b"**bold** start", 14), None);
        assert_eq!(enter(b"- - -", 5), None);
        assert_eq!(enter(b"***", 3), None);
        assert_eq!(enter(b"-no space", 4), None);
        assert_eq!(enter(b"2024 was a year", 6), None);
        // The caret is inside the marker: an ordinary line break.
        assert_eq!(enter(b"- one", 1), None);
        assert_eq!(lead(b"10. ten").map(|l| l.step), Some(4));
    }
    #[test]
    fn toggles_bold_and_italic() {
        let run = |text: &str, a: usize, b: usize, width: usize| {
            let (span, new, keep) = toggle(text.as_bytes(), a, b, width);
            let mut out = text.as_bytes().to_vec();
            out.splice(span.0..span.1, new);
            let out = String::from_utf8(out).unwrap();
            let kept = out[keep.0..keep.1].to_owned();
            (out, kept)
        };
        assert_eq!(run("a word b", 2, 6, 2), ("a **word** b".into(), "word".into()));
        assert_eq!(run("a **word** b", 4, 8, 2), ("a word b".into(), "word".into()));
        assert_eq!(run("a **word** b", 2, 10, 2), ("a word b".into(), "word".into()));
        assert_eq!(run("a word b", 2, 6, 1), ("a *word* b".into(), "word".into()));
        assert_eq!(run("a *word* b", 3, 7, 1), ("a word b".into(), "word".into()));
        // Italic inside bold adds a third star, and comes off again.
        assert_eq!(run("a **word** b", 4, 8, 1), ("a ***word*** b".into(), "word".into()));
        assert_eq!(run("a ***word*** b", 5, 9, 1), ("a **word** b".into(), "word".into()));
        assert_eq!(run("ab", 1, 1, 2), ("a****b".into(), "".into()));
    }
}
