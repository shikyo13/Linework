//! The options of one command line, applied in order into the settings every
//! command reads. A later option undoes an earlier one wherever the two
//! decide the same thing, as in ripgrep.
use crate::options::{self, Given, Parsed};
use crate::{
    input::{self, Encoding},
    matcher::{self, Case, Engine, Syntax},
    pick::{Choice, Types},
};
use std::path::PathBuf;

/// What a search reports.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    /// The matching lines (the default).
    #[default]
    Lines,
    /// Matching lines per file (`-c`).
    Count,
    /// Matches per file (`--count-matches`).
    CountMatches,
    /// Files with a match (`-l`).
    WithMatches,
    /// Files without one (`--files-without-match`).
    WithoutMatch,
}

/// How binary files are treated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Binary {
    /// Walked files stop at a zero byte, named ones report a match (the default).
    Auto,
    /// Every file reports "binary file matches" (`--binary`, `-uuu`, grep's default).
    Convert,
    /// No detection (`-a`, `--null-data`).
    Text,
    /// Binary files never match (grep's `--binary-files=without-match`).
    Skip,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Never,
    Auto,
    Always,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortBy {
    /// lw's order: path order ignoring letter case.
    Default,
    /// Names compared as bytes, folder by folder (`--sort path`).
    Path,
    Modified,
    Accessed,
    Created,
}

/// grep's `--directories`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Folders {
    Recurse,
    Read,
    Skip,
}

pub struct Settings {
    // Patterns.
    pub patterns: Vec<String>,
    /// Patterns came from -e or -f, so every other word is a path.
    pub explicit_patterns: bool,
    pub matcher: matcher::Spec,
    pub invert: bool,
    // Files.
    pub choices: Vec<Choice>,
    pub types: Types,
    pub glob_fold: bool,
    pub hidden: bool,
    pub follow: bool,
    pub max_depth: Option<usize>,
    pub max_filesize: Option<u64>,
    pub one_file_system: bool,
    pub ignore: bool,
    pub ignore_dot: bool,
    pub ignore_vcs: bool,
    pub ignore_exclude: bool,
    pub ignore_global: bool,
    pub ignore_parent: bool,
    pub ignore_files: Vec<PathBuf>,
    pub use_ignore_files: bool,
    pub ignore_fold: bool,
    pub require_git: bool,
    pub sort: SortBy,
    pub sort_reverse: bool,
    pub list_files: bool,
    pub folders: Folders,
    pub skip_devices: bool,
    // Reading.
    pub binary: Binary,
    pub encoding: Encoding,
    pub unpack: bool,
    pub pre: Option<String>,
    pub pre_globs: Vec<String>,
    pub label: Option<String>,
    pub threads: Option<usize>,
    // Showing.
    pub before: Option<usize>,
    pub after: Option<usize>,
    pub context: Option<usize>,
    pub passthru: bool,
    pub context_separator: Option<Vec<u8>>,
    pub field_context: Vec<u8>,
    pub field_match: Vec<u8>,
    pub line_number: Option<bool>,
    pub column: bool,
    pub byte_offset: bool,
    pub unix_offsets: bool,
    pub only_matching: bool,
    pub replace: Option<String>,
    pub trim: bool,
    pub max_columns: Option<u64>,
    pub max_columns_preview: bool,
    pub whole_lines: bool,
    pub vimgrep: bool,
    pub heading: Option<bool>,
    pub with_filename: Option<bool>,
    pub null: bool,
    pub initial_tab: bool,
    pub path_separator: Option<u8>,
    pub color: Color,
    pub colors: Vec<String>,
    pub hyperlink_format: Option<String>,
    pub hostname_bin: Option<String>,
    pub line_buffered: Option<bool>,
    // Reporting.
    pub mode: Mode,
    /// lw's JSON objects (`--json`): for lines, or with -c, -l and the like, for counts and names; then a summary.
    pub json: bool,
    pub include_zero: bool,
    pub max_count: Option<u64>,
    pub stop_on_nonmatch: bool,
    pub quiet: bool,
    pub stats: bool,
    pub messages: bool,
    pub ignore_messages: bool,
    pub debug: bool,
    // lw's own.
    pub near: Vec<(String, u64, Near)>,
    pub and: Vec<String>,
    pub not: Vec<String>,
    pub scope: bool,
    pub since: Option<String>,
    pub until: Option<String>,
    pub changed: bool,
    pub mark: Option<String>,
    pub budget: Option<usize>,
    pub dry_run: bool,
    pub list: bool,
    pub account: bool,
    pub samples: Option<String>,
    // Everything else.
    pub generate: Option<String>,
    pub pcre2_version: bool,
    pub type_list: bool,
    pub version: bool,
    pub help: bool,
    /// The words that are not options.
    pub words: Vec<String>,
}

