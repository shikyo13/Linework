//! Every option lw takes, in one table, and the parser that reads a command
//! line against it.
//!
//! The names follow ripgrep, and grep where ripgrep has no option of that
//! name, so what people know from either works here. Short options combine
//! (`-iw`), take their value attached or as the next word (`-m5`, `-m 5`),
//! and long ones take `--name=value` or `--name value`. grep's `-NUM` is
//! `--context NUM`. Words after `--` are never options.
//!
//! Parsing keeps every option in the order given, so that a later one can
//! undo an earlier one (`-i` then `-s`), as both tools do.

/// What an option takes after it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Takes {
    Nothing,
    /// A value, named for the help.
    Value(&'static str),
    /// A value only as `--name=value`; alone it means its default (grep's `--color`).
    Optional(&'static str),
}

/// Where an option is listed in the help.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    Patterns,
    Files,
    Input,
    Output,
    Results,
    Lw,
    Other,
}

#[derive(Debug)]
pub struct Spec {
    pub long: &'static str,
    pub short: Option<char>,
    pub takes: Takes,
    pub group: Group,
    pub help: &'static str,
}

const fn spec(long: &'static str, short: Option<char>, takes: Takes, group: Group, help: &'static str) -> Spec {
    Spec { long, short, takes, group, help }
}

use Group::*;
use Takes::*;

