//! During a drag, present Explorer, the tab strip, and the live editor windows
//! with the separator in one composition transaction. Native editing and wrap
//! continue in the underlying windows. Storage is bounded by the viewport.
use super::{Layout, visual::Edge};
use crate::{
    frame,
    frame::sidebar_art::Art,
    win::{H, hwnd},
};
use std::ptr::null_mut;
use windows::{
    Win32::{
        Foundation::{E_FAIL, POINT},
        Graphics::{
            Direct2D::Common::D2D_RECT_F,
            Direct3D11::*,
            DirectComposition::*,
            Dxgi::{Common::*, IDXGISurface},
        },
    },
    core::{Error, Interface, Result},
};
use windows_sys::Win32::{Graphics::Gdi::*, UI::WindowsAndMessaging::*};

pub(super) struct Content {
    art: Art,
    panes: Vec<(H, [i32; 4], Pixels)>,
    split: Option<i32>,
}
pub(super) struct Pixels {
    width: i32,
    height: i32,
    data: Vec<u32>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct View {
    doc: isize,
    first: isize,
    horiz: isize,
    caret: isize,
    anchor: isize,
    zoom: isize,
    wrap: isize,
}
impl View {
    fn read(h: H) -> Self {
        use linework_editor::sci::*;
        Self {
            doc: crate::win::msg(h, SCI_GETDOCPOINTER, 0, 0),
            first: crate::win::msg(h, SCI_GETFIRSTVISIBLELINE, 0, 0),
            horiz: crate::win::msg(h, SCI_GETXOFFSET, 0, 0),
            caret: crate::win::msg(h, SCI_GETCURRENTPOS, 0, 0),
            anchor: crate::win::msg(h, SCI_GETANCHOR, 0, 0),
            zoom: crate::win::msg(h, SCI_GETZOOM, 0, 0),
            wrap: crate::win::msg(h, SCI_GETWRAPMODE, 0, 0),
        }
    }
}
struct Source {
    id: H,
    technology: usize,
    view: View,
    width: i32,
    height: i32,
    dirty: bool,
}
thread_local! {static SOURCES:std::cell::RefCell<Vec<Source>>=const{std::cell::RefCell::new(Vec::new())};}

fn pane_pixels(h: H, width: i32, height: i32) -> Option<Pixels> {
    if width <= 0 || height <= 0 {
        return None;
    }
    let canvas = Canvas::new(width, height).ok()?;
    unsafe {
        crate::win::fill(
            canvas.dc,
            crate::win::rect(0, 0, width, height),
            crate::win::colors(crate::win::LIGHT.get()).bg,
        );
        SendMessageW(hwnd(h), WM_PRINTCLIENT, canvas.dc as usize, PRF_CLIENT as _);
        GdiFlush();
        let data =
            std::slice::from_raw_parts(canvas.pixels, width as usize * height as usize).to_vec();
        Some(Pixels {
            width,
            height,
            data,
        })
    }
}
pub(super) fn capture(host: H, side: i32) -> Content {
    use linework_editor::sci::*;
    restore_sources();
    let mut panes = Vec::new();
    let mut split = None;
    unsafe {
        for id in [700, 701, 622, 623, 710, 711] {
            let h = GetDlgItem(hwnd(host), id);
            if h.is_null() || IsWindowVisible(h) == 0 {
                continue;
            }
            let mut r = std::mem::zeroed();
            GetClientRect(h, &mut r);
            MapWindowPoints(
                h,
                hwnd(host),
                (&mut r as *mut windows_sys::Win32::Foundation::RECT).cast(),
                2,
            );
            if id == 701 && r.left > side + 2 {
                split = Some(r.left);
            }
            let (width, height) = (r.right - r.left, r.bottom - r.top);
            if id == 700 || id == 701 {
                let technology = crate::win::msg(h as H, SCI_GETTECHNOLOGY, 0, 0) as usize;
                // A HWND render target draws to the window even when WM_PRINTCLIENT
                // supplies a DC. DirectWrite's DC target paints the same editor here.
                if technology != SC_TECHNOLOGY_DEFAULT as usize
                    && technology != SC_TECHNOLOGY_DIRECTWRITEDC as usize
                {
                    crate::win::msg(
                        h as H,
                        SCI_SETTECHNOLOGY,
                        SC_TECHNOLOGY_DIRECTWRITEDC as usize,
                        0,
                    );
                }
                let view = View::read(h as H);
                SOURCES.with_borrow_mut(|v| {
                    v.push(Source {
                        id: h as H,
                        technology,
                        view,
                        width,
                        height,
                        dirty: false,
                    })
                });
            }
            if let Some(pixels) = pane_pixels(h as H, width, height) {
                panes.push((h as H, [r.left, r.top, r.right, r.bottom], pixels));
            }
        }
    }
    Content {
        art: Art::read(host, side),
        panes,
        split,
    }
}
pub(super) fn restore_sources() {
    use linework_editor::sci::*;
    let sources = SOURCES.take();
    for s in sources {
        if unsafe { IsWindow(hwnd(s.id)) } != 0
            && crate::win::msg(s.id, SCI_GETTECHNOLOGY, 0, 0) as usize != s.technology
        {
            crate::win::msg(s.id, SCI_SETTECHNOLOGY, s.technology, 0);
        }
    }
}
pub(super) fn document_changed(h: H) {
    SOURCES.with_borrow_mut(|v| {
        if let Some(s) = v.iter_mut().find(|s| s.id == h) {
            s.dirty = true;
        }
    });
}
pub(super) fn updated(h: H) -> Option<Pixels> {
    if !SOURCES.with_borrow(|v| v.iter().any(|s| s.id == h)) {
        return None;
    }
    let (w, ht) = crate::win::client(h);
    let view = View::read(h);
    let needed = SOURCES.with_borrow_mut(|v| {
        let Some(s) = v.iter_mut().find(|s| s.id == h) else {
            return false;
        };
        if !s.dirty
            && s.view == view
            && ht == s.height
            && (w == s.width || view.wrap == 0 && w < s.width)
        {
            return false;
        }
        s.dirty = false;
        s.width = w;
        s.height = ht;
        s.view = view;
        true
    });
    needed.then(|| pane_pixels(h, w, ht)).flatten()
}

struct Canvas {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    pixels: *const u32,
    width: i32,
}
impl Canvas {
    fn new(w: i32, h: i32) -> Result<Self> {
        unsafe {
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = w;
            info.bmiHeader.biHeight = -h;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            let dc = CreateCompatibleDC(null_mut());
            if dc.is_null() {
                return Err(Error::from(E_FAIL));
            }
            let mut bits = null_mut();
            let bmp = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if bmp.is_null() {
                DeleteDC(dc);
                return Err(Error::from(E_FAIL));
            }
            let old = SelectObject(dc, bmp);
            if old.is_null() || old as isize == -1 {
                DeleteObject(bmp);
                DeleteDC(dc);
                return Err(Error::from(E_FAIL));
            }
            Ok(Self {
                dc,
                bmp,
                old,
                pixels: bits.cast(),
                width: w,
            })
        }
    }
}
impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bmp);
            DeleteDC(self.dc);
        }
    }
}

