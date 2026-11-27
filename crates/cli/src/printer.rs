//! How lines are shown: the standard `path:line:column:offset:text` form with
//! its separators, headings, colours and links; one line per match (`-o`,
//! `--vimgrep`); replacements (`-r`); long lines (`-M`, or lw's window);
//! the binary-file messages; the per-file summaries (`-c`, `-l`); and lw's
//! JSON objects. Byte for byte as ripgrep 15 prints them, except where lw's
//! help says otherwise (paths use `/`; JSON is lw's own form).
use crate::settings::{Mode, Settings};
use linework_search::{
    matcher::{Groups, Query, Template},
    searcher::Sink,
    sweep::LONG_LINE,
};
use serde_json::json;

/// A colour and style for one kind of text.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Style {
    fg: Option<Paint>,
    bg: Option<Paint>,
    bold: bool,
    underline: bool,
    italic: bool,
    intense: bool,
    /// `match:none`: no escapes around matches at all.
    off: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Paint {
    Named(u8),
    Index(u8),
    Rgb(u8, u8, u8),
}

impl Paint {
    fn code(self, background: bool, intense: bool) -> String {
        let base = if background { 40 } else { 30 };
        match self {
            Paint::Named(n) if intense => format!("\x1b[{};5;{}m", base + 8, n + 8),
            Paint::Named(n) => format!("\x1b[{}m", base + u32::from(n)),
            Paint::Index(n) => format!("\x1b[{};5;{n}m", base + 8),
            Paint::Rgb(r, g, b) => format!("\x1b[{};2;{r};{g};{b}m", base + 8),
        }
    }
}

impl Style {
    fn fg(n: u8) -> Self {
        Self { fg: Some(Paint::Named(n)), ..Self::default() }
    }

    /// The escape that starts text in this style: a reset, the attributes, then the colours.
    fn start(&self, out: &mut Vec<u8>) {
        if self.off {
            return;
        }
        out.extend_from_slice(b"\x1b[0m");
        self.resume(out);
    }

    /// The style's escapes without the reset in front.
    fn resume(&self, out: &mut Vec<u8>) {
        if self.off {
            return;
        }
        for (on, code) in [(self.bold, "\x1b[1m"), (self.italic, "\x1b[3m"), (self.underline, "\x1b[4m")] {
            if on {
                out.extend_from_slice(code.as_bytes());
            }
        }
        if let Some(fg) = self.fg {
            out.extend_from_slice(fg.code(false, self.intense).as_bytes());
        }
        if let Some(bg) = self.bg {
            out.extend_from_slice(bg.code(true, self.intense).as_bytes());
        }
    }

    fn end(&self, out: &mut Vec<u8>) {
        if !self.off {
            out.extend_from_slice(b"\x1b[0m");
        }
    }

    fn wrap(&self, out: &mut Vec<u8>, text: &[u8]) {
        self.start(out);
        out.extend_from_slice(text);
        self.end(out);
    }
}

/// The colours of each kind of text.
#[derive(Clone, Debug)]
pub struct Palette {
    pub path: Style,
    pub line: Style,
    pub column: Style,
    pub matched: Style,
    /// The rest of a matching line; no escapes unless `--colors highlight:...` sets it.
    pub highlight: Style,
}

impl Default for Palette {
    /// ripgrep's colours: green line numbers, bold red matches, and paths in
    /// cyan on Windows, magenta elsewhere.
    fn default() -> Self {
        Self {
            path: Style::fg(if cfg!(windows) { 6 } else { 5 }),
            line: Style::fg(2),
            column: Style::default(),
            matched: Style { bold: true, ..Style::fg(1) },
            highlight: Style { off: true, ..Style::default() },
        }
    }
}

impl Palette {
    /// Applies one `--colors` value: `TYPE:fg:COLOR`, `TYPE:bg:COLOR`, `TYPE:style:STYLE` or `TYPE:none`.
    pub fn apply(&mut self, spec: &str) -> Result<(), String> {
        let mut parts = spec.splitn(3, ':');
        let kind = parts.next().unwrap_or("");
        let style = match kind {
            "path" => &mut self.path,
            "line" => &mut self.line,
            "column" => &mut self.column,
            "match" => &mut self.matched,
            "highlight" => &mut self.highlight,
            other => return Err(format!("--colors: unrecognized output type '{other}'. Choose from: path, line, column, match, highlight.")),
        };
        match (parts.next(), parts.next()) {
            (Some("none"), None) => {
                // The wrappers stay, the colour goes; for matches, the escapes go too.
                *style = Style { off: kind == "match" || kind == "highlight", ..Style::default() };
            }
            (Some(which @ ("fg" | "bg")), Some(color)) => {
                let paint = paint(color)?;
                style.off = false;
                if which == "fg" {
                    style.fg = Some(paint);
                } else {
                    style.bg = Some(paint);
                }
            }
            (Some("style"), Some(name)) => {
                style.off = false;
                match name {
                    "bold" => style.bold = true,
                    "nobold" => style.bold = false,
                    "underline" => style.underline = true,
                    "nounderline" => style.underline = false,
                    "italic" => style.italic = true,
                    "noitalic" => style.italic = false,
                    "intense" => style.intense = true,
                    "nointense" => style.intense = false,
                    other => return Err(format!("--colors: unrecognized style attribute '{other}'. Choose from: nobold, bold, nointense, intense, nounderline, underline, noitalic, italic.")),
                }
            }
            _ => return Err(format!("--colors: {spec:?} is not TYPE:fg:COLOR, TYPE:bg:COLOR, TYPE:style:STYLE or TYPE:none")),
        }
        Ok(())
    }
}

