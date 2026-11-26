//! The compositor uses the same chrome and row drawing as the native window.
//! Only visible Explorer rows are needed while the divider has capture.
use super::*;
use windows_sys::Win32::UI::Controls::*;

struct Scroll {
    list: H,
    gap: i32,
    width: i32,
    y: i32,
    height: i32,
    arrow: i32,
    thumb: [i32; 2],
}

pub(crate) struct Art {
    frame: Frame,
    rows: Vec<(usize, String)>,
    selected: Option<usize>,
    list: [i32; 4],
    row_height: i32,
    scroll: Option<Scroll>,
    pub(crate) top_end: i32,
}

impl Art {
    pub(crate) fn tab_reserve(&self) -> i32 {
        super::tab_reserve(self.frame.can_preview)
    }
    pub(crate) fn caps(&self, height: i32) -> Vec<(i32, i32, i32)> {
        let mut rows = vec![(HEAD, HEAD + 44, 44)];
        if !self.frame.workspace.is_empty() && self.frame.tabs.len() > 4 {
            rows.push((HEAD + 44, OPEN_Y, 44));
        }
        for (i, (_, dirty)) in self.frame.tabs.iter().enumerate() {
            if !dirty {
                continue;
            }
            if self.frame.workspace.is_empty() && self.frame.side == 0 {
                if let Some(n) = self.rows.iter().position(|(idx, _)| *idx == i) {
                    let y = self.list[1] + n as i32 * self.row_height;
                    rows.push((y, y + self.row_height, 76));
                }
            } else if self.frame.side == 0 {
                let first = self
                    .frame
                    .active
                    .unwrap_or(0)
                    .saturating_sub(3)
                    .min(self.frame.tabs.len().saturating_sub(4));
                if (first..first + 4).contains(&i) {
                    let y = OPEN_Y + (i - first) as i32 * ROW;
                    rows.push((y, y + ROW, 76));
                }
            }
        }
        let body_cap = self.list[2].max(15);
        let mut result = Vec::new();
        let mut last = HEAD;
        rows.sort_by_key(|r| r.0);
        for (top, bottom, width) in rows {
            let top = top.max(last).min(height);
            let bottom = bottom.min(height);
            if top > last {
                result.push((last, top, body_cap));
            }
            if bottom > top {
                result.push((top, bottom, width));
            }
            last = top.max(bottom);
        }
        if last < height {
            result.push((last, height, body_cap));
        }
        result
    }
    pub(crate) fn read(host: H, side: i32) -> Self {
        let frame = FRAME.with_borrow(Clone::clone);
        let dpi = dpi(host);
        let logical = |px: i32| (px * 96 + dpi / 2) / dpi;
        let mut rows = Vec::new();
        let mut list = [0; 4];
        let mut row_height = ROW;
        let mut selected = None;
        let mut scroll = None;
        unsafe {
            let h = GetDlgItem(hwnd(host), 601);
            if !h.is_null() && IsWindowVisible(h) != 0 {
                let mut r = std::mem::zeroed();
                GetClientRect(h, &mut r);
                MapWindowPoints(h, hwnd(host), (&mut r as *mut RECT).cast(), 2);
                list = [
                    logical(r.left),
                    logical(r.top),
                    logical(side - r.right),
                    logical(r.bottom),
                ];
                let first = msg(h as H, LB_GETTOPINDEX, 0, 0).max(0) as usize;
                let count = msg(h as H, LB_GETCOUNT, 0, 0).max(0) as usize;
                row_height = logical(msg(h as H, LB_GETITEMHEIGHT, 0, 0).max(1) as i32).max(1);
                selected = crate::win::selected(h as H);
                let mut bar: SCROLLBARINFO = std::mem::zeroed();
                bar.cbSize = std::mem::size_of_val(&bar) as _;
                if GetScrollBarInfo(h, OBJID_VSCROLL, &mut bar) != 0
                    && bar.rgstate[0] & 0x8000 == 0
                    && bar.xyThumbBottom > bar.xyThumbTop
                {
                    let r = &mut bar.rcScrollBar;
                    MapWindowPoints(std::ptr::null_mut(), hwnd(host), (r as *mut RECT).cast(), 2);
                    scroll = Some(Scroll {
                        list: h as H,
                        gap: side - r.right,
                        width: r.right - r.left,
                        y: r.top,
                        height: r.bottom - r.top,
                        arrow: bar.dxyLineButton,
                        thumb: [bar.xyThumbTop, bar.xyThumbBottom],
                    });
                }
                let visible = ((list[3] - list[1]).max(0) / row_height + 1) as usize;
                rows.extend((first..count.min(first + visible)).map(|i| (i, list_text(h as H, i))));
            }
        }
        let top_end = HEAD + TABS + 8;
        Self {
            frame,
            rows,
            selected,
            list,
            row_height,
            scroll,
            top_end,
        }
    }

