use super::{Editor, sci::*};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Theme {
    Dark,
    Light,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaretStyle {
    Line,
    Wide,
    Block,
}

// Colors below are written as RGB, while native COLORREF stores BGR.
fn bgr(rgb: u32) -> isize {
    ((rgb & 0xff00) | ((rgb & 0xff) << 16) | (rgb >> 16)) as isize
}
fn rgba(rgb: u32) -> isize {
    bgr(rgb) | (0xffisize << 24)
}
struct Palette {
    bg: u32,
    fg: u32,
    gutter: u32,
    subtle: u32,
    comment: u32,
    keyword: u32,
    string: u32,
    number: u32,
    symbol: u32,
    tag: u32,
    operator: u32,
    accent: u32,
    active: u32,
    selection: u32,
    warning: u32,
}
macro_rules! color_roles {
    ($(($name:ident,$field:ident,$label:literal)),+) => {
        #[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
        pub enum ColorRole { $($name),+ }
        impl ColorRole {
            pub const ALL: &'static [Self] = &[$(Self::$name),+];
            pub fn label(self)->&'static str { match self { $(Self::$name=>$label),+ } }
            fn channel(self,p:&mut Palette)->&mut u32 { match self { $(Self::$name=>&mut p.$field),+ } }
        }
    }
}
color_roles! {
    (Background,bg,"Editor background"),(Text,fg,"Default text"),(Comment,comment,"Comments"),
    (Keyword,keyword,"Keywords and commands"),(String,string,"Strings and characters"),
    (Number,number,"Numbers, constants and escapes"),(Symbol,symbol,"Types, symbols and headings"),
    (Tag,tag,"Tags, macros and directives"),(Operator,operator,"Operators and delimiters"),
    (Caret,accent,"Carets, links and matching braces"),(Selection,selection,"Selected text background"),
    (ActiveLine,active,"Current line background"),(Warning,warning,"Errors and spelling underlines"),
    (Gutter,gutter,"Gutter background"),(LineNumbers,subtle,"Line numbers and folding controls")
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ColorScheme {
    pub dark: BTreeMap<ColorRole, u32>,
    pub light: BTreeMap<ColorRole, u32>,
}
impl ColorScheme {
    fn palette(&self, theme: Theme) -> Palette {
        let mut p = theme.palette();
        let colors = match theme {
            Theme::Dark => &self.dark,
            Theme::Light => &self.light,
        };
        for (&role, &value) in colors {
            *role.channel(&mut p) = value & 0xffffff;
        }
        p
    }
    pub fn color(&self, theme: Theme, role: ColorRole) -> u32 {
        *role.channel(&mut self.palette(theme))
    }
    pub fn set(&mut self, theme: Theme, role: ColorRole, value: u32) {
        match theme {
            Theme::Dark => &mut self.dark,
            Theme::Light => &mut self.light,
        }
        .insert(role, value & 0xffffff);
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HighlightOptions {
    pub syntax: bool,
    pub current_line: bool,
    pub brace_pairs: bool,
}
impl Default for HighlightOptions {
    fn default() -> Self {
        Self {
            syntax: true,
            current_line: true,
            brace_pairs: true,
        }
    }
}
pub const MARK_COLORS: [(&str, u32); 5] = [
    ("Amber", 0xEDB749),
    ("Mint", 0x66C998),
    ("Blue", 0x62AFE4),
    ("Rose", 0xEB869D),
    ("Violet", 0xB69AE4),
];
impl Theme {
    fn palette(self) -> Palette {
        match self {
            Theme::Dark => Palette {
                bg: 0x18191F,
                fg: 0xE7E5F0,
                gutter: 0x18191F,
                subtle: 0x7C7B8F,
                comment: 0x9693A5,
                keyword: 0xCDB2F4,
                string: 0x9EDBC1,
                number: 0xF3C18E,
                symbol: 0x90C9EA,
                tag: 0xF2A7A1,
                operator: 0xC7CED9,
                accent: 0xCFB5F6,
                active: 0x22232C,
                selection: 0x514760,
                warning: 0xF29595,
            },
            Theme::Light => Palette {
                bg: 0xFCFCFA,
                fg: 0x262D36,
                gutter: 0xF2F3F0,
                subtle: 0x626F73,
                comment: 0x537461,
                keyword: 0x6B4198,
                string: 0x296130,
                number: 0x904B17,
                symbol: 0x125B83,
                tag: 0x984341,
                operator: 0x444B57,
                accent: 0x006E58,
                active: 0xEFF3EF,
                selection: 0xC5DBF0,
                warning: 0xBA3948,
            },
        }
    }
}

type Styles = HashMap<String, Vec<(usize, String)>>;
fn definitions() -> &'static Styles {
    static MAP: OnceLock<Styles> = OnceLock::new();
    MAP.get_or_init(|| serde_json::from_str(include_str!("../assets/styles.json")).unwrap())
}
impl Editor {
    pub fn set_appearance(&self, scheme: &ColorScheme, options: HighlightOptions) {
        self.colors.replace(scheme.clone());
        self.highlights.set(options);
        self.set_theme(self.theme.get());
        self.refresh_braces();
    }
    pub fn set_theme(&self, theme: Theme) {
        self.theme.set(theme);
        let p = self.colors.borrow().palette(theme);
        self.send(SCI_STYLERESETDEFAULT, 0, 0);
        // Menlo is the fixed-width font every Mac has; 13 points there reads
        // about as large as 11 on Windows.
        #[cfg(target_os = "macos")]
        let (font, size) = ("Menlo", 1300);
        #[cfg(not(target_os = "macos"))]
        let (font, size) = ("Cascadia Mono", 1100);
        self.put_str(SCI_STYLESETFONT, 32, font);
        self.send(SCI_STYLESETSIZEFRACTIONAL, 32, size);
        self.send(SCI_STYLESETFORE, 32, bgr(p.fg));
        self.send(SCI_STYLESETBACK, 32, bgr(p.bg));
        self.send(SCI_STYLECLEARALL, 0, 0);
        self.send(SCI_STYLESETFORE, 33, bgr(p.subtle));
        self.send(SCI_STYLESETBACK, 33, bgr(p.gutter));
        self.send(SCI_STYLESETFORE, 34, bgr(p.accent));
        self.send(SCI_STYLESETBOLD, 34, 1);
        self.send(SCI_STYLESETFORE, 35, bgr(p.warning));
        self.send(SCI_STYLESETFORE, 37, bgr(p.gutter));
        self.send(SCI_STYLESETSIZEFRACTIONAL, 33, 950);
        self.send(SCI_SETMARGINTYPEN, 0, 1);
        self.send(SCI_SETMARGINMASKN, 0, 0);
        self.send(SCI_SETMARGINTYPEN, 1, 6);
        self.send(SCI_SETMARGINBACKN, 1, bgr(p.gutter));
        self.send(SCI_SETMARGINMASKN, 1, 1);
        self.send(SCI_SETMARGINSENSITIVEN, 1, 1);
        self.send(SCI_MARKERDEFINE, 0, 0);
        self.send(SCI_MARKERSETFORE, 0, bgr(p.accent));
        self.send(SCI_MARKERSETBACK, 0, bgr(p.accent));
        self.send(SCI_SETMARGINTYPEN, 2, 0);
        self.send(SCI_SETMARGINMASKN, 2, 0xfe000000u32 as isize);
        self.send(SCI_SETMARGINSENSITIVEN, 2, 1);
        self.send(SCI_SETFOLDMARGINCOLOUR, 1, bgr(p.gutter));
        self.send(SCI_SETFOLDMARGINHICOLOUR, 1, bgr(p.gutter));
        for (slot, symbol) in [
            (25, 2),
            (26, 6),
            (27, 5),
            (28, 5),
            (29, 5),
            (30, 2),
            (31, 6),
        ] {
            self.send(SCI_MARKERDEFINE, slot, symbol);
            self.send(SCI_MARKERSETFORE, slot, bgr(p.subtle));
            self.send(SCI_MARKERSETBACK, slot, bgr(p.subtle));
        }
        self.send(SCI_SETSELECTIONLAYER, 1, 0);
        // The current-line highlight is the base layer. Drawn in the same layer
        // as the selection it is painted after it, and being opaque it hid
        // every selection on the caret's line.
        self.send(SCI_SETCARETLINELAYER, 0, 0);
        for (element, color) in [
            (0, p.fg),
            (1, p.bg),
            (2, p.fg),
            (3, p.selection),
            (11, p.selection),
            (13, p.selection),
            (15, p.selection),
            (17, p.selection),
            (19, p.selection),
            (40, p.accent),
            (41, p.accent),
            (50, p.active),
            (60, p.subtle),
        ] {
            self.send(SCI_SETELEMENTCOLOUR, element, rgba(color));
        }
        for (indicator, style, color) in [(8, 11, p.warning), (9, 7, p.accent)] {
            self.send(SCI_INDICSETSTYLE, indicator, style);
            self.send(SCI_INDICSETFORE, indicator, bgr(color));
            self.send(SCI_INDICSETUNDER, indicator, 1);
        }
        for (slot, (_, color)) in MARK_COLORS.iter().enumerate() {
            let indicator = 10 + slot;
            self.send(SCI_INDICSETSTYLE, indicator, INDIC_STRAIGHTBOX as isize);
            self.send(SCI_INDICSETFORE, indicator, bgr(*color));
            self.send(SCI_INDICSETUNDER, indicator, 1);
            self.send(SCI_INDICSETALPHA, indicator, 65);
            self.send(SCI_INDICSETOUTLINEALPHA, indicator, 155);
        }
        self.set_current_line_highlight(self.highlights.get().current_line);
        self.send(SCI_INDICSETALPHA, 9, 45);
        self.send(SCI_INDICSETOUTLINEALPHA, 9, 105);
        self.send(SCI_SETEXTRAASCENT, 2, 0);
        self.send(SCI_SETEXTRADESCENT, 2, 0);
        self.apply_lexer_styles();
        self.refresh_margins();
    }
    pub fn set_dpi(&self, dpi: u32) {
        self.dpi.set(dpi.clamp(72, 480));
        self.refresh_margins();
    }
    pub fn refresh_margins(&self) {
        let lines = self.send(SCI_GETLINECOUNT, 0, 0).max(1) as u64;
        let digits = (lines.ilog10() + 1).max(3) as usize;
        let scale = self.dpi.get() as isize;
        let width = self.put_str(SCI_TEXTWIDTH, 33, &"9".repeat(digits));
        self.send(SCI_SETMARGINWIDTHN, 0, width + 8 * scale / 96);
        self.send(SCI_SETMARGINWIDTHN, 1, 11 * scale / 96);
        self.send(SCI_SETMARGINWIDTHN, 2, 14 * scale / 96);
        self.send(SCI_SETMARGINLEFT, 0, 6 * scale / 96);
    }
    pub fn set_caret(&self, style: CaretStyle, width: usize, blink_ms: u32) {
        self.caret_style.set(style);
        self.send(
            SCI_SETCARETSTYLE,
            if style == CaretStyle::Block { 2 } else { 1 },
            0,
        );
        self.send(
            SCI_SETCARETWIDTH,
            if style == CaretStyle::Wide {
                width.max(3)
            } else {
                width
            }
            .clamp(1, 8),
            0,
        );
        self.send(SCI_SETCARETPERIOD, blink_ms.min(2000) as usize, 0);
    }
    pub fn set_current_line_highlight(&self, on: bool) {
        self.highlights.set(HighlightOptions {
            current_line: on,
            ..self.highlights.get()
        });
        if on {
            self.send(
                SCI_SETELEMENTCOLOUR,
                50,
                rgba(self.colors.borrow().palette(self.theme.get()).active),
            );
        } else {
            self.send(SCI_RESETELEMENTCOLOUR, 50, 0);
        }
    }
    fn style_tag(&self, style: usize, msg: u32) -> String {
        let length = unsafe { self.send_ptr(msg, style, 0) };
        if !(1..1024).contains(&length) {
            return String::new();
        }
        let mut bytes = vec![0; length as usize + 1];
        unsafe {
            self.send_ptr(msg, style, bytes.as_mut_ptr() as isize);
        }
        String::from_utf8_lossy(&bytes[..length as usize]).to_ascii_lowercase()
    }
    /// Lexical runs for package commands that inspect Sublime scope selectors.
    pub fn package_scope_runs(&self) -> (Vec<(usize, u8)>, Vec<String>) {
        let mut names = vec![String::new(); 256];
        let mut runs = vec![];
        let language = self.language.get();
        let fallback = definitions().get(&language.lexer);
        let mut used = [false; 256];
        let bytes = self.bytes(0, self.len(), true);
        for (i, pair) in bytes.chunks_exact(2).enumerate() {
            let id = pair[1];
            if runs.last().is_some_and(|&(_, last)| id == last) {
                continue;
            }
            runs.push((i, id));
            let n = id as usize;
            if std::mem::replace(&mut used[n], true) {
                continue;
            }
            if self.is_custom_grammar() {
                names[n] = [
                    "",
                    "comment",
                    "string",
                    "keyword",
                    "constant",
                    "entity.name",
                    "entity.name.tag",
                    "punctuation",
                    "invalid",
                ]
                .get(n)
                .unwrap_or(&"")
                .to_string();
                continue;
            }
            let mut tag = self.style_tag(n, SCI_NAMEOFSTYLE);
            if tag.is_empty() {
                if let Some((_, v)) = fallback.and_then(|f| f.iter().find(|r| r.0 == n)) {
                    tag = v.to_lowercase();
                }
            }
            let combined = format!("{} {}", tag, self.style_tag(n, SCI_TAGSOFSTYLE));
            names[n] = if combined.contains("comment") {
                "comment"
            } else if combined.contains("string")
                || combined.contains("character")
                || tag.ends_with("_char")
            {
                "string.quoted"
            } else if combined.contains("keyword") || tag.ends_with("_word") {
                "keyword"
            } else if combined.contains("operator") {
                "keyword.operator"
            } else if combined.contains("number") {
                "constant.numeric"
            } else if combined.contains("identifier") {
                "variable.other"
            } else if combined.contains("class") {
                "entity.name.class"
            } else if combined.contains("function") {
                "entity.name.function"
            } else if combined.contains("error") {
                "invalid"
            } else {
                ""
            }
            .into();
        }
        (runs, names)
    }
    pub(super) fn apply_lexer_styles(&self) {
        if self.is_custom_grammar() {
            let p = self.colors.borrow().palette(self.theme.get());
            let syntax = self.highlights.get().syntax;
            for (id, color) in [
                p.fg, p.comment, p.string, p.keyword, p.number, p.symbol, p.tag, p.operator,
                p.warning,
            ]
            .into_iter()
            .enumerate()
            {
                self.send(SCI_STYLESETFORE, id, bgr(if syntax { color } else { p.fg }));
                self.send(SCI_STYLESETBACK, id, bgr(p.bg));
                self.send(SCI_STYLESETBOLD, id, (syntax && id == 3) as isize);
                self.send(SCI_STYLESETITALIC, id, (syntax && id == 1) as isize);
            }
            let mut prose = [false; 256];
            prose[1] = true;
            prose[2] = true;
            self.prose_styles.set(prose);
            return;
        }
        let lang = self.language.get();
        let p = self.colors.borrow().palette(self.theme.get());
        let syntax = self.highlights.get().syntax;
        let mut prose = [lang.name == "Plain Text"; 256];
        let fallback = definitions().get(&lang.lexer);
        let count = self.send(SCI_GETNAMEDSTYLES, 0, 0).clamp(0, 256) as usize;
        let count = count
            .max(
                fallback
                    .and_then(|v| v.iter().map(|s| s.0 + 1).max())
                    .unwrap_or(0),
            )
            .min(256);
        for style in 0..count {
            if (32..40).contains(&style) {
                continue;
            }
            let meta = self.style_tag(style, SCI_TAGSOFSTYLE);
            let mut name = self.style_tag(style, SCI_NAMEOFSTYLE);
            if name.is_empty() {
                if let Some((_, v)) =
                    fallback.and_then(|list| list.iter().find(|entry| entry.0 == style))
                {
                    name = v.to_ascii_lowercase();
                }
            }
            let suffix = name.rsplit('_').next().unwrap_or("");
            let contains = |s: &str| meta.contains(s) || suffix.contains(s);
            let comment = contains("comment");
            let string = contains("string")
                || contains("character")
                || suffix == "char"
                || suffix == "template"
                || contains("heredoc");
            let heading = contains("header")
                || contains("heading")
                || suffix == "strong1"
                || suffix == "strong2";
            let color = if comment {
                p.comment
            } else if contains("error") && !contains("unknown") {
                p.warning
            } else if string || (lang.name == "Markdown" && contains("code")) {
                p.string
            } else if contains("numeric")
                || contains("number")
                || contains("boolean")
                || contains("escape")
                || contains("constant")
            {
                p.number
            } else if heading
                || contains("class")
                || contains("function")
                || contains("defname")
                || contains("type")
                || suffix == "word2"
                || suffix == "attribute"
                || contains("property")
            {
                p.symbol
            } else if contains("preprocessor")
                || contains("macro")
                || suffix == "preproc"
                || contains("directive")
                || contains("tag")
            {
                p.tag
            } else if contains("keyword")
                || suffix.starts_with("word")
                || contains("command")
                || contains("statement")
                || contains("builtin")
            {
                p.keyword
            } else if contains("link") || contains("reference") || contains("decorator") {
                p.accent
            } else if contains("operator") || contains("delimiter") {
                p.operator
            } else {
                p.fg
            };
            self.send(
                SCI_STYLESETFORE,
                style,
                bgr(if syntax { color } else { p.fg }),
            );
            self.send(SCI_STYLESETBACK, style, bgr(p.bg));
            self.send(SCI_STYLESETBOLD, style, (syntax && heading) as isize);
            self.send(
                SCI_STYLESETITALIC,
                style,
                (syntax && (suffix == "em1" || suffix == "em2")) as isize,
            );
            self.send(
                SCI_STYLESETUNDERLINE,
                style,
                (syntax && contains("link")) as isize,
            );
            prose[style] = comment
                || lang.name == "Plain Text"
                || (lang.name == "Markdown"
                    && !contains("code")
                    && !contains("link")
                    && !contains("front"))
                || (lang.name == "HTML" && style == 0);
        }
        self.prose_styles.set(prose);
    }
}