/// A colour: a name, a number 0 to 255, or R,G,B; numbers in decimal or 0x hex.
fn paint(text: &str) -> Result<Paint, String> {
    const NAMES: [&str; 8] = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];
    if let Some(n) = NAMES.iter().position(|n| *n == text) {
        return Ok(Paint::Named(n as u8));
    }
    let byte = |t: &str| -> Option<u8> {
        let t = t.trim();
        match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
            Some(hex) => u8::from_str_radix(hex, 16).ok(),
            None => t.parse().ok(),
        }
    };
    let parts: Vec<&str> = text.split(',').collect();
    match parts.as_slice() {
        [one] => byte(one).map(Paint::Index),
        [r, g, b] => match (byte(r), byte(g), byte(b)) {
            (Some(r), Some(g), Some(b)) => Some(Paint::Rgb(r, g, b)),
            _ => None,
        },
        _ => None,
    }
    .ok_or_else(|| format!("--colors: unrecognized color name '{text}'. Choose from: black, blue, green, red, cyan, magenta, yellow, white"))
}

/// Paths made into terminal links (OSC 8), from `--hyperlink-format`.
#[derive(Clone, Debug)]
pub struct Links {
    format: String,
    host: String,
}

impl Links {
    /// The format, with ripgrep's names for common ones, checked as ripgrep checks it.
    pub fn new(format: &str, host_program: Option<&str>) -> Result<Option<Self>, String> {
        let format = match format {
            "" | "none" => return Ok(None),
            // As ripgrep: with the machine's name away from Windows.
            "default" => if cfg!(windows) { "file://{path}" } else { "file://{host}{path}" },
            "file" => "file://{host}{path}",
            "cursor" => "cursor://file{path}:{line}:{column}",
            "grep+" => "grep+://{path}:{line}",
            "kitty" => "file://{host}{path}#{line}",
            "macvim" => "mvim://open?url=file://{path}&line={line}&column={column}",
            "textmate" => "txmt://open?url=file://{path}&line={line}&column={column}",
            "vscode" => "vscode://file{path}:{line}:{column}",
            "vscode-insiders" => "vscode-insiders://file{path}:{line}:{column}",
            "vscodium" => "vscodium://file{path}:{line}:{column}",
            other => other,
        };
        let scheme = format.split_once(':').map(|(s, _)| s).unwrap_or("");
        if scheme.is_empty() || !scheme.bytes().all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b)) {
            return Err("--hyperlink-format: the hyperlink format must start with a valid URL scheme, i.e., [0-9A-Za-z+-.]+:".into());
        }
        if !format.contains("{path}") {
            return Err("--hyperlink-format: the {path} variable is required in a hyperlink format".into());
        }
        let mut rest = format;
        while let Some(open) = rest.find('{') {
            let close = rest[open..].find('}').map(|c| open + c).ok_or("--hyperlink-format: a { has no closing }")?;
            let name = &rest[open + 1..close];
            if !["path", "line", "column", "host", "wslprefix"].contains(&name) {
                return Err(format!("--hyperlink-format: invalid hyperlink format variable: '{name}', choose from: path, line, column, host, wslprefix"));
            }
            rest = &rest[close + 1..];
        }
        let host = if format.contains("{host}") { hostname(host_program)? } else { String::new() };
        Ok(Some(Self { format: format.to_owned(), host }))
    }

    /// The link for a file at a line and column.
    fn url(&self, absolute: &str, line: u64, column: u64) -> String {
        self.format
            .replace("{path}", &encode_path(absolute))
            .replace("{line}", &line.to_string())
            .replace("{column}", &column.to_string())
            .replace("{host}", &self.host)
            .replace("{wslprefix}", "")
    }
}

/// The machine's name for links: what `--hostname-bin` prints, or the system's own name for it.
fn hostname(program: Option<&str>) -> Result<String, String> {
    if let Some(program) = program {
        let output = std::process::Command::new(program).output().map_err(|e| format!("--hostname-bin {program}: {e}"))?;
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }
    #[cfg(windows)]
    return Ok(std::env::var("COMPUTERNAME").unwrap_or_default());
    #[cfg(unix)]
    {
        let mut name = [0u8; 256];
        if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } != 0 {
            return Ok(String::new());
        }
        let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        Ok(String::from_utf8_lossy(&name[..len]).into_owned())
    }
}

