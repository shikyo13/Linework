use crate::{
    surface::{Surface, file_icon},
    win::*,
};
use std::cell::RefCell;
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    UI::{Controls::DRAWITEMSTRUCT, WindowsAndMessaging::*},
};

#[path = "frame_controls.rs"]
pub(crate) mod controls;
#[derive(Clone, Debug, PartialEq)]
pub enum Hit {
    Cmd(String),
    Menu(String),
    Tab(usize),
    Close(usize),
    Side(u8),
    Recent(usize),
    ScrollTabs(i32),
}
#[derive(Default, Clone, PartialEq)]
pub struct Frame {
    pub light: bool,
    pub sidebar: bool,
    pub sidebar_width: i32,
    pub side: u8,
    pub workspace: String,
    pub tabs: Vec<(String, bool)>,
    pub active: Option<usize>,
    pub alternate: Option<usize>,
    pub first_tab: usize,
    pub split: bool,
    pub find: bool,
    pub replace: bool,
    pub case_: bool,
    pub regex: bool,
    pub whole: bool,
    pub search_folder: bool,
    pub selection_scope: bool,
    pub recent: Vec<String>,
    pub recording: bool,
    pub preview: bool,
    pub can_preview: bool,
    pub document_preview: bool,
    pub report: String,
    /// The sidebar's search view shows the search panel.
    pub search_panel: bool,
    pub panel: PanelState,
    /// The last folder search's totals, and the lw command that finds the same lines.
    pub summary: String,
}

/// The search panel's switches, as drawn.
#[derive(Default, Clone, PartialEq)]
pub struct PanelState {
    pub case_: bool,
    pub whole: bool,
    pub regex: bool,
    pub scope: bool,
    pub hidden: bool,
    pub ignored: bool,
    /// The files, conditions and context fields are shown.
    pub more: bool,
    pub context: u32,
    pub near_lines: u32,
}

/// A line of folder search results, drawn in the sidebar list.
#[derive(Clone)]
pub enum Rich {
    /// A file's heading: its name, its folder, and how many matches it has.
    File { name: String, folder: String, count: usize },
    /// The definitions the following lines are in, outermost first.
    Scope(String),
    /// A line: its number, its text, its matches as byte ranges, whether it
    /// is context, the replacements of its matches when previewing, and
    /// whether its replacements are checked.
    Line { number: u64, text: String, ranges: Vec<(usize, usize)>, context: bool, replaced: Option<Vec<String>>, checked: Option<bool> },
    /// Between lines that are not next to each other.
    Break,
    Note(String),
}