    pub(crate) fn paint(&mut self, dc: HDC, width: i32, height: i32, dpi: i32, side_px: i32) {
        let side = (side_px * 96 + dpi / 2) / dpi;
        self.frame.sidebar_width = side;
        let s = Surface::new(dc, dpi);
        let w = width * 96 / dpi;
        let p = colors(self.frame.light);
        let shell = Shell {
            s: &s,
            f: &self.frame,
            p,
            w,
            h: height * 96 / dpi,
        };
        HITS.with_borrow_mut(Vec::clear);
        shell.side(side, false);
        shell.tabs(side);
        s.fill(side, HEAD + TABS, w - side + 2, 9, p.bg);
        let [x, y, right_pad, bottom] = self.list;
        if !self.rows.is_empty() {
            let clip = s.r(x, y, side - x - right_pad, bottom - y);
            let rows = Surface::clipped(dc, dpi, clip);
            let saved = unsafe { SaveDC(dc) };
            unsafe {
                IntersectClipRect(dc, clip.left, clip.top, clip.right, clip.bottom);
            }
            for (n, (index, name)) in self.rows.iter().enumerate() {
                let top = y + n as i32 * self.row_height;
                sidebar_row(
                    &rows,
                    &self.frame,
                    name,
                    *index,
                    self.selected == Some(*index),
                    [x, top, side - x - right_pad, self.row_height],
                );
            }
            unsafe {
                RestoreDC(dc, saved);
            }
        }
        if let Some(bar) = &self.scroll {
            unsafe {
                let theme = OpenThemeData(hwnd(bar.list), wide("Scrollbar").as_ptr());
                let left = side_px - bar.gap - bar.width;
                let part = |from: i32, to: i32, kind: i32, state: i32, color: u32| {
                    let r = rect(left, bar.y + from, bar.width, (to - from).max(0));
                    if to > from
                        && (theme == 0
                            || DrawThemeBackground(theme, dc, kind, state, &r, std::ptr::null())
                                < 0)
                    {
                        fill(dc, r, color);
                    }
                };
                part(0, bar.arrow, SBP_ARROWBTN, ABS_UPNORMAL, p.raised);
                part(
                    bar.arrow,
                    bar.thumb[0],
                    SBP_UPPERTRACKVERT,
                    SCRBS_NORMAL,
                    p.panel,
                );
                part(
                    bar.thumb[0],
                    bar.thumb[1],
                    SBP_THUMBBTNVERT,
                    SCRBS_NORMAL,
                    p.dim,
                );
                part(
                    bar.thumb[1],
                    bar.height - bar.arrow,
                    SBP_LOWERTRACKVERT,
                    SCRBS_NORMAL,
                    p.panel,
                );
                part(
                    bar.height - bar.arrow,
                    bar.height,
                    SBP_ARROWBTN,
                    ABS_DOWNNORMAL,
                    p.raised,
                );
                if theme != 0 {
                    CloseThemeData(theme);
                }
            }
        }
    }
}