/// Every option. An option whose name starts with `no-` is undone by the same
/// name without it, and one listed here as `name` by `no-name`, where the
/// table lists that form too.
pub const SPECS: &[Spec] = &[
    // Patterns and how they match.
    spec("regexp", Some('e'), Value("PATTERN"), Patterns, "a pattern to search for; may be given many times; then every other word is a path"),
    spec("file", Some('f'), Value("PATTERNFILE"), Patterns, "read patterns from a file, one per line (- for standard input)"),
    spec("fixed-strings", Some('F'), Nothing, Patterns, "take every pattern as plain text"),
    spec("no-fixed-strings", None, Nothing, Patterns, "undo -F"),
    spec("basic-regexp", Some('G'), Nothing, Patterns, "read patterns in grep's basic syntax, where \\( \\) \\{ \\} \\| \\+ \\? are special"),
    spec("extended-regexp", None, Nothing, Patterns, "read patterns as regular expressions even without special characters"),
    spec("pcre2", Some('P'), Nothing, Patterns, "use PCRE2, which adds look-around and back-references"),
    spec("no-pcre2", None, Nothing, Patterns, "undo -P"),
    spec("engine", None, Value("ENGINE"), Patterns, "default, pcre2, or auto (PCRE2 only when a pattern needs it)"),
    spec("auto-hybrid-regex", None, Nothing, Patterns, "the same as --engine auto"),
    spec("no-auto-hybrid-regex", None, Nothing, Patterns, "the same as --engine default"),
    spec("no-pcre2-unicode", None, Nothing, Patterns, "the same as --no-unicode"),
    spec("pcre2-unicode", None, Nothing, Patterns, "undo --no-pcre2-unicode"),
    spec("ignore-case", Some('i'), Nothing, Patterns, "ignore letter case"),
    spec("case-sensitive", Some('s'), Nothing, Patterns, "match letter case (the default); undoes -i and -S"),
    spec("smart-case", Some('S'), Nothing, Patterns, "ignore case unless a pattern has a capital letter"),
    spec("word-regexp", Some('w'), Nothing, Patterns, "match whole words only"),
    spec("line-regexp", Some('x'), Nothing, Patterns, "match whole lines only"),
    spec("invert-match", Some('v'), Nothing, Patterns, "show the lines that do not match"),
    spec("no-invert-match", None, Nothing, Patterns, "undo -v"),
    spec("multiline", Some('U'), Nothing, Patterns, "let a pattern match across lines"),
    spec("no-multiline", None, Nothing, Patterns, "undo -U"),
    spec("multiline-dotall", None, Nothing, Patterns, "with -U, let . match a line break too"),
    spec("no-multiline-dotall", None, Nothing, Patterns, "undo --multiline-dotall"),
    spec("crlf", None, Nothing, Patterns, "treat CRLF as a line end, so $ matches before a CR"),
    spec("no-crlf", None, Nothing, Patterns, "undo --crlf"),
    spec("null-data", None, Nothing, Patterns, "lines end at a zero byte, not a line feed"),
    spec("no-unicode", None, Nothing, Patterns, "\\w, \\d, \\s and case folding cover ASCII only"),
    spec("unicode", None, Nothing, Patterns, "undo --no-unicode"),
    spec("dfa-size-limit", None, Value("NUM+SUFFIX?"), Patterns, "the most memory a pattern's search may use, such as 10M"),
    spec("regex-size-limit", None, Value("NUM+SUFFIX?"), Patterns, "the largest a compiled pattern may be, such as 10M"),
    // Which files.
    spec("glob", Some('g'), Value("GLOB"), Files, "only files matching GLOB; !GLOB leaves them out"),
    spec("iglob", None, Value("GLOB"), Files, "like -g, ignoring letter case"),
    spec("glob-case-insensitive", None, Nothing, Files, "every -g ignores letter case"),
    spec("no-glob-case-insensitive", None, Nothing, Files, "undo --glob-case-insensitive"),
    spec("type", Some('t'), Value("TYPE"), Files, "only files of TYPE (lw --type-list shows them)"),
    spec("type-not", Some('T'), Value("TYPE"), Files, "leave out files of TYPE"),
    spec("type-add", None, Value("TYPESPEC"), Files, "add globs to a type: name:*.ext or name:include:other,types"),
    spec("type-clear", None, Value("TYPE"), Files, "forget a type's globs"),
    spec("type-list", None, Nothing, Files, "list the file types and stop"),
    spec("hidden", Some('.'), Nothing, Files, "include hidden files and names starting with a dot"),
    spec("no-hidden", None, Nothing, Files, "undo --hidden"),
    spec("follow", Some('L'), Nothing, Files, "follow links to files and folders"),
    spec("no-follow", None, Nothing, Files, "undo -L"),
    spec("max-depth", Some('d'), Value("NUM"), Files, "go at most NUM folders deep (0: only the paths given)"),
    spec("max-filesize", None, Value("NUM+SUFFIX?"), Files, "leave out files larger than this, such as 50K or 2M"),
    spec("one-file-system", None, Nothing, Files, "do not go into other drives or volumes"),
    spec("no-one-file-system", None, Nothing, Files, "undo --one-file-system"),
    spec("no-ignore", None, Nothing, Files, "apply no ignore files at all"),
    spec("ignore", None, Nothing, Files, "undo --no-ignore"),
    spec("no-ignore-dot", None, Nothing, Files, "do not apply .ignore and .rgignore files"),
    spec("ignore-dot", None, Nothing, Files, "undo --no-ignore-dot"),
    spec("no-ignore-vcs", None, Nothing, Files, "do not apply .gitignore files and .git/info/exclude"),
    spec("ignore-vcs", None, Nothing, Files, "undo --no-ignore-vcs"),
    spec("no-ignore-exclude", None, Nothing, Files, "do not apply .git/info/exclude"),
    spec("ignore-exclude", None, Nothing, Files, "undo --no-ignore-exclude"),
    spec("no-ignore-global", None, Nothing, Files, "do not apply Git's global ignore file"),
    spec("ignore-global", None, Nothing, Files, "undo --no-ignore-global"),
    spec("no-ignore-parent", None, Nothing, Files, "do not apply ignore files in folders above the searched one"),
    spec("ignore-parent", None, Nothing, Files, "undo --no-ignore-parent"),
    spec("no-ignore-files", None, Nothing, Files, "do not apply the files given with --ignore-file"),
    spec("ignore-files", None, Nothing, Files, "undo --no-ignore-files"),
    spec("ignore-file", None, Value("PATH"), Files, "apply this ignore file too, in .gitignore form"),
    spec("ignore-file-case-insensitive", None, Nothing, Files, "ignore files match names ignoring letter case"),
    spec("no-ignore-file-case-insensitive", None, Nothing, Files, "undo --ignore-file-case-insensitive"),
    spec("no-require-git", None, Nothing, Files, "apply .gitignore files outside Git work trees too"),
    spec("require-git", None, Nothing, Files, "undo --no-require-git"),
    spec("unix-byte-offsets", None, Nothing, Output, "grep's name: give -b offsets as if each CR before a line feed were not there"),
    spec("unrestricted", Some('u'), Nothing, Files, "-u: --no-ignore; -uu: and --hidden; -uuu: and --binary"),
    spec("include", None, Value("GLOB"), Files, "grep's name: only files whose name matches GLOB"),
    spec("exclude", None, Value("GLOB"), Files, "grep's name: leave out files whose name matches GLOB"),
    spec("exclude-dir", None, Value("GLOB"), Files, "grep's name: do not go into folders whose name matches GLOB"),
    spec("exclude-from", None, Value("FILE"), Files, "grep's name: leave out files matching any glob in FILE"),
    spec("recursive", None, Nothing, Files, "grep's name; folders are always searched through"),
    spec("dereference-recursive", Some('R'), Nothing, Files, "grep's name for -L"),
    spec("directories", None, Value("ACTION"), Files, "grep's name: recurse (the default) or skip folders named as paths"),
    spec("devices", Some('D'), Value("ACTION"), Files, "grep's name: read or skip devices (Windows has none to read)"),
    spec("sort", None, Value("SORTBY"), Files, "order files by path (the default), modified, accessed, created, or none"),
    spec("sortr", None, Value("SORTBY"), Files, "like --sort, in reverse"),
    spec("sort-files", None, Nothing, Files, "the same as --sort path"),
    spec("no-sort-files", None, Nothing, Files, "undo --sort-files"),
    spec("files", None, Nothing, Files, "list the files that would be searched, instead of searching"),
    // Reading files.
    spec("text", Some('a'), Nothing, Input, "search binary files as if they were text"),
    spec("no-text", None, Nothing, Input, "undo -a"),
    spec("binary", None, Nothing, Input, "search binary files, but print only that they match"),
    spec("no-binary", None, Nothing, Input, "undo --binary"),
    spec("binary-files", None, Value("TYPE"), Input, "grep's name: binary, text, or without-match"),
    spec("encoding", Some('E'), Value("ENCODING"), Input, "read files in this encoding (utf-16le, windows-1252, ...); none turns off byte-order-mark sniffing"),
    spec("no-encoding", None, Nothing, Input, "undo -E"),
    spec("search-zip", Some('z'), Nothing, Input, "search inside gzip, bzip2, xz, lzma, lz4, zstd, brotli and .Z files"),
    spec("no-search-zip", None, Nothing, Input, "undo -z"),
    spec("pre", None, Value("COMMAND"), Input, "search what COMMAND prints for each file, given the file's path"),
    spec("no-pre", None, Nothing, Input, "undo --pre"),
    spec("pre-glob", None, Value("GLOB"), Input, "use --pre only for files matching GLOB"),
    spec("label", None, Value("LABEL"), Input, "the name shown for standard input"),
    spec("threads", Some('j'), Value("NUM"), Input, "search on NUM threads"),
    spec("mmap", None, Nothing, Input, "accepted for ripgrep's sake; lw chooses how to read by itself"),
    spec("no-mmap", None, Nothing, Input, "accepted for ripgrep's sake; lw chooses how to read by itself"),
    // How matches are shown.
    spec("after-context", Some('A'), Value("NUM"), Output, "show NUM lines after each match"),
    spec("before-context", Some('B'), Value("NUM"), Output, "show NUM lines before each match"),
    spec("context", Some('C'), Value("NUM"), Output, "show NUM lines before and after each match"),
    spec("context-separator", None, Value("SEPARATOR"), Output, "the line between groups of context (default --)"),
    spec("no-context-separator", None, Nothing, Output, "no line between groups of context"),
    spec("field-context-separator", None, Value("SEPARATOR"), Output, "what follows the path and number of a context line (default -)"),
    spec("field-match-separator", None, Value("SEPARATOR"), Output, "what follows the path and number of a matching line (default :)"),
    spec("line-number", Some('n'), Nothing, Output, "show line numbers (lw's default)"),
    spec("no-line-number", Some('N'), Nothing, Output, "do not show line numbers"),
    spec("column", None, Nothing, Output, "show the column of the first match"),
    spec("no-column", None, Nothing, Output, "undo --column"),
    spec("byte-offset", Some('b'), Nothing, Output, "show where each line, or with -o each match, starts in the file, from 0"),
    spec("no-byte-offset", None, Nothing, Output, "undo -b"),
    spec("only-matching", Some('o'), Nothing, Output, "show only the matched text, each match on its own line"),
    spec("replace", Some('r'), Value("REPLACEMENT"), Output, "show each match replaced ($1, ${name}); files are not changed"),
    spec("passthru", None, Nothing, Output, "show every line, matching or not"),
    spec("trim", None, Nothing, Output, "leave out the spaces and tabs a line starts with"),
    spec("no-trim", None, Nothing, Output, "undo --trim"),
    spec("max-columns", Some('M'), Value("NUM"), Output, "leave out lines longer than NUM bytes, saying how many matches they hold"),
    spec("max-columns-preview", None, Nothing, Output, "with -M, show the start of a long line instead"),
    spec("no-max-columns-preview", None, Nothing, Output, "undo --max-columns-preview"),
    spec("whole-lines", None, Nothing, Output, "show lines over 1,000 bytes whole, not just the part around the match"),
    spec("vimgrep", None, Nothing, Output, "every match on its own line as path:line:column:text"),
    spec("heading", None, Nothing, Output, "each file's name once, above its lines"),
    spec("no-heading", None, Nothing, Output, "the file's name on every line"),
    spec("with-filename", Some('H'), Nothing, Output, "show the file's name even for one file"),
    spec("no-filename", Some('I'), Nothing, Output, "never show the file's name"),
    spec("null", Some('0'), Nothing, Output, "end each path with a zero byte"),
    spec("initial-tab", None, Nothing, Output, "grep's name: a tab before each line's text"),
    spec("path-separator", None, Value("SEPARATOR"), Output, "the character between folder names in paths (default /)"),
    spec("color", None, Optional("WHEN"), Output, "colours: never, auto (in a console), always, or ansi"),
    spec("colors", None, Value("COLOR_SPEC"), Output, "a colour: path:fg:magenta, match:style:bold, line:bg:0x10,0x10,0x10, ..."),
    spec("pretty", Some('p'), Nothing, Output, "--color always --heading --line-number"),
    spec("hyperlink-format", None, Value("FORMAT"), Output, "make paths links: default, file, vscode, or a pattern with {path} {line} {column} {host}"),
    spec("hostname-bin", None, Value("COMMAND"), Output, "a program that prints the host name for links"),
    spec("line-buffered", None, Nothing, Output, "write each line out at once"),
    spec("no-line-buffered", None, Nothing, Output, "undo --line-buffered"),
    spec("block-buffered", None, Nothing, Output, "write output in blocks"),
    spec("no-block-buffered", None, Nothing, Output, "undo --block-buffered"),
    // What is reported.
    spec("count", Some('c'), Nothing, Results, "only the number of matching lines in each file"),
    spec("count-matches", None, Nothing, Results, "only the number of matches in each file"),
    spec("include-zero", None, Nothing, Results, "with -c, list files with no matches too"),
    spec("no-include-zero", None, Nothing, Results, "undo --include-zero"),
    spec("files-with-matches", Some('l'), Nothing, Results, "only the names of files with a match"),
    spec("files-without-match", None, Nothing, Results, "only the names of files without a match"),
    spec("max-count", Some('m'), Value("NUM"), Results, "at most NUM matching lines from each file"),
    spec("stop-on-nonmatch", None, Nothing, Results, "stop reading a file at its first non-matching line after a match"),
    spec("quiet", Some('q'), Nothing, Results, "print nothing; the exit code says whether anything matched"),
    spec("json", None, Nothing, Results, "one JSON object per line, then a summary (lw help output)"),
    spec("no-json", None, Nothing, Results, "undo --json"),
    spec("stats", None, Nothing, Results, "print the totals at the end"),
    spec("no-stats", None, Nothing, Results, "undo --stats"),
    spec("no-messages", None, Nothing, Results, "do not report files that could not be read"),
    spec("messages", None, Nothing, Results, "undo --no-messages"),
    spec("no-ignore-messages", None, Nothing, Results, "do not report ignore files that could not be read"),
    spec("ignore-messages", None, Nothing, Results, "undo --no-ignore-messages"),
    spec("debug", None, Nothing, Results, "say why each file is searched or left out"),
    spec("trace", None, Nothing, Results, "--debug and more"),
    // lw's own.
    spec("near", None, Value("PATTERN N"), Lw, "keep a line only if PATTERN is within N lines (before or after; see --near-before)"),
    spec("near-before", None, Value("PATTERN N"), Lw, "keep a line only if PATTERN is within N lines before it"),
    spec("near-after", None, Value("PATTERN N"), Lw, "keep a line only if PATTERN is within N lines after it"),
    spec("and", None, Value("PATTERN"), Lw, "keep only files that also contain PATTERN"),
    spec("not", None, Value("PATTERN"), Lw, "keep only files that do not contain PATTERN"),
    spec("scope", None, Nothing, Lw, "show the functions, classes or sections each line sits in"),
    spec("since", None, Value("TIME"), Lw, "in a time-ordered log, start at this time"),
    spec("until", None, Value("TIME"), Lw, "in a time-ordered log, end at this time"),
    spec("changed", None, Nothing, Lw, "search only files changed since lw changes last looked"),
    spec("mark", None, Value("NAME"), Lw, "the folder snapshot to use (default last)"),
    spec("budget", None, Value("BYTES"), Lw, "stop printing after this many bytes, at a line break"),
    spec("dry-run", None, Nothing, Lw, "show what would change and change nothing"),
    spec("list", None, Nothing, Lw, "list instead of acting (lw undo --list)"),
    spec("account", None, Nothing, Lw, "print where the time went, step by step"),
    spec("samples", None, Value("FILE"), Lw, "write stack samples of the run to FILE"),
    // Everything else.
    spec("config", None, Nothing, Other, "undo --no-config"),
    spec("no-config", None, Nothing, Other, "do not read the file named by LW_CONFIG_PATH"),
    spec("generate", None, Value("KIND"), Other, "write man, complete-bash, complete-zsh, complete-fish or complete-powershell"),
    spec("pcre2-version", None, Nothing, Other, "print the version of PCRE2 lw uses"),
    spec("version", Some('V'), Nothing, Other, "print lw's version"),
    spec("help", Some('h'), Nothing, Other, "show the help"),
];

