//! Native, read-only change review. The same controls serve replacement previews
//! and file comparisons; starting a review never changes an editor buffer.
use crate::{
    app::{App, Worker},
    compare::{self, Comparison},
    dialogs, forms,
    sci::*,
    win::*,
};
use linework_project::SearchReport;
use linework_storage::Document;
use std::{io, ops::Range, path::PathBuf, ptr::null_mut, thread};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    UI::{Input::KeyboardAndMouse::*, Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass}, WindowsAndMessaging::*},
};

const LIST: i32 = 810;
const SS_NOPREFIX: u32 = 0x80;
const PREV: i32 = 811;
const NEXT: i32 = 812;
const TOGGLE: i32 = 813;
const ALL: i32 = 814;
const EARLIER: i32 = 815;
const LATER: i32 = 816;
const PAGE: u64 = 32 * 1024;
const LIST_PAGE: u64 = 128;

enum Review<'a> {
    Replace {
        report: &'a SearchReport,
        checked: &'a mut [bool],
        root: PathBuf,
    },
    Compare(&'a mut Comparison),
}
struct Detail {
    title: String,
    labels: [String; 2],
    values: [String; 2],
    highlights: [Vec<Range<usize>>; 2],
    gutters: [Vec<String>; 2],
    pages: u64,
}
fn count(n: u64) -> u64 {
    n.div_ceil(PAGE).max(1)
}
fn show_chars(s: &str) -> String {
    s.replace('\0', "␀")
}
fn normalized(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}
// Context must actually be equal on both sides. Never borrow a changed line from
// a neighboring hunk or read an unbounded line simply to decorate the review.
fn shared_context(d: &Comparison, h: &compare::Hunk, before: bool) -> io::Result<Vec<String>> {
    let mut rows = Vec::new();
    for by in 0..2 {
        let mut pair = Vec::with_capacity(2);
        for i in 0..2 {
            let line = if before {
                h.sides[i].start.checked_sub(by + 1)
            } else {
                Some(h.sides[i].end + by)
            };
            let Some(line) = line else {
                return Ok(if before {
                    rows.into_iter().rev().collect()
                } else {
                    rows
                });
            };
            let r = compare::byte_range(&d.docs[i], &(line..line + 1));
            if r.is_empty() || r.end - r.start > 4096 {
                break;
            }
            pair.push(d.docs[i].read(r.start, (r.end - r.start) as usize)?);
        }
        if pair.len() != 2 || pair[0] != pair[1] {
            break;
        }
        rows.push(normalized(&show_chars(&String::from_utf8_lossy(&pair[0]))));
    }
    if before {
        rows.reverse();
    }
    Ok(rows)
}
fn page_str(s: &str, page: u64) -> (String, usize) {
    let mut a = (page * PAGE).min(s.len() as u64) as usize;
    let mut b = ((page + 1) * PAGE).min(s.len() as u64) as usize;
    while !s.is_char_boundary(a) {
        a += 1;
    }
    while !s.is_char_boundary(b) {
        b += 1;
    }
    (show_chars(&s[a..b]), a)
}
fn deltas(a: &str, b: &str) -> [Range<usize>; 2] {
    let front = a
        .chars()
        .zip(b.chars())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x.len_utf8())
        .sum::<usize>();
    let tail = a[front..]
        .chars()
        .rev()
        .zip(b[front..].chars().rev())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x.len_utf8())
        .sum::<usize>();
    [front..a.len() - tail, front..b.len() - tail]
}
fn lines_label(r: &Range<u64>) -> String {
    match r.end - r.start {
        0 => format!("insertion point {}", r.start + 1),
        1 => format!("line {}", r.start + 1),
        _ => format!("lines {}–{}", r.start + 1, r.end),
    }
}
impl Review<'_> {
    fn len(&self) -> u64 {
        match self {
            Self::Replace { checked, .. } => checked.len() as u64,
            Self::Compare(d) => d.count,
        }
    }
    fn summary(&self) -> String {
        match self {
            Self::Replace {
                checked, report, ..
            } => {
                let n = checked.iter().filter(|v| **v).count();
                let mut s = format!("{n} included   ·   {} excluded", checked.len() - n);
                if report.truncated {
                    s.push_str("   ·   result limit reached");
                }
                if report.cancelled {
                    s.push_str("   ·   search canceled");
                }
                if !report.errors.is_empty() {
                    s.push_str(&format!("   ·   {} scan issues", report.errors.len()));
                }
                s
            }
            Self::Compare(d) => format!(
                "{} change{}   ·   Read-only comparison",
                d.count,
                if d.count == 1 { "" } else { "s" }
            ),
        }
    }
    fn label(&mut self, n: u64) -> io::Result<String> {
        Ok(match self {
            Self::Replace {
                report,
                checked,
                root,
            } => {
                let h = &report.hits[n as usize];
                let path = h.path.strip_prefix(&*root).unwrap_or(&h.path);
                format!(
                    "{}    {}   ·   line {}, column {}",
                    if checked[n as usize] {
                        "Included"
                    } else {
                        "Excluded"
                    },
                    path.display(),
                    h.line,
                    h.column
                )
            }
            Self::Compare(d) => {
                let h = d.hunk(n)?;
                format!(
                    "{}    Original {}    →    Revised {}",
                    h.kind(),
                    lines_label(&h.sides[0]),
                    lines_label(&h.sides[1])
                )
            }
        })
    }
    fn detail(&mut self, n: u64, page: u64) -> io::Result<Detail> {
        match self {
            Self::Replace {
                report,
                checked,
                root,
            } => {
                let h = &report.hits[n as usize];
                let (pre, post) = h.preview_context;
                let p = &h.preview[..pre];
                let s = &h.preview[h.preview.len() - post..];
                let whole = [
                    format!("{p}{}{s}", h.matched),
                    format!("{p}{}{s}", h.replacement.as_deref().unwrap_or("")),
                ];
                let pages = count(whole[0].len().max(whole[1].len()) as u64);
                let mut values = [String::new(), String::new()];
                let mut highlights = [vec![], vec![]];
                for i in 0..2 {
                    let (text, offset) = page_str(&whole[i], page);
                    let mark = pre..whole[i].len() - post;
                    let a = mark.start.max(offset);
                    let b = mark.end.min(offset + text.len()).min(whole[i].len());
                    // NUL display expands to a three-byte printable glyph.
                    let raw_end = (offset + PAGE as usize).min(whole[i].len());
                    let mut raw_end = raw_end;
                    while !whole[i].is_char_boundary(raw_end) {
                        raw_end += 1;
                    }
                    let b = b.min(raw_end);
                    if a < b && whole[i].is_char_boundary(a) && whole[i].is_char_boundary(b) {
                        highlights[i].push(
                            show_chars(&whole[i][offset..a]).len()
                                ..show_chars(&whole[i][offset..b]).len(),
                        );
                    }
                    values[i] = text;
                }
                let path = h.path.strip_prefix(&*root).unwrap_or(&h.path);
                Ok(Detail {
                    title: format!(
                        "{}   ·   line {}, column {}   ·   {}",
                        path.display(),
                        h.line,
                        h.column,
                        if checked[n as usize] {
                            "Included"
                        } else {
                            "Excluded"
                        }
                    ),
                    labels: ["−  Before".into(), "+  After replacement".into()],
                    values,
                    highlights,
                    gutters: Default::default(),
                    pages,
                })
            }
            Self::Compare(d) => {
                let h = d.hunk(n)?;
                let rr = [
                    compare::byte_range(&d.docs[0], &h.sides[0]),
                    compare::byte_range(&d.docs[1], &h.sides[1]),
                ];
                let pages = count(rr.iter().map(|r| r.end - r.start).max().unwrap());
                let mut values = [String::new(), String::new()];
                let mut labels = [String::new(), String::new()];
                for i in 0..2 {
                    let doc = &d.docs[i];
                    let r = &rr[i];
                    let mut start = (r.start + page * PAGE).min(r.end);
                    let mut end = (r.start + (page + 1) * PAGE).min(r.end);
                    let adjust = |p: &mut u64| -> io::Result<()> {
                        if *p < r.end && *p > r.start {
                            for byte in doc.read(*p, 3)? {
                                if byte & 0xc0 != 0x80 {
                                    break;
                                }
                                *p += 1;
                            }
                        }
                        Ok(())
                    };
                    adjust(&mut start)?;
                    adjust(&mut end)?;
                    let raw = doc.read(start, (end - start) as usize)?;
                    values[i] = show_chars(&String::from_utf8_lossy(&raw));
                    labels[i] = format!(
                        "{}  {}   ·   {}",
                        if i == 0 {
                            "−  Original"
                        } else {
                            "+  Revised"
                        },
                        d.names[i],
                        lines_label(&h.sides[i])
                    );
                }
                let mut highlights = [vec![], vec![]];
                // Expose pure end-of-line changes, including removal of the last newline.
                if values[0] != values[1]
                    && normalized(&values[0]).trim_end_matches('\n')
                        == normalized(&values[1]).trim_end_matches('\n')
                {
                    for v in &mut values {
                        *v = v.replace("\r\n", "␍␊\n").replace('\r', "␍\n");
                        if !v.contains('␊') && !v.contains('␍') {
                            *v = v.replace('\n', "␊\n");
                        }
                    }
                } else {
                    for v in &mut values {
                        *v = normalized(v);
                    }
                }
                let mut aa = values[0].split_inclusive(['\n']);
                let mut bb = values[1].split_inclusive(['\n']);
                let mut offsets = [0, 0];
                loop {
                    let a = aa.next();
                    let b = bb.next();
                    if a.is_none() && b.is_none() {
                        break;
                    }
                    let a = a.unwrap_or("");
                    let b = b.unwrap_or("");
                    let changed = deltas(a, b);
                    for i in 0..2 {
                        let r = &changed[i];
                        if !r.is_empty() {
                            highlights[i].push(offsets[i] + r.start..offsets[i] + r.end);
                        }
                    }
                    offsets[0] += a.len();
                    offsets[1] += b.len();
                }
                let mut gutters: [Vec<String>; 2] = Default::default();
                if pages == 1 && h.sides.iter().all(|r| r.end - r.start <= 512) {
                    let before = shared_context(d, &h, true)?;
                    let after = shared_context(d, &h, false)?;
                    let longest = h.sides.iter().map(|r| r.end - r.start).max().unwrap();
                    for i in 0..2 {
                        let r = &h.sides[i];
                        let prefix = before.concat();
                        let base = prefix.len();
                        for v in &mut highlights[i] {
                            v.start += base;
                            v.end += base;
                        }
                        let mut body = prefix;
                        body.push_str(&values[i]);
                        let pad = longest - (r.end - r.start);
                        if !body.is_empty()
                            && !body.ends_with('\n')
                            && (pad > 0 || !after.is_empty())
                        {
                            body.push('\n');
                        }
                        for _ in 0..pad {
                            body.push('\n');
                        }
                        body.push_str(&after.concat());
                        values[i] = body;
                        gutters[i].extend(
                            (r.start - before.len() as u64..r.start).map(|k| format!("{}", k + 1)),
                        );
                        gutters[i].extend(
                            (r.start..r.end)
                                .map(|k| format!("{} {}", if i == 0 { "−" } else { "+" }, k + 1)),
                        );
                        gutters[i].extend((0..pad).map(|_| "·".into()));
                        gutters[i].extend(
                            (r.end..r.end + after.len() as u64).map(|k| format!("{}", k + 1)),
                        );
                    }
                }
                Ok(Detail {
                    title: format!("{}   ·   change {} of {}", h.kind(), n + 1, d.count),
                    labels,
                    values,
                    highlights,
                    gutters,
                    pages,
                })
            }
        }
    }
}

