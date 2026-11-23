use crate::{frame, state::Step, win::*};
use linework_editor::{Notification, sci as n};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    ffi::CStr,
    path::PathBuf,
    ptr::null,
};
use windows_sys::Win32::{
    Foundation::*,
    UI::{Shell::*, WindowsAndMessaging::*},
};
#[derive(Clone, Copy, Default)]
pub struct Binding {
    pub id: u64,
    pub large: bool,
    pub base: u64,
}
thread_local! {pub static SKIP_MACRO:Cell<bool>=const{Cell::new(false)};static QUIET:Cell<u32>=const{Cell::new(0)};pub static EVENTS:RefCell<VecDeque<Event>>=const{RefCell::new(VecDeque::new())};}
#[derive(Clone, Default)]
struct Projection {
    binding: Binding,
    map: std::sync::Arc<linework_storage::FoldMap>,
}
thread_local! {static PROJECTIONS: RefCell<[Projection;2]>=RefCell::new(Default::default());}
pub fn install(p: usize, binding: Binding, map: std::sync::Arc<linework_storage::FoldMap>) {
    PROJECTIONS.with_borrow_mut(|v| v[p] = Projection { binding, map });
}
pub fn binding(p: usize) -> Binding {
    PROJECTIONS.with_borrow(|v| v[p].binding)
}
pub fn view(p: usize, at: u64) -> u64 {
    PROJECTIONS.with_borrow(|v| v[p].map.visible(at))
}
pub fn physical(p: usize, at: u64) -> u64 {
    PROJECTIONS.with_borrow(|v| v[p].map.physical(at))
}
pub fn point(p: usize, local: u64) -> u64 {
    PROJECTIONS.with_borrow(|v| {
        let s = &v[p];
        s.map.physical(s.map.visible(s.binding.base) + local)
    })
}
pub fn end_point(p: usize, local: u64) -> u64 {
    PROJECTIONS.with_borrow(|v| {
        let s = &v[p];
        s.map.physical_end(s.map.visible(s.binding.base) + local)
    })
}
pub fn range(p: usize, start: u64, end: u64) -> std::ops::Range<u64> {
    let lo = point(p, start);
    lo..if start == end { lo } else { end_point(p, end) }
}
pub fn offset(p: usize, at: u64) -> u64 {
    view(p, at).saturating_sub(view(p, binding(p).base))
}
pub(crate) fn is_quiet() -> bool {
    QUIET.get() != 0
}
pub fn quiet<T>(f: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            QUIET.set(QUIET.get() - 1)
        }
    }
    QUIET.set(QUIET.get() + 1);
    let _reset = Reset;
    f()
}
thread_local! {static DRAG_SIDE:Cell<bool>=const{Cell::new(false)};}
thread_local! {static DRAG_PREVIEW:Cell<Option<i32>>=const{Cell::new(None)};}
thread_local! {static SIZING:Cell<bool>=const{Cell::new(false)};}
pub(crate) fn divider_dragging() -> bool {
    DRAG_SIDE.get() || DRAG_PREVIEW.get().is_some() || crate::sidebar_grip::captured()
}
const APPLY_DIVIDER: u32 = WM_APP + 168;
pub(crate) const WORK_READY: u32 = WM_APP + 181;
thread_local! { static PENDING_DIVIDER: Cell<Option<(bool, i32)>> = const { Cell::new(None) }; }
fn apply_divider() {
    if let Some((sidebar, x)) = PENDING_DIVIDER.take() {
        push(if sidebar {
            Event::SideWidth(x, false)
        } else {
            Event::PreviewWidth(x, false)
        });
    }
}
pub enum Event {
    Activated,
    SideWidth(i32, bool),
    PreviewWidth(i32, bool),
    Size,
    ScaleChanged,
    Close,
    MapJump(u32),
    MapReady,
    /// The wheel turned over the document map: the message's two parameters.
    MapWheel(usize, isize),
    Hit(frame::Hit, i32, i32),
    SideOpen,
    Scroll(usize, u32, i32),
    Timer,
    WorkReady,
    MacroTick,
    Drop(Vec<PathBuf>),
    Launch(Vec<crate::launch::Target>),
    Notify(usize, u64, Notification, Vec<u8>),
    Edit(usize, Binding, u64, u64, Vec<u8>),
    /// Pane, document, position, bytes deleted, bytes added, and Scintilla's
    /// modification flags (who made the change: the user, undo or redo).
    Modified(usize, u64, u64, u64, u64, i32),
    Macro(Step),
    Context(i32, i32, H),
    /// Something the reader did in the document preview pane.
    Preview(crate::preview::Signal),
}
pub fn push(e: Event) {
    EVENTS.with_borrow_mut(|q| match (q.back_mut(), e) {
        (Some(Event::Size), Event::Size) => {}
        (Some(last @ Event::SideWidth(_, false)), next @ Event::SideWidth(..)) => *last = next,
        (Some(last @ Event::PreviewWidth(_, false)), next @ Event::PreviewWidth(..)) => *last = next,
        (_, next) => q.push_back(next),
    })
}
pub fn pop() -> Option<Event> {
    EVENTS.with_borrow_mut(|q| q.pop_front())
}
// A document with no pane can still receive a queued Undo through the hidden editor.
// Collect its native edit spans so bookmarks follow grouped, disjoint changes too.
type EditSpans = Vec<(u64, u64, u64)>;
thread_local! {static BRIDGE_HISTORY: RefCell<Option<EditSpans>> = const { RefCell::new(None) };}
pub fn bridge_history(f: impl FnOnce()) -> EditSpans {
    BRIDGE_HISTORY.replace(Some(Vec::new()));
    f();
    BRIDGE_HISTORY.take().unwrap_or_default()
}
pub unsafe extern "system" fn proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    unsafe {
        if let Some(result) = crate::npp::message(h, m, w, l) {
            return result;
        }
        let host = h as H;
        if crate::sidebar_grip::host_message(host, m, w, l) {
            return 0;
        }
        if let Some(result) = crate::chrome::message(h, m, w, l) {
            return result;
        }
        match m {
            WM_PAINT => {
                frame::paint(host);
                0
            }
            WM_ERASEBKGND => 1,
            frame::rendering::PRESENT => {
                frame::rendering::present_pending(host);
                0
            }
            WM_ENTERSIZEMOVE => { SIZING.set(true); 0 }
            WM_EXITSIZEMOVE => { SIZING.set(false); push(Event::Size); 0 }
            WM_SIZE => {
                if w != SIZE_MINIMIZED as usize && !(SIZING.get() && crate::npp::live_resize(host)) {
                    push(Event::Size);
                }
                0
            }
            WM_CLOSE => {
                push(Event::Close);
                0
            }
            WM_DESTROY => {
                crate::sidebar_grip::shutdown();
                frame::controls::release();
                crate::launch::unregister_window(h);
                PostQuitMessage(0);
                0
            }
            WM_ACTIVATE if (w & 0xffff) != WA_INACTIVE as usize => {
                push(Event::Activated);
                DefWindowProcW(h, m, w, l)
            }
            WM_TIMER => {
                push(Event::Timer);
                0
            }
            WORK_READY => {
                push(Event::WorkReady);
                0
            }
            crate::macros::TICK => {
                push(Event::MacroTick);
                0
            }
            WM_LBUTTONDOWN => {
                let x = l as i16 as i32;
                let y = (l >> 16) as i16 as i32;
                let grab = frame::FRAME.with_borrow(|f| {
                    f.sidebar
                        && (x - size(host, f.sidebar_width)).abs() <= size(host, 5)
                        && y > size(host, frame::HEAD)
                });
                if grab {
                    DRAG_SIDE.set(true);
                    windows_sys::Win32::UI::Input::KeyboardAndMouse::SetCapture(h);
                } else if let Some(offset) = crate::preview::divider_offset(host, x, y) {
                    DRAG_PREVIEW.set(Some(offset));
                    windows_sys::Win32::UI::Input::KeyboardAndMouse::SetCapture(h);
                }
                0
            }
            WM_MOUSEMOVE if DRAG_SIDE.get() || DRAG_PREVIEW.get().is_some() => {
                let x = l as i16 as i32 + DRAG_PREVIEW.get().unwrap_or(0);
                if PENDING_DIVIDER.replace(Some((DRAG_SIDE.get(), x))).is_none()
                    && PostMessageW(h, APPLY_DIVIDER, 0, 0) == 0
                {
                    apply_divider();
                }
                0
            }
            APPLY_DIVIDER => {
                apply_divider();
                0
            }
            WM_SETCURSOR if (l & 65535) as u32 == HTCLIENT => {
                let mut pt = POINT { x: 0, y: 0 };
                GetCursorPos(&mut pt);
                windows_sys::Win32::Graphics::Gdi::ScreenToClient(h, &mut pt);
                if frame::FRAME.with_borrow(|f| {
                    f.sidebar
                        && (pt.x - size(host, f.sidebar_width)).abs() <= size(host, 5)
                        && pt.y > size(host, frame::HEAD)
                }) || crate::preview::divider_offset(host, pt.x, pt.y).is_some() {
                    SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_SIZEWE));
                    1
                } else {
                    DefWindowProcW(h, m, w, l)
                }
            }
            WM_MOUSEWHEEL => {
                // Tabs and other frame controls pass the wheel up to here.
                // Over the tab strip it moves through tabs that do not fit.
                let mut pt = POINT {
                    x: l as i16 as i32,
                    y: (l >> 16) as i16 as i32,
                };
                windows_sys::Win32::Graphics::Gdi::ScreenToClient(h, &mut pt);
                let left = frame::FRAME.with_borrow(|f| {
                    size(host, if f.sidebar { f.sidebar_width } else { frame::RAIL })
                });
                let top = size(host, frame::HEAD);
                if pt.x < left || pt.y < top || pt.y >= top + size(host, frame::TABS) {
                    return DefWindowProcW(h, m, w, l);
                }
                let step = frame::Hit::ScrollTabs(if ((w >> 16) as i16) > 0 { -1 } else { 1 });
                // Only as far as the strip's own arrows would go.
                if frame::HITS.with_borrow(|hits| hits.iter().any(|(_, hit)| *hit == step)) {
                    push(Event::Hit(step, pt.x, pt.y));
                }
                0
            }
            WM_CAPTURECHANGED => {
                apply_divider();
                DRAG_SIDE.set(false);
                DRAG_PREVIEW.set(None);
                for id in [700, 701] {
                    let child = GetDlgItem(h, id);
                    if !child.is_null() {
                        SendMessageW(child, crate::editor_resize::FLUSH, 0, 0);
                    }
                }
                0
            }
            WM_LBUTTONUP => {
                let x = l as i16 as i32;
                if let Some(offset) = DRAG_PREVIEW.take() {
                    PENDING_DIVIDER.take();
                    windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
                    push(Event::PreviewWidth(x + offset, true));
                    return 0;
                }
                if DRAG_SIDE.replace(false) {
                    PENDING_DIVIDER.take();
                    windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
                    push(Event::SideWidth(x, true));
                    return 0;
                }
                let y = (l >> 16) as i16 as i32;
                if let Some(hit) = frame::hit(x, y) {
                    push(Event::Hit(hit, x, y))
                }
                0
            }
            WM_COMMAND
                if w & 65535 == 601
                    && (w >> 16 == LBN_DBLCLK as usize
                        || w >> 16 == LBN_SELCHANGE as usize
                            && frame::FRAME
                                .with_borrow(|f| f.side == 0 && f.workspace.is_empty())) =>
            {
                push(Event::SideOpen);
                0
            }
            WM_VSCROLL if l != 0 => {
                push(Event::Scroll(
                    (GetDlgCtrlID(hwnd(l)) - 710).max(0) as usize,
                    (w & 65535) as u32,
                    (w >> 16) as i32,
                ));
                0
            }
            WM_CONTEXTMENU => {
                push(Event::Context(
                    l as i16 as i32,
                    (l >> 16) as i16 as i32,
                    w as H,
                ));
                0
            }
            WM_NOTIFY if l != 0 && QUIET.get() == 0 => {
                let header = &*(l as *const linework_editor::NotifyHeader);
                if header.id_from == 702 && header.code == n::SCN_MODIFIED {
                    let nt = &*(l as *const Notification);
                    if nt.modification_type & 3 != 0 {
                        BRIDGE_HISTORY.with_borrow_mut(|spans| {
                            if let Some(spans) = spans {
                                let length = nt.length.max(0) as u64;
                                let added = nt.modification_type & 1 != 0;
                                spans.push((nt.position.max(0) as u64, if added {0} else {length}, if added {length} else {0}));
                            }
                        });
                    }
                    return 0;
                }
                if !(700..=701).contains(&header.id_from) {
                    return 0;
                }
                let pane = header.id_from - 700;
                let b = binding(pane);
                if b.id == 0 {
                    return 0;
                }
                let _native_plugins = crate::npp::after_notification(l, b);
                let mut nt = *(l as *const Notification);
                let code = nt.nmhdr.code;
                if code == n::SCN_MODIFIED {
                    if nt.modification_type & 7 != 0 {crate::sidebar_grip::editor_changed(nt.nmhdr.hwnd_from as H);}
                    if nt.modification_type & 3 == 0 {
                        return 0;
                    }
                    if b.large && nt.position >= 0 && nt.length >= 0 {
                        let add = nt.modification_type & 1 != 0;
                        let bytes = if add && !nt.text.is_null() {
                            std::slice::from_raw_parts(nt.text.cast::<u8>(), nt.length as usize)
                                .to_vec()
                        } else {
                            Vec::new()
                        };
                        push(Event::Edit(
                            pane,
                            b,
                            point(pane, nt.position as u64),
                            if add {
                                0
                            } else {
                                end_point(pane, (nt.position + nt.length) as u64)
                                    .saturating_sub(point(pane, nt.position as u64))
                            },
                            bytes,
                        ));
                    } else {
                        push(Event::Modified(
                            pane,
                            b.id,
                            nt.position.max(0) as u64,
                            if nt.modification_type & 2 != 0 {
                                nt.length.max(0) as u64
                            } else {
                                0
                            },
                            if nt.modification_type & 1 != 0 {
                                nt.length.max(0) as u64
                            } else {
                                0
                            },
                            nt.modification_type,
                        ));
                    }
                    nt.text = null();
                    nt.l_param = 0;
                    push(Event::Notify(pane, b.id, nt, vec![]));
                    return 0;
                }
                if code == n::SCN_MACRORECORD {
                    if SKIP_MACRO.get() {
                        return 0;
                    }
                    if let Some(step) = crate::macros::capture_from(
                        nt.nmhdr.hwnd_from as isize,
                        nt.message as u32,
                        nt.w_param,
                        nt.l_param,
                    ) {
                        push(Event::Macro(step))
                    }
                    return 0;
                }
                if [
                    n::SCN_UPDATEUI,
                    n::SCN_CHARADDED,
                    n::SCN_MARGINCLICK,
                    n::SCN_FOCUSIN,
                    n::SCN_AUTOCCOMPLETED,
                    n::SCN_AUTOCSELECTION,
                    n::SCN_SAVEPOINTLEFT,
                    n::SCN_SAVEPOINTREACHED,
                    n::SCN_DWELLSTART,
                ]
                .contains(&code)
                {
                    let txt = if !nt.text.is_null()
                        && (code == n::SCN_AUTOCSELECTION || code == n::SCN_AUTOCCOMPLETED)
                    {
                        CStr::from_ptr(nt.text).to_bytes_with_nul().to_vec()
                    } else {
                        Vec::new()
                    };
                    nt.text = null();
                    nt.l_param = 0;
                    push(Event::Notify(pane, b.id, nt, txt));
                }
                0
            }
            WM_COPYDATA => crate::launch::receive(l),
            WM_DROPFILES => {
                let mut paths = Vec::new();
                let drop = w as HDROP;
                let count = DragQueryFileW(drop, u32::MAX, std::ptr::null_mut(), 0);
                for i in 0..count {
                    let mut s =
                        vec![0; DragQueryFileW(drop, i, std::ptr::null_mut(), 0) as usize + 1];
                    let n = DragQueryFileW(drop, i, s.as_mut_ptr(), s.len() as u32);
                    paths.push(PathBuf::from(String::from_utf16_lossy(&s[..n as usize])));
                }
                DragFinish(drop);
                push(Event::Drop(paths));
                0
            }
            WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORSTATIC => {
                ctlcolor(w as _, LIGHT.get(), m != WM_CTLCOLORSTATIC)
            }
            WM_DPICHANGED => {
                let r = &*(l as *const RECT);
                SetWindowPos(
                    h,
                    std::ptr::null_mut(),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                push(Event::ScaleChanged);
                0
            }
            _ => DefWindowProcW(h, m, w, l),
        }
    }
}