/// Other long names for options in the table: grep's, and ripgrep's second spellings.
pub const LONG_ALIASES: &[(&str, &str)] = &[
    ("colour", "color"),
    ("passthrough", "passthru"),
    ("maxdepth", "max-depth"),
    ("silent", "quiet"),
    ("perl-regexp", "pcre2"),
    ("group-separator", "context-separator"),
    ("no-group-separator", "no-context-separator"),
];

/// grep's letters that ripgrep leaves free: -y is -i, -Z is -0.
pub const SHORT_ALIASES: &[(char, &str)] = &[('y', "ignore-case"), ('Z', "null")];

/// One option as given: its table entry and its value, if it takes one.
#[derive(Clone, Debug)]
pub struct Given {
    pub long: &'static str,
    pub value: Option<String>,
}

/// A command line read against the table: the options in order, and the other words.
#[derive(Default, Debug)]
pub struct Parsed {
    pub given: Vec<Given>,
    pub words: Vec<String>,
}

pub fn find_long(name: &str) -> Option<&'static Spec> {
    let name = LONG_ALIASES.iter().find(|(alias, _)| *alias == name).map_or(name, |(_, canonical)| *canonical);
    SPECS.iter().find(|s| s.long == name)
}

pub fn find_short(c: char) -> Option<&'static Spec> {
    match SHORT_ALIASES.iter().find(|(letter, _)| *letter == c) {
        Some((_, long)) => find_long(long),
        None => SPECS.iter().find(|s| s.short == Some(c)),
    }
}

