//! Patterns: how the words given on the command line become one matcher.
//!
//! Every pattern is read once by the regex library's parser, with the
//! options applied, and printed back as one final pattern. That final form is
//! what the folder search and the single-file engine both compile, so the two
//! always agree. Outside multiline mode the line end is taken out of every
//! character class, so a match never crosses a line, as in ripgrep, and a
//! pattern that names a line break itself is an error.
//!
//! PCRE2 (`-P`, the library ripgrep uses) serves look-around and
//! back-references; `--engine auto` turns to it only when a pattern needs it.
use regex_syntax::hir::{self, Class, Hir, HirKind};
use std::sync::OnceLock;

/// How the pattern words are read.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Syntax {
    /// Plain text unless a word holds one of `\ ^ $ . | ? * + ( ) [ ] { }`.
    #[default]
    Auto,
    /// Always plain text (`-F`).
    Fixed,
    /// Always a regular expression (`--extended-regexp`, `-e` with a regex).
    Regex,
    /// grep's basic syntax (`-G`).
    Basic,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Engine {
    /// Rust's regex library.
    #[default]
    Default,
    /// PCRE2 always (`-P`).
    Pcre2,
    /// PCRE2 only for patterns the default engine cannot take.
    Auto,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Case {
    #[default]
    Sensitive,
    Insensitive,
    /// Insensitive unless a pattern has an upper-case letter (`-S`).
    Smart,
}

/// Everything that decides how text is matched.
#[derive(Clone, Debug)]
pub struct Spec {
    pub patterns: Vec<String>,
    pub syntax: Syntax,
    pub engine: Engine,
    pub case: Case,
    pub word: bool,
    pub line: bool,
    pub multiline: bool,
    pub dotall: bool,
    pub crlf: bool,
    pub null_data: bool,
    pub unicode: bool,
    pub size_limit: Option<usize>,
    pub dfa_size_limit: Option<usize>,
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            patterns: Vec::new(),
            syntax: Syntax::Auto,
            engine: Engine::Default,
            case: Case::Sensitive,
            word: false,
            line: false,
            multiline: false,
            dotall: false,
            crlf: false,
            null_data: false,
            unicode: true,
            size_limit: None,
            dfa_size_limit: None,
        }
    }
}

const SPECIAL: &str = r"\^$.|?*+()[]{}";

/// The PCRE2 library's version and whether its compiler to machine code is there, as ripgrep reports it.
pub fn pcre2_version() -> String {
    let (major, minor) = pcre2::version();
    let jit = if pcre2::is_jit_available() { "available" } else { "unavailable" };
    format!("PCRE2 {major}.{minor} is available (JIT is {jit})")
}

/// Whether lw's automatic rule reads `pattern` as a regular expression.
pub fn looks_like_regex(pattern: &str) -> bool {
    pattern.chars().any(|c| SPECIAL.contains(c))
}

enum Engine2 {
    /// One plain, case-sensitive string: a byte search, no pattern engine.
    Literal(memchr::memmem::Finder<'static>),
    Regex(regex::bytes::Regex),
    Pcre(pcre2::bytes::Regex),
    /// No patterns at all (an empty -f file): nothing matches.
    Never,
}

/// A compiled set of patterns.
pub struct Query {
    engine: Engine2,
    /// The final pattern, as the single-file engine is given it; None for PCRE2.
    final_pattern: Option<String>,
    /// The text when the patterns are one plain string, for the fast paths.
    plain: Option<(String, bool)>,
    multiline: bool,
    line_end: u8,
    /// Whether a match can hold the line end (only possible with -U).
    spans_lines: bool,
    /// The first error PCRE2 reported while matching (a match limit), kept to be reported once.
    failure: OnceLock<String>,
}

/// Capture positions of one match, by group number; group 0 is the match.
pub type Groups = Vec<Option<(usize, usize)>>;

impl Query {
    /// One pattern with lw's automatic rule, as the older callers ask.
    pub fn new(pattern: &str, plain: bool, ignore_case: bool, whole_word: bool) -> Result<Self, String> {
        Self::build(&Spec {
            patterns: vec![pattern.to_owned()],
            syntax: if plain { Syntax::Fixed } else { Syntax::Regex },
            case: if ignore_case { Case::Insensitive } else { Case::Sensitive },
            word: whole_word,
            ..Spec::default()
        })
    }