/// The search panel's layout in the sidebar, from the top. The fields are
/// child windows placed at these heights; the switches are drawn between them.
pub const PANEL_TOP: i32 = HEAD + 50;
pub const FIELD_H: i32 = 30;
const PANEL_STEP: i32 = 36;
/// The width of the labels before the panel's More fields.
pub const PANEL_LABEL: i32 = 46;
/// The narrowest sidebar the panel is laid out for.
pub const PANEL_WIDTH: i32 = 320;
/// The top of field `i`: 0 find, 1 replace, then with More open 2 files, 3
/// skip, 4 types, 5 near, 6 also has, 7 lacks.
pub fn panel_field_y(i: usize) -> i32 {
    match i {
        0 => PANEL_TOP,
        1 => PANEL_TOP + PANEL_STEP,
        n => PANEL_TOP + 2 * PANEL_STEP + 34 + (n as i32 - 2) * PANEL_STEP,
    }
}
/// The row of switches under the find and replace fields.
fn panel_switches_y() -> i32 {
    PANEL_TOP + 2 * PANEL_STEP
}
/// The row of context, hidden and ignored switches at the end of More.
fn panel_extra_y() -> i32 {
    panel_field_y(8)
}
/// Where the panel ends and its summary line starts.
pub fn panel_bottom(more: bool) -> i32 {
    if more {
        panel_extra_y() + 34
    } else {
        panel_switches_y() + 34
    }
}
/// How tall the find bar is: its row, and the replace row.
pub fn findbar_height(replace: bool) -> i32 {
    40 + if replace { 36 } else { 0 }
}
thread_local! {
    /// The rows of the folder search results, drawn in place of the list's plain text.
    pub static RICH: RefCell<Vec<Rich>> = const { RefCell::new(Vec::new()) };
    pub static FRAME:RefCell<Frame>=RefCell::new(Frame::default());
    pub static HITS:RefCell<Vec<(RECT,Hit)>>=const {RefCell::new(Vec::new())};
    static HOT:RefCell<Option<Hit>>=const {RefCell::new(None)};
}
pub const HEAD: i32 = 44;
pub const TABS: i32 = 41;
pub const FOOT: i32 = 29;
pub const RAIL: i32 = 48;
pub const ROW: i32 = 29;
pub fn tab_reserve(preview: bool) -> i32 { if preview { 183 } else { 99 } }
const OPEN_Y: i32 = HEAD + 78;
pub fn list_y(side: u8, folder: bool, tabs: usize, caption: bool, panel: Option<bool>) -> i32 {
    if let (2, Some(more)) = (side, panel) {
        panel_bottom(more) + 30
    } else if side == 0 && folder {
        OPEN_Y + ROW * tabs.min(4).max(1) as i32 + 50
    } else if side == 0 || caption {
        OPEN_Y
    } else {
        HEAD + 55
    }
}
pub fn tab_width(name: &str) -> i32 {
    (name.chars().count() as i32 * 7 + 69).clamp(116, 219)
}
fn cmd(id: &str) -> Hit {
    Hit::Cmd(id.into())
}
struct Shell<'a> {
    s: &'a Surface,
    p: Colors,
    f: &'a Frame,
    w: i32,
    h: i32,
}
impl Shell<'_> {
    fn hit(&self, x: i32, y: i32, w: i32, h: i32, action: Hit) {
        HITS.with_borrow_mut(|v| v.push((self.s.r(x, y, w, h), action)))
    }
    fn hot(&self, a: &Hit) -> bool {
        HOT.with_borrow(|v| v.as_ref() == Some(a))
    }
    fn button(&self, x: i32, y: i32, w: i32, h: i32, name: &str, id: &str, on: bool) {
        let (s, p) = (self.s, self.p);
        let a = cmd(id);
        let (background, foreground, outline) = if high_contrast() {
            unsafe {
                if on || self.hot(&a) {
                    (GetSysColor(COLOR_HIGHLIGHT), GetSysColor(COLOR_HIGHLIGHTTEXT), GetSysColor(COLOR_HIGHLIGHTTEXT))
                } else {
                    (GetSysColor(COLOR_WINDOW), GetSysColor(COLOR_WINDOWTEXT), GetSysColor(COLOR_WINDOWTEXT))
                }
            }
        } else {
            (if on || self.hot(&a) { p.active } else { p.raised },
             if on { p.accent } else { p.text }, if on { p.accent } else { p.line })
        };
        s.round(
            x,
            y,
            w,
            h,
            5,
            background,
            Some(outline),
        );
        s.text(
            x + 3,
            y,
            w - 6,
            h,
            name,
            foreground,
            13,
            500,
            DT_CENTER,
        );
        self.hit(x, y, w, h, a);
    }
    fn icon(&self, x: i32, y: i32, w: i32, h: i32, g: u16, a: Hit, on: bool, fs: i32) {
        let p = self.p;
        let hot = self.hot(&a);
        if on || hot {
            self.s
                .round(x, y, w, h, 6, if on { p.active } else { p.raised }, None);
        }
        self.s
            .glyph(x, y, w, h, g, if on { p.accent } else { p.dim }, fs);
        self.hit(x, y, w, h, a);
    }
    fn header(&self, zoomed: bool) {
        let (s, p, w) = (self.s, self.p, self.w);
        s.fill(0, 0, w, HEAD, p.panel);
        s.fill(0, HEAD - 1, w, 1, p.line);
        s.logo(13, 10, 25);
        s.text(45, 0, 82, HEAD, "Linework", p.text, 17, 650, 0);
        let mut x = 132;
        for (name, sw) in [
            ("File", 35),
            ("Edit", 38),
            ("Selection", 65),
            ("Search", 54),
            ("Go", 32),
            ("View", 41),
            ("Macro", 49),
            ("Tools", 43),
            ("Help", 39),
        ] {
            let a = Hit::Menu(name.into());
            if self.hot(&a) {
                s.round(x, 7, sw, 29, 5, p.raised, None);
            }
            s.text(x, 1, sw, HEAD - 2, name, p.dim, 13, 500, DT_CENTER);
            self.hit(x, 4, sw, 35, a);
            x += sw;
        }
        let free = w - 139 - x;
        if free >= 54 {
            let bw = if free >= 185 { 164 } else { 34 };
            let bx = x + (free - bw) / 2;
            let a = cmd("palette");
            s.round(
                bx,
                8,
                bw,
                28,
                6,
                if self.hot(&a) { p.active } else { p.raised },
                Some(p.line),
            );
            s.glyph(bx + 3, 8, 27, 28, 0xE721, p.dim, 12);
            if bw > 50 {
                s.text(
                    bx + 32,
                    8,
                    bw - 38,
                    28,
                    "Command palette",
                    p.dim,
                    13,
                    450,
                    0,
                );
            }
            self.hit(bx, 7, bw, 30, a);
        }
        for (i, (g, id)) in [
            (0xE921, "winmin"),
            (if zoomed { 0xE923 } else { 0xE922 }, "winmax"),
            (0xE8BB, "winclose"),
        ]
        .into_iter()
        .enumerate()
        {
            let x = w - 132 + i as i32 * 44;
            let a = cmd(id);
            if self.hot(&a) {
                s.fill(
                    x,
                    0,
                    44,
                    HEAD - 1,
                    if i == 2 { hx(0xAF3546) } else { p.raised },
                );
            }
            s.glyph(x, 0, 44, HEAD - 1, g, p.text, 10);
            self.hit(x, 0, 44, HEAD - 1, a);
        }
    }
    fn side(&self, side: i32, edge: bool) {
        let (s, p, f, h) = (self.s, self.p, self.f, self.h);
        s.fill(
            0,
            HEAD,
            RAIL,
            h - HEAD - FOOT,
            if f.light { hx(0xEEEAF4) } else { hx(0x121318) },
        );
        s.fill(RAIL - 1, HEAD, 1, h - HEAD - FOOT, p.line);
        for (i, (g, a, on)) in [
            (0xE81E, Hit::Side(0), f.sidebar && f.side == 0),
            (0xE721, cmd("find"), f.find),
            (0xE8D4, Hit::Side(1), f.sidebar && f.side == 1),
            (0xE179, Hit::Side(2), f.sidebar && f.side == 2),
            (0xE734, cmd("marks"), false),
        ]
        .into_iter()
        .enumerate()
        {
            self.icon(5, HEAD + 9 + i as i32 * 43, 37, 37, g, a, on, 17);
        }
        self.icon(5, h - FOOT - 45, 37, 37, 0xE765, cmd("help"), false, 18);
        if !f.sidebar {
            return;
        }
        let x = RAIL + 15;
        let avail = side - x - 12;
        s.fill(RAIL, HEAD, side - RAIL, h - HEAD - FOOT, p.panel);
        if edge {
            s.fill(side - 1, HEAD, 1, h - HEAD - FOOT, p.line);
        }
        s.text(
            x,
            HEAD + 4,
            avail - 32,
            42,
            ["Explorer", "Symbols", "Search"]
                .get(f.side as usize)
                .unwrap_or(&"Explorer"),
            p.text,
            16,
            620,
            0,
        );
        self.icon(
            side - 39,
            HEAD + 9,
            30,
            29,
            if f.side == 0 { 0xE8DA } else { 0xE721 },
            if f.side == 0 {
                cmd("folder")
            } else if f.side == 1 {
                cmd("symbols")
            } else {
                cmd("findproject")
            },
            false,
            15,
        );
        if f.side == 0 {
            s.small(x, HEAD + 48, avail, 27, "OPEN EDITORS", p.dim);
            let folder = !f.workspace.is_empty();
            let count = f.tabs.len().min(if folder { 4 } else { 6 });
            if f.tabs.is_empty() {
                s.text(x, OPEN_Y, avail, ROW, "No open files", p.dim, 13, 400, 0);
            }
            if folder {
                let offset = f
                    .active
                    .unwrap_or(0)
                    .saturating_sub(3)
                    .min(f.tabs.len().saturating_sub(4));
                if f.tabs.len() > 4 {
                    self.icon(
                        side - 41,
                        HEAD + 45,
                        29,
                        29,
                        0xE712,
                        cmd("opentabs"),
                        false,
                        13,
                    );
                }
                for (n, (name, dirty)) in f.tabs.iter().enumerate().skip(offset).take(4) {
                    let y = OPEN_Y + (n - offset) as i32 * ROW;
                    let a = Hit::Tab(n);
                    let current = f.active == Some(n);
                    if current || self.hot(&a) {
                        s.round(
                            x - 5,
                            y + 1,
                            avail + 8,
                            ROW - 2,
                            5,
                            if current { p.active } else { p.raised },
                            None,
                        );
                    }
                    let (g, col) = file_icon(name, f.light);
                    s.glyph(x - 1, y, 20, ROW, g, col, 13);
                    s.text(x + 24, y, avail - if *dirty { 80 } else { 32 }, ROW, name, p.text, 13, 400, 0);
                    if *dirty {
                        s.text(side - 71, y, 55, ROW, "Unsaved", p.accent, 12, 600, DT_RIGHT);
                    }
                    self.hit(x - 5, y, avail + 8, ROW, a);
                }
            }
            let band = OPEN_Y + ROW * count.max(1) as i32 + 12;
            s.fill(x - 2, band, avail + 4, 1, p.line);
            let title = if folder {
                f.report.to_uppercase()
            } else {
                "FOLDER".into()
            };
            s.small(x, band + 9, avail, 27, &title, p.dim);
            if !folder {
                self.button(x, band + 41, 115, 30, "Open folder…", "folder", false);
            }
        } else if f.side == 2 && f.search_panel {
            self.panel(x, avail);
        } else if !f.report.is_empty() {
            s.small(x, HEAD + 48, avail, 27, &f.report, p.dim);
        }
        if f.preview && f.side == 2 {
            let half = (avail - 4) / 2;
            self.button(x, h - FOOT - 34, half, 27, "Toggle all", "toggleall", false);
            self.button(
                x + half + 5,
                h - FOOT - 34,
                half,
                27,
                "Apply checked",
                "applypreview",
                true,
            );
        }
    }
    /// The search panel's switches around its fields, and its summary line.
    fn panel(&self, x: i32, avail: i32) {
        let (s, p, f) = (self.s, self.p, self.f);
        let ps = &f.panel;
        self.button(x + avail - 70, panel_field_y(1) + 1, 70, FIELD_H - 2, "Preview", "p:preview", f.preview);
        let y = panel_switches_y();
        for (i, (name, id, on)) in [("Aa", "p:case", ps.case_), ("W", "p:word", ps.whole), (".*", "p:regex", ps.regex), ("", "p:scope", ps.scope)]
            .into_iter()
            .enumerate()
        {
            self.button(x + i as i32 * 34, y, 30, 28, name, id, on);
        }
        // The definitions switch, drawn with the outline's symbol.
        s.glyph(x + 3 * 34, y, 30, 28, 0xE943, if ps.scope { p.accent } else { p.text }, 12);
        self.button(x + avail - 64, y, 64, 28, if ps.more { "Less" } else { "More" }, "p:more", ps.more);
        if ps.more {
            for (i, label) in ["Files", "Skip", "Types", "Near", "Also", "Lacks"].into_iter().enumerate() {
                s.text(x, panel_field_y(i + 2), PANEL_LABEL - 4, FIELD_H, label, p.dim, 12, 500, 0);
            }
            self.stepper(x + avail - 70, panel_field_y(5) + 1, ps.near_lines, "p:near-", "p:near+");
            let ey = panel_extra_y();
            s.text(x, ey, 54, 28, "Context", p.dim, 12, 500, 0);
            self.stepper(x + 52, ey, ps.context, "p:ctx-", "p:ctx+");
            self.button(x + 128, ey, 54, 28, "Hidden", "p:hidden", ps.hidden);
            self.button(x + 186, ey, 59, 28, "Ignored", "p:ignored", ps.ignored);
        }
        let sy = panel_bottom(ps.more);
        s.fill(x - 2, sy - 4, avail + 4, 1, p.line);
        s.small(x, sy, avail - 30, 26, &f.summary, p.dim);
        if !f.summary.is_empty() {
            self.icon(x + avail - 26, sy, 26, 26, 0xE8C8, cmd("p:copy"), false, 12);
        }
    }
    /// A number with a button on each side to lower and raise it.
    fn stepper(&self, x: i32, y: i32, value: u32, lower: &str, raise: &str) {
        self.button(x, y, 22, 28, "−", lower, false);
        self.s.text(x + 22, y, 26, 28, &value.to_string(), self.p.text, 13, 600, DT_CENTER);
        self.button(x + 48, y, 22, 28, "+", raise, false);
    }
    fn tabs(&self, side: i32) {
        let (s, p, f, w) = (self.s, self.p, self.f, self.w);
        s.fill(side, HEAD, w - side, TABS, p.panel);
        s.fill(side, HEAD + TABS - 1, w - side, 1, p.line);
        let mut x = side;
        if f.tabs.is_empty() {
            s.fill(x, HEAD, 114, TABS, p.bg);
            s.fill(x, HEAD, 114, 2, p.accent);
            s.glyph(x + 10, HEAD, 24, TABS, 0xE80F, p.accent, 13);
            s.text(x + 40, HEAD, 68, TABS, "Start", p.text, 14, 500, 0);
        }
        if f.first_tab > 0 {
            self.icon(
                x + 1,
                HEAD + 5,
                23,
                TABS - 10,
                0xE76B,
                Hit::ScrollTabs(-1),
                false,
                11,
            );
            x += 25;
        }
        for (i, (name, dirty)) in f.tabs.iter().enumerate().skip(f.first_tab) {
            let width = tab_width(name);
            if x + width > w - tab_reserve(f.can_preview) && i > f.first_tab {
                self.icon(
                    x,
                    HEAD + 5,
                    22,
                    TABS - 10,
                    0xE76C,
                    Hit::ScrollTabs(1),
                    false,
                    11,
                );
                break;
            }
            let active = f.active == Some(i);
            let a = Hit::Tab(i);
            s.fill(
                x,
                HEAD,
                width,
                TABS - 1,
                if active {
                    p.bg
                } else if self.hot(&a) {
                    p.raised
                } else {
                    p.panel
                },
            );
            if active || f.alternate == Some(i) {
                s.fill(x, HEAD, width, 2, if active { p.accent } else { p.dim });
            }
            s.fill(x + width - 1, HEAD, 1, TABS, p.line);
            let (g, col) = file_icon(name, f.light);
            s.glyph(x + 8, HEAD, 23, TABS, g, col, 13);
            s.text(
                x + 35,
                HEAD + if *dirty { 1 } else { 0 },
                width - 61,
                if *dirty { 23 } else { TABS },
                name,
                if active { p.text } else { p.dim },
                13,
                500,
                0,
            );
            if *dirty {
                s.text(x + 35, HEAD + 22, width - 61, 17, "Unsaved", p.accent, 11, 600, 0);
            }
            self.hit(x, HEAD, width - 25, TABS, a);
            self.icon(
                x + width - 26,
                HEAD + 9,
                21,
                23,
                0xE711,
                Hit::Close(i),
                false,
                10,
            );
            x += width;
        }
        if f.can_preview {
            self.button(w - 161, HEAD + 6, 78, 28, "Preview", "docpreview", f.document_preview);
        }
        self.icon(w - 73, HEAD + 5, 31, 30, 0xE710, cmd("new"), false, 13);
        self.icon(w - 38, HEAD + 5, 31, 30, 0xE7FB, cmd("split"), f.split, 14);
    }
    fn search(&self, side: i32) {
        let (s, p, f, w) = (self.s, self.p, self.f, self.w);
        let y = HEAD + TABS;
        let row = findbar_height(f.replace);
        s.fill(side, y, w - side, row, p.panel);
        s.fill(side, y + row - 1, w - side, 1, p.line);
        let ry = y + 42;
        let bx = w - 327;
        for (i, (name, id, on)) in [
            ("Aa", "case", f.case_),
            (".*", "regex", f.regex),
            ("W", "word", f.whole),
            ("↑", "findprev", false),
            ("↓", "findnext", false),
            (
                if f.search_folder {
                    "Folder"
                } else if f.selection_scope {
                    "Selection"
                } else {
                    "File"
                },
                "scope",
                f.search_folder,
            ),
            ("×", "hidefind", false),
        ]
        .into_iter()
        .enumerate()
        {
            self.button(
                bx + [0, 38, 76, 114, 152, 190, 276][i],
                y + 6,
                if i == 5 { 82 } else { 34 },
                27,
                name,
                id,
                on,
            );
        }
        if f.replace {
            self.button(bx, ry, 101, 27, "Replace next", "replaceone", false);
            self.button(
                bx + 107,
                ry,
                134,
                27,
                if f.search_folder {
                    "Preview changes"
                } else {
                    "Replace all"
                },
                "replaceall",
                true,
            );
        }
    }
    fn home(&self, side: i32) {
        let (s, p, f) = (self.s, self.p, self.f);
        let avail = self.w - side;
        let margin = if avail < 570 { 24 } else { 48 };
        let width = (avail - 2 * margin).min(746);
        let x = side + (avail - width) / 2;
        let y = HEAD
            + TABS
            + (if f.find { findbar_height(f.replace) } else { 0 })
            + ((self.h - 400) / 6).clamp(25, 57);
        s.logo(x, y + 7, 52);
        s.text(
            x + 67,
            y - 2,
            width - 67,
            62,
            "Linework",
            p.text,
            46,
            500,
            0,
        );
        s.small(x + 1, y + 88, width, 22, "START EDITING", p.dim);
        let card = (width - 20) / 3;
        for (i, (name, key, id, g, col)) in [
            ("New file", "Ctrl + N", "new", 0xE7C3, 0xCAB0F3),
            ("Open file", "Ctrl + O", "open", 0xE8E5, 0xA3D8C0),
            (
                "Open folder",
                "Ctrl + Shift + O",
                "folder",
                0xE8B7,
                0xEFBE90,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let x = x + i as i32 * (card + 10);
            let a = cmd(id);
            let hot = self.hot(&a);
            s.round(
                x,
                y + 119,
                card,
                94,
                8,
                if hot { p.active } else { p.raised },
                Some(if hot { p.accent } else { p.line }),
            );
            s.glyph(
                x + 14,
                y + 130,
                24,
                24,
                g,
                if f.light { p.accent } else { hx(col) },
                22,
            );
            s.text(x + 15, y + 154, card - 24, 25, name, p.text, 16, 600, 0);
            s.small(x + 15, y + 180, card - 24, 21, key, p.dim);
            self.hit(x, y + 119, card, 94, a);
        }
        let recent = !f.recent.is_empty();
        s.text(
            x,
            y + 239,
            width,
            33,
            if recent {
                "Recent files"
            } else {
                "Keyboard shortcuts"
            },
            p.text,
            17,
            600,
            0,
        );
        let keys = [
            ("Command palette", "Ctrl + Shift + P", "palette"),
            ("Quick open", "Ctrl + P", "quick"),
            ("Find in folder", "Ctrl + Shift + F", "findproject"),
            ("Keyboard shortcuts", "F1", "help"),
        ];
        for i in 0..if recent {
            f.recent.len().min(4)
        } else {
            keys.len()
        } {
            let y = y + 280 + i as i32 * 39;
            if y + 32 > self.h - FOOT {
                break;
            }
            let (name, key, action) = if recent {
                let path = std::path::Path::new(&f.recent[i]);
                (
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    path.parent().unwrap_or(path).display().to_string(),
                    Hit::Recent(i),
                )
            } else {
                let (n, k, a) = keys[i];
                (n.to_string(), k.to_string(), cmd(a))
            };
            if self.hot(&action) {
                s.round(x - 5, y, width + 10, 37, 5, p.raised, None);
            }
            s.fill(x, y + 38, width, 1, p.line);
            s.text(x + 2, y, width / 2, 37, &name, p.text, 14, 400, 0);
            s.draw_text(
                x + width / 2,
                y,
                width / 2 - 3,
                37,
                &key,
                p.dim,
                12,
                400,
                !recent,
                DT_RIGHT,
            );
            self.hit(x, y, width, 38, action);
        }
    }
}
#[path = "frame_paint.rs"]
pub(crate) mod rendering;
pub use rendering::paint;
pub fn hit(x: i32, y: i32) -> Option<Hit> {
    HITS.with_borrow(|v| {
        v.iter()
            .rev()
            .find(|(r, _)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
            .map(|(_, a)| a.clone())
    })
}
// A leave from the previous child can arrive after the new child was entered.
pub(crate) fn hover_at_pointer(host: H) {
    unsafe {
        let mut point = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut point) != 0 {
            let window = WindowFromPoint(point);
            if window == hwnd(host) || (GetParent(window) == hwnd(host) && controls::is_face(window)) {
                ScreenToClient(hwnd(host), &mut point);
                hover(host, hit(point.x, point.y));
                return;
            }
        }
        hover(host, None);
    }
}
pub fn hover(h: H, a: Option<Hit>) {
    HOT.with_borrow_mut(|prev| {
        if *prev != a {
            HITS.with_borrow(|v| {
                for (r, id) in v {
                    if Some(id) == prev.as_ref() || Some(id) == a.as_ref() {
                        rendering::request_hover(h, r);
                    }
                }
            });
            *prev = a;
        }
    });
}
fn list_text(h: H, i: usize) -> String {
    let n = msg(h, LB_GETTEXTLEN, i, 0);
    if !(0..=131072).contains(&n) {
        return String::new();
    }
    let mut v = vec![0u16; n as usize + 1];
    msg(h, LB_GETTEXT, i, v.as_mut_ptr() as _);
    String::from_utf16_lossy(&v[..n as usize])
}
pub fn draw_list(h: H, d: &DRAWITEMSTRUCT, chooser: bool) {
    let p = colors(LIGHT.get());
    let rc = d.rcItem;
    fill(d.hDC, rc, p.panel);
    if d.itemID == u32::MAX {
        return;
    }
    let txt = list_text(d.hwndItem as H, d.itemID as _);
    let scale = dpi(h);
    let s = Surface::new(d.hDC, scale);
    let (x, y, w, ht) = (
        rc.left * 96 / scale,
        rc.top * 96 / scale,
        (rc.right - rc.left) * 96 / scale,
        (rc.bottom - rc.top) * 96 / scale,
    );
    let selected = d.itemState & 1 != 0;
    if chooser {
        if selected {
            s.round(x + 2, y + 1, w - 4, ht - 2, 5, p.active, None);
        }
        let parts: Vec<_> = txt.split('\t').collect();
        if parts.len() == 3 {
            s.text(x + 13, y, 80, ht, parts[0], p.dim, 12, 500, 0);
            s.text(
                x + 99,
                y,
                w - 242,
                ht,
                parts[1],
                p.text,
                14,
                if selected { 600 } else { 450 },
                0,
            );
            s.draw_text(
                x + w - 152,
                y,
                139,
                ht,
                parts[2],
                p.dim,
                11,
                400,
                true,
                DT_RIGHT,
            );
        } else {
            s.text(x + 12, y, w - 24, ht, &txt, p.text, 14, 450, 0);
        }
        return;
    }
    FRAME.with_borrow(|f| {
        let drawn = f.side == 2
            && RICH.with_borrow(|rows| rows.get(d.itemID as usize).map(|row| rich_row(&s, f, row, selected, [x, y, w, ht])).is_some());
        if !drawn {
            sidebar_row(&s, f, &txt, d.itemID as usize, selected, [x, y, w, ht]);
        }
    });
}