/// Which side of a line --near looks on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Near {
    Either,
    Before,
    After,
}

/// A separator as written, with `\t`, `\n`, `\r`, `\0`, `\\` and `\xHH` turned into their bytes.
pub fn unescape(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            let simple = match next {
                b't' => Some(b'\t'),
                b'n' => Some(b'\n'),
                b'r' => Some(b'\r'),
                b'0' => Some(0),
                b'\\' => Some(b'\\'),
                _ => None,
            };
            if let Some(byte) = simple {
                out.push(byte);
                i += 2;
                continue;
            }
            if next == b'x' && i + 4 <= bytes.len() && text.is_char_boundary(i + 4) {
                if let Ok(byte) = u8::from_str_radix(&text[i + 2..i + 4], 16) {
                    out.push(byte);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

impl Settings {
    /// The settings for one command line, with the configuration file's
    /// options in front of it unless `--no-config` is given. A configuration
    /// file that cannot be read is reported to `warn` and left out.
    pub fn read(args: Vec<String>, warn: impl FnOnce(String)) -> Result<Self, String> {
        let config = if args.iter().any(|a| a == "--no-config") { Vec::new() } else { options::config().unwrap_or_else(|e| {
            warn(e);
            Vec::new()
        }) };
        let parsed = options::parse(config.into_iter().chain(args))?;
        Self::from(parsed)
    }

    /// The settings for these words alone, with no configuration file: what
    /// the editor uses, so a search there means the same as the same words
    /// given to lw.
    pub fn from_words(words: impl IntoIterator<Item = String>) -> Result<Self, String> {
        Self::from(options::parse(words)?)
    }

    fn from(parsed: Parsed) -> Result<Self, String> {
        let mut s = Self::default_settings();
        let mut unrestricted = 0;
        for Given { long, value } in parsed.given {
            let value = value.unwrap_or_default();
            let number = |name: &str| options::number(&value, name);
            match long {
                // Patterns.
                "regexp" => {
                    s.explicit_patterns = true;
                    s.patterns.push(value);
                }
                "file" => {
                    s.explicit_patterns = true;
                    let text = if value == "-" {
                        let mut text = String::new();
                        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text).map_err(|e| format!("reading patterns from standard input: {e}"))?;
                        text
                    } else {
                        std::fs::read_to_string(&value).map_err(|e| format!("{value}: {e}"))?
                    };
                    s.patterns.extend(text.lines().map(|l| l.strip_suffix('\r').unwrap_or(l).to_owned()));
                }
                "fixed-strings" => s.matcher.syntax = Syntax::Fixed,
                "no-fixed-strings" => s.matcher.syntax = Syntax::Auto,
                "basic-regexp" => s.matcher.syntax = Syntax::Basic,
                "extended-regexp" => s.matcher.syntax = Syntax::Regex,
                "pcre2" => s.matcher.engine = Engine::Pcre2,
                "no-pcre2" => s.matcher.engine = Engine::Default,
                "auto-hybrid-regex" => s.matcher.engine = Engine::Auto,
                "no-auto-hybrid-regex" => s.matcher.engine = Engine::Default,
                "engine" => {
                    s.matcher.engine = match value.as_str() {
                        "default" => Engine::Default,
                        "pcre2" => Engine::Pcre2,
                        "auto" => Engine::Auto,
                        other => return Err(format!("--engine is default, pcre2 or auto, not {other}")),
                    }
                }
                "no-pcre2-unicode" | "no-unicode" => s.matcher.unicode = false,
                "pcre2-unicode" | "unicode" => s.matcher.unicode = true,
                "ignore-case" => s.matcher.case = Case::Insensitive,
                "case-sensitive" => s.matcher.case = Case::Sensitive,
                "smart-case" => s.matcher.case = Case::Smart,
                "word-regexp" => (s.matcher.word, s.matcher.line) = (true, false),
                "line-regexp" => (s.matcher.word, s.matcher.line) = (false, true),
                "invert-match" => s.invert = true,
                "no-invert-match" => s.invert = false,
                "multiline" => (s.matcher.multiline, s.stop_on_nonmatch) = (true, false),
                "no-multiline" => s.matcher.multiline = false,
                "multiline-dotall" => s.matcher.dotall = true,
                "no-multiline-dotall" => s.matcher.dotall = false,
                "crlf" => (s.matcher.crlf, s.matcher.null_data) = (true, false),
                "no-crlf" => s.matcher.crlf = false,
                "null-data" => (s.matcher.null_data, s.matcher.crlf) = (true, false),
                "dfa-size-limit" => s.matcher.dfa_size_limit = Some(size(&value, long)? as usize),
                "regex-size-limit" => s.matcher.size_limit = Some(size(&value, long)? as usize),
                // Files.
                "glob" => s.choices.push(Choice::Glob { glob: value, fold: false }),
                "iglob" => s.choices.push(Choice::Glob { glob: value, fold: true }),
                "glob-case-insensitive" => s.glob_fold = true,
                "no-glob-case-insensitive" => s.glob_fold = false,
                "type" => s.choices.push(Choice::Type(value)),
                "type-not" => s.choices.push(Choice::NotType(value)),
                "type-add" => s.types.add(&value)?,
                "type-clear" => s.types.clear(&value),
                "type-list" => s.type_list = true,
                "hidden" => s.hidden = true,
                "no-hidden" => s.hidden = false,
                "follow" | "dereference-recursive" => s.follow = true,
                "no-follow" => s.follow = false,
                "max-depth" => s.max_depth = Some(number(long)? as usize),
                "max-filesize" => s.max_filesize = Some(size(&value, long)?),
                "one-file-system" => s.one_file_system = true,
                "no-one-file-system" => s.one_file_system = false,
                "no-ignore" => s.ignore = false,
                "ignore" => (s.ignore, unrestricted) = (true, 0),
                "no-ignore-dot" => s.ignore_dot = false,
                "ignore-dot" => s.ignore_dot = true,
                "no-ignore-vcs" => s.ignore_vcs = false,
                "ignore-vcs" => s.ignore_vcs = true,
                "no-ignore-exclude" => s.ignore_exclude = false,
                "ignore-exclude" => s.ignore_exclude = true,
                "no-ignore-global" => s.ignore_global = false,
                "ignore-global" => s.ignore_global = true,
                "no-ignore-parent" => s.ignore_parent = false,
                "ignore-parent" => s.ignore_parent = true,
                "no-ignore-files" => s.use_ignore_files = false,
                "ignore-files" => s.use_ignore_files = true,
                "ignore-file" => s.ignore_files.push(PathBuf::from(value)),
                "ignore-file-case-insensitive" => s.ignore_fold = true,
                "no-ignore-file-case-insensitive" => s.ignore_fold = false,
                "no-require-git" => s.require_git = false,
                "require-git" => s.require_git = true,
                "unrestricted" => {
                    unrestricted += 1;
                    s.ignore = false;
                    if unrestricted >= 2 {
                        s.hidden = true;
                    }
                    if unrestricted >= 3 {
                        s.binary = Binary::Convert;
                    }
                }
                "include" => s.choices.push(Choice::Include(value)),
                "exclude" => s.choices.push(Choice::Exclude(value)),
                "exclude-dir" => s.choices.push(Choice::ExcludeDir(value)),
                "exclude-from" => {
                    let text = std::fs::read_to_string(&value).map_err(|e| format!("{value}: {e}"))?;
                    s.choices.extend(text.lines().map(str::trim).filter(|l| !l.is_empty()).map(|l| Choice::Exclude(l.to_owned())));
                }
                "recursive" => s.folders = Folders::Recurse,
                "directories" => {
                    s.folders = match value.as_str() {
                        "recurse" => Folders::Recurse,
                        "read" => Folders::Read,
                        "skip" => Folders::Skip,
                        other => return Err(format!("--directories is read, recurse or skip, not {other}")),
                    }
                }
                "devices" => {
                    s.skip_devices = match value.as_str() {
                        "read" => false,
                        "skip" => true,
                        other => return Err(format!("--devices is read or skip, not {other}")),
                    }
                }
                "sort" | "sortr" => {
                    s.sort_reverse = long == "sortr";
                    s.sort = match value.as_str() {
                        "none" => SortBy::Default,
                        "path" => SortBy::Path,
                        "modified" => SortBy::Modified,
                        "accessed" => SortBy::Accessed,
                        "created" => SortBy::Created,
                        other => return Err(format!("--{long} is none, path, modified, accessed or created, not {other}")),
                    }
                }
                "sort-files" => (s.sort, s.sort_reverse) = (SortBy::Path, false),
                "no-sort-files" => s.sort = SortBy::Default,
                "files" => s.list_files = true,
                // Reading.
                "text" => s.binary = Binary::Text,
                "no-text" | "no-binary" => s.binary = Binary::Auto,
                "binary" => s.binary = Binary::Convert,
                "binary-files" => {
                    s.binary = match value.as_str() {
                        "binary" => Binary::Convert,
                        "text" => Binary::Text,
                        "without-match" => Binary::Skip,
                        other => return Err(format!("--binary-files is binary, text or without-match, not {other}")),
                    }
                }
                "encoding" => s.encoding = Encoding::from_label(&value)?,
                "no-encoding" => s.encoding = Encoding::Sniff,
                "search-zip" => (s.unpack, s.pre) = (true, None),
                "no-search-zip" => s.unpack = false,
                "pre" => (s.pre, s.unpack) = ((!value.is_empty()).then_some(value), false),
                "no-pre" => s.pre = None,
                "pre-glob" => s.pre_globs.push(value),
                "label" => s.label = Some(value),
                "threads" => s.threads = Some(number(long)? as usize).filter(|&n| n > 0),
                "mmap" | "no-mmap" => {}
                // Showing.
                "after-context" => (s.after, s.passthru) = (Some(number(long)? as usize), false),
                "before-context" => (s.before, s.passthru) = (Some(number(long)? as usize), false),
                "context" => (s.context, s.passthru) = (Some(number(long)? as usize), false),
                "context-separator" => s.context_separator = Some(unescape(&value)),
                "no-context-separator" => s.context_separator = None,
                "field-context-separator" => s.field_context = unescape(&value),
                "field-match-separator" => s.field_match = unescape(&value),
                "line-number" => s.line_number = Some(true),
                "no-line-number" => s.line_number = Some(false),
                "column" => s.column = true,
                "no-column" => s.column = false,
                "byte-offset" => s.byte_offset = true,
                "no-byte-offset" => s.byte_offset = false,
                "unix-byte-offsets" => s.unix_offsets = true,
                "only-matching" => s.only_matching = true,
                "replace" => s.replace = Some(value),
                "passthru" => (s.passthru, s.before, s.after, s.context) = (true, None, None, None),
                "trim" => s.trim = true,
                "no-trim" => s.trim = false,
                "max-columns" => s.max_columns = Some(number(long)?).filter(|&n| n > 0),
                "max-columns-preview" => s.max_columns_preview = true,
                "no-max-columns-preview" => s.max_columns_preview = false,
                "whole-lines" => s.whole_lines = true,
                "vimgrep" => s.vimgrep = true,
                "heading" => s.heading = Some(true),
                "no-heading" => s.heading = Some(false),
                "with-filename" => s.with_filename = Some(true),
                "no-filename" => s.with_filename = Some(false),
                "null" => s.null = true,
                "initial-tab" => s.initial_tab = true,
                "path-separator" => {
                    let bytes = unescape(&value);
                    s.path_separator = match bytes.as_slice() {
                        [] => None,
                        [one] => Some(*one),
                        _ => return Err(format!("a path separator must be exactly one byte, not {value:?}")),
                    }
                }
                "color" => {
                    s.color = match value.as_str() {
                        "" | "auto" => Color::Auto,
                        "never" => Color::Never,
                        "always" | "ansi" => Color::Always,
                        other => return Err(format!("--color is never, auto, always or ansi, not {other}")),
                    }
                }
                "colors" => s.colors.push(value),
                "pretty" => (s.color, s.heading, s.line_number) = (Color::Always, Some(true), Some(true)),
                "hyperlink-format" => s.hyperlink_format = Some(value),
                "hostname-bin" => s.hostname_bin = Some(value),
                "line-buffered" => s.line_buffered = Some(true),
                "no-line-buffered" | "no-block-buffered" => s.line_buffered = None,
                "block-buffered" => s.line_buffered = Some(false),
                // Reporting.
                "count" => s.mode = Mode::Count,
                "count-matches" => s.mode = Mode::CountMatches,
                "files-with-matches" => s.mode = Mode::WithMatches,
                "files-without-match" => s.mode = Mode::WithoutMatch,
                "json" => s.json = true,
                "no-json" => s.json = false,
                "include-zero" => s.include_zero = true,
                "no-include-zero" => s.include_zero = false,
                "max-count" => s.max_count = Some(number(long)?),
                "stop-on-nonmatch" => (s.stop_on_nonmatch, s.matcher.multiline) = (true, false),
                "quiet" => s.quiet = true,
                "stats" => s.stats = true,
                "no-stats" => s.stats = false,
                "no-messages" => s.messages = false,
                "messages" => s.messages = true,
                "no-ignore-messages" => s.ignore_messages = false,
                "ignore-messages" => s.ignore_messages = true,
                "debug" | "trace" => s.debug = true,
                // lw's own.
                "near" | "near-before" | "near-after" => {
                    let (lines, pattern) = value.split_once('\n').expect("parser joins them");
                    let lines = options::number(lines, long)?;
                    let side = match long {
                        "near-before" => Near::Before,
                        "near-after" => Near::After,
                        _ => Near::Either,
                    };
                    s.near.push((pattern.to_owned(), lines, side));
                }
                "and" => s.and.push(value),
                "not" => s.not.push(value),
                "scope" => s.scope = true,
                "since" => s.since = Some(value),
                "until" => s.until = Some(value),
                "changed" => s.changed = true,
                "mark" => s.mark = Some(value),
                "budget" => s.budget = Some(number(long)? as usize),
                "dry-run" => s.dry_run = true,
                "list" => s.list = true,
                "account" => s.account = true,
                "samples" => s.samples = Some(value),
                "config" | "no-config" => {}
                "generate" => {
                    if !["man", "complete-bash", "complete-zsh", "complete-fish", "complete-powershell"].contains(&value.as_str()) {
                        return Err(format!("--generate is man, complete-bash, complete-zsh, complete-fish or complete-powershell, not {value}"));
                    }
                    s.generate = Some(value);
                }
                "pcre2-version" => s.pcre2_version = true,
                "version" => s.version = true,
                "help" => s.help = true,
                other => return Err(format!("--{other} is not handled; this is a bug in lw")),
            }
        }
        s.words = parsed.words;
        // A pattern with line breaks is one pattern per line, as in grep, unless it may match across lines.
        if !s.matcher.multiline {
            s.patterns = s.patterns.iter().flat_map(|p| p.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l).to_owned()).collect::<Vec<_>>()).collect();
        }
        if !s.use_ignore_files {
            s.ignore_files.clear();
        }
        if s.matcher.null_data && s.binary == Binary::Auto {
            s.binary = Binary::Text;
        }
        Ok(s)
    }

    fn default_settings() -> Self {
        Self {
            patterns: Vec::new(),
            explicit_patterns: false,
            matcher: matcher::Spec::default(),
            invert: false,
            choices: Vec::new(),
            types: Types::default(),
            glob_fold: false,
            hidden: false,
            follow: false,
            max_depth: None,
            max_filesize: None,
            one_file_system: false,
            ignore: true,
            ignore_dot: true,
            ignore_vcs: true,
            ignore_exclude: true,
            ignore_global: true,
            ignore_parent: true,
            ignore_files: Vec::new(),
            use_ignore_files: true,
            ignore_fold: false,
            require_git: true,
            sort: SortBy::Default,
            sort_reverse: false,
            list_files: false,
            folders: Folders::Recurse,
            skip_devices: false,
            binary: Binary::Auto,
            encoding: Encoding::Sniff,
            unpack: false,
            pre: None,
            pre_globs: Vec::new(),
            label: None,
            threads: None,
            before: None,
            after: None,
            context: None,
            passthru: false,
            context_separator: Some(b"--".to_vec()),
            field_context: b"-".to_vec(),
            field_match: b":".to_vec(),
            line_number: None,
            column: false,
            byte_offset: false,
            unix_offsets: false,
            only_matching: false,
            replace: None,
            trim: false,
            max_columns: None,
            max_columns_preview: false,
            whole_lines: false,
            vimgrep: false,
            heading: None,
            with_filename: None,
            null: false,
            initial_tab: false,
            path_separator: None,
            color: Color::Auto,
            colors: Vec::new(),
            hyperlink_format: None,
            hostname_bin: None,
            line_buffered: None,
            mode: Mode::Lines,
            json: false,
            include_zero: false,
            max_count: None,
            stop_on_nonmatch: false,
            quiet: false,
            stats: false,
            messages: true,
            ignore_messages: true,
            debug: false,
            near: Vec::new(),
            and: Vec::new(),
            not: Vec::new(),
            scope: false,
            since: None,
            until: None,
            changed: false,
            mark: None,
            budget: None,
            dry_run: false,
            list: false,
            account: false,
            samples: None,
            generate: None,
            pcre2_version: false,
            type_list: false,
            version: false,
            help: false,
            words: Vec::new(),
        }
    }

    /// The patterns and the paths: with `-e` or `-f` every word is a path,
    /// otherwise the first word is the pattern (one per line unless `-U`).
    pub fn patterns_and_paths(&self) -> Result<(Vec<String>, Vec<String>), String> {
        let mut words = self.words.clone();
        let mut patterns = self.patterns.clone();
        if !self.explicit_patterns && !self.list_files {
            if words.is_empty() {
                return Err("lw needs a pattern to search for; lw help find shows how".into());
            }
            let first = words.remove(0);
            if self.matcher.multiline {
                patterns.push(first);
            } else {
                patterns.extend(first.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l).to_owned()));
            }
        }
        Ok((patterns, words))
    }

    /// Lines of context before and after: `-B` and `-A` outrank `-C` whichever comes first.
    pub fn context_lines(&self) -> (usize, usize) {
        let c = self.context.unwrap_or(0);
        (self.before.unwrap_or(c), self.after.unwrap_or(c))
    }

    /// The input settings for files and standard input.
    pub fn input(&self) -> Result<input::Config, String> {
        let pre_globs = (!self.pre_globs.is_empty()).then(|| {
            let rules = crate::ignore::Rules::from_lines(self.pre_globs.iter().map(|g| (g.as_bytes(), false)), "--pre-glob");
            (rules, self.pre_globs.iter().any(|g| !g.starts_with('!')))
        });
        Ok(input::Config { encoding: self.encoding, unpack: self.unpack, pre: self.pre.clone(), pre_globs })
    }
}