/// How many single-character edits turn `a` into `b`.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (above + 1).min(row[j] + 1).min(diagonal + usize::from(ca != cb));
            diagonal = above;
        }
    }
    row[b.len()]
}

/// The option names a misspelled one most likely meant: the nearest by edits, at most a third of it changed.
fn suggest(name: &str) -> String {
    let mut close: Vec<(usize, &str)> = SPECS.iter().map(|s| (distance(name, s.long), s.long)).filter(|(d, _)| *d <= (name.len() / 3).max(1)).collect();
    close.sort();
    let close: Vec<&str> = close.into_iter().take(3).map(|(_, l)| l).collect();
    if close.is_empty() { String::new() } else { format!("; did you mean --{}?", close.join(", --")) }
}

/// One part of a command line as read against the table, with the words it came from.
#[derive(Clone, Debug)]
pub enum Piece {
    /// An option and its value (`--near`'s is the number of lines, a line break, then the pattern).
    Option { spec: &'static Spec, value: Option<String>, words: std::ops::Range<usize> },
    /// A word that is not an option: a pattern or a path.
    Word { index: usize },
    /// `--`: every later word is not an option.
    Separator { index: usize },
    /// Something that cannot be read, and why.
    Problem { message: String, words: std::ops::Range<usize> },
}

/// Reads `args` against the table, part by part, going on after a problem.
/// `parse` is this with the first problem as its error.
pub fn pieces(args: &[String]) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut plain = false;
    // The next word, as an option's value.
    let next = |i: &mut usize| -> Option<String> {
        let value = args.get(*i + 1).cloned()?;
        *i += 1;
        Some(value)
    };
    while i < args.len() {
        let arg = &args[i];
        let first = i;
        if plain || arg == "-" || !arg.starts_with('-') {
            out.push(Piece::Word { index: i });
            i += 1;
            continue;
        }
        if arg == "--" {
            plain = true;
            out.push(Piece::Separator { index: i });
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (long, None),
            };
            let Some(spec) = find_long(name) else {
                out.push(Piece::Problem { message: format!("there is no option --{name}{}", suggest(name)), words: i..i + 1 });
                i += 1;
                continue;
            };
            let value = match (spec.takes, attached) {
                (Nothing, Some(_)) => Err(format!("--{name} takes no value")),
                (Nothing, None) => Ok(None),
                (Optional(_), value) => Ok(value),
                (Value(_), Some(value)) => Ok(Some(value)),
                (Value(what), None) => next(&mut i).map(Some).ok_or_else(|| format!("--{name} needs {what}")),
            };
            out.push(option(spec, value, first, &mut i, &next));
            i += 1;
            continue;
        }
        // grep's -NUM is --context NUM.
        let short = &arg[1..];
        // -0 alone is ripgrep's --null, which outranks grep's context of 0.
        if short != "0" && short.bytes().all(|b| b.is_ascii_digit()) {
            let spec = find_long("context").expect("in the table");
            out.push(Piece::Option { spec, value: Some(short.to_owned()), words: i..i + 1 });
            i += 1;
            continue;
        }
        let mut chars = short.char_indices();
        let mut failed = false;
        while let Some((at, c)) = chars.next() {
            let Some(spec) = find_short(c) else {
                out.push(Piece::Problem { message: format!("there is no option -{c}"), words: first..first + 1 });
                break;
            };
            match spec.takes {
                Nothing | Optional(_) => {
                    let piece = option(spec, Ok(None), first, &mut i, &next);
                    failed |= matches!(piece, Piece::Problem { .. });
                    out.push(piece);
                }
                Value(what) => {
                    let rest = &short[at + c.len_utf8()..];
                    let value = if rest.is_empty() { next(&mut i).ok_or_else(|| format!("-{c} needs {what}")) } else { Ok(rest.to_owned()) };
                    out.push(option(spec, value.map(Some), first, &mut i, &next));
                    break;
                }
            }
            if failed {
                break;
            }
        }
        i += 1;
    }
    out
}