/// An absolute path as a URL path: `/` between names, a `/` in front, and
/// anything outside letters, digits and `/-._~:` percent-encoded.
fn encode_path(absolute: &str) -> String {
    let mut out = String::with_capacity(absolute.len() + 8);
    let path = absolute.replace('\\', "/");
    if !path.starts_with('/') {
        out.push('/');
    }
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Everything about how results look, the same for every file of a run.
pub struct Look {
    pub mode: Mode,
    pub quiet: bool,
    pub numbers: bool,
    pub column: bool,
    pub offsets: bool,
    /// One line per match (`-o`); never with `-v`.
    pub only: bool,
    pub replace: Option<Template>,
    pub trim: bool,
    pub max_columns: Option<u64>,
    pub preview: bool,
    /// lw's window around the match for lines over `LONG_LINE` bytes, when `-M` is not given.
    pub window: bool,
    pub vimgrep: bool,
    pub heading: bool,
    pub paths: bool,
    pub null: bool,
    pub tab: bool,
    pub path_separator: Option<u8>,
    pub field_match: Vec<u8>,
    pub field_context: Vec<u8>,
    /// The line between groups of context; None for no line.
    pub separator: Option<Vec<u8>>,
    /// Whether any context is shown, so groups and files are parted.
    pub context: bool,
    pub colors: Option<Palette>,
    pub links: Option<Links>,
    /// What ends a line lw writes itself: a line feed, CRLF with `--crlf`, a zero byte with `--null-data`.
    pub end: &'static [u8],
    pub invert: bool,
    pub scope: bool,
    /// Matches are counted per line (for -c with -o, --count-matches, stats, JSON, colours, -M, --column).
    pub track: bool,
    /// lw's JSON objects instead of lines, counts and names.
    pub json: bool,
    /// Each match is followed on its own (colours, -r, --column, --vimgrep), which -M's texts report.
    pub per_match: bool,
    pub include_zero: bool,
    /// --stats: files are read to the end even when one match would do, so the totals are whole.
    pub stats: bool,
}

impl Look {
    /// The look of a run from its settings; `paths` is whether paths are
    /// shown by default (several paths or a folder), `console` whether
    /// output goes to a console, and `stdin_only` whether the only input is
    /// what is piped in.
    pub fn new(s: &Settings, paths: bool, console: bool, stdin_only: bool) -> Result<Self, String> {
        let color = match s.color {
            crate::settings::Color::Always => true,
            crate::settings::Color::Never => false,
            crate::settings::Color::Auto => {
                console
                    && std::env::var_os("NO_COLOR").is_none()
                    && std::env::var("TERM").map_or(true, |t| t != "dumb")
                    && !s.vimgrep
                    && !s.json
            }
        };
        let colors = if color && !s.json {
            let mut palette = Palette::default();
            for spec in &s.colors {
                palette.apply(spec)?;
            }
            Some(palette)
        } else {
            for spec in &s.colors {
                Palette::default().apply(spec)?;
            }
            None
        };
        let links = match &s.hyperlink_format {
            Some(format) => Links::new(format, s.hostname_bin.as_deref())?.filter(|_| colors.is_some()),
            None => None,
        };
        let (before, after) = s.context_lines();
        let only = s.only_matching && !s.invert;
        let vimgrep = s.vimgrep;
        let column = (s.column || vimgrep) && !s.invert;
        // ripgrep's rule: line numbers by default in a console when files are searched, never for
        // piped input alone (so lw filters a pipe as grep does), always in JSON; -n and -N decide
        // when given, and --column and --vimgrep bring them unless -N says otherwise.
        let by_default = s.json || (console && !stdin_only);
        let numbers = s.line_number.unwrap_or(by_default) || ((s.column || vimgrep) && s.line_number != Some(false));
        let end: &'static [u8] = if s.matcher.null_data { b"\0" } else if s.matcher.crlf { b"\r\n" } else { b"\n" };
        let replace = s.replace.as_deref().map(Template::new);
        let per_match = colors.is_some() || replace.is_some() || column;
        let track = colors.is_some() || replace.is_some() || column || s.max_columns.is_some() || s.stats || s.mode == Mode::CountMatches || s.json || (s.mode == Mode::Count && s.only_matching);
        Ok(Self {
            mode: s.mode,
            quiet: s.quiet,
            numbers,
            column,
            offsets: s.byte_offset,
            only,
            replace,
            trim: s.trim,
            max_columns: s.max_columns,
            preview: s.max_columns_preview,
            // Lines over 1,000 bytes show the part around the match when files are searched; piped input
            // alone passes through whole, as a filter should (Zero: "Whole lines for piped input only").
            window: s.max_columns.is_none() && !s.whole_lines && !stdin_only,
            vimgrep,
            heading: !vimgrep && s.heading.unwrap_or(console) && s.mode == Mode::Lines,
            paths: vimgrep || s.with_filename.unwrap_or(paths),
            null: s.null,
            tab: s.initial_tab,
            path_separator: s.path_separator,
            field_match: s.field_match.clone(),
            field_context: s.field_context.clone(),
            separator: s.context_separator.clone(),
            context: (before > 0 || after > 0) && !s.passthru,
            colors,
            links,
            end,
            invert: s.invert,
            scope: s.scope,
            track,
            json: s.json,
            per_match,
            include_zero: s.include_zero,
            stats: s.stats,
        })
    }

    /// A path as it is printed: with --path-separator for every / and \.
    pub fn shown_path(&self, path: &str) -> Vec<u8> {
        match self.path_separator {
            Some(sep) => path.bytes().map(|b| if b == b'/' || b == b'\\' { sep } else { b }).collect(),
            None => path.as_bytes().to_vec(),
        }
    }

    /// What goes between the output of two files that both printed lines.
    pub fn between_files(&self, out: &mut Vec<u8>) {
        if self.mode != Mode::Lines || self.quiet {
            return;
        }
        if self.heading {
            out.extend_from_slice(self.end);
        } else if self.context {
            if let Some(separator) = &self.separator {
                out.extend_from_slice(separator);
                out.push(b'\n');
            }
        }
    }
}

/// What one file's search found, for the totals.
#[derive(Clone, Copy, Debug, Default)]
pub struct Found {
    /// Selected lines (with -v, the lines that do not match).
    pub lines: u64,
    pub matches: u64,
}

/// The printer for one file: the sink its search writes into.
pub struct FileOut<'a> {
    look: &'a Look,
    query: &'a Query,
    path: Vec<u8>,
    /// The file's absolute path, for links.
    absolute: Option<String>,
    pub out: Vec<u8>,
    pub found: Found,
    /// Width of line numbers and offsets with grep's -T: the digits of the file's size.
    width: usize,
    headed: bool,
    /// After a zero byte in convert mode: lines are counted, not printed.
    pub silenced: bool,
    /// Stop at the first selected line (-l, -q, --files-without-match).
    stop_at_first: bool,
    groups: Groups,
    /// With --scope: lines kept until the file ends, when their definitions are known.
    deferred: Option<Vec<Deferred>>,
    /// Bytes printed for this file.
    pub printed: u64,
}

