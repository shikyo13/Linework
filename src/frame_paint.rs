use super::*;
use std::ptr::null_mut;

/// Selected once into its memory DC. The parent and its accessible children
/// read the same completed image, including during synchronous native layouts.
struct Image {
    dc: HDC,
    bitmap: HBITMAP,
    original: HGDIOBJ,
    capacity: (i32, i32),
    scene: Option<Scene>,
}
impl Image {
    fn new(h: HWND, w: i32, ht: i32) -> Option<Self> {
        let bucket = |n: i32| (n + 63) & !63;
        let capacity = (bucket(w), bucket(ht));
        unsafe {
            let screen = GetDC(h);
            if screen.is_null() {
                return None;
            }
            let dc = CreateCompatibleDC(screen);
            let bitmap = CreateCompatibleBitmap(screen, capacity.0, capacity.1);
            ReleaseDC(h, screen);
            let original = if !dc.is_null() && !bitmap.is_null() {
                SelectObject(dc, bitmap)
            } else {
                null_mut()
            };
            if original.is_null() || original as isize == -1 {
                if !dc.is_null() {
                    DeleteDC(dc);
                }
                if !bitmap.is_null() {
                    DeleteObject(bitmap);
                }
                return None;
            }
            Some(Self {
                dc,
                bitmap,
                original,
                capacity,
                scene: None,
            })
        }
    }
}
impl Drop for Image {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.original);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}
struct Scene {
    geometry: (i32, i32, i32, bool),
    frame: Frame,
    hot: Option<Hit>,
}
thread_local! { static IMAGE: RefCell<Option<Image>> = const { RefCell::new(None) }; }
thread_local! { static PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
pub(crate) const PRESENT: u32 = WM_APP + 164;
pub(crate) fn request_layout(h: H, damage: Option<RECT>) {
    if let Some(r) = damage {
        unsafe {
            InvalidateRect(hwnd(h), &r, 0);
        }
    }
    // WM_PAINT alone can wait behind a continuous stream of pointer input.
    // Keep just one presentation in the queue, without a fixed-rate timer.
    if !PENDING.replace(true) && unsafe { PostMessageW(hwnd(h), PRESENT, 0, 0) } == 0 {
        PENDING.set(false);
    }
}
pub(crate) fn present_pending(h: H) {
    PENDING.set(false);
    unsafe {
        RedrawWindow(
            hwnd(h),
            std::ptr::null(),
            null_mut(),
            RDW_UPDATENOW | RDW_NOCHILDREN,
        );
    }
    controls::paint_pending();
}

fn render(
    dc: HDC,
    f: &Frame,
    geometry: (i32, i32, i32, bool),
    damage: RECT,
    previous_side: Option<i32>,
) {
    let (px, py, scale, zoomed) = geometry;
    let saved = unsafe { SaveDC(dc) };
    unsafe {
        IntersectClipRect(dc, damage.left, damage.top, damage.right, damage.bottom);
    }
    {
        let s = Surface::clipped(dc, scale, damage);
        let (w, ht) = (px * 96 / scale, py * 96 / scale);
        let p = colors(f.light);
        let shell = Shell {
            s: &s,
            p,
            f,
            w,
            h: ht,
        };
        HITS.with_borrow_mut(Vec::clear);
        let side = if f.sidebar { f.sidebar_width } else { RAIL };
        // Tabs and the sidebar repaint their own backgrounds. Below the tabs,
        // only the strip uncovered by a shrinking sidebar needs clearing.
        if let Some(old_side) = previous_side {
            if old_side > side {
                s.fill(side, HEAD, old_side - side + 1, ht - HEAD - FOOT, p.bg);
            }
        } else {
            s.fill(0, 0, w + 2, ht + 2, p.bg);
        }
        shell.header(zoomed);
        shell.side(side, !crate::sidebar_grip::owns_edge());
        shell.tabs(side);
        if let Some((caption, measured)) = &f.zeros {
            shell.zeros(side, caption, *measured);
        }
        if f.tabs.is_empty() {
            shell.home(side);
        }
        s.fill(0, ht - FOOT, w, FOOT, p.panel);
        s.fill(0, ht - FOOT, w, 1, p.line);
        s.round(
            14,
            ht - 17,
            7,
            7,
            3,
            if f.recording {
                hx(0xEE8394)
            } else if f.light {
                hx(0x53856F)
            } else {
                hx(0x9DCEB7)
            },
            None,
        );
    }
    unsafe {
        RestoreDC(dc, saved);
    }
}

fn prepare(h: H) -> Option<RECT> {
    let (w, ht) = client(h);
    if w <= 0 || ht <= 0 {
        return None;
    }
    let scale = dpi(h);
    let geometry = (w, ht, scale, unsafe { IsZoomed(hwnd(h)) != 0 });
    IMAGE.with_borrow_mut(|slot| {
        if slot.as_ref().is_none_or(|img| {
            w > img.capacity.0
                || ht > img.capacity.1
                || (img.capacity.0 > w * 2 && img.capacity.1 > ht * 2)
        }) {
            *slot = Image::new(hwnd(h), w, ht);
        }
        let img = slot.as_mut()?;
        FRAME.with_borrow(|f| {
            HOT.with_borrow(|hot| {
                let mut damage = rect(0, 0, w, ht);
                let mut previous_side = None;
                if let Some(old) = &img.scene {
                    if old.geometry == geometry && old.frame == *f {
                        if old.hot == *hot {
                            return None;
                        }
                        let mut area: RECT = unsafe { std::mem::zeroed() };
                        HITS.with_borrow(|items| {
                            for (r, hit) in items {
                                if Some(hit) == hot.as_ref() || Some(hit) == old.hot.as_ref() {
                                    let prev = area;
                                    unsafe {
                                        UnionRect(&mut area, &prev, r);
                                    }
                                }
                            }
                        });
                        if area.right > area.left && area.bottom > area.top {
                            damage = area;
                        }
                    } else if old.geometry == geometry
                        && old.hot == *hot
                        && old.frame.light == f.light
                        && old.frame.recording == f.recording
                    {
                        damage.top = (HEAD * scale + 48) / 96;
                        damage.bottom = (ht * 96 / scale - FOOT) * scale / 96 + 1;
                        if f.sidebar
                            && !f.tabs.is_empty()
                            && f.sidebar_width != old.frame.sidebar_width
                        {
                            let mut check = f.clone();
                            check.sidebar_width = old.frame.sidebar_width;
                            if check == old.frame {
                                damage.left = (RAIL * scale + 48) / 96;
                                previous_side = Some(old.frame.sidebar_width);
                            }
                        }
                    }
                }
                render(img.dc, f, geometry, damage, previous_side);
                img.scene = Some(Scene {
                    geometry,
                    frame: f.clone(),
                    hot: hot.clone(),
                });
                Some(damage)
            })
        })
    })
}
pub(super) fn copy(dc: HDC, dst: RECT, x: i32, y: i32) -> bool {
    IMAGE.with_borrow(|slot| {
        slot.as_ref().is_some_and(|img| unsafe {
            BitBlt(
                dc,
                dst.left,
                dst.top,
                dst.right - dst.left,
                dst.bottom - dst.top,
                img.dc,
                x,
                y,
                SRCCOPY,
            ) != 0
        })
    })
}
pub fn paint(h: H) {
    // Moving HWNDs can expose pixels previously clipped from the parent. Let
    // Windows collect that damage before BeginPaint takes the update region.
    if let Some(damage) = prepare(h) {
        let mut layout = WindowLayout::new(h);
        controls::stage(h, damage, &mut layout);
        layout.apply();
    }
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let out = BeginPaint(hwnd(h), &mut ps);
        let r = ps.rcPaint;
        if r.right > r.left && r.bottom > r.top && !copy(out, r, r.left, r.top) {
            let (w, ht) = client(h);
            FRAME.with_borrow(|f| render(out, f, (w, ht, dpi(h), IsZoomed(hwnd(h)) != 0), r, None));
        }
        EndPaint(hwnd(h), &ps);
    }
}
/// The divider, tabs, and editor get their new coordinates in the same native
/// window-position batch. Synchronous owner-draw callbacks see the new image.
pub(crate) fn stage_layout(h: H, layout: &mut WindowLayout) -> Option<RECT> {
    let damage = prepare(h)?;
    controls::stage(h, damage, layout);
    Some(damage)
}
pub(crate) fn reset() {
    IMAGE.take();
}
pub(crate) fn request_hover(h: H, r: &RECT) {
    unsafe {
        RedrawWindow(
            hwnd(h),
            r,
            null_mut(),
            RDW_INVALIDATE | RDW_INTERNALPAINT | RDW_NOERASE,
        );
    }
}
