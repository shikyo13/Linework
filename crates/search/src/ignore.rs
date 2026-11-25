//! Ignore rules in the form Git defines for `.gitignore`, read from one file
//! and applied to paths relative to that file's folder.
//!
//! A rule is one line. `#` starts a comment, a leading `!` turns the rule into
//! an exception, a trailing `/` limits it to folders. A rule with a `/`
//! anywhere but at its end is tied to the file's own folder; any other rule
//! applies to a name at every depth below it. `*` and `?` never cross a `/`,
//! `[...]` is a set of characters, and `**` stands for any number of folders.
//! Within one file the last rule that matches decides.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ignored,
    Kept,
}

#[derive(Debug)]
enum Token {
    Bytes(Vec<u8>),
    /// `?`: one byte that is not a slash.
    One,
    /// `*`: any run of bytes without a slash.
    Run,
    /// `[...]`: one byte in, or with `negated` not in, the listed ranges.
    Set { ranges: Vec<(u8, u8)>, negated: bool },
    /// `**/` at the start or `/**/` in the middle: any number of whole folders.
    Folders,
    /// `/**` at the end: everything below.
    Below,
}

#[derive(Debug)]
struct Rule {
    tokens: Vec<Token>,
    exception: bool,
    folders_only: bool,
    /// Tied to the ignore file's folder; otherwise the last name alone is tested.
    tied: bool,
    /// Matched ignoring letter case (ASCII and Unicode simple folding).
    fold: bool,
    /// The line as written, for saying which rule decided.
    text: String,
}

#[derive(Debug, Default)]
pub struct Rules {
    rules: Vec<Rule>,
    /// Some rule ignores letter case, so paths are folded once for them.
    folding: bool,
    /// Where the rules came from, for saying which rule decided.
    source: String,
}

/// The rule that decided a verdict: where it came from and how it is written.
#[derive(Debug, Clone, Copy)]
pub struct Decided<'a> {
    pub verdict: Verdict,
    pub source: &'a str,
    pub rule: &'a str,
}

impl Rules {
    pub fn parse(text: &[u8]) -> Self {
        Self::parse_from(text, "", false)
    }

    /// Rules read from `source` (a file's path, for messages), all ignoring letter case when `fold`.
    pub fn parse_from(text: &[u8], source: &str, fold: bool) -> Self {
        let text = text.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(text);
        Self::from_lines(text.split(|&b| b == b'\n').map(|line| (line.strip_suffix(b"\r").unwrap_or(line), fold)), source)
    }

    /// Rules from lines, each with its own choice of ignoring letter case.
    pub fn from_lines<'l>(lines: impl IntoIterator<Item = (&'l [u8], bool)>, source: &str) -> Self {
        let rules: Vec<Rule> = lines.into_iter().filter_map(|(line, fold)| rule(line, fold)).collect();
        let folding = rules.iter().any(|r| r.fold);
        Self { rules, folding, source: source.to_owned() }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// `path` is relative to the ignore file's folder, with `/` between names.
    /// None when no rule speaks about it.
    pub fn verdict(&self, path: &[u8], is_folder: bool) -> Option<Verdict> {
        self.decide(path, is_folder).map(|d| d.verdict)
    }

    /// The verdict and the rule that gave it.
    pub fn decide(&self, path: &[u8], is_folder: bool) -> Option<Decided<'_>> {
        let folded = self.folding.then(|| fold(path));
        let name_of = |p: &'_ [u8]| -> usize { memchr::memrchr(b'/', p).map_or(0, |at| at + 1) };
        self.rules.iter().rev().find_map(|rule| {
            if rule.folders_only && !is_folder {
                return None;
            }
            let full: &[u8] = match (&folded, rule.fold) {
                (Some(folded), true) => folded,
                _ => path,
            };
            let subject = if rule.tied { full } else { &full[name_of(full)..] };
            matches(&rule.tokens, subject).then_some(Decided {
                verdict: if rule.exception { Verdict::Kept } else { Verdict::Ignored },
                source: &self.source,
                rule: &rule.text,
            })
        })
    }
}