struct Deferred {
    matched: bool,
    ended: bool,
    number: u64,
    offset: u64,
    text: Vec<u8>,
    after_break: bool,
}

impl<'a> FileOut<'a> {
    pub fn new(look: &'a Look, query: &'a Query, path: &str, absolute: Option<String>, size: Option<u64>) -> Self {
        let width = size.map_or(20, |s| s.max(1).to_string().len());
        let stop_at_first = (look.quiet || matches!(look.mode, Mode::WithMatches | Mode::WithoutMatch)) && !look.stats;
        Self {
            look,
            query,
            path: look.shown_path(path),
            absolute,
            out: Vec::new(),
            found: Found::default(),
            width,
            headed: false,
            silenced: false,
            stop_at_first,
            groups: Groups::new(),
            deferred: (look.scope && look.mode == Mode::Lines).then(Vec::new),
            printed: 0,
        }
    }

    /// The path as it is printed.
    pub fn path(&self) -> &[u8] {
        &self.path
    }

    /// Whether this file's search can stop: a summary that needs only one match has it.
    pub fn satisfied(&self) -> bool {
        self.stop_at_first && self.found.lines > 0
    }

    /// The matches in `text`, as byte ranges, into `ranges`.
    fn matches_in(&self, text: &[u8], ranges: &mut Vec<(usize, usize)>) {
        ranges.clear();
        if self.look.invert {
            return;
        }
        self.query.each(text, |s, e| {
            ranges.push((s, e));
            ranges.len() < 1_000_000
        });
    }

    fn heading(&mut self) {
        if self.headed || !self.look.heading || !self.look.paths {
            self.headed = true;
            return;
        }
        self.headed = true;
        let look = self.look;
        let link = look.links.as_ref().zip(self.absolute.as_deref()).map(|(links, abs)| links.url(abs, 1, 1));
        if let Some(url) = &link {
            self.out.extend_from_slice(format!("\x1b]8;;{url}\x1b\\").as_bytes());
        }
        match &look.colors {
            Some(palette) => palette.path.wrap(&mut self.out, &self.path),
            None => self.out.extend_from_slice(&self.path),
        }
        if link.is_some() {
            self.out.extend_from_slice(b"\x1b]8;;\x1b\\");
        }
        self.out.extend_from_slice(if look.null { b"\0" } else { look.end });
    }

    /// The fields before a line's text: path, line number, column, offset, each followed by its separator.
    fn prefix(&mut self, matched: bool, number: u64, column: Option<u64>, offset: u64) {
        let look = self.look;
        let sep: &[u8] = if matched { &look.field_match } else { &look.field_context };
        let mut fields: Vec<(Vec<u8>, Option<&Style>)> = Vec::new();
        let palette = look.colors.as_ref();
        let path_shown = look.paths && !look.heading;
        if look.numbers {
            let n = if look.tab { format!("{number:>width$}", width = self.width) } else { number.to_string() };
            fields.push((n.into_bytes(), palette.map(|p| &p.line)));
        }
        if let (Some(column), true) = (column, matched) {
            fields.push((column.to_string().into_bytes(), palette.map(|p| &p.column)));
        }
        if look.offsets {
            let o = if look.tab { format!("{offset:>width$}", width = self.width) } else { offset.to_string() };
            fields.push((o.into_bytes(), palette.map(|p| &p.column)));
        }
        let link = if path_shown {
            look.links.as_ref().zip(self.absolute.as_deref()).map(|(links, abs)| links.url(abs, if look.numbers { number } else { 1 }, column.unwrap_or(1)))
        } else {
            None
        };
        if let Some(url) = &link {
            self.out.extend_from_slice(format!("\x1b]8;;{url}\x1b\\").as_bytes());
        }
        if path_shown {
            match palette {
                Some(p) => p.path.wrap(&mut self.out, &self.path),
                None => self.out.extend_from_slice(&self.path),
            }
            if look.null {
                self.out.push(0);
            } else if !fields.is_empty() || link.is_none() {
                self.out.extend_from_slice(sep);
            }
        }
        let count = fields.len();
        for (i, (text, style)) in fields.into_iter().enumerate() {
            match style {
                Some(style) => style.wrap(&mut self.out, &text),
                None => self.out.extend_from_slice(&text),
            }
            // A link covers the path and the fields after it, up to the last separator.
            if i + 1 == count && link.is_some() {
                self.out.extend_from_slice(b"\x1b]8;;\x1b\\");
            }
            self.out.extend_from_slice(sep);
        }
        if count == 0 && link.is_some() {
            self.out.extend_from_slice(b"\x1b]8;;\x1b\\");
            if !look.null {
                self.out.extend_from_slice(sep);
            }
        }
        if look.tab && (path_shown || count > 0) {
            self.out.push(b'\t');
        }
    }