/// The largest character boundary of `text` at or before `at`.
fn boundary(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// A row of folder search results.
fn rich_row(s: &Surface, f: &Frame, row: &Rich, selected: bool, bounds: [i32; 4]) {
    let [x, y, w, ht] = bounds;
    let p = colors(f.light);
    if selected {
        s.round(x + 1, y + 1, w - 2, ht - 2, 5, p.raised, None);
    }
    match row {
        Rich::File { name, folder, count } => {
            let (g, col) = file_icon(name, f.light);
            s.glyph(x + 3, y, 23, ht, g, col, 13);
            let count = count.to_string();
            let count_w = s.measure(&count, 11, 600, true) + 8;
            let name_w = s.measure(name, 13, 600, false).min(w - 31 - count_w - 10);
            s.text(x + 31, y, name_w + 2, ht, name, p.text, 13, 600, 0);
            s.text(x + 39 + name_w, y, w - 47 - name_w - count_w, ht, folder, p.dim, 11, 400, 0);
            s.draw_text(x + w - count_w - 4, y, count_w, ht, &count, p.dim, 11, 600, true, DT_RIGHT);
        }
        Rich::Scope(chain) => {
            s.glyph(x + 10, y, 18, ht, 0xE943, p.accent, 10);
            s.text(x + 30, y, w - 34, ht, chain, p.dim, 11, 500, 0);
        }
        Rich::Line { number, text, ranges, context, replaced, checked } => {
            let gutter = 44;
            s.draw_text(x + 2, y, gutter - 10, ht, &number.to_string(), p.dim, 11, 400, true, DT_RIGHT);
            let mut tx = x + gutter;
            if let Some(on) = checked {
                s.text(tx, y, 18, ht, if *on { "☑" } else { "☐" }, if *on { p.accent } else { p.dim }, 13, 400, 0);
                tx += 20;
            }
            let limit = x + w - 4;
            // Tabs and stray carriage returns show as spaces: one byte each, so the match ranges still hold.
            let text: String = text.chars().map(|c| if c == '\t' || c == '\r' { ' ' } else { c }).collect();
            let indent = text.len() - text.trim_start().len();
            let first = ranges.first().map_or(indent, |r| r.0.max(indent));
            // Keep the first match in view: a long start is cut, with a mark that it was.
            let mut from = indent;
            if s.measure(&text[indent..first], 12, 400, true) > (limit - tx) * 2 / 5 {
                from = boundary(&text, first.saturating_sub(12));
                let mark = s.measure("…", 12, 400, true);
                s.draw_text(tx, y, mark + 2, ht, "…", p.dim, 12, 400, true, 0);
                tx += mark;
            }
            let plain_ink = if *context { p.dim } else { p.text };
            // Draws one piece; false once the row is full.
            // Draws one piece: plain, marked as a match, or struck out as replaced. False once the row is full.
            let piece = |tx: &mut i32, part: &str, ink: u32, weight: i32, marked: bool, struck: bool| -> bool {
                if part.is_empty() {
                    return true;
                }
                let part_w = s.measure(part, 12, weight, true);
                let room = limit - *tx;
                if room <= 8 {
                    return false;
                }
                if marked {
                    s.round(*tx - 1, y + 5, part_w.min(room) + 2, ht - 10, 3, p.active, None);
                }
                s.draw_text(*tx, y, part_w.min(room) + 1, ht, part, ink, 12, weight, true, 0);
                if struck {
                    s.fill(*tx, y + ht / 2, part_w.min(room), 1, ink);
                }
                *tx += part_w + if struck { 3 } else { 0 };
                part_w <= room
            };
            let mut at = from;
            for (n, &(start, end)) in ranges.iter().enumerate() {
                let (start, end) = (boundary(&text, start.max(from)), boundary(&text, end.max(from)));
                if start < at {
                    continue;
                }
                if !piece(&mut tx, &text[at..start], plain_ink, 400, false, false) {
                    return;
                }
                match replaced.as_ref().and_then(|r| r.get(n)) {
                    Some(new) => {
                        if !piece(&mut tx, &text[start..end], p.dim, 400, false, true) || !piece(&mut tx, new, p.accent, 600, true, false) {
                            return;
                        }
                    }
                    None => {
                        if !piece(&mut tx, &text[start..end], p.accent, 600, true, false) {
                            return;
                        }
                    }
                }
                at = end;
            }
            piece(&mut tx, &text[at..], plain_ink, 400, false, false);
        }
        Rich::Break => s.text(x, y, w, ht, "⋯", p.dim, 13, 400, DT_CENTER),
        Rich::Note(note) => s.text(x + 8, y, w - 12, ht, note, p.dim, 12, 400, 0),
    }
}
fn sidebar_row(s: &Surface, f: &Frame, txt: &str, index: usize, selected: bool, bounds: [i32;4]) {
        let [x,y,w,ht] = bounds;
        let p = colors(f.light);
        let name = txt.trim().trim_start_matches(['›', '‹', '●']).trim_start();
        let active = f.side == 0
            && f.tabs
                .get(f.active.unwrap_or(usize::MAX))
                .is_some_and(|(n, _)| n == name);
        if selected || active {
            s.round(
                x + 1,
                y + 1,
                w - 2,
                ht - 2,
                5,
                if active { p.active } else { p.raised },
                None,
            );
        }
        let (g, col) = if txt.starts_with('›') || txt.starts_with('‹') {
            (0xE8B7, if f.light { p.accent } else { hx(0xD9B892) })
        } else if f.side == 0 {
            file_icon(name, f.light)
        } else if f.side == 1 {
            (0xE943, p.accent)
        } else {
            (0xE721, p.dim)
        };
        s.glyph(x + 3, y, 23, ht, g, col, 13);
        let dirty = f.side == 0 && f.workspace.is_empty()
            && f.tabs.get(index).is_some_and(|(_, dirty)| *dirty);
        s.text(
            x + 31,
            y,
            w - if dirty { 91 } else { 39 },
            ht,
            name,
            if active { p.accent } else { p.text },
            13,
            if active { 550 } else { 400 },
            0,
        );
        if dirty {
            s.text(x + w - 62, y, 54, ht, "Unsaved", p.accent, 12, 600, DT_RIGHT);
        }
}

#[path = "sidebar_art.rs"]
pub(crate) mod sidebar_art;

pub fn draw_pane_header(h: H, d: &DRAWITEMSTRUCT) {
    buffered(d.hDC, d.rcItem, |dc| paint_pane_header(h, d, dc));
}
fn paint_pane_header(h: H, d: &DRAWITEMSTRUCT, dc: HDC) {
    let palette = colors(LIGHT.get());
    let caption = text(d.hwndItem as H);
    let active = caption.starts_with("Active ");
    let (bg, fg, edge) = if high_contrast() {
        unsafe {
            if active {
                (GetSysColor(COLOR_HIGHLIGHT), GetSysColor(COLOR_HIGHLIGHTTEXT), GetSysColor(COLOR_HIGHLIGHTTEXT))
            } else {
                (GetSysColor(COLOR_WINDOW), GetSysColor(COLOR_WINDOWTEXT), GetSysColor(COLOR_WINDOWTEXT))
            }
        }
    } else if active {
        (palette.active, palette.text, palette.accent)
    } else {
        (palette.panel, palette.dim, palette.line)
    };
    let rect = d.rcItem;
    fill(dc, rect, bg);
    let mut stripe = rect;
    stripe.bottom = stripe.top + size(h, if active { 3 } else { 1 }).max(1);
    fill(dc, stripe, edge);
    let scale = dpi(h);
    let width = (rect.right - rect.left) * 96 / scale;
    let height = (rect.bottom - rect.top) * 96 / scale;
    let surface = Surface::new(dc, scale);
    // The accessible name includes "pane". Its location already provides that context visually.
    let visible = caption.replacen(" pane · ", " · ", 1);
    surface.text(10, 2, width - 17, height - 2, &visible, fg, 12, if active { 600 } else { 400 }, 0);
}