    pub fn build(spec: &Spec) -> Result<Self, String> {
        let line_end = if spec.null_data { 0 } else { b'\n' };
        // Each word as a regular expression, and whether it was plain text.
        let mut sources = Vec::with_capacity(spec.patterns.len());
        let mut all_plain = true;
        for pattern in &spec.patterns {
            let plain = match spec.syntax {
                Syntax::Fixed => true,
                Syntax::Auto => !looks_like_regex(pattern),
                Syntax::Regex | Syntax::Basic => false,
            };
            all_plain &= plain;
            sources.push(match (plain, spec.syntax) {
                (true, _) => regex_syntax::escape(pattern),
                (false, Syntax::Basic) => basic(pattern)?,
                (false, _) => pattern.clone(),
            });
        }
        let insensitive = match spec.case {
            Case::Sensitive => false,
            Case::Insensitive => true,
            // Insensitive when the patterns hold a letter written as itself and
            // none of them is upper case; `\W` alone has no such letter.
            Case::Smart => {
                let (mut any, mut upper) = (false, false);
                for (source, original) in sources.iter().zip(&spec.patterns) {
                    let (has_literal, has_upper) =
                        literals(source).unwrap_or_else(|| (!original.is_empty(), original.chars().any(char::is_uppercase)));
                    any |= has_literal;
                    upper |= has_upper;
                }
                any && !upper
            }
        };
        if spec.patterns.is_empty() {
            return Ok(Self { engine: Engine2::Never, final_pattern: None, plain: None, multiline: spec.multiline, line_end, spans_lines: spans_lines(&sources, spec), failure: OnceLock::new() });
        }
        // One plain string, matched as it is: no pattern engine needed.
        if all_plain && sources.len() == 1 && !spec.word && !spec.line && !spec.patterns[0].is_empty() {
            let text = spec.patterns[0].clone();
            if !spec.multiline && text.as_bytes().contains(&line_end) {
                return Err(line_end_error(line_end));
            }
            let plain = Some((text.clone(), insensitive));
            if !insensitive {
                let finder = memchr::memmem::Finder::new(text.as_bytes()).into_owned();
                return Ok(Self { engine: Engine2::Literal(finder), final_pattern: Some(regex_syntax::escape(&text)), plain, multiline: spec.multiline, line_end, spans_lines: spans_lines(&sources, spec), failure: OnceLock::new() });
            }
            let built = build_regex(&[regex_syntax::escape(&text)], spec, insensitive)?;
            return Ok(Self { engine: Engine2::Regex(built.1), final_pattern: Some(built.0), plain, multiline: spec.multiline, line_end, spans_lines: spans_lines(&sources, spec), failure: OnceLock::new() });
        }
        let use_pcre = match spec.engine {
            Engine::Pcre2 => true,
            Engine::Default => false,
            Engine::Auto => build_regex(&sources, spec, insensitive).is_err() && build_pcre(&sources, spec, insensitive).is_ok(),
        };
        if use_pcre {
            let regex = build_pcre(&sources, spec, insensitive)?;
            return Ok(Self { engine: Engine2::Pcre(regex), final_pattern: None, plain: None, multiline: spec.multiline, line_end, spans_lines: spans_lines(&sources, spec), failure: OnceLock::new() });
        }
        let (pattern, regex) = build_regex(&sources, spec, insensitive)?;
        Ok(Self { engine: Engine2::Regex(regex), final_pattern: Some(pattern), plain: None, multiline: spec.multiline, line_end, spans_lines: spans_lines(&sources, spec), failure: OnceLock::new() })
    }

    /// The final pattern for another engine to compile (case and options are
    /// written into it); None when only PCRE2 can run it.
    pub fn final_pattern(&self) -> Option<&str> {
        self.final_pattern.as_deref()
    }