    /// The end of a printed line: its own line end as read (a CRLF line's CR
    /// is in its text), or for a last line that had none, the one lw writes
    /// (CRLF with --crlf, a zero byte with --null-data), as ripgrep does.
    fn finish_line(&mut self, ended: bool) {
        if ended {
            self.out.push(self.query.line_end());
        } else {
            self.out.extend_from_slice(self.look.end);
        }
    }

    /// A line's text with its matches marked, the rest highlighted if asked,
    /// and a long line cut down as `-M` or lw's window says.
    fn body(&mut self, matched: bool, text: &[u8], ranges: &[(usize, usize)], total: usize) {
        let look = self.look;
        // -M: the length counts the line end, as in ripgrep.
        if let Some(most) = look.max_columns {
            if text.len() as u64 + 1 > most {
                if look.preview {
                    let cut = char_boundary(text, most as usize);
                    let later = ranges.iter().filter(|(s, _)| *s >= cut).count();
                    let shown: Vec<(usize, usize)> = ranges.iter().filter(|(s, _)| *s < cut).map(|&(s, e)| (s, e.min(cut))).collect();
                    self.marked(matched, &text[..cut], &shown);
                    let tail = if look.per_match && matched && !look.invert { format!(" [... {later} more matches]") } else { " [... omitted end of long line]".to_owned() };
                    self.out.extend_from_slice(tail.as_bytes());
                } else {
                    let note = match (matched, look.per_match && !look.invert) {
                        (false, _) => "[Omitted long context line]".to_owned(),
                        (true, true) => format!("[Omitted long line with {total} matches]"),
                        (true, false) => "[Omitted long matching line]".to_owned(),
                    };
                    self.out.extend_from_slice(note.as_bytes());
                }
                return;
            }
        }
        if look.window && text.len() > LONG_LINE {
            let at = ranges.first().map_or(0, |r| r.0);
            let shown = linework_search::sweep::clip(text, at);
            if shown.start != 0 {
                self.out.extend_from_slice(format!("[{} bytes omitted] ", shown.start).as_bytes());
            }
            let inner: Vec<(usize, usize)> = ranges
                .iter()
                .filter(|(s, e)| *s >= shown.start && *e <= shown.end)
                .map(|&(s, e)| (s - shown.start, e - shown.start))
                .collect();
            self.marked(matched, &text[shown.clone()], &inner);
            if shown.end != text.len() {
                self.out.extend_from_slice(format!(" [{} bytes omitted]", text.len() - shown.end).as_bytes());
            }
            return;
        }
        self.marked(matched, text, ranges);
    }

    /// Text with its matches in the match colour and the rest in the highlight colour.
    fn marked(&mut self, matched: bool, text: &[u8], ranges: &[(usize, usize)]) {
        let Some(palette) = self.look.colors.as_ref().filter(|_| matched && !self.look.invert) else {
            self.out.extend_from_slice(text);
            return;
        };
        let highlight = !palette.highlight.off;
        if highlight {
            palette.highlight.start(&mut self.out);
        }
        let mut at = 0;
        for &(s, e) in ranges {
            if s < at || e > text.len() {
                continue;
            }
            self.out.extend_from_slice(&text[at..s]);
            palette.matched.wrap(&mut self.out, &text[s..e]);
            if highlight {
                palette.highlight.resume(&mut self.out);
            }
            at = e;
        }
        self.out.extend_from_slice(&text[at..]);
        if highlight {
            palette.highlight.end(&mut self.out);
        }
    }

    /// The text of a line with each match replaced (`-r`), and where the replacements are.
    fn replaced(&mut self, text: &[u8], template: &Template) -> (Vec<u8>, Vec<(usize, usize)>) {
        let mut out = Vec::with_capacity(text.len());
        let mut marks = Vec::new();
        let mut at = 0;
        let mut from = 0;
        while from <= text.len() {
            let Some((s, e)) = self.query.captures(text, from, &mut self.groups) else { break };
            out.extend_from_slice(&text[at..s]);
            let begin = out.len();
            template.expand(self.query, text, &self.groups, &mut out);
            marks.push((begin, out.len()));
            at = e;
            from = if e > s { e } else { e + 1 };
        }
        out.extend_from_slice(&text[at.min(text.len())..]);
        (out, marks)
    }