struct Panel {
    h: H,
    replace: bool,
    list: H,
    summary: H,
    title: H,
    labels: [H; 2],
    values: [H; 2],
    page: H,
    prev: H,
    next: H,
    toggle: H,
    all: H,
    earlier: H,
    later: H,
    apply: H,
    close: H,
}
unsafe extern "system" fn resize_panel(h: HWND, m: u32, w: usize, l: isize, _: usize, data: usize) -> isize {
    unsafe {
        if m == WM_SIZE && w != SIZE_MINIMIZED as usize {
            (&*(data as *const Panel)).layout();
        }
        DefSubclassProc(h, m, w, l)
    }
}
impl Panel {
    fn new(owner: H, replace: bool) -> Self {
        let scale = dpi(owner);
        let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        unsafe {
            GetMonitorInfoW(
                MonitorFromWindow(hwnd(owner), MONITOR_DEFAULTTONEAREST),
                &mut info,
            );
        }
        let area = info.rcWork;
        let mw = (area.right - area.left) * 96 / scale;
        let mh = (area.bottom - area.top) * 96 / scale;
        let (ow, oh) = client(owner);
        let w = (ow * 96 / scale - 12).clamp(400, 1120).min(mw.max(360));
        let ht = (oh * 96 / scale - 12).clamp(350, 730).min(mh.max(300));
        let h = forms::dialog(
            owner,
            if replace {
                "Review replacements"
            } else {
                "Compare files"
            },
            w,
            ht,
        );
        unsafe {
            let style = GetWindowLongPtrW(hwnd(h), GWL_STYLE);
            SetWindowLongPtrW(
                hwnd(h),
                GWL_STYLE,
                style | (WS_THICKFRAME | WS_MAXIMIZEBOX) as isize,
            );
            let mut r: RECT = std::mem::zeroed();
            GetWindowRect(hwnd(h), &mut r);
            let x = r.left.min(area.right - (r.right - r.left)).max(area.left);
            let y = r.top.min(area.bottom - (r.bottom - r.top)).max(area.top);
            SetWindowPos(
                hwnd(h),
                null_mut(),
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
        let ctl = |id, class, name, style| forms::control(h, id, class, name, style, 0, 0, 1, 1);
        let summary = ctl(825, "STATIC", "", 0);
        let list = ctl(
            LIST,
            "LISTBOX",
            "",
            WS_TABSTOP | WS_BORDER | WS_VSCROLL | (LBS_NOTIFY | LBS_NOINTEGRALHEIGHT) as u32,
        );
        let title = ctl(826, "STATIC", "", SS_NOPREFIX as u32);
        let labels = [
            ctl(821, "STATIC", "", SS_NOPREFIX as u32),
            ctl(822, "STATIC", "", SS_NOPREFIX as u32),
        ];
        let values = [
            ctl(823, "Scintilla", "", WS_TABSTOP | WS_BORDER),
            ctl(824, "Scintilla", "", WS_TABSTOP | WS_BORDER),
        ];
        let page = ctl(827, "STATIC", "", 0);
        let button = |id, label| ctl(id, "BUTTON", label, WS_TABSTOP);
        let prev = button(PREV, "‹  Previous");
        let next = button(NEXT, "Next  ›");
        let toggle = button(TOGGLE, "Exclude");
        let all = button(ALL, "Exclude all");
        let earlier = button(EARLIER, "Earlier text");
        let later = button(LATER, "More text");
        let apply = button(1, if replace { "Apply included" } else { "Show in editor" });
        let close = button(2, if replace { "Cancel" } else { "Close" });
        let panel = Self {
            h,
            replace,
            list,
            summary,
            title,
            labels,
            values,
            page,
            prev,
            next,
            toggle,
            all,
            earlier,
            later,
            apply,
            close,
        };
        panel.layout();
        let p = colors(LIGHT.get());
        for (i, v) in values.into_iter().enumerate() {
            msg(v, SETCODEPAGE, 65001, 0);
            msg(v, SETWRAPMODE, 1, 0);
            msg(v, SETMARGINLEFT, 0, size(h, 9) as _);
            msg(v, SETMARGINTYPEN, 0, 0);
            for margin in 0..5 {
                msg(v, SETMARGINWIDTHN, margin, 0);
            }
            msg(v, STYLESETFONT, 32, c"Cascadia Mono".as_ptr() as _);
            msg(v, STYLESETSIZE, 32, 10);
            msg(v, STYLESETBACK, 32, p.raised as _);
            msg(v, STYLESETFORE, 32, p.text as _);
            msg(v, STYLECLEARALL, 0, 0);
            msg(v, STYLESETFORE, 33, p.dim as _);
            msg(v, STYLESETBACK, 33, p.panel as _);
            let bg = if high_contrast() {
                p.active
            } else {
                match (LIGHT.get(), i) {
                    (true, 0) => hx(0xFAD6D8),
                    (true, _) => hx(0xCBEBD9),
                    (false, 0) => hx(0x643946),
                    _ => hx(0x235345),
                }
            };
            msg(v, STYLESETBACK, 1, bg as _);
            msg(
                v,
                STYLESETFORE,
                1,
                if high_contrast() { p.accent } else { p.text } as _,
            );
            msg(v, STYLESETBOLD, 1, 1);
            msg(v, SETCARETFORE, p.text as _, 0);
            msg(v, SETUNDOCOLLECTION, 0, 0);
            msg(v, SETACCESSIBILITY, 1, 0);
        }
        for handle in labels.into_iter().chain([title]) {
            msg(
                handle,
                WM_SETFONT,
                font(size(h, 15), 600, false) as usize,
                1,
            );
        }
        panel
    }
    fn layout(&self) {
        let rep = self.replace;
        let h = self.h;
        let (w, ht) = client(h);
        let scale = dpi(h);
        let px = |n: i32| n * scale / 96;
        let (w, ht) = (w * 96 / scale, ht * 96 / scale);
        let mut layout = WindowLayout::new(h);
        let mut at = |id, x, y, w, ht| {
            let visible = rep || ![self.toggle, self.all].contains(&id);
            layout.set(id, visible.then(|| rect(px(x), px(y), px(w), px(ht))));
        };
        let margin = 14;
        let full = w - 2 * margin;
        let footer = ht - 42;
        let compact = rep && w < 456;
        let nav = footer - if compact { 67 } else { 33 };
        let list_y = 40;
        let list_h = (ht / 5).clamp(42, 115);
        at(self.summary, margin, 12, full - 198, 24);
        at(self.prev, w - 195, 7, 90, 27);
        at(self.next, w - 99, 7, 85, 27);
        at(self.list, margin, list_y, full, list_h);
        let t = list_y + list_h + 10;
        at(self.title, margin, t, full, 24);
        let y = t + 27;
        if w >= 680 {
            let col = (full - 12) / 2;
            for i in 0..2 {
                let x = margin + i as i32 * (col + 12);
                at(self.labels[i], x, y, col, 39);
                at(self.values[i], x, y + 40, col, nav - y - 44);
            }
        } else {
            let half = (nav - y - 3) / 2;
            for i in 0..2 {
                let yy = y + i as i32 * half;
                at(self.labels[i], margin, yy, full, 22);
                at(self.values[i], margin, yy + 22, full, half - 25);
            }
        }
        at(self.page, margin, nav + 3, full - 205, 24);
        at(self.earlier, w - 210, nav, 95, 26);
        at(self.later, w - 110, nav, 96, 26);
        let stage_y = if compact { footer - 33 } else { footer };
        at(self.toggle, margin, stage_y, 83, 30);
        at(self.all, margin + 89, stage_y, 95, 30);
        at(self.apply, w - 235, footer, 125, 30);
        at(self.close, w - 102, footer, 88, 30);
        layout.apply();
    }
    fn sync_scroll(&self, previous: &mut [isize; 2]) {
        let first = self.values.map(|v| msg(v, GETFIRSTVISIBLELINE, 0, 0));
        if let Some(i) = (0..2).find(|&i| first[i] != previous[i]) {
            let source = self.values[i];
            let other = self.values[1 - i];
            let line = msg(source, DOCLINEFROMVISIBLE, first[i] as _, 0);
            let sub = first[i] - msg(source, VISIBLEFROMDOCLINE, line as _, 0);
            let target = msg(other, VISIBLEFROMDOCLINE, line as _, 0)
                + sub.min((msg(other, WRAPCOUNT, line as _, 0) - 1).max(0));
            if target >= 0 && target != first[1 - i] {
                msg(other, SETFIRSTVISIBLELINE, target as _, 0);
            }
        }
        *previous = self.values.map(|v| msg(v, GETFIRSTVISIBLELINE, 0, 0));
    }
    fn show_detail(&self, r: &mut Review, n: u64, page: u64, reload: bool) -> io::Result<u64> {
        let d = r.detail(n, page)?;
        if reload {
            let base = n / LIST_PAGE * LIST_PAGE;
            let labels = (base..(base + LIST_PAGE).min(r.len()))
                .map(|x| r.label(x))
                .collect::<io::Result<Vec<_>>>()?;
            list_set(self.list, labels.iter().map(String::as_str), Some((n % LIST_PAGE) as usize));
        } else {
            msg(self.list, LB_SETCURSEL, (n % LIST_PAGE) as _, 0);
        }
        set_text(self.summary, &r.summary());
        set_text(self.title, &d.title);
        for i in 0..2 {
            let v = self.values[i];
            set_text(self.labels[i], &d.labels[i]);
            msg(v, SETREADONLY, 0, 0);
            msg(v, CLEARALL, 0, 0);
            let s = &d.values[i];
            msg(v, ADDTEXT, s.len(), s.as_ptr() as _);
            msg(v, MARGINTEXTCLEARALL, 0, 0);
            let gutter = &d.gutters[i];
            let width = gutter.iter().map(|v| v.chars().count()).max().unwrap_or(0);
            msg(v, SETMARGINTYPEN, 0, 5);
            msg(
                v,
                SETMARGINWIDTHN,
                0,
                if width > 0 {
                    size(self.h, 12 + width as i32 * 8) as _
                } else {
                    0
                },
            );
            for (line, label) in gutter.iter().enumerate() {
                let label = std::ffi::CString::new(label.as_bytes()).unwrap();
                msg(v, MARGINSETTEXT, line, label.as_ptr() as _);
                msg(v, MARGINSETSTYLE, line, 33);
            }
            for range in &d.highlights[i] {
                msg(v, STARTSTYLING, range.start, 0);
                msg(v, SETSTYLING, range.len(), 1);
            }
            msg(v, SETSEL, 0, 0);
            msg(v, SETFIRSTVISIBLELINE, 0, 0);
            msg(v, SETREADONLY, 1, 0);
        }
        if let Review::Replace { checked, .. } = r {
            let included = checked.iter().filter(|v| **v).count();
            set_text(
                self.apply,
                &format!(
                    "Apply {} change{}",
                    included,
                    if included == 1 { "" } else { "s" }
                ),
            );
            set_text(
                self.toggle,
                if checked[n as usize] {
                    "Exclude"
                } else {
                    "Include"
                },
            );
            set_text(
                self.all,
                if checked.iter().all(|x| *x) {
                    "Exclude all"
                } else {
                    "Include all"
                },
            );
            enable(self.apply, checked.iter().any(|x| *x));
        }
        let prefix = if d.pages > 1 {
            format!("Text page {} of {}   ·   ", page + 1, d.pages)
        } else {
            String::new()
        };
        let help = if matches!(r, Review::Replace { .. }) {
            "Space includes or excludes    ·    Ctrl+Enter applies"
        } else {
            "F7 next    ·    Shift+F7 previous    ·    Enter shows it in the editor"
        };
        set_text(self.page, &format!("{prefix}{help}"));
        show(self.earlier, d.pages > 1);
        show(self.later, d.pages > 1);
        enable(self.prev, n > 0);
        enable(self.next, n + 1 < r.len());
        enable(self.earlier, page > 0);
        enable(self.later, page + 1 < d.pages);
        Ok(d.pages)
    }
}
fn enable(h: H, on: bool) {
    unsafe {
        EnableWindow(hwnd(h), i32::from(on));
    }
}
// `shown` receives the change a comparison was left at when the reader asked
// to see it in the editor (Enter, a double click or the Show in editor button).
fn show_review(owner: H, mut r: Review, initial: u64, shown: &mut Option<u64>) -> io::Result<bool> {
    if r.len() == 0 {
        return Ok(false);
    }
    let rep = matches!(r, Review::Replace { .. });
    let p = Panel::new(owner, rep);
    let h = p.h;
    // p lives on this UI thread until after the hook is removed. WM_SIZE also
    // arrives inside the native resize loop, between outer GetMessage calls.
    let hooked = unsafe { SetWindowSubclass(hwnd(h), Some(resize_panel), 1, &p as *const Panel as usize) != 0 };
    let mut n = initial.min(r.len() - 1);
    let mut page = 0;
    let old = focused();
    let result = (|| -> io::Result<bool> {
        let mut pages = p.show_detail(&mut r, n, page, true)?;
        let mut geometry = client(h);
        let mut first_lines = [0; 2];
        enable(owner, false);
        crate::win::show_popup(h, owner);
        focus(p.list);
        loop {
            if !hooked && client(h) != geometry {
                geometry = client(h);
                p.layout();
            }
            if !rep {
                p.sync_scroll(&mut first_lines);
            }
            let mut m: MSG = unsafe { std::mem::zeroed() };
            let got = unsafe { GetMessageW(&mut m, null_mut(), 0, 0) };
            if got <= 0 {
                if got == 0 {
                    unsafe {
                        PostQuitMessage(m.wParam as i32);
                    }
                }
                return Ok(false);
            }
            let mut action = 0;
            let mut to = n;
            let mut refresh = false;
            if m.hwnd == hwnd(h) && m.message == forms::FINISH {
                if m.wParam == 0 {
                    return Ok(false);
                }
                if rep {
                    action = 1;
                } else {
                    *shown = Some(n);
                    return Ok(true);
                }
            } else if m.hwnd == hwnd(h) && m.message == forms::CONTROL_EVENT {
                action = (m.wParam & 0xffff) as i32;
                if action == LIST {
                    if let Some(j) = selected(p.list) {
                        to = n / LIST_PAGE * LIST_PAGE + j as u64;
                    }
                    action = 0;
                    if !rep && m.wParam >> 16 == LBN_DBLCLK as usize && to < r.len() {
                        *shown = Some(to);
                        return Ok(true);
                    }
                }
            } else if m.message == WM_KEYDOWN
                && (m.hwnd == hwnd(h) || unsafe { IsChild(hwnd(h), m.hwnd) } != 0)
            {
                let vk = m.wParam as u16;
                let ctrl = down(VK_CONTROL as _);
                if vk == VK_ESCAPE {
                    return Ok(false);
                }
                if vk == VK_F7 {
                    action = if down(VK_SHIFT as _) { PREV } else { NEXT };
                }
                if vk == VK_RETURN && ctrl && rep {
                    action = 1;
                }
                if vk == VK_RETURN && !ctrl && m.hwnd != hwnd(p.list) {
                    // These buttons are owner-drawn, and the dialog manager does
                    // not treat an owner-drawn button as a push button: Enter on
                    // Cancel or Next would reach the default action and apply the
                    // replacements. Enter presses the focused button instead.
                    match unsafe { GetDlgCtrlID(m.hwnd) } {
                        2 => return Ok(false),
                        id if (PREV..=LATER).contains(&id) => action = id,
                        _ => {}
                    }
                }
                if m.hwnd == hwnd(p.list) {
                    match vk {
                        VK_UP => action = PREV,
                        VK_DOWN => action = NEXT,
                        VK_SPACE | VK_RETURN if rep => action = TOGGLE,
                        VK_RETURN => {
                            *shown = Some(n);
                            return Ok(true);
                        }
                        VK_NEXT => to = (n + LIST_PAGE).min(r.len() - 1),
                        VK_PRIOR => to = n.saturating_sub(LIST_PAGE),
                        VK_HOME => to = 0,
                        VK_END => to = r.len() - 1,
                        _ => {}
                    }
                }
            }
            match action {
                1 => {
                    if let Review::Replace { checked, .. } = &r
                        && checked.iter().any(|x| *x)
                    {
                        return Ok(true);
                    }
                }
                PREV => to = n.saturating_sub(1),
                NEXT => to = (n + 1).min(r.len() - 1),
                EARLIER => {
                    page = page.saturating_sub(1);
                    refresh = true;
                }
                LATER => {
                    page = (page + 1).min(pages - 1);
                    refresh = true;
                }
                TOGGLE | ALL => {
                    if let Review::Replace { checked, .. } = &mut r {
                        if action == TOGGLE {
                            checked[n as usize] = !checked[n as usize];
                        } else {
                            let all = checked.iter().all(|x| *x);
                            checked.fill(!all);
                        }
                        refresh = true;
                    }
                }
                _ => {}
            }
            if to != n || refresh {
                let reload = to / LIST_PAGE != n / LIST_PAGE || matches!(action, TOGGLE | ALL);
                if to != n {
                    page = 0;
                    n = to;
                }
                pages = p.show_detail(&mut r, n, page, reload)?;
            } else if action == 0 && unsafe { IsDialogMessageW(hwnd(h), &m) } == 0 {
                unsafe {
                    TranslateMessage(&m);
                    DispatchMessageW(&m);
                }
            }
        }
    })();
    enable(owner, true);
    unsafe { RemoveWindowSubclass(hwnd(h), Some(resize_panel), 1); }
    // Back to the owner only when the review window had the keyboard.
    let had_keyboard = unsafe { GetActiveWindow() } == hwnd(h);
    destroy(h);
    if had_keyboard {
        unsafe {
            SetActiveWindow(hwnd(owner));
        }
    }
    if unsafe { IsChild(hwnd(owner), hwnd(old)) } != 0 {
        focus(old);
    }
    result
}
impl App {
    pub(crate) fn review_replacements(&mut self, index: usize) {
        let root = self.root_dir();
        let Some(report) = self.report.as_ref() else {
            return;
        };
        if report.hits.is_empty() {
            return;
        }
        let answer = show_review(
            self.h,
            Review::Replace {
                report,
                checked: &mut self.checked,
                root,
            },
            index as u64,
            &mut None,
        );
        match answer {
            Ok(true) => self.project_apply(),
            Ok(false) => {
                self.checked.fill(false);
                self.preview_mode = false;
                self.report = None;
                self.show_rows(
                    2,
                    "Replacement canceled · no changes applied".into(),
                    vec![],
                );
                self.notice("Replacement canceled · no changes applied");
            }
            Err(e) => self.notice(e.to_string()),
        }
    }
    pub(crate) fn compare_current(&mut self, saved: bool) {
        self.drain_edits();
        let Some(i) = self.current() else {
            self.notice("Open the revised file first");
            return;
        };
        let b = &self.buffers[i];
        let revised = (b.id, b.rev);
        let right_name = format!("{}{}", b.name, if b.dirty { " (unsaved)" } else { "" });
        let right = if !b.large && b.dirty {
            let mut d = Document::empty();
            if let Err(e) = d.replace(0, 0, self.buffer_text(i).as_bytes()) {
                self.notice(e.to_string());
                return;
            }
            d
        } else {
            b.doc.clone()
        };
        let path = if saved {
            let Some(p) = b.path.clone() else {
                self.notice("Save this draft once, or compare with another file");
                return;
            };
            p
        } else {
            let Some(p) = dialogs::files(self.h)
                .set_title("Choose the original file to compare")
                .set_directory(self.root_dir())
                .pick_file()
            else {
                return;
            };
            p
        };
        let left_name = format!(
            "{}{}",
            path.file_name().unwrap_or_default().to_string_lossy(),
            if saved { " (saved)" } else { "" }
        );
        let (job, cancel) = self.new_job("compare");
        self.compared = Some(revised);
        let tx = self.tx.clone();
        self.notice("Comparing files…");
        thread::spawn(move || {
            let result = Document::open(&path)
                .and_then(|left| compare::run([left, right], [left_name, right_name], &cancel));
            let _ = tx.send(Worker::Compared(job, result.map_err(|e| e.to_string())));
        });
    }
    pub(crate) fn show_comparison(&mut self, result: Result<Comparison, String>) {
        match result {
            Ok(mut d) if d.count > 0 => {
                let mut shown = None;
                match show_review(self.h, Review::Compare(&mut d), 0, &mut shown) {
                    Ok(_) => {
                        if let Some(n) = shown {
                            self.show_compared_change(&mut d, n);
                        }
                    }
                    Err(e) => self.notice(e.to_string()),
                }
            }
            Ok(d) => alert(
                self.h,
                "Compare files",
                &format!("{} and {} have identical text.", d.names[0], d.names[1]),
            ),
            Err(e) => self.notice(e),
        }
    }
    /// Leaves a comparison at one change: the caret goes to its first line in
    /// the revised text, which is the document the comparison was started from.
    fn show_compared_change(&mut self, d: &mut Comparison, n: u64) {
        let Some((id, rev)) = self.compared else {
            return;
        };
        let Ok(hunk) = d.hunk(n) else {
            return;
        };
        self.drain_edits();
        let Some(i) = self.idx(id) else {
            self.notice("The compared file is no longer open");
            return;
        };
        self.remember_location();
        if self.current() != Some(i) {
            self.select(i, self.active, None);
        }
        let at = d.docs[1].line_start(hunk.sides[1].start);
        self.go(at, 0);
        if self.buffers[i].rev != rev {
            self.notice("This file changed after the comparison, so the line may have moved");
            return;
        }
        let what = if hunk.sides[1].is_empty() {
            "text was removed here".to_string()
        } else {
            format!("{} {}", hunk.kind().to_lowercase(), lines_label(&hunk.sides[1]))
        };
        self.notice(format!("Change {} of {} · {what}", n + 1, d.count));
    }
}