    /// How the editor's file engine is given this query: the text, whether it is a
    /// regular expression, and whether case matters. A plain string goes as it
    /// is, with its case choice; anything else as the final pattern, which holds
    /// its case and options. None when only PCRE2 can run it.
    pub fn engine_form(&self) -> Option<(String, bool, bool)> {
        match self.plain_text() {
            Some((text, insensitive)) => Some((text.to_owned(), false, !insensitive)),
            None => self.final_pattern().map(|p| (p.to_owned(), true, true)),
        }
    }

    /// The text and whether case is ignored, when the patterns are one plain string.
    pub fn plain_text(&self) -> Option<(&str, bool)> {
        self.plain.as_ref().map(|(t, i)| (t.as_str(), *i))
    }

    pub fn is_pcre(&self) -> bool {
        matches!(self.engine, Engine2::Pcre(_))
    }

    pub fn multiline(&self) -> bool {
        self.multiline
    }

    /// Whether a match can hold a line end, so that with -U a count is of matches, not lines.
    pub fn can_match_line_end(&self) -> bool {
        self.spans_lines
    }

    /// The byte lines end with: a line feed, or a zero byte with --null-data.
    pub fn line_end(&self) -> u8 {
        self.line_end
    }

    /// An error PCRE2 reported while matching, once.
    pub fn failure(&self) -> Option<&str> {
        self.failure.get().map(String::as_str)
    }

    /// The next match at or after `from`, as a byte range.
    pub fn next(&self, bytes: &[u8], from: usize) -> Option<(usize, usize)> {
        if from > bytes.len() {
            return None;
        }
        match &self.engine {
            Engine2::Literal(finder) => finder.find(&bytes[from..]).map(|at| (from + at, from + at + finder.needle().len())),
            Engine2::Regex(regex) => regex.find_at(bytes, from).map(|m| (m.start(), m.end())),
            Engine2::Pcre(regex) => match regex.find_at(bytes, from) {
                Ok(found) => found.map(|m| (m.start(), m.end())),
                Err(e) => {
                    let _ = self.failure.set(e.to_string());
                    None
                }
            },
            Engine2::Never => None,
        }
    }

    /// Whether the pattern matches anywhere in `line`.
    pub fn within(&self, line: &[u8]) -> bool {
        match &self.engine {
            Engine2::Literal(finder) => finder.find(line).is_some(),
            Engine2::Regex(regex) => regex.is_match(line),
            Engine2::Pcre(regex) => match regex.is_match(line) {
                Ok(found) => found,
                Err(e) => {
                    let _ = self.failure.set(e.to_string());
                    false
                }
            },
            Engine2::Never => false,
        }
    }

    /// Every match in `bytes`, in order, without overlaps, as regex iterators
    /// give them: an empty match is followed by a search one character
    /// further on, and an empty match right where the last match ended is
    /// passed over (`o*` in `foo bar`: empty, `oo`, then empty at 4 to 7).
    pub fn each(&self, bytes: &[u8], mut visit: impl FnMut(usize, usize) -> bool) {
        let mut at = 0;
        let mut last_end = None;
        while let Some((start, end)) = self.next(bytes, at) {
            if start == end && last_end == Some(start) {
                at = start + char_len(bytes, start);
                if at > bytes.len() {
                    return;
                }
                continue;
            }
            if !visit(start, end) {
                return;
            }
            last_end = Some(end);
            at = if end > start { end } else { start + char_len(bytes, start) };
            if at > bytes.len() {
                return;
            }
        }
    }