    /// One selected line, or the lines of one multiline match.
    fn print_matched(&mut self, number: u64, offset: u64, text: &[u8], ended: bool) {
        let look: &'a Look = self.look;
        let mut ranges = Vec::new();
        if look.track || look.only || look.vimgrep || look.window {
            self.matches_in(text, &mut ranges);
        }
        // -r over a multiline match: the whole match replaced, printed as one line.
        if let Some(template) = &look.replace {
            if !look.only && !look.vimgrep {
                let (text2, marks) = self.replaced(text, template);
                let column = look.column.then(|| ranges.first().map_or(1, |r| r.0 as u64 + 1));
                self.prefix(true, number, column, offset);
                let shown = if look.trim { trim(&text2) } else { &text2[..] };
                let skip = text2.len() - shown.len();
                let marks: Vec<(usize, usize)> = marks.iter().filter(|(s, _)| *s >= skip).map(|&(s, e)| (s - skip, e - skip)).collect();
                let shown = shown.to_vec();
                self.body(true, &shown, &marks, marks.len());
                self.finish_line(ended);
                return;
            }
        }
        let end = self.query.line_end();
        // Each line of the block: its own number, offset and matches.
        let mut line_start = 0usize;
        let mut line_number = number;
        loop {
            let line_end = memchr::memchr(end, &text[line_start..]).map_or(text.len(), |e| line_start + e);
            let line = &text[line_start..line_end];
            // Every line of a block but the last has its line end; the last has the block's.
            let line_ended = line_end < text.len() || ended;
            // A match belongs to the line it starts on (an empty one too, at
            // the line's start or end), and to each later line it runs into.
            let inside: Vec<(usize, usize)> = ranges
                .iter()
                .filter(|(s, e)| (*s >= line_start && *s <= line_end) || (*s < line_start && *e > line_start))
                .map(|&(s, e)| (s.max(line_start) - line_start, e.min(line_end) - line_start))
                .collect();
            let line_offset = offset + line_start as u64;
            if look.only || look.vimgrep {
                // One printed line per match on this line (for -U, only the matches starting here, for --vimgrep).
                for &(s, e) in &inside {
                    let column = Some(s as u64 + 1);
                    let match_offset = if look.only { line_offset + s as u64 } else { line_offset };
                    self.prefix(true, line_number, column.filter(|_| look.column), match_offset);
                    if look.only {
                        let piece = match &look.replace {
                            Some(template) => {
                                let mut groups = Groups::new();
                                let mut out = Vec::new();
                                if self.query.captures(text, line_start + s, &mut groups).is_some() {
                                    template.expand(self.query, text, &groups, &mut out);
                                }
                                out
                            }
                            None => line[s..e].to_vec(),
                        };
                        let len = piece.len();
                        self.marked(true, &piece, &[(0, len)]);
                        self.out.extend_from_slice(look.end);
                    } else {
                        // --vimgrep prints the line once per match, marking that match alone.
                        let body = if look.trim { trim(line) } else { line };
                        let skip = line.len() - body.len();
                        let marks: Vec<(usize, usize)> = if s >= skip { vec![(s - skip, e - skip)] } else { Vec::new() };
                        let body = body.to_vec();
                        self.body(true, &body, &marks, inside.len());
                        self.finish_line(line_ended);
                    }
                }
                // --vimgrep with -U shows only the first line of each match.
                if look.vimgrep && !look.only && self.query.multiline() {
                    break;
                }
            } else {
                let column = look.column.then(|| inside.first().map_or(1, |r| r.0 as u64 + 1));
                self.prefix(true, line_number, column, line_offset);
                let body = if look.trim { trim(line) } else { line };
                let skip = line.len() - body.len();
                let marks: Vec<(usize, usize)> = inside.iter().filter(|(a, _)| *a >= skip).map(|&(a, b)| (a - skip, b - skip)).collect();
                let body = body.to_vec();
                self.body(true, &body, &marks, inside.len());
                self.finish_line(line_ended);
            }
            if line_end >= text.len() {
                break;
            }
            line_start = line_end + 1;
            line_number += u64::from(line_number > 0);
        }
    }

    fn print_context(&mut self, number: u64, offset: u64, text: &[u8], ended: bool) {
        self.prefix(false, number, None, offset);
        let body = if self.look.trim { trim(text) } else { text };
        let body = body.to_vec();
        self.body(false, &body, &[], 0);
        self.finish_line(ended);
    }

    fn json_line(&mut self, matched: bool, number: u64, offset: u64, text: &[u8], scope: Option<&[linework_project::Scope]>) {
        let look: &'a Look = self.look;
        let bare = text.strip_suffix(b"\r").unwrap_or(text);
        let mut ranges = Vec::new();
        if matched {
            self.matches_in(bare, &mut ranges);
        }
        let first = ranges.first().map_or(0, |r| r.0);
        let shown = if look.window { linework_search::sweep::clip(bare, first) } else { 0..bare.len() };
        let inside: Vec<[usize; 2]> = ranges
            .iter()
            .filter(|(s, e)| *s >= shown.start && *e <= shown.end)
            .map(|&(s, e)| [s - shown.start, e - shown.start])
            .collect();
        let mut value = json!({
            "path": String::from_utf8_lossy(&self.path),
            "line": number,
            "text": String::from_utf8_lossy(&bare[shown.clone()]),
        });
        if matched {
            value["column"] = json!(String::from_utf8_lossy(&bare[..first]).chars().count() + 1);
            value["byte"] = json!(offset + first as u64);
            value["matches"] = json!(inside);
            if let Some(template) = &look.replace {
                let (replaced, _) = self.replaced(bare, template);
                value["replaced"] = json!(String::from_utf8_lossy(&replaced));
            }
        } else {
            value["context"] = json!(true);
            value["byte"] = json!(offset);
        }
        if shown != (0..bare.len()) {
            value["omitted_before"] = json!(shown.start);
            value["omitted_after"] = json!(bare.len() - shown.end);
        }
        if let Some(chain) = scope.filter(|c| !c.is_empty()) {
            let inner = chain.last().expect("not empty");
            value["scope"] = json!({"name": inner.name, "kind": inner.kind, "line": inner.line, "end_line": inner.end_line,
                "within": chain.iter().map(|s| json!({"name": s.name, "kind": s.kind, "line": s.line, "end_line": s.end_line})).collect::<Vec<_>>()});
        }
        serde_json::to_writer(&mut self.out, &value).expect("writing to memory");
        self.out.push(b'\n');
    }

