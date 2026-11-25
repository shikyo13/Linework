use std::{
    cell::RefCell,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::*, Gdi::*},
    System::LibraryLoader::GetModuleHandleW,
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

pub type H = isize;
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
pub fn hwnd(h: H) -> HWND {
    h as HWND
}
pub fn module() -> H {
    unsafe { GetModuleHandleW(null()) as H }
}
pub fn rgb(r: u32, g: u32, b: u32) -> u32 {
    r | (g << 8) | (b << 16)
}
pub fn hx(value: u32) -> u32 {
    rgb(value >> 16, (value >> 8) & 255, value & 255)
}
#[derive(Clone, Copy)]
pub struct Colors {
    pub bg: u32,
    pub panel: u32,
    pub raised: u32,
    pub line: u32,
    pub text: u32,
    pub dim: u32,
    pub accent: u32,
    pub active: u32,
}
thread_local! { static CONTRAST: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) }; }
pub fn refresh_contrast() {
    CONTRAST.set(None);
}
pub fn high_contrast() -> bool {
    use windows_sys::Win32::UI::Accessibility::{HIGHCONTRASTW, HCF_HIGHCONTRASTON};
    CONTRAST.get().unwrap_or_else(|| unsafe {
        let mut settings = HIGHCONTRASTW {
            cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
            dwFlags: 0,
            lpszDefaultScheme: null_mut(),
        };
        let on = SystemParametersInfoW(SPI_GETHIGHCONTRAST, settings.cbSize,
            (&mut settings as *mut HIGHCONTRASTW).cast(), 0) != 0
            && settings.dwFlags & HCF_HIGHCONTRASTON != 0;
        CONTRAST.set(Some(on));
        on
    })
}
pub fn colors(light: bool) -> Colors {
    let p = if light {
        [
            0xFCFBFE, 0xF6F4FA, 0xFFFFFF, 0xDDD9E5, 0x262332, 0x696479, 0x684C97, 0xEAE3F5,
        ]
    } else {
        [
            0x18191F, 0x15161B, 0x202128, 0x30313B, 0xF0EDF7, 0xA3A1B2, 0xCCB2F5, 0x2B2637,
        ]
    };
    Colors {
        bg: hx(p[0]),
        panel: hx(p[1]),
        raised: hx(p[2]),
        line: hx(p[3]),
        text: hx(p[4]),
        dim: hx(p[5]),
        accent: hx(p[6]),
        active: hx(p[7]),
    }
}
thread_local! {static FONTS:RefCell<Vec<(i32,i32,bool,H)>>=const{RefCell::new(Vec::new())};pub static LIGHT:std::cell::Cell<bool>=const{std::cell::Cell::new(false)};}
pub fn font(size: i32, weight: i32, mono: bool) -> H {
    FONTS.with_borrow_mut(|f| {
        if let Some(x) = f
            .iter()
            .find(|x| x.0 == size && x.1 == weight && x.2 == mono)
        {
            return x.3;
        }
        let h = unsafe {
            CreateFontW(
                -size,
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                0,
                0,
                DEFAULT_QUALITY as u32,
                0,
                wide(if mono { "Cascadia Mono" } else { "Segoe UI" }).as_ptr(),
            ) as H
        };
        f.push((size, weight, mono, h));
        h
    })
}
pub fn msg(h: H, m: u32, w: usize, l: isize) -> isize {
    unsafe { SendMessageW(hwnd(h), m, w, l) }
}
pub fn post(h: H, m: u32, w: usize, l: isize) {
    unsafe {
        PostMessageW(hwnd(h), m, w, l);
    }
}
pub fn text(h: H) -> String {
    unsafe {
        let mut s = vec![0u16; GetWindowTextLengthW(hwnd(h)) as usize + 1];
        let n = GetWindowTextW(hwnd(h), s.as_mut_ptr(), s.len() as i32);
        String::from_utf16_lossy(&s[..n.max(0) as usize])
    }
}
pub fn set_text(h: H, t: &str) {
    unsafe {
        SetWindowTextW(hwnd(h), wide(t).as_ptr());
    }
}
pub fn focus(h: H) {
    unsafe {
        SetFocus(hwnd(h));
    }
}
pub fn focused() -> H {
    unsafe { GetFocus() as H }
}
pub fn down(v: i32) -> bool {
    unsafe { GetKeyState(v) < 0 }
}
pub fn show(h: H, on: bool) {
    unsafe {
        ShowWindow(hwnd(h), if on { SW_SHOWNA } else { SW_HIDE });
    }
}
pub fn rect(x: i32, y: i32, w: i32, h: i32) -> RECT {
    RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}