/// One option read from the words from `first` to `i`. `--near` and its
/// directions take a pattern and a number of lines, as two words.
fn option(spec: &'static Spec, value: Result<Option<String>, String>, first: usize, i: &mut usize, next: &impl Fn(&mut usize) -> Option<String>) -> Piece {
    let value = match (spec.long, value) {
        (_, Err(message)) => return Piece::Problem { message, words: first..*i + 1 },
        ("near" | "near-before" | "near-after", Ok(value)) => {
            let pattern = value.expect("takes a value");
            match next(i) {
                Some(lines) => Some(format!("{lines}\n{pattern}")),
                None => return Piece::Problem { message: format!("--{} needs a pattern and a number of lines", spec.long), words: first..*i + 1 },
            }
        }
        (_, Ok(value)) => value,
    };
    // A long option, or a value taken from the next word, ends with the word `i` points at.
    let end = if matches!(spec.takes, Nothing | Optional(_)) && first == *i { first + 1 } else { *i + 1 };
    Piece::Option { spec, value, words: first..end }
}

/// Reads `args` (without the program and command names) against the table.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Parsed, String> {
    let args: Vec<String> = args.into_iter().collect();
    let mut parsed = Parsed::default();
    for piece in pieces(&args) {
        match piece {
            Piece::Option { spec, value, .. } => parsed.given.push(Given { long: spec.long, value }),
            Piece::Word { index } => parsed.words.push(args[index].clone()),
            Piece::Separator { .. } => {}
            Piece::Problem { message, .. } => return Err(message),
        }
    }
    Ok(parsed)
}