    /// A selected line of which only a part was read (a line over a mebibyte,
    /// from the single-file engine): `text` starts `text_start - line_start`
    /// bytes into the line, and with `cut` the line goes on past it. A whole
    /// line goes the usual way.
    pub fn part_of_line(&mut self, number: u64, line_start: u64, text_start: u64, text: &[u8], cut: bool, ended: bool) -> bool {
        if text_start == line_start && !cut {
            return self.matched(number, line_start, text, ended);
        }
        self.count(text);
        if self.stop_at_first {
            return false;
        }
        // -q prints nothing, even when --stats has the file read to its end.
        if self.look.quiet {
            return true;
        }
        let look = self.look;
        if self.silenced || look.mode != Mode::Lines {
            return true;
        }
        let mut ranges = Vec::new();
        self.matches_in(text, &mut ranges);
        let first = ranges.first().map_or(0, |r| r.0);
        let before = text_start - line_start;
        let shown = linework_search::sweep::clip(text, first);
        if look.json {
            let inside: Vec<[usize; 2]> =
                ranges.iter().filter(|(s, e)| *s >= shown.start && *e <= shown.end).map(|&(s, e)| [s - shown.start, e - shown.start]).collect();
            let value = json!({
                "path": String::from_utf8_lossy(&self.path), "line": number, "column": null,
                "byte": text_start + first as u64, "text": String::from_utf8_lossy(&text[shown.clone()]), "matches": inside,
                "omitted_before": before + shown.start as u64, "omitted_after": if cut { json!(null) } else { json!(text.len() - shown.end) },
            });
            serde_json::to_writer(&mut self.out, &value).expect("writing to memory");
            self.out.push(b'\n');
            return true;
        }
        self.heading();
        // The column is not known: the line's start was not read.
        self.prefix(true, number, None, line_start);
        self.out.extend_from_slice(format!("[{} bytes omitted] ", before + shown.start as u64).as_bytes());
        let inner: Vec<(usize, usize)> = ranges.iter().filter(|(s, e)| *s >= shown.start && *e <= shown.end).map(|&(s, e)| (s - shown.start, e - shown.start)).collect();
        self.marked(true, &text[shown.clone()], &inner);
        if cut {
            self.out.extend_from_slice(b" [rest of line omitted]");
        } else if shown.end != text.len() {
            self.out.extend_from_slice(format!(" [{} bytes omitted]", text.len() - shown.end).as_bytes());
        }
        self.out.extend_from_slice(look.end);
        true
    }

    /// `PATH: binary file matches (found "\0" byte around offset N)`, once.
    pub fn binary_matches(&mut self, offset: u64) {
        if self.look.mode != Mode::Lines || self.look.quiet {
            return;
        }
        // grep's own wording, when lw is standing in for grep.
        if crate::alias::name().is_some_and(|name| name.ends_with("grep")) {
            self.out.extend_from_slice(b"Binary file ");
            self.out.extend_from_slice(if self.path.is_empty() { b"(standard input)" } else { &self.path });
            self.out.extend_from_slice(b" matches\n");
            return;
        }
        self.heading();
        self.message(&format!("binary file matches (found \"\\0\" byte around offset {offset})"));
    }

    /// `PATH: WARNING: stopped searching binary file after match (found "\0" byte around offset N)`.
    pub fn binary_stopped(&mut self, offset: u64) {
        if self.look.mode != Mode::Lines || self.look.quiet {
            return;
        }
        self.message(&format!("WARNING: stopped searching binary file after match (found \"\\0\" byte around offset {offset})"));
    }

    fn message(&mut self, text: &str) {
        if self.look.paths && !self.look.heading {
            self.out.extend_from_slice(&self.path);
            self.out.extend_from_slice(b": ");
        }
        self.out.extend_from_slice(text.as_bytes());
        self.out.push(b'\n');
    }

    /// Ends the file: the summary line of -c, -l and the like, and the lines
    /// held back for --scope, given the file's whole text for its outline.
    pub fn finish(&mut self, whole: Option<&[u8]>, source_path: &str) {
        let look = self.look;
        if let Some(deferred) = self.deferred.take() {
            let offsets: Vec<usize> = deferred.iter().map(|d| d.offset as usize).collect();
            let chains = match whole.and_then(|w| std::str::from_utf8(w).ok()) {
                Some(text) => linework_project::scope_chains(std::path::Path::new(source_path), text, &offsets),
                None => vec![Vec::new(); offsets.len()],
            };
            let mut shown: Option<String> = None;
            for (line, chain) in deferred.iter().zip(&chains) {
                if look.json {
                    self.json_line(line.matched, line.number, line.offset, &line.text, Some(chain));
                    continue;
                }
                if line.after_break {
                    self.context_break_now();
                }
                let key = chain.iter().map(|s| format!("{} {}", s.kind, s.name)).collect::<Vec<_>>().join(" > ");
                if !chain.is_empty() && shown.as_deref() != Some(key.as_str()) {
                    let inner = chain.last().expect("not empty");
                    let span = if inner.line == inner.end_line { format!("line {}", inner.line) } else { format!("lines {}-{}", inner.line, inner.end_line) };
                    self.heading();
                    self.out.extend_from_slice(format!("  {key} ({span})").as_bytes());
                    self.out.extend_from_slice(look.end);
                }
                shown = Some(key);
                if line.matched {
                    self.print_matched(line.number, line.offset, &line.text, line.ended);
                } else {
                    self.print_context(line.number, line.offset, &line.text, line.ended);
                }
            }
        }
        let count = match look.mode {
            Mode::Count if look.only => Some(self.found.matches),
            Mode::Count if self.query.multiline() && self.query.can_match_line_end() => Some(self.found.matches),
            Mode::Count => Some(self.found.lines),
            Mode::CountMatches if look.invert => Some(self.found.lines),
            Mode::CountMatches => Some(self.found.matches),
            _ => None,
        };
        if look.quiet {
            return;
        }
        if let (Some(count), true) = (count, look.json) {
            if count > 0 || look.include_zero {
                let value = json!({"path": String::from_utf8_lossy(&self.path), "count": count});
                serde_json::to_writer(&mut self.out, &value).expect("writing to memory");
                self.out.push(b'\n');
            }
        } else if let Some(count) = count {
            if count > 0 || look.include_zero {
                if look.paths {
                    self.path_out();
                    self.out.push(if look.null { 0 } else { b':' });
                }
                self.out.extend_from_slice(count.to_string().as_bytes());
                self.out.extend_from_slice(look.end);
            }
        }
        let listed = match look.mode {
            Mode::WithMatches => self.found.lines > 0,
            Mode::WithoutMatch => self.found.lines == 0,
            _ => false,
        };
        if listed && look.json {
            serde_json::to_writer(&mut self.out, &json!({"path": String::from_utf8_lossy(&self.path)})).expect("writing to memory");
            self.out.push(b'\n');
        } else if listed {
            self.path_out();
            self.out.extend_from_slice(if look.null { b"\0" } else { look.end });
        }
        self.printed = self.out.len() as u64;
    }