/// Text with letter case folded, for rules that ignore it.
fn fold(bytes: &[u8]) -> Vec<u8> {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_lowercase().into_bytes(),
        Err(_) => bytes.to_ascii_lowercase(),
    }
}

fn rule(line: &[u8], fold_case: bool) -> Option<Rule> {
    let text = String::from_utf8_lossy(line).into_owned();
    let line_folded;
    let line = if fold_case {
        line_folded = fold(line);
        &line_folded[..]
    } else {
        line
    };
    // Trailing spaces do not count unless the last one is escaped.
    let mut end = line.len();
    while end > 0 && line[end - 1] == b' ' && !(end > 1 && line[end - 2] == b'\\') {
        end -= 1;
    }
    let mut line = &line[..end];
    if line.is_empty() || line[0] == b'#' {
        return None;
    }
    let exception = line[0] == b'!';
    if exception {
        line = &line[1..];
    }
    let folders_only = line.last() == Some(&b'/');
    if folders_only {
        line = &line[..line.len() - 1];
    }
    let tied = line.contains(&b'/');
    if line.first() == Some(&b'/') {
        line = &line[1..];
    }
    if line.is_empty() {
        return None;
    }
    Some(Rule { tokens: tokens(line), exception, folders_only, tied, fold: fold_case, text })
}

fn tokens(mut p: &[u8]) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    let push = |out: &mut Vec<Token>, byte: u8| match out.last_mut() {
        Some(Token::Bytes(bytes)) => bytes.push(byte),
        _ => out.push(Token::Bytes(vec![byte])),
    };
    let mut at_name_start = true;
    while let Some(&c) = p.first() {
        match c {
            b'*' if p.starts_with(b"**") && at_name_start && (p.len() == 2 || p[2] == b'/') => {
                if p.len() == 2 {
                    // `a/**`: drop the slash already pushed and match everything below.
                    if let Some(Token::Bytes(bytes)) = out.last_mut() {
                        if bytes.last() == Some(&b'/') {
                            bytes.pop();
                            if bytes.is_empty() {
                                out.pop();
                            }
                        }
                    }
                    out.push(if out.is_empty() { Token::Folders } else { Token::Below });
                    if matches!(out.last(), Some(Token::Folders)) {
                        // A bare `**` matches every path.
                        out.push(Token::Run);
                    }
                    p = &p[2..];
                } else {
                    out.push(Token::Folders);
                    p = &p[3..];
                }
                at_name_start = true;
                continue;
            }
            b'*' => {
                while p.first() == Some(&b'*') {
                    p = &p[1..];
                }
                out.push(Token::Run);
                at_name_start = false;
                continue;
            }
            b'?' => out.push(Token::One),
            b'[' => match set(p) {
                Some((token, used)) => {
                    out.push(token);
                    p = &p[used..];
                    at_name_start = false;
                    continue;
                }
                None => push(&mut out, b'['),
            },
            b'\\' if p.len() > 1 => {
                push(&mut out, p[1]);
                p = &p[2..];
                at_name_start = false;
                continue;
            }
            _ => push(&mut out, c),
        }
        at_name_start = c == b'/';
        p = &p[1..];
    }
    out
}

/// A `[...]` set at the start of `p`, and how many bytes it takes.
fn set(p: &[u8]) -> Option<(Token, usize)> {
    let mut i = 1;
    let negated = matches!(p.get(i), Some(b'!' | b'^'));
    if negated {
        i += 1;
    }
    let mut ranges = Vec::new();
    let first = i;
    loop {
        let mut c = *p.get(i)?;
        if c == b']' && i > first {
            return Some((Token::Set { ranges, negated }, i + 1));
        }
        if c == b'\\' {
            i += 1;
            c = *p.get(i)?;
        }
        i += 1;
        if p.get(i) == Some(&b'-') && p.get(i + 1).is_some_and(|&next| next != b']') {
            let mut high = p[i + 1];
            i += 2;
            if high == b'\\' {
                high = *p.get(i)?;
                i += 1;
            }
            ranges.push((c, high));
        } else {
            ranges.push((c, c));
        }
    }
}