    /// The groups of the match that starts at or after `from`, or None.
    pub fn captures(&self, bytes: &[u8], from: usize, groups: &mut Groups) -> Option<(usize, usize)> {
        groups.clear();
        match &self.engine {
            Engine2::Literal(_) | Engine2::Never => {
                let found = self.next(bytes, from)?;
                groups.push(Some(found));
                Some(found)
            }
            Engine2::Regex(regex) => {
                let mut locations = regex.capture_locations();
                let m = regex.captures_read_at(&mut locations, bytes, from)?;
                groups.extend((0..locations.len()).map(|i| locations.get(i)));
                Some((m.start(), m.end()))
            }
            Engine2::Pcre(regex) => {
                let mut locations = regex.capture_locations();
                match regex.captures_read_at(&mut locations, bytes, from) {
                    Ok(Some(m)) => {
                        groups.extend((0..locations.len()).map(|i| locations.get(i)));
                        Some((m.start(), m.end()))
                    }
                    Ok(None) => None,
                    Err(e) => {
                        let _ = self.failure.set(e.to_string());
                        None
                    }
                }
            }
        }
    }

    /// The group number for a group name.
    pub fn group(&self, name: &str) -> Option<usize> {
        match &self.engine {
            Engine2::Regex(regex) => regex.capture_names().position(|n| n == Some(name)),
            Engine2::Pcre(regex) => regex.capture_names().iter().position(|n| n.as_deref() == Some(name)),
            _ => None,
        }
    }
}

/// Bytes in the character at `at`: 1 for invalid UTF-8 or past the end.
fn char_len(bytes: &[u8], at: usize) -> usize {
    match bytes.get(at) {
        Some(&b) if b >= 0xF0 => 4,
        Some(&b) if b >= 0xE0 => 3,
        Some(&b) if b >= 0xC0 => 2,
        _ => 1,
    }
    .min(bytes.len().saturating_sub(at).max(1))
}

fn line_end_error(line_end: u8) -> String {
    let name = if line_end == 0 { r"\x00" } else { r"\n" };
    format!("the pattern can match a line end ({name}), which a line never holds; use -U (--multiline) to match across lines")
}

/// Whether a regular expression has a character written as itself, and
/// whether one of those is upper case (`\S` or `\W` name classes and count
/// as neither). None when it does not parse.
fn literals(source: &str) -> Option<(bool, bool)> {
    use regex_syntax::ast::{self, Ast};
    fn walk(ast: &Ast, found: &mut (bool, bool)) {
        match ast {
            Ast::Literal(lit) => note(lit.c, found),
            Ast::ClassBracketed(class) => set(&class.kind, found),
            Ast::Repetition(rep) => walk(&rep.ast, found),
            Ast::Group(group) => walk(&group.ast, found),
            Ast::Alternation(alt) => alt.asts.iter().for_each(|a| walk(a, found)),
            Ast::Concat(concat) => concat.asts.iter().for_each(|a| walk(a, found)),
            _ => {}
        }
    }
    fn note(c: char, found: &mut (bool, bool)) {
        found.0 = true;
        found.1 |= c.is_uppercase();
    }
    fn set(class: &ast::ClassSet, found: &mut (bool, bool)) {
        match class {
            ast::ClassSet::Item(item) => item_of(item, found),
            ast::ClassSet::BinaryOp(op) => {
                set(&op.lhs, found);
                set(&op.rhs, found);
            }
        }
    }
    fn item_of(item: &ast::ClassSetItem, found: &mut (bool, bool)) {
        match item {
            ast::ClassSetItem::Literal(lit) => note(lit.c, found),
            ast::ClassSetItem::Range(range) => {
                note(range.start.c, found);
                note(range.end.c, found);
            }
            ast::ClassSetItem::Bracketed(class) => set(&class.kind, found),
            ast::ClassSetItem::Union(union) => union.items.iter().for_each(|i| item_of(i, found)),
            _ => {}
        }
    }
    let ast = regex_syntax::ast::parse::Parser::new().parse(source).ok()?;
    let mut found = (false, false);
    walk(&ast, &mut found);
    Some(found)
}

/// Whether any of the patterns, read with the options, can match the line end.
/// Patterns PCRE2 alone reads are taken to be able to, unless they cannot even
/// name it.
fn spans_lines(sources: &[String], spec: &Spec) -> bool {
    if !spec.multiline {
        return false;
    }
    let line_end = if spec.null_data { 0 } else { b'\n' };
    fn holds(hir: &Hir, end: u8) -> bool {
        match hir.kind() {
            HirKind::Literal(hir::Literal(bytes)) => bytes.contains(&end),
            HirKind::Class(Class::Unicode(c)) => c.ranges().iter().any(|r| r.start() <= char::from(end) && char::from(end) <= r.end()),
            HirKind::Class(Class::Bytes(c)) => c.ranges().iter().any(|r| r.start() <= end && end <= r.end()),
            HirKind::Repetition(rep) => holds(&rep.sub, end),
            HirKind::Capture(cap) => holds(&cap.sub, end),
            HirKind::Concat(subs) | HirKind::Alternation(subs) => subs.iter().any(|h| holds(h, end)),
            HirKind::Empty | HirKind::Look(_) => false,
        }
    }
    sources.iter().any(|source| {
        let mut parser = regex_syntax::ParserBuilder::new();
        parser.multi_line(true).dot_matches_new_line(spec.dotall).unicode(spec.unicode).utf8(false).line_terminator(line_end);
        match parser.build().parse(source) {
            Ok(hir) => holds(&hir, line_end),
            // PCRE2's own syntax: a line end named, a class that holds it, or dot-all on.
            Err(_) => source.contains(r"\n") || source.contains(r"\s") || source.contains("[^") || source.contains("(?s"),
        }
    })
}

/// Takes the line end out of every class, so no match crosses a line; a line end written as itself is an error.
fn strip(hir: Hir, line_end: u8) -> Result<Hir, String> {
    Ok(match hir.into_kind() {
        HirKind::Literal(hir::Literal(bytes)) => {
            if bytes.contains(&line_end) {
                return Err(line_end_error(line_end));
            }
            Hir::literal(bytes)
        }
        HirKind::Class(class) => {
            let class = match class {
                Class::Unicode(mut c) => {
                    let end = char::from(line_end);
                    c.difference(&hir::ClassUnicode::new([hir::ClassUnicodeRange::new(end, end)]));
                    Class::Unicode(c)
                }
                Class::Bytes(mut c) => {
                    c.difference(&hir::ClassBytes::new([hir::ClassBytesRange::new(line_end, line_end)]));
                    Class::Bytes(c)
                }
            };
            Hir::class(class)
        }
        HirKind::Repetition(mut rep) => {
            rep.sub = Box::new(strip(*rep.sub, line_end)?);
            Hir::repetition(rep)
        }
        HirKind::Capture(mut cap) => {
            cap.sub = Box::new(strip(*cap.sub, line_end)?);
            Hir::capture(cap)
        }
        HirKind::Concat(subs) => Hir::concat(subs.into_iter().map(|h| strip(h, line_end)).collect::<Result<_, _>>()?),
        HirKind::Alternation(subs) => Hir::alternation(subs.into_iter().map(|h| strip(h, line_end)).collect::<Result<_, _>>()?),
        kind @ (HirKind::Empty | HirKind::Look(_)) => match kind {
            HirKind::Empty => Hir::empty(),
            HirKind::Look(look) => Hir::look(look),
            _ => unreachable!(),
        },
    })
}

/// The patterns joined, with -w and -x, parsed with the options and printed
/// back as one pattern, and that pattern compiled.
fn build_regex(sources: &[String], spec: &Spec, insensitive: bool) -> Result<(String, regex::bytes::Regex), String> {
    let joined = sources.iter().map(|s| format!("(?:{s})")).collect::<Vec<_>>().join("|");
    let wrapped = match (spec.word, spec.line) {
        (_, true) => format!("^(?:{joined})$"),
        (true, false) => format!(r"\b{{start-half}}(?:{joined})\b{{end-half}}"),
        _ => joined,
    };
    let line_end = if spec.null_data { 0 } else { b'\n' };
    let mut parser = regex_syntax::ParserBuilder::new();
    parser
        .case_insensitive(insensitive)
        .multi_line(true)
        .crlf(spec.crlf)
        .line_terminator(line_end)
        .dot_matches_new_line(spec.multiline && spec.dotall)
        .unicode(spec.unicode)
        .utf8(false)
        .octal(false);
    let hir = parser.build().parse(&wrapped).map_err(|e| e.to_string())?;
    let hir = if spec.multiline { hir } else { strip(hir, line_end)? };
    let pattern = hir.to_string();
    let mut builder = regex::bytes::RegexBuilder::new(&pattern);
    builder.unicode(spec.unicode).line_terminator(line_end);
    if let Some(limit) = spec.size_limit {
        builder.size_limit(limit);
    }
    if let Some(limit) = spec.dfa_size_limit {
        builder.dfa_size_limit(limit);
    }
    let regex = builder.build().map_err(|e| e.to_string())?;
    Ok((pattern, regex))
}

/// The patterns joined, with -w and -x, compiled by PCRE2.
fn build_pcre(sources: &[String], spec: &Spec, insensitive: bool) -> Result<pcre2::bytes::Regex, String> {
    let joined = sources.iter().map(|s| format!("(?:{s})")).collect::<Vec<_>>().join("|");
    let pattern = match (spec.word, spec.line) {
        (_, true) => format!("^(?:{joined})$"),
        (true, false) => format!(r"(?<!\w)(?:{joined})(?!\w)"),
        _ => joined,
    };
    let mut builder = pcre2::bytes::RegexBuilder::new();
    builder.caseless(insensitive).multi_line(true).dotall(spec.multiline && spec.dotall).crlf(spec.crlf).jit_if_available(true).max_jit_stack_size(Some(10 << 20));
    if spec.unicode {
        builder.utf(true).ucp(true);
    }
    // Outside -U, PCRE2's matches are judged line by line, so a pattern that
    // needs a line end matches nothing there, as in ripgrep.
    builder.build(&pattern).map_err(|e| format!("PCRE2: {e}"))
}

/// grep's basic syntax as a regular expression: \( \) \{ \} \| \+ \? are the
/// operators, and ( ) { } | + ? stand for themselves. A * at the start of the
/// pattern or of a group or branch, a ^ not at the start, and a $ not at the
/// end are plain characters. \< and \> are word starts and ends.
pub fn basic(pattern: &str) -> Result<String, String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::with_capacity(pattern.len() + 8);
    let mut i = 0;
    // Where an atom may start: the pattern's start, after \( or \|.
    let mut at_start = true;
    while i < chars.len() {
        let c = chars[i];
        let starting = at_start;
        at_start = false;
        match c {
            '\\' => {
                let Some(&next) = chars.get(i + 1) else { return Err("the pattern ends with a lone \\".into()) };
                i += 2;
                match next {
                    '(' => {
                        out.push('(');
                        at_start = true;
                    }
                    ')' => out.push(')'),
                    '{' => out.push('{'),
                    '}' => out.push('}'),
                    '|' => {
                        out.push('|');
                        at_start = true;
                    }
                    '+' => out.push('+'),
                    '?' => out.push('?'),
                    '<' => out.push_str(r"\b{start}"),
                    '>' => out.push_str(r"\b{end}"),
                    '1'..='9' => return Err("back-references (\\1 to \\9) need -P".into()),
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
                continue;
            }
            '(' | ')' | '{' | '}' | '|' | '+' | '?' => {
                out.push('\\');
                out.push(c);
            }
            '*' if starting => out.push_str(r"\*"),
            '^' if !starting => out.push_str(r"\^"),
            '$' => {
                let last = i + 1 == chars.len() || (chars[i + 1] == '\\' && matches!(chars.get(i + 2), Some(')' | '|')));
                out.push_str(if last { "$" } else { r"\$" });
            }
            '^' => {
                out.push('^');
                at_start = true;
            }
            '[' => {
                i = bracket(&chars, i, &mut out)?;
                continue;
            }
            _ => out.push(c),
        }
        i += 1;
    }
    Ok(out)
}

/// Copies the POSIX bracket expression starting at `chars[at]` into Rust's
/// class syntax, and returns where it ends. In POSIX a backslash is a plain
/// character there and a `]` first in the list is a member; in Rust `\`, `[`,
/// `&&`, `--` and `~~` mean more, so they are escaped. `[:name:]` classes are
/// the same in both; `[=x=]` and `[.x.]` have no Rust form.
fn bracket(chars: &[char], at: usize, out: &mut String) -> Result<usize, String> {
    let mut j = at + 1;
    out.push('[');
    if chars.get(j) == Some(&'^') {
        out.push('^');
        j += 1;
    }
    let first = j;
    loop {
        let Some(&c) = chars.get(j) else { return Err("a [ has no closing ]".into()) };
        match c {
            ']' if j > first => {
                out.push(']');
                return Ok(j + 1);
            }
            '[' if chars.get(j + 1) == Some(&':') => {
                let close = (j + 2..chars.len().saturating_sub(1)).find(|&k| chars[k] == ':' && chars[k + 1] == ']').ok_or("a [: has no closing :]")?;
                out.extend(&chars[j..close + 2]);
                j = close + 2;
            }
            '[' if matches!(chars.get(j + 1), Some('=' | '.')) => {
                return Err("equivalence classes [= =] and collating symbols [. .] are not supported".into());
            }
            '\\' | '[' | ']' | '&' | '~' => {
                out.push('\\');
                out.push(c);
                j += 1;
            }
            // A - that would read as Rust's set difference (--) is a plain dash.
            '-' if chars.get(j + 1) == Some(&'-') => {
                out.push_str(r"\-");
                j += 1;
            }
            _ => {
                out.push(c);
                j += 1;
            }
        }
    }
}

/// A replacement written for `-r`: `$1`, `${1}`, `$name`, `${name}` insert a
/// group, `$$` a dollar sign; anything else is copied.
pub struct Template {
    parts: Vec<Part>,
}

enum Part {
    Text(Vec<u8>),
    Number(usize),
    Name(String),
}

impl Template {
    pub fn new(text: &str) -> Self {
        let bytes = text.as_bytes();
        let mut parts = Vec::new();
        let mut plain = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'$' {
                plain.push(bytes[i]);
                i += 1;
                continue;
            }
            if bytes.get(i + 1) == Some(&b'$') {
                plain.push(b'$');
                i += 2;
                continue;
            }
            let (name, used) = if bytes.get(i + 1) == Some(&b'{') {
                match bytes[i + 2..].iter().position(|&b| b == b'}') {
                    Some(close) => (&text[i + 2..i + 2 + close], close + 3),
                    None => ("", 0),
                }
            } else {
                let len = bytes[i + 1..].iter().take_while(|b| b.is_ascii_alphanumeric() || **b == b'_').count();
                (&text[i + 1..i + 1 + len], len + 1)
            };
            if name.is_empty() {
                plain.push(b'$');
                i += 1;
                continue;
            }
            if !plain.is_empty() {
                parts.push(Part::Text(std::mem::take(&mut plain)));
            }
            parts.push(match name.parse() {
                Ok(n) => Part::Number(n),
                Err(_) => Part::Name(name.to_owned()),
            });
            i += used;
        }
        if !plain.is_empty() {
            parts.push(Part::Text(plain));
        }
        Self { parts }
    }