/// Reads the configuration file named by `LW_CONFIG_PATH`: one argument per
/// line, blank lines and lines starting with `#` left out. Its options come
/// before the command line's, so the command line wins.
pub fn config() -> Result<Vec<String>, String> {
    let Some(path) = std::env::var_os("LW_CONFIG_PATH").filter(|p| !p.is_empty()) else { return Ok(Vec::new()) };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("LW_CONFIG_PATH {}: {e}", std::path::Path::new(&path).display()))?;
    Ok(text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_owned).collect())
}

/// A whole number for an option.
pub fn number(text: &str, name: &str) -> Result<u64, String> {
    text.parse().map_err(|_| format!("--{name} needs a whole number, not {text}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(words: &[&str]) -> Parsed {
        parse(words.iter().map(|w| (*w).to_owned())).unwrap()
    }

    fn names(p: &Parsed) -> Vec<(&str, Option<&str>)> {
        p.given.iter().map(|g| (g.long, g.value.as_deref())).collect()
    }

    #[test]
    fn options_read_as_ripgrep_and_grep_read_them() {
        let p = read(&["-iwC3", "-m5", "--max-count=7", "-e", "-x-", "pat", "src", "--", "-v"]);
        assert_eq!(
            names(&p),
            [("ignore-case", None), ("word-regexp", None), ("context", Some("3")), ("max-count", Some("5")), ("max-count", Some("7")), ("regexp", Some("-x-"))]
        );
        assert_eq!(p.words, ["pat", "src", "-v"]);
        assert_eq!(names(&read(&["-5"])), [("context", Some("5"))]);
        assert_eq!(names(&read(&["--color"])), [("color", None)]);
        assert_eq!(names(&read(&["--colour=always", "-yZ", "--group-separator", "=="])), [("color", Some("always")), ("ignore-case", None), ("null", None), ("context-separator", Some("=="))]);
        assert_eq!(names(&read(&["--color=never"])), [("color", Some("never"))]);
        assert_eq!(names(&read(&["--near", "x", "3"])), [("near", Some("3\nx"))]);
        assert_eq!(read(&["-", "a"]).words, ["-", "a"]);
        assert!(parse(["--colr".to_owned()]).unwrap_err().contains("did you mean"));
        assert!(parse(["-m".to_owned()]).is_err());
        assert!(parse(["--files=1".to_owned()]).is_err());
        // Every short letter and long name is used once.
        for (i, a) in SPECS.iter().enumerate() {
            for b in &SPECS[i + 1..] {
                assert_ne!(a.long, b.long);
                assert!(a.short.is_none() || a.short != b.short, "-{:?} twice", a.short);
            }
        }
    }
}