fn matches(tokens: &[Token], path: &[u8]) -> bool {
    let Some((token, rest)) = tokens.split_first() else { return path.is_empty() };
    match token {
        Token::Bytes(bytes) => path.starts_with(bytes) && matches(rest, &path[bytes.len()..]),
        Token::One => path.first().is_some_and(|&b| b != b'/') && matches(rest, &path[1..]),
        Token::Set { ranges, negated } => {
            path.first().is_some_and(|&b| b != b'/' && ranges.iter().any(|&(low, high)| low <= b && b <= high) != *negated)
                && matches(rest, &path[1..])
        }
        Token::Run => {
            let reach = memchr::memchr(b'/', path).unwrap_or(path.len());
            (0..=reach).any(|n| matches(rest, &path[n..]))
        }
        Token::Folders => {
            // Zero folders, or skip one whole name and its slash and try again.
            let mut at = 0;
            loop {
                if matches(rest, &path[at..]) {
                    return true;
                }
                match memchr::memchr(b'/', &path[at..]) {
                    Some(slash) => at += slash + 1,
                    None => return false,
                }
            }
        }
        Token::Below => path.first() == Some(&b'/') && path.len() > 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(rules: &str, path: &str, is_folder: bool) -> Option<Verdict> {
        Rules::parse(rules.as_bytes()).verdict(path.as_bytes(), is_folder)
    }

    #[test]
    fn rules_follow_gits_documented_cases() {
        use Verdict::{Ignored, Kept};
        // A plain name applies at every depth; a slash ties it to the file's folder.
        assert_eq!(verdict("target", "target", true), Some(Ignored));
        assert_eq!(verdict("target", "a/b/target", false), Some(Ignored));
        assert_eq!(verdict("/target", "a/target", true), None);
        assert_eq!(verdict("doc/frotz", "doc/frotz", true), Some(Ignored));
        assert_eq!(verdict("doc/frotz", "a/doc/frotz", true), None);
        // A trailing slash limits the rule to folders, and does not tie it.
        assert_eq!(verdict("build/", "x/build", true), Some(Ignored));
        assert_eq!(verdict("build/", "x/build", false), None);
        // Stars stop at a slash; two stars stand for folders.
        assert_eq!(verdict("*.log", "a/b/c.log", false), Some(Ignored));
        assert_eq!(verdict("a/*.log", "a/b/c.log", false), None);
        assert_eq!(verdict("**/foo", "foo", false), Some(Ignored));
        assert_eq!(verdict("**/foo/bar", "x/y/foo/bar", false), Some(Ignored));
        assert_eq!(verdict("abc/**", "abc/x/y", false), Some(Ignored));
        assert_eq!(verdict("abc/**", "abc", true), None);
        assert_eq!(verdict("a/**/b", "a/b", false), Some(Ignored));
        assert_eq!(verdict("a/**/b", "a/x/y/b", false), Some(Ignored));
        assert_eq!(verdict("a/**/b", "a/x/yb", false), None);
        // Sets, single characters, escapes.
        assert_eq!(verdict("file[0-9].txt", "file7.txt", false), Some(Ignored));
        assert_eq!(verdict("file[!0-9].txt", "file7.txt", false), None);
        assert_eq!(verdict("fil?.txt", "file.txt", false), Some(Ignored));
        assert_eq!(verdict("fil?.txt", "fil/.txt", false), None);
        assert_eq!(verdict("\\#notes", "#notes", false), Some(Ignored));
        assert_eq!(verdict("\\!important", "!important", false), Some(Ignored));
        // The last matching rule decides; comments and blank lines say nothing.
        assert_eq!(verdict("*.log\n!keep.log", "keep.log", false), Some(Kept));
        assert_eq!(verdict("!keep.log\n*.log", "keep.log", false), Some(Ignored));
        assert_eq!(verdict("# all logs\n\n*.log   \r\n", "x.log", false), Some(Ignored));
        assert_eq!(verdict("name\\ ", "name ", false), Some(Ignored));
    }
}
