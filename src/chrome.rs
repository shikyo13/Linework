use crate::{frame, win::*};
use std::{
    cell::RefCell,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::{InvalidateRect, ScreenToClient},
    UI::{Controls::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

pub fn theme_child(h: H, light: bool) {
    unsafe {
        SetWindowTheme(
            hwnd(h),
            wide(if light {
                "Explorer"
            } else {
                "DarkMode_Explorer"
            })
            .as_ptr(),
            null(),
        );
    }
}
fn hover(h: HWND, x: i32, y: i32, nc: bool) {
    let a = frame::hit(x, y);
    frame::hover(h as H, a.clone());
    let mut t = TRACKMOUSEEVENT {
        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as _,
        dwFlags: TME_LEAVE | if nc { TME_NONCLIENT } else { 0 },
        hwndTrack: h,
        dwHoverTime: 0,
    };
    unsafe {
        TrackMouseEvent(&mut t);
    }
}
fn point(h: HWND, l: isize) -> POINT {
    let mut p = POINT {
        x: l as i16 as i32,
        y: (l >> 16) as i16 as i32,
    };
    unsafe {
        ScreenToClient(h, &mut p);
    }
    p
}
thread_local! {static CAPTION_DOWN:std::cell::Cell<u32>=const{std::cell::Cell::new(0)};}
pub unsafe fn message(h: HWND, m: u32, w: usize, l: isize) -> Option<isize> {
    unsafe {
        match m {
            WM_GETMINMAXINFO => {
                let v = &mut *(l as *mut MINMAXINFO);
                let normal = DefWindowProcW(h, m, w, l);
                v.ptMinTrackSize.x = size(h as H, 760);
                v.ptMinTrackSize.y = size(h as H, 500);
                Some(normal)
            }
            WM_NCCALCSIZE => {
                // Preserve Windows' resize frame on three sides. Replace only the caption.
                let top = (*(l as *mut RECT)).top;
                let ret = DefWindowProcW(h, m, w, l);
                let rc = &mut *(l as *mut RECT);
                let frame = windows_sys::Win32::UI::HiDpi::GetSystemMetricsForDpi(
                    SM_CYSIZEFRAME,
                    dpi(h as H) as _,
                ) + windows_sys::Win32::UI::HiDpi::GetSystemMetricsForDpi(
                    SM_CXPADDEDBORDER,
                    dpi(h as H) as _,
                );
                rc.top = top + if IsZoomed(h) != 0 { frame } else { 1 };
                Some(ret)
            }
            WM_NCHITTEST => {
                let native = DefWindowProcW(h, m, w, l);
                if ![
                    HTCLIENT,
                    HTCAPTION,
                    HTSYSMENU,
                    HTMINBUTTON,
                    HTMAXBUTTON,
                    HTCLOSE,
                ]
                .contains(&(native as u32))
                {
                    return Some(native);
                }
                let p = point(h, l);
                let (w, _) = client(h as H);
                let edge = size(h as H, 5);
                if IsZoomed(h) == 0 && p.y < edge {
                    return Some(if p.x < edge * 2 {
                        HTTOPLEFT
                    } else if p.x > w - edge * 2 {
                        HTTOPRIGHT
                    } else {
                        HTTOP
                    } as _);
                }
                if p.y < size(h as H, frame::HEAD) {
                    let id = if p.x >= w - size(h as H, 44) {
                        HTCLOSE
                    } else if p.x >= w - size(h as H, 88) {
                        HTMAXBUTTON
                    } else if p.x >= w - size(h as H, 132) {
                        HTMINBUTTON
                    } else if frame::hit(p.x, p.y).is_some() {
                        HTCLIENT
                    } else {
                        HTCAPTION
                    };
                    return Some(id as _);
                }
                Some(HTCLIENT as _)
            }
            WM_NCMOUSEMOVE => {
                let p = point(h, l);
                hover(h, p.x, p.y, true);
                None
            }
            WM_NCMOUSELEAVE | WM_MOUSELEAVE => {
                frame::hover_at_pointer(h as H);
                None
            }
            WM_MOUSEMOVE if GetCapture() != h => {
                hover(h, l as i16 as i32, (l >> 16) as i16 as i32, false);
                None
            }
            WM_NCLBUTTONDOWN if [HTCLOSE, HTMINBUTTON, HTMAXBUTTON].contains(&(w as u32)) => {
                SetCapture(h);
                CAPTION_DOWN.set(w as u32);
                Some(0)
            }
            WM_CAPTURECHANGED => {
                CAPTION_DOWN.set(0);
                None
            }
            WM_LBUTTONUP | WM_NCLBUTTONUP if CAPTION_DOWN.get() != 0 => {
                let button = CAPTION_DOWN.replace(0);
                let p = if m == WM_NCLBUTTONUP {
                    point(h, l)
                } else {
                    POINT {
                        x: l as i16 as i32,
                        y: (l >> 16) as i16 as i32,
                    }
                };
                let target = frame::hit(p.x, p.y);
                ReleaseCapture();
                let id = match button {
                    HTCLOSE => "winclose",
                    HTMINBUTTON => "winmin",
                    _ => "winmax",
                };
                if matches!(target,Some(frame::Hit::Cmd(ref s)) if s==id) {
                    let action = match button {
                        HTCLOSE => SC_CLOSE,
                        HTMINBUTTON => SC_MINIMIZE,
                        _ if IsZoomed(h) != 0 => SC_RESTORE,
                        _ => SC_MAXIMIZE,
                    };
                    PostMessageW(h, WM_SYSCOMMAND, action as _, 0);
                }
                Some(0)
            }
            WM_DRAWITEM if w == 0 && l != 0 && popup::draw(&*(l as *const DRAWITEMSTRUCT)) => Some(1),
            WM_MEASUREITEM if w == 0 && l != 0 && popup::measure(&mut *(l as *mut MEASUREITEMSTRUCT)) => Some(1),
            WM_MENUCHAR => popup::menu_char(l as HMENU, w as u16),
            WM_DRAWITEM
                if w >= 4000 && frame::controls::draw(w, &*(l as *const DRAWITEMSTRUCT)) =>
            {
                Some(1)
            }
            WM_CTLCOLORBTN if frame::controls::is_face(hwnd(l)) => {
                Some(windows_sys::Win32::Graphics::Gdi::GetStockObject(windows_sys::Win32::Graphics::Gdi::NULL_BRUSH) as isize)
            }
            WM_CTLCOLORSTATIC if matches!(GetDlgCtrlID(hwnd(l)), 622 | 623) => {
                Some(windows_sys::Win32::Graphics::Gdi::GetStockObject(windows_sys::Win32::Graphics::Gdi::NULL_BRUSH) as isize)
            }
            WM_COMMAND
                if l != 0
                    && w >> 16 == BN_CLICKED as usize
                    && frame::controls::invoke(w & 65535, l) =>
            {
                Some(0)
            }
            WM_DRAWITEM if matches!(w, 622 | 623) => {
                frame::draw_pane_header(h as H, &*(l as *const DRAWITEMSTRUCT));
                Some(1)
            }
            WM_COMMAND if matches!(w & 65535, 622 | 623) && w >> 16 == 0 => {
                focus(GetDlgItem(h, 700 + (w as i32 & 1)) as H);
                Some(0)
            }
            WM_SYSCOLORCHANGE | WM_SETTINGCHANGE => {
                refresh_contrast();
                TIPS.with_borrow_mut(|tips| tips.appearance = None);
                rail_tips(h as H);
                frame::rendering::reset();
                invalidate(h as H);
                for id in [601, 622, 623] {
                    let child = GetDlgItem(h, id);
                    if !child.is_null() {
                        invalidate(child as H);
                    }
                }
                None
            }
            WM_DRAWITEM if w == 601 => {
                frame::draw_list(h as H, &*(l as *const DRAWITEMSTRUCT), false);
                Some(1)
            }
            WM_MEASUREITEM if w == 601 => {
                (*(l as *mut MEASUREITEMSTRUCT)).itemHeight = size(h as H, frame::ROW) as _;
                Some(1)
            }
            WM_CTLCOLORLISTBOX if GetDlgCtrlID(hwnd(l)) == 601 => {
                Some(ctlcolor(w as _, LIGHT.get(), false))
            }
            WM_SETCURSOR if (l & 65535) as u32 == HTCLIENT && w == h as usize => {
                let mut p = POINT { x: 0, y: 0 };
                GetCursorPos(&mut p);
                ScreenToClient(h, &mut p);
                if frame::hit(p.x, p.y).is_some() {
                    SetCursor(LoadCursorW(null_mut(), IDC_HAND));
                    Some(1)
                } else {
                    None
                }
            }
            WM_ACTIVATE => {
                InvalidateRect(h, null(), 0);
                None
            }
            _ => None,
        }
    }
}

// Native tooltips keep labels available on the compact activity rail without taking keyboard focus.
#[derive(Default)]
struct RailTips {
    window: H,
    scale: i32,
    height: i32,
    appearance: Option<(bool, bool)>,
    names: Vec<Vec<u16>>,
}
thread_local! { static TIPS: RefCell<RailTips> = RefCell::default(); }
pub fn rail_tips(host: H) {
    let scale = dpi(host);
    let height = client(host).1;
    TIPS.with_borrow_mut(|state| unsafe {
        if state.window == 0 {
            state.window = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                TOOLTIPS_CLASSW,
                null(),
                WS_POPUP | TTS_ALWAYSTIP | TTS_NOPREFIX,
                0,
                0,
                0,
                0,
                hwnd(host),
                null_mut(),
                module() as _,
                null(),
            ) as H;
            if state.window == 0 {
                return;
            }
            state.names = [
                "Explorer",
                "Search",
                "Symbols in file",
                "Bookmarks",
                "Keyboard shortcuts",
            ]
            .iter()
            .map(|s| wide(s))
            .collect();
        }
        let tip = state.window;
        let light = LIGHT.get();
        let contrast = high_contrast();
        if state.appearance != Some((light, contrast)) {
            // A visually styled tooltip ignores TTM_SETTIP*COLOR. Restore the
            // system style and its accessibility colors in High Contrast.
            let empty = [0u16];
            let style = if contrast { null() } else { empty.as_ptr() };
            SetWindowTheme(hwnd(tip), style, style);
            let palette = colors(light);
            let (back, text) = if contrast {
                use windows_sys::Win32::Graphics::Gdi::{GetSysColor, COLOR_INFOBK, COLOR_INFOTEXT};
                (GetSysColor(COLOR_INFOBK), GetSysColor(COLOR_INFOTEXT))
            } else {
                (palette.raised, palette.text)
            };
            msg(tip, TTM_SETTIPBKCOLOR, back as usize, 0);
            msg(tip, TTM_SETTIPTEXTCOLOR, text as usize, 0);
            state.appearance = Some((light, contrast));
        }
        if scale == state.scale && height == state.height {
            return;
        }
        let first_layout = state.scale == 0;
        for (i, name) in state.names.iter_mut().enumerate() {
            let y = if i < 4 {
                size(host, frame::HEAD + 9 + i as i32 * 43)
            } else {
                height - size(host, frame::FOOT + 45)
            };
            let mut t: TTTOOLINFOW = std::mem::zeroed();
            t.cbSize = std::mem::size_of_val(&t) as _;
            t.hwnd = hwnd(host);
            t.uFlags = TTF_SUBCLASS;
            t.uId = i + 1;
            t.rect = rect(size(host, 5), y, size(host, 37), size(host, 37));
            t.lpszText = name.as_mut_ptr();
            msg(
                tip,
                if first_layout {
                    TTM_ADDTOOLW
                } else {
                    TTM_NEWTOOLRECTW
                },
                0,
                &t as *const _ as _,
            );
        }
        state.scale = scale;
        state.height = height;
    });
}