    fn path_out(&mut self) {
        let look = self.look;
        let link = look.links.as_ref().zip(self.absolute.as_deref()).map(|(links, abs)| links.url(abs, 1, 1));
        if let Some(url) = &link {
            self.out.extend_from_slice(format!("\x1b]8;;{url}\x1b\\").as_bytes());
        }
        match &look.colors {
            Some(palette) => palette.path.wrap(&mut self.out, &self.path),
            None => self.out.extend_from_slice(&self.path),
        }
        if link.is_some() {
            self.out.extend_from_slice(b"\x1b]8;;\x1b\\");
        }
    }

    fn context_break_now(&mut self) {
        if let Some(separator) = &self.look.separator {
            self.out.extend_from_slice(separator);
            self.out.push(b'\n');
        }
    }

    /// Counts the matches on a selected line, for the totals and the summaries.
    fn count(&mut self, text: &[u8]) {
        let lines = if self.query.multiline() { memchr::memchr_iter(self.query.line_end(), text).count() as u64 + 1 } else { 1 };
        self.found.lines += lines;
        // -c with -U counts matches when a match can hold a line end, as in ripgrep.
        let spanning = self.look.mode == Mode::Count && self.query.multiline() && self.query.can_match_line_end();
        if (self.look.track || spanning) && !self.look.invert {
            let mut n = 0u64;
            self.query.each(text, |_, _| {
                n += 1;
                true
            });
            self.found.matches += n;
        }
    }
}

impl Sink for FileOut<'_> {
    fn matched(&mut self, number: u64, offset: u64, text: &[u8], ended: bool) -> bool {
        self.count(text);
        if self.stop_at_first {
            return false;
        }
        // -q prints nothing, even when --stats has the file read to its end.
        if self.look.quiet {
            return true;
        }
        let look = self.look;
        if look.mode != Mode::Lines {
            return true;
        }
        if self.silenced {
            return true;
        }
        if let Some(deferred) = &mut self.deferred {
            deferred.push(Deferred { matched: true, ended, number, offset, text: text.to_vec(), after_break: false });
            return true;
        }
        if look.json {
            self.json_line(true, number, offset, text, None);
            return true;
        }
        self.heading();
        self.print_matched(number, offset, text, ended);
        true
    }

    fn context(&mut self, number: u64, offset: u64, text: &[u8], _after: bool, ended: bool) -> bool {
        let look = self.look;
        if self.silenced || look.mode != Mode::Lines || self.stop_at_first || look.quiet {
            return true;
        }
        if let Some(deferred) = &mut self.deferred {
            deferred.push(Deferred { matched: false, ended, number, offset, text: text.to_vec(), after_break: false });
            return true;
        }
        if look.json {
            self.json_line(false, number, offset, text, None);
            return true;
        }
        self.heading();
        self.print_context(number, offset, text, ended);
        true
    }

    fn context_break(&mut self) -> bool {
        let look = self.look;
        if self.silenced || look.mode != Mode::Lines || self.stop_at_first || look.quiet {
            return true;
        }
        if let Some(deferred) = &mut self.deferred {
            deferred.push(Deferred { matched: false, ended: true, number: 0, offset: 0, text: Vec::new(), after_break: true });
            return true;
        }
        self.context_break_now();
        true
    }
}

/// The text without the spaces and tabs it starts with (ASCII white space, as ripgrep's --trim).
fn trim(text: &[u8]) -> &[u8] {
    let n = text.iter().take_while(|b| matches!(b, b' ' | b'\t' | b'\x0B' | b'\x0C' | b'\r' | b'\n')).count();
    &text[n..]
}

/// The largest position at most `chars` characters into `text`, at a character boundary.
fn char_boundary(text: &[u8], chars: usize) -> usize {
    let mut seen = 0;
    for (i, b) in text.iter().enumerate() {
        if (b & 0xC0) != 0x80 {
            if seen == chars {
                return i;
            }
            seen += 1;
        }
    }
    text.len()
}