fn clip(visual: &IDCompositionVisual, l: i32, t: i32, r: i32, b: i32) -> Result<()> {
    unsafe {
        visual.SetClip2(&D2D_RECT_F {
            left: l as _,
            top: t as _,
            right: r.max(l) as _,
            bottom: b.max(t) as _,
        })
    }
}

// Side caps contain right-aligned controls. The filename and its highlight
// share one retained image, clipped in the same transaction as the live panes.
struct Cap {
    visual: IDCompositionVisual,
    left: i32,
}
pub(super) struct Scene {
    pub(super) group: IDCompositionVisual,
    left: IDCompositionVisual,
    top: IDCompositionVisual,
    caps: Vec<Cap>,
    panes: Vec<(H, IDCompositionVisual, [i32; 4])>,
    cfg: Layout,
    max_side: i32,
    min_side: i32,
    top_right: i32,
    top_end: i32,
    split: Option<i32>,
    last: Option<i32>,
}
fn artwork(
    edge: &Edge,
    pixels: *const u32,
    stride: i32,
    width: i32,
    height: i32,
) -> Result<IDCompositionSurface> {
    unsafe {
        GdiFlush();
        let image = edge.device.CreateSurface(
            width as _,
            height as _,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            DXGI_ALPHA_MODE_IGNORE,
        )?;
        let mut offset = POINT::default();
        let target: IDXGISurface = image.BeginDraw(None, &mut offset)?;
        let texture = target.cast::<ID3D11Texture2D>();
        if let Ok(tex) = &texture {
            let box_ = D3D11_BOX {
                left: offset.x as _,
                top: offset.y as _,
                right: (offset.x + width) as _,
                bottom: (offset.y + height) as _,
                front: 0,
                back: 1,
            };
            edge.context.UpdateSubresource(
                tex,
                0,
                Some(&box_),
                pixels.cast(),
                (stride * 4) as _,
                0,
            );
        }
        image.EndDraw()?;
        texture?;
        Ok(image)
    }
}
impl Scene {
    pub(super) fn new(edge: &Edge, cfg: Layout, mut data: Content) -> Result<Self> {
        unsafe {
            let group = edge.device.CreateVisual()?;
            let canvas = Canvas::new(cfg.width, cfg.height)?;
            let max_side = cfg.from_pointer(cfg.width).clamp(1, cfg.width);
            let min_side = cfg.from_pointer(0).clamp(1, max_side);
            let head = cfg.px(frame::HEAD);
            let bottom = (cfg.height - cfg.px(frame::FOOT)).max(head);
            let top_end = cfg.px(data.art.top_end).min(bottom);
            let top_right = (cfg.width - cfg.px(data.art.tab_reserve())).max(min_side);
            data.art
                .paint(canvas.dc, cfg.width, cfg.height, cfg.dpi, max_side);
            let side_art = artwork(edge, canvas.pixels, canvas.width, max_side, cfg.height)?;
            let add = |texture: &IDCompositionSurface| -> Result<IDCompositionVisual> {
                let v = edge.device.CreateVisual()?;
                v.SetContent(texture)?;
                group.AddVisual(&v, true, None::<&IDCompositionVisual>)?;
                Ok(v)
            };
            let left = add(&side_art)?;
            let mut caps = Vec::new();
            for (y, end, width) in data.art.caps(bottom * 96 / cfg.dpi) {
                let v = add(&side_art)?;
                let cap_x = max_side - cfg.px(width);
                clip(&v, cap_x, cfg.px(y), max_side, cfg.px(end))?;
                caps.push(Cap {
                    visual: v,
                    left: cap_x,
                });
            }
            data.art
                .paint(canvas.dc, cfg.width, cfg.height, cfg.dpi, min_side);
            let tab_art = artwork(edge, canvas.pixels, canvas.width, cfg.width, top_end)?;
            let top = add(&tab_art)?;
            let fixed = add(&tab_art)?;
            clip(&fixed, top_right, head, cfg.width, top_end)?;
            let mut panes = Vec::new();
            for (h, r, pixels) in data.panes {
                let v = add(&artwork(
                    edge,
                    pixels.data.as_ptr(),
                    pixels.width,
                    pixels.width,
                    pixels.height,
                )?)?;
                v.SetOffsetY2(r[1] as f32)?;
                panes.push((h, v, r));
            }
            Ok(Self {
                group,
                left,
                top,
                caps,
                panes,
                cfg,
                max_side,
                min_side,
                top_right,
                top_end,
                split: data.split,
                last: None,
            })
        }
    }
    pub(super) fn update_pane(&self, edge: &Edge, h: H, pixels: &Pixels) -> Result<()> {
        if let Some((_, v, _)) = self.panes.iter().find(|p| p.0 == h) {
            unsafe {
                v.SetContent(&artwork(
                    edge,
                    pixels.data.as_ptr(),
                    pixels.width,
                    pixels.width,
                    pixels.height,
                )?)?;
                edge.device.Commit()?;
            }
        }
        Ok(())
    }
    pub(super) fn draw(&mut self, side: i32) -> Result<()> {
        if self.last == Some(side) {
            return Ok(());
        }
        let c = self.cfg;
        let head = c.px(frame::HEAD);
        clip(
            &self.left,
            c.px(frame::RAIL),
            head,
            side,
            c.height - c.px(frame::FOOT),
        )?;
        for cap in &self.caps {
            debug_assert!(cap.left <= self.max_side);
            unsafe {
                cap.visual.SetOffsetX2((side - self.max_side) as f32)?;
            }
        }
        let tab_shift = side - self.min_side;
        unsafe {
            self.top.SetOffsetX2(tab_shift as f32)?;
        }
        clip(
            &self.top,
            self.min_side,
            head,
            self.top_right - tab_shift,
            self.top_end,
        )?;
        let delta = side - c.side;
        for (_, v, [l, t, r, b]) in &self.panes {
            let shift = |x: i32| {
                if (x - c.side).abs() <= 1 {
                    delta
                } else if self.split.is_some_and(|s| (x - s).abs() <= 4) {
                    delta / 2
                } else {
                    0
                }
            };
            let x = l + shift(*l);
            let right = r + shift(*r);
            unsafe {
                v.SetOffsetX2(x as f32)?;
            }
            clip(v, 0, 0, right - x, b - t)?;
        }
        self.last = Some(side);
        Ok(())
    }
}
