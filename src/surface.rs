//! DPI-scaled vector chrome. The editor paints text independently with DirectWrite.
use crate::win::*;
use std::{cell::RefCell, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::RECT,
    Graphics::{Gdi::*, GdiPlus::*},
};

pub fn initialize() -> Result<(), String> {
    let mut token = 0;
    let settings = GdiplusStartupInput {
        GdiplusVersion: 1,
        ..unsafe { std::mem::zeroed() }
    };
    let status = unsafe { GdiplusStartup(&mut token, &settings, null_mut()) };
    if status == 0 {
        Result::Ok(())
    } else {
        Err(format!(
            "Unable to initialize the drawing system ({status})"
        ))
    }
}
fn argb(color: u32) -> u32 {
    0xff000000 | ((color & 255) << 16) | (color & 0xff00) | (color >> 16)
}
thread_local! {static GLYPHS:RefCell<Vec<(i32,H)>>=const {RefCell::new(Vec::new())};}
pub struct Surface {
    pub dc: HDC,
    pub scale: i32,
    graphics: *mut GpGraphics,
    clip: Option<RECT>,
}
impl Surface {
    pub fn new(dc: HDC, scale: i32) -> Self {
        let mut graphics = null_mut();
        unsafe {
            GdipCreateFromHDC(dc, &mut graphics);
            GdipSetSmoothingMode(graphics, SmoothingModeAntiAlias);
        }
        Self {
            dc,
            scale,
            graphics,
            clip: None,
        }
    }
    pub fn clipped(dc: HDC, scale: i32, r: RECT) -> Self {
        let mut surface = Self::new(dc, scale);
        surface.clip = Some(r);
        unsafe {
            GdipSetClipRectI(surface.graphics, r.left, r.top, r.right - r.left, r.bottom - r.top, CombineModeReplace);
        }
        surface
    }
    fn visible(&self, x: i32, y: i32, w: i32, h: i32) -> bool {
        self.clip.is_none_or(|clip| {
            let r = self.r(x, y, w, h);
            r.left < clip.right && r.right > clip.left && r.top < clip.bottom && r.bottom > clip.top
        })
    }
    pub fn px(&self, v: i32) -> i32 {
        (v * self.scale + 48) / 96
    }
    pub fn r(&self, x: i32, y: i32, w: i32, h: i32) -> RECT {
        RECT {
            left: self.px(x),
            top: self.px(y),
            right: self.px(x + w),
            bottom: self.px(y + h),
        }
    }
    pub fn fill(&self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        if self.visible(x, y, w, h) {
            fill(self.dc, self.r(x, y, w, h), color);
        }
    }
    pub fn text(
        &self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        s: &str,
        c: u32,
        fs: i32,
        wt: i32,
        flags: u32,
    ) {
        self.draw_text(x, y, w, h, s, c, fs, wt, false, flags);
    }
    pub fn draw_text(
        &self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        t: &str,
        color: u32,
        fs: i32,
        wt: i32,
        mono: bool,
        flags: u32,
    ) {
        if w <= 0 || h <= 0 || t.is_empty() || !self.visible(x, y, w, h) {
            return;
        }
        label(self.dc, self.r(x, y, w, h), t, color, self.px(fs), wt, mono, flags);
    }
    /// The width of `t` in the scale's units, as `draw_text` would draw it.
    pub fn measure(&self, t: &str, fs: i32, wt: i32, mono: bool) -> i32 {
        (measure(self.dc, t, self.px(fs), wt, mono) * 96 + self.scale - 1) / self.scale
    }
    pub fn small(&self, x: i32, y: i32, w: i32, h: i32, s: &str, c: u32) {
        self.draw_text(x, y, w, h, s, c, 11, 400, true, 0);
    }
    pub fn round(&self, x: i32, y: i32, w: i32, h: i32, r: i32, inside: u32, outline: Option<u32>) {
        if w < 2 || h < 2 || !self.visible(x, y, w, h) {
            return;
        }
        let k = self.scale as f32 / 96.;
        let (x, y, w, h) = (
            x as f32 * k + 0.5,
            y as f32 * k + 0.5,
            w as f32 * k - 1.,
            h as f32 * k - 1.,
        );
        let d = (2 * r) as f32 * k;
        unsafe {
            let mut path = null_mut();
            GdipCreatePath(FillModeAlternate, &mut path);
            for (a, b, t) in [
                (x, y, 180.),
                (x + w - d, y, 270.),
                (x + w - d, y + h - d, 0.),
                (x, y + h - d, 90.),
            ] {
                GdipAddPathArc(path, a, b, d, d, t, 90.);
            }
            GdipClosePathFigure(path);
            let mut brush = null_mut();
            GdipCreateSolidFill(argb(inside), &mut brush);
            GdipFillPath(self.graphics, brush as _, path);
            GdipDeleteBrush(brush as _);
            if let Some(c) = outline {
                let mut pen = null_mut();
                GdipCreatePen1(argb(c), k, UnitPixel, &mut pen);
                GdipDrawPath(self.graphics, pen, path);
                GdipDeletePen(pen);
            }
            GdipDeletePath(path);
        }
    }
    pub fn glyph(&self, x: i32, y: i32, w: i32, h: i32, code: u16, c: u32, fs: i32) {
        if !self.visible(x, y, w, h) { return; }
        if self.outline(x, y, w, h, code, c, fs) {
            return;
        }
        let fs = self.px(fs);
        let f = GLYPHS.with_borrow_mut(|list| {
            if let Some(e) = list.iter().find(|e| e.0 == fs) {
                return e.1;
            }
            let f = unsafe {
                CreateFontW(
                    -fs,
                    0,
                    0,
                    0,
                    400,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET as _,
                    0,
                    0,
                    DEFAULT_QUALITY as _,
                    0,
                    wide("Segoe MDL2 Assets").as_ptr(),
                ) as H
            };
            list.push((fs, f));
            f
        });
        unsafe {
            let old = SelectObject(self.dc, f as _);
            SetBkMode(self.dc, TRANSPARENT as _);
            SetTextColor(self.dc, c);
            DrawTextW(
                self.dc,
                &code,
                1,
                &mut self.r(x, y, w, h),
                DT_VCENTER | DT_CENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(self.dc, old);
        }
    }
    fn outline(&self, x: i32, y: i32, w: i32, h: i32, code: u16, color: u32, fs: i32) -> bool {
        // Line drawings share a 24-unit grid. Use the same silhouette in the rail, tabs and launcher.
        let lines: &[&[(i8, i8)]] = match code {
            0xE8A5 => &[
                &[(5, 2), (14, 2), (19, 7), (19, 22), (5, 22), (5, 2)],
                &[(14, 2), (14, 8), (19, 8)],
            ],
            0xE7C3 => &[
                &[(5, 2), (14, 2), (19, 7), (19, 22), (5, 22), (5, 2)],
                &[(14, 2), (14, 8), (19, 8)],
                &[(9, 14), (15, 14)],
                &[(12, 11), (12, 17)],
            ],
            0xE8DA | 0xE8E5 => &[
                &[(2, 18), (2, 6), (4, 4), (9, 4), (12, 7), (18, 7), (20, 9)],
                &[(2, 19), (6, 10), (22, 10), (19, 19), (2, 19)],
            ],
            0xE8B7 => &[&[
                (2, 6),
                (4, 4),
                (9, 4),
                (12, 7),
                (20, 7),
                (22, 9),
                (22, 18),
                (20, 20),
                (4, 20),
                (2, 18),
                (2, 6),
            ]],
            0xE81E => &[
                &[(8, 2), (16, 2), (20, 6), (20, 17), (8, 17), (8, 2)],
                &[(16, 2), (16, 7), (20, 7)],
                &[(5, 7), (3, 7), (3, 21), (15, 21), (15, 19)],
            ],
            0xE734 => &[&[(6, 3), (18, 3), (18, 21), (12, 16), (6, 21), (6, 3)]],
            0xE7FB => &[
                &[(3, 5), (21, 5), (21, 20), (3, 20), (3, 5)],
                &[(12, 5), (12, 20)],
            ],
            0xE943 | 0xE8D4 => &[
                &[(8, 7), (3, 12), (8, 17)],
                &[(16, 7), (21, 12), (16, 17)],
                &[(14, 4), (10, 20)],
            ],
            _ => return false,
        };
        let k = self.scale as f32 / 96.;
        let fs = fs as f32;
        let dx = x as f32 + (w as f32 - fs) * 0.5;
        let dy = y as f32 + (h as f32 - fs) * 0.5;
        unsafe {
            let mut pen = null_mut();
            GdipCreatePen1(
                argb(color),
                (fs / 13.0).clamp(1.15, 1.7) * k,
                UnitPixel,
                &mut pen,
            );
            GdipSetPenLineJoin(pen, LineJoinRound);
            GdipSetPenStartCap(pen, LineCapRound);
            GdipSetPenEndCap(pen, LineCapRound);
            for line in lines {
                let pts: Vec<_> = line
                    .iter()
                    .map(|&(a, b)| PointF {
                        X: (dx + a as f32 * fs / 24.) * k,
                        Y: (dy + b as f32 * fs / 24.) * k,
                    })
                    .collect();
                GdipDrawLines(self.graphics, pen, pts.as_ptr(), pts.len() as _);
            }
            GdipDeletePen(pen);
        }
        true
    }
    pub fn logo(&self, x: i32, y: i32, s: i32) {
        if !self.visible(x, y, s, s) { return; }
        let k = self.scale as f32 / 96.;
        for (off, width, height, col) in [
            (19., 14., 8., 0x9176BA),
            (14., 14., 8., 0xB69BDF),
            (9., 14., 8., 0xDEC9FA),
            (9., 4.8, 2.8, 0x8264A5),
        ] {
            let pts = [
                (-width, off),
                (0., off - height),
                (width, off),
                (0., off + height),
            ]
            .map(|(a, b)| PointF {
                X: (x as f32 + (16. + a) * s as f32 / 32.) * k,
                Y: (y as f32 + b * s as f32 / 32.) * k,
            });
            unsafe {
                let mut brush = null_mut();
                GdipCreateSolidFill(argb(hx(col)), &mut brush);
                GdipFillPolygon2(self.graphics, brush as _, pts.as_ptr(), 4);
                GdipDeleteBrush(brush as _);
            }
        }
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            GdipDeleteGraphics(self.graphics);
        }
    }
}

pub fn file_icon(name: &str, light: bool) -> (u16, u32) {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let (glyph, dark, day) = match ext.as_str() {
        "rs" | "cpp" | "c" | "h" | "go" => (0xE943, 0xE8B494, 0x975227),
        "ts" | "tsx" | "py" | "toml" => (0xE943, 0x9FC9EB, 0x335E88),
        "js" | "jsx" | "json" | "yaml" | "yml" => (0xE943, 0xE8CC8F, 0x80551D),
        "md" | "rst" | "txt" => (0xE8A5, 0xA5D6C3, 0x316B59),
        _ => (0xE8A5, 0xBCA6D6, 0x765094),
    };
    (glyph, hx(if light { day } else { dark }))
}