    /// Appends the replacement for one match, given its groups in `bytes`.
    pub fn expand(&self, query: &Query, bytes: &[u8], groups: &Groups, out: &mut Vec<u8>) {
        for part in &self.parts {
            let group = match part {
                Part::Text(text) => {
                    out.extend_from_slice(text);
                    continue;
                }
                Part::Number(n) => Some(*n),
                Part::Name(name) => query.group(name),
            };
            if let Some(Some((start, end))) = group.and_then(|n| groups.get(n)) {
                out.extend_from_slice(&bytes[*start..*end]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(patterns: &[&str], change: impl FnOnce(&mut Spec)) -> Query {
        let mut spec = Spec { patterns: patterns.iter().map(|p| (*p).to_owned()).collect(), ..Spec::default() };
        change(&mut spec);
        Query::build(&spec).unwrap()
    }

    fn all(q: &Query, text: &str) -> Vec<String> {
        let mut found = Vec::new();
        q.each(text.as_bytes(), |s, e| {
            found.push(text[s..e].to_owned());
            true
        });
        found
    }

    #[test]
    fn patterns_combine_and_take_their_options() {
        assert_eq!(all(&query(&["a.c", "x"], |_| {}), "abc x a.c"), ["abc", "x", "a.c"]);
        assert_eq!(all(&query(&["a.c"], |s| s.syntax = Syntax::Fixed), "abc a.c"), ["a.c"]);
        // -w: the match may start or end with a non-word character.
        assert_eq!(all(&query(&["-x"], |s| s.word = true), "a-x b -x-y"), ["-x"]);
        assert_eq!(all(&query(&["ab"], |s| s.word = true), "ab abc cab ab"), ["ab", "ab"]);
        assert_eq!(all(&query(&["ab"], |s| s.line = true), "ab\nabc\nab"), ["ab", "ab"]);
        // Smart case: a capital letter makes it case-sensitive; \S does not.
        assert_eq!(all(&query(&["ab"], |s| s.case = Case::Smart), "AB ab"), ["AB", "ab"]);
        assert_eq!(all(&query(&["Ab"], |s| s.case = Case::Smart), "AB Ab"), ["Ab"]);
        assert_eq!(all(&query(&[r"a\S"], |s| s.case = Case::Smart), "AX ab"), ["AX", "ab"]);
        // A pattern with no letter of its own stays case-sensitive.
        assert_eq!(all(&query(&[r"F\w+", r"\W"], |s| s.case = Case::Smart), "Foo FOO foo"), ["Foo", " ", "FOO", " "]);        // No match crosses a line, and a line break in a pattern is refused, unless -U.
        assert_eq!(all(&query(&[r"a\sb"], |_| {}), "a\nb a b"), ["a b"]);
        assert!(Query::build(&Spec { patterns: vec![r"a\nb".into()], ..Spec::default() }).is_err());
        assert_eq!(all(&query(&[r"a\sb"], |s| s.multiline = true), "a\nb"), ["a\nb"]);
        // $ before a CR only with --crlf.
        assert_eq!(all(&query(&["b$"], |_| {}), "ab\r\n"), Vec::<String>::new());
        assert_eq!(all(&query(&["b$"], |s| s.crlf = true), "ab\r\n"), ["b"]);
        // PCRE2 for look-around; auto chooses it only when needed.
        assert_eq!(all(&query(&[r"a(?=b)"], |s| s.engine = Engine::Pcre2), "ab ac"), ["a"]);
        assert!(query(&[r"a(?=b)"], |s| s.engine = Engine::Auto).is_pcre());
        assert!(!query(&["ab"], |s| s.engine = Engine::Auto).is_pcre());
        // grep's basic syntax.
        assert_eq!(basic(r"\(ab\)\{2\}c+").unwrap(), r"(ab){2}c\+");
        assert_eq!(basic(r"*a|b$x").unwrap(), r"\*a\|b\$x");
        assert_eq!(basic(r"[a\]b]").unwrap(), r"[a\\]b]");
        assert_eq!(basic(r"[]a[b&&c]").unwrap(), r"[\]a\[b\&\&c]");
        assert_eq!(basic(r"[^[:digit:]x]").unwrap(), r"[^[:digit:]x]");
        assert_eq!(all(&query(&[r"[]a]x"], |s| s.syntax = Syntax::Basic), "]x ax bx"), ["]x", "ax"]);
        assert_eq!(all(&query(&[r"\(ab\)\{2\}"], |s| s.syntax = Syntax::Basic), "abab ab"), ["abab"]);
    }

    #[test]
    fn replacements_insert_groups() {
        let q = query(&[r"(?P<user>\w+)@(\w+)"], |_| {});
        let text = b"x joe@site y";
        let mut groups = Groups::new();
        q.captures(text, 0, &mut groups).unwrap();
        let mut out = Vec::new();
        Template::new("${user} at $2 for $$5 $9").expand(&q, text, &groups, &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "joe at site for $5 ");
    }
}