pub fn client(h: H) -> (i32, i32) {
    unsafe {
        let mut r = std::mem::zeroed();
        GetClientRect(hwnd(h), &mut r);
        (r.right, r.bottom)
    }
}
pub fn place(h: H, x: i32, y: i32, w: i32, ht: i32) {
    unsafe {
        SetWindowPos(
            hwnd(h),
            null_mut(),
            x,
            y,
            w.max(0),
            ht.max(0),
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Apply a sibling layout together. Comparing the real bounds also respects
/// controls resized or hidden by Windows between app layout passes.
pub struct WindowLayout {
    parent: H,
    changes: Vec<(H, RECT, u32)>,
    redraw: Vec<H>,
}
impl WindowLayout {
    pub fn new(parent: H) -> Self {
        Self { parent, changes: Vec::new(), redraw: Vec::new() }
    }
    pub fn set(&mut self, h: H, bounds: Option<RECT>) {
        if h == 0 { return; }
        unsafe {
            let visible = GetWindowLongW(hwnd(h), GWL_STYLE) as u32 & WS_VISIBLE != 0;
            let mut flags = SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER;
            let target = match bounds {
                None if !visible => return,
                None => {
                    flags |= SWP_HIDEWINDOW | SWP_NOMOVE | SWP_NOSIZE;
                    rect(0, 0, 0, 0)
                }
                Some(mut r) => {
                    r.right = r.right.max(r.left);
                    r.bottom = r.bottom.max(r.top);
                    let mut old: RECT = std::mem::zeroed();
                    if GetWindowRect(hwnd(h), &mut old) != 0 {
                        MapWindowPoints(null_mut(), hwnd(self.parent), (&mut old as *mut RECT).cast(), 2);
                        if r.left == old.left && r.top == old.top { flags |= SWP_NOMOVE; }
                        if r.right-r.left == old.right-old.left && r.bottom-r.top == old.bottom-old.top { flags |= SWP_NOSIZE; }
                        if visible && flags & (SWP_NOMOVE | SWP_NOSIZE) == (SWP_NOMOVE | SWP_NOSIZE) { return; }
                    }
                    if !visible { flags |= SWP_SHOWWINDOW; }
                    r
                }
            };
            self.changes.push((h, target, flags));
        }
    }
    pub fn redraw(&mut self, h: H) {
        self.redraw.push(h);
    }
    pub fn apply(self) {
        self.apply_positions();
        for h in self.redraw { invalidate(h); }
    }
    fn apply_positions(&self) {
        if self.changes.is_empty() { return; }
        unsafe {
            let mut batch = BeginDeferWindowPos(self.changes.len() as i32);
            for &(h, r, flags) in &self.changes {
                if batch.is_null() { break; }
                batch = DeferWindowPos(batch, hwnd(h), null_mut(), r.left, r.top,
                    r.right - r.left, r.bottom - r.top, flags);
            }
            if !batch.is_null() && EndDeferWindowPos(batch) != 0 { return; }
            // A failed DeferWindowPos discards its entire batch.
            for &(h, r, flags) in &self.changes {
                SetWindowPos(hwnd(h), null_mut(), r.left, r.top, r.right-r.left, r.bottom-r.top, flags);
            }
        }
    }
}
pub fn invalidate(h: H) {
    unsafe {
        InvalidateRect(hwnd(h), null(), 0);
    }
}
pub fn dpi(h: H) -> i32 {
    unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForWindow(hwnd(h)).max(96) as i32 }
}
pub fn size(h: H, n: i32) -> i32 {
    n * dpi(h) / 96
}
pub fn child(parent: H, id: i32, class: &str, label: &str, flags: u32) -> H {
    unsafe {
        let h = CreateWindowExW(
            0,
            wide(class).as_ptr(),
            wide(label).as_ptr(),
            WS_CHILD | flags,
            0,
            0,
            0,
            0,
            hwnd(parent),
            id as usize as HMENU,
            module() as _,
            null(),
        ) as H;
        msg(
            h,
            WM_SETFONT,
            font(size(parent, 14), 400, false) as usize,
            1,
        );
        h
    }
}
pub fn destroy(h: H) {
    unsafe {
        DestroyWindow(hwnd(h));
    }
}
pub fn dark_chrome(h: H, dark: bool) {
    let val = dark as i32;
    unsafe {
        DwmSetWindowAttribute(hwnd(h), 20, &val as *const _ as _, 4);
    }
}
pub fn register(name: &str, proc: WNDPROC) -> Result<(), String> {
    unsafe {
        let icon = LoadIconW(module() as _, 1usize as _);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as _,
            style: CS_DBLCLKS,
            lpfnWndProc: proc,
            hInstance: module() as _,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hIcon: icon,
            hIconSm: icon,
            ..std::mem::zeroed()
        };
        let n = wide(name);
        let wc = WNDCLASSEXW {
            lpszClassName: n.as_ptr(),
            ..wc
        };
        if RegisterClassExW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }
}
pub fn ctlcolor(dc: H, light: bool, edit: bool) -> isize {
    let c = colors(light);
    let bg = if edit { c.raised } else { c.panel };
    unsafe {
        SetTextColor(dc as HDC, c.text);
        SetBkColor(dc as HDC, bg);
        SetDCBrushColor(dc as HDC, bg);
        GetStockObject(DC_BRUSH) as isize
    }
}
pub fn alert(owner: H, title: &str, message: &str) {
    unsafe {
        MessageBoxW(
            hwnd(owner),
            wide(message).as_ptr(),
            wide(title).as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}
pub fn confirm(owner: H, title: &str, message: &str, cancel: bool) -> i32 {
    unsafe {
        MessageBoxW(
            hwnd(owner),
            wide(message).as_ptr(),
            wide(title).as_ptr(),
            if cancel { MB_YESNOCANCEL } else { MB_YESNO } | MB_ICONQUESTION,
        )
    }
}
pub fn fill(dc: HDC, r: RECT, color: u32) {
    unsafe {
        SetDCBrushColor(dc, color);
        FillRect(dc, &r, GetStockObject(DC_BRUSH) as _);
    }
}
/// Commit a completely painted native control in one blit. Buffered Paint
/// reuses its surfaces for the thread and preserves the target coordinates.
pub fn buffered(dc: HDC, area: RECT, draw: impl FnOnce(HDC)) {
    use windows_sys::Win32::UI::Controls::{
        BPBF_COMPATIBLEBITMAP, BeginBufferedPaint, BufferedPaintInit,
        BufferedPaintUnInit, EndBufferedPaint,
    };
    struct Buffering(bool);
    impl Drop for Buffering {
        fn drop(&mut self) {
            if self.0 { unsafe { BufferedPaintUnInit(); } }
        }
    }
    thread_local! {
        static READY: Buffering = Buffering(unsafe { BufferedPaintInit() >= 0 });
    }
    let mut memory = null_mut();
    let handle = READY.with(|ready| if ready.0 && area.right > area.left && area.bottom > area.top {
        unsafe { BeginBufferedPaint(dc, &area, BPBF_COMPATIBLEBITMAP, null(), &mut memory) }
    } else { 0 });
    if handle == 0 {
        draw(dc);
    } else {
        draw(memory);
        unsafe { EndBufferedPaint(handle, 1); }
    }
}
pub fn label(
    dc: HDC,
    r: RECT,
    s: &str,
    color: u32,
    size: i32,
    weight: i32,
    mono: bool,
    flags: u32,
) {
    unsafe {
        let old = SelectObject(dc, font(size, weight, mono) as _);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, color);
        let mut r = r;
        let s = wide(s);
        DrawTextW(
            dc,
            s.as_ptr(),
            (s.len() - 1) as i32,
            &mut r,
            DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX | flags,
        );
        SelectObject(dc, old);
    }
}
/// The width in pixels of `s` in the font `label` would use.
pub fn measure(dc: HDC, s: &str, size: i32, weight: i32, mono: bool) -> i32 {
    if s.is_empty() {
        return 0;
    }
    unsafe {
        let old = SelectObject(dc, font(size, weight, mono) as _);
        let text = wide(s);
        let mut extent: SIZE = std::mem::zeroed();
        GetTextExtentPoint32W(dc, text.as_ptr(), (text.len() - 1) as i32, &mut extent);
        SelectObject(dc, old);
        extent.cx
    }
}
#[path = "popup_menu.rs"]
pub(crate) mod popup;
pub(crate) use popup::{PopupItem, menu_at};
pub fn list_set<'a>(h: H, items: impl IntoIterator<Item = &'a str>, selected: Option<usize>) {
    // DefWindowProc restores WS_VISIBLE when redraw is reenabled. A hidden
    // list can be updated directly without accidentally showing it.
    let visible = unsafe { GetWindowLongW(hwnd(h), GWL_STYLE) as u32 & WS_VISIBLE != 0 };
    if visible { msg(h, WM_SETREDRAW, 0, 0); }
    msg(h, LB_RESETCONTENT, 0, 0);
    for i in items {
        msg(h, LB_ADDSTRING, 0, wide(i).as_ptr() as _);
    }
    if let Some(i) = selected {
        msg(h, LB_SETCURSEL, i, 0);
    }
    if visible {
        msg(h, WM_SETREDRAW, 1, 0);
        invalidate(h);
    }
}
pub fn selected(h: H) -> Option<usize> {
    let s = msg(h, LB_GETCURSEL, 0, 0);
    (s >= 0).then_some(s as usize)
}

/// Probe the active layout without changing its dead-key state. Windows
/// synthesizes a left-Control keypress for layouts that implement AltGr.
pub fn altgr_text(m: &MSG) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    unsafe {
        if GetKeyState(VK_RMENU as i32) >= 0 || GetKeyState(VK_LCONTROL as i32) >= 0 {
            return false;
        }
        let mut state = [0u8; 256];
        let mut text = [0u16; 8];
        GetKeyboardState(state.as_mut_ptr()) != 0
            && ToUnicodeEx(
                m.wParam as u32,
                ((m.lParam >> 16) & 255) as u32,
                state.as_ptr(),
                text.as_mut_ptr(),
                text.len() as i32,
                4,
                GetKeyboardLayout(0),
            ) != 0
    }
}