/// A size with an optional upper-case K, M or G suffix (powers of 1024), as ripgrep reads it.
fn size(text: &str, name: &str) -> Result<u64, String> {
    let (digits, factor) = match text.as_bytes().last() {
        Some(b'K') => (&text[..text.len() - 1], 1u64 << 10),
        Some(b'M') => (&text[..text.len() - 1], 1 << 20),
        Some(b'G') => (&text[..text.len() - 1], 1 << 30),
        _ => (text, 1),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "--{name}: invalid size {text:?}; it should be digits followed by an optional K, M or G (upper case)"
        ));
    }
    digits.parse::<u64>().ok().and_then(|n| n.checked_mul(factor)).ok_or_else(|| format!("--{name}: {text} is too large"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(words: &[&str]) -> Settings {
        Settings::from(options::parse(words.iter().map(|w| (*w).to_owned())).unwrap()).unwrap()
    }

    #[test]
    fn later_options_undo_earlier_ones() {
        assert_eq!(settings(&["-i", "-s"]).matcher.case, Case::Sensitive);
        assert_eq!(settings(&["-s", "-i"]).matcher.case, Case::Insensitive);
        let wx = settings(&["-w", "-x"]);
        assert!(wx.matcher.line && !wx.matcher.word);
        assert_eq!(settings(&["-A2", "-C1"]).context_lines(), (1, 2));
        assert_eq!(settings(&["-C1", "-A2"]).context_lines(), (1, 2));
        assert!(!settings(&["--passthru", "-C1"]).passthru);
        assert!(settings(&["-C1", "--passthru"]).passthru);
        assert_eq!(settings(&["-l", "-c"]).mode, Mode::Count);
        assert_eq!(settings(&["-c", "-l"]).mode, Mode::WithMatches);
        let u3 = settings(&["-uuu"]);
        assert!(!u3.ignore && u3.hidden && u3.binary == Binary::Convert);
        assert!(settings(&["-u", "--ignore"]).ignore);
        let z = settings(&["--pre", "x", "-z"]);
        assert!(z.unpack && z.pre.is_none());
        let pre = settings(&["-z", "--pre", "x"]);
        assert!(!pre.unpack && pre.pre.as_deref() == Some("x"));
        assert_eq!(settings(&["-e", "a\nb"]).patterns, ["a", "b"]);
        assert!(settings(&["--null-data"]).binary == Binary::Text);
    }

    #[test]
    fn sizes_and_separators_read_as_ripgrep_reads_them() {
        assert_eq!(size("2K", "x").unwrap(), 2048);
        assert!(size("1k", "x").is_err() && size("1.5K", "x").is_err() && size("1T", "x").is_err());
        assert_eq!(unescape(r"\t\x41-"), b"\tA-");
        assert_eq!(unescape(r"a\qb"), br"a\qb");
    }
}
