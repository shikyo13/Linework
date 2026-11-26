//! Windows: every HWND is an AppKit view. A child window is a view inside its
//! parent's view; a top-level window is the content view of an NSWindow. The
//! handle is the view's address, so a view made elsewhere (the Scintilla editor)
//! is a window too as soon as it is placed inside one of ours.
use crate::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use crate::Win32::Graphics::Gdi::*;
use crate::Win32::UI::Shell::SUBCLASSPROC;
use crate::Win32::UI::WindowsAndMessaging::*;
use crate::imp::gdi::{self, ns_rect, wide_name};
use crate::imp::view::{self, LwView};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, Message, msg_send, sel};
use objc2_app_kit::{NSApplication, NSBackingStoreType, NSColor, NSCursor, NSEvent, NSScreen, NSView, NSWindow, NSWindowStyleMask};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

pub(crate) type Proc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

/// Linework's main window draws its own caption, so its view covers the whole
/// window and the system's title bar is not shown.
const OWN_CAPTION: &str = "LineworkMain";

pub(crate) struct Window {
    pub(crate) view: Retained<NSView>,
    pub(crate) frame: RefCell<Option<Retained<NSWindow>>>,
    pub(crate) class: String,
    pub(crate) proc: Cell<Option<Proc>>,
    pub(crate) style: Cell<u32>,
    pub(crate) ex: Cell<u32>,
    pub(crate) id: Cell<isize>,
    pub(crate) user: Cell<isize>,
    pub(crate) owner: Cell<usize>,
    pub(crate) text: RefCell<String>,
    pub(crate) font: Cell<isize>,
    pub(crate) props: RefCell<Vec<(String, isize)>>,
    pub(crate) subclasses: RefCell<Vec<(usize, usize, usize)>>,
    /// A view we did not make, such as the editor.
    pub(crate) foreign: bool,
    /// Keys go to the view itself (the editor, a text field) once the front
    /// end has had its look at them.
    pub(crate) native_keys: Cell<bool>,
    pub(crate) enabled: Cell<bool>,
    pub(crate) own_caption: bool,
    pub(crate) control: RefCell<Option<Box<dyn std::any::Any>>>,
}

struct Class {
    proc: Option<Proc>,
}

thread_local! {
    static WINDOWS: RefCell<HashMap<usize, Rc<Window>>> = RefCell::new(HashMap::new());
    static CLASSES: RefCell<HashMap<String, Class>> = RefCell::new(HashMap::new());
    static CAPTURE: Cell<usize> = const { Cell::new(0) };
    /// The window being painted and the part that needs it.
    static PAINTING: Cell<(usize, RECT)> = const { Cell::new((0, RECT { left: 0, top: 0, right: 0, bottom: 0 })) };
    /// Which subclass procedure of which window is running, innermost last.
    static RUNNING: RefCell<Vec<(usize, usize)>> = const { RefCell::new(Vec::new()) };
    static FOCUSING: Cell<bool> = const { Cell::new(false) };
    /// The window of ours last made active or given the keys. macOS has no key
    /// window while another program is in front, and the front end still has
    /// one in mind.
    static ACTIVE: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
}

pub(crate) fn mtm() -> MainThreadMarker {
    MainThreadMarker::new().expect("windows belong to the main thread")
}

pub(crate) fn app() -> Retained<NSApplication> {
    NSApplication::sharedApplication(mtm())
}

pub(crate) fn find(hwnd: HWND) -> Option<Rc<Window>> {
    WINDOWS.with_borrow(|windows| windows.get(&(hwnd as usize)).cloned())
}

pub(crate) fn handle(view: &NSView) -> HWND {
    view as *const NSView as HWND
}

/// The window a view belongs to: itself, or the nearest view around it that is one.
pub(crate) fn owner_of(view: &NSView) -> Option<Rc<Window>> {
    let mut at = Some(view.retain());
    while let Some(view) = at {
        if let Some(found) = find(handle(&view)) {
            return Some(found);
        }
        at = unsafe { view.superview() };
    }
    None
}

pub(crate) fn focusing() -> bool {
    FOCUSING.get()
}

fn register(view: Retained<NSView>, class: &str, proc: Option<Proc>, style: u32, ex: u32, id: isize, foreign: bool) -> Rc<Window> {
    let window = Rc::new(Window {
        frame: RefCell::new(None),
        class: class.to_owned(),
        proc: Cell::new(proc),
        style: Cell::new(style),
        ex: Cell::new(ex),
        id: Cell::new(id),
        user: Cell::new(0),
        owner: Cell::new(0),
        text: RefCell::new(String::new()),
        font: Cell::new(0),
        props: RefCell::new(Vec::new()),
        subclasses: RefCell::new(Vec::new()),
        foreign,
        native_keys: Cell::new(foreign),
        enabled: Cell::new(true),
        own_caption: class == OWN_CAPTION,
        control: RefCell::new(None),
        view,
    });
    WINDOWS.with_borrow_mut(|windows| windows.insert(handle(&window.view) as usize, window.clone()));
    window
}

/// The editor's view, placed in one of our windows by the editor crate, becomes a window.
pub(crate) fn adopt(view: &NSView) {
    if find(handle(view)).is_some() {
        return;
    }
    // Only the editor: the views AppKit puts inside our own controls are not windows.
    let scintilla: bool = unsafe { msg_send![view, respondsToSelector: sel!(message:wParam:lParam:)] };
    if scintilla {
        register(view.retain(), "Scintilla", Some(foreign_proc), WS_CHILD | WS_VISIBLE, 0, 0, true);
    }
}

pub(crate) fn forget(view: &NSView) {
    WINDOWS.with_borrow_mut(|windows| windows.remove(&(handle(view) as usize)));
}

/// What a view of someone else's does with a message: the editor takes its own
/// messages, and nothing else means anything to it.
unsafe extern "system" fn foreign_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let Some(window) = find(hwnd) else { return 0 };
    if window.class == "Scintilla" && (2000..5000).contains(&message) {
        // Text sizes are in points, and a point is 96/72 of a pixel on Windows
        // but one pixel here; the editor crate's macOS file does the same for
        // the messages it sends itself.
        let (message, l, back) = match message {
            2055 => (2061, l * 400 / 3, 1),
            2061 => (2061, l * 4 / 3, 1),
            2373 => (2373, 0, 1),
            2485 | 2062 | 2374 => (message, l, 2),
            _ => (message, l, 0),
        };
        let w = if message == 2373 { ((w as isize * 4 + 1) / 3) as usize } else { w };
        let result: isize = unsafe { msg_send![&*window.view, message: message, wParam: w, lParam: l] };
        return if back == 2 { result * 3 / 4 } else { result };
    }
    unsafe { DefWindowProcW(hwnd, message, w, l) }
}

// --- Sending ------------------------------------------------------------

pub(crate) fn send(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let Some(window) = find(hwnd) else { return 0 };
    let top = window.subclasses.borrow().len();
    call_level(&window, hwnd, top, message, w, l)
}

/// Runs the procedure at `level`: a subclass procedure when above zero, the
/// window's own at zero.
fn call_level(window: &Rc<Window>, hwnd: HWND, level: usize, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if level == 0 {
        return match window.proc.get() {
            Some(proc) => unsafe { proc(hwnd, message, w, l) },
            None => unsafe { DefWindowProcW(hwnd, message, w, l) },
        };
    }
    let Some(&(proc, id, data)) = window.subclasses.borrow().get(level - 1) else { return 0 };
    let proc: SUBCLASSPROC = unsafe { std::mem::transmute(proc) };
    RUNNING.with_borrow_mut(|running| running.push((hwnd as usize, level)));
    let result = match proc {
        Some(proc) => unsafe { proc(hwnd, message, w, l, id, data) },
        None => 0,
    };
    RUNNING.with_borrow_mut(|running| running.pop());
    result
}

pub unsafe fn SendMessageW(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    send(hwnd, msg, wparam, lparam)
}

pub unsafe fn DefSubclassProc(hwnd: HWND, umsg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(window) = find(hwnd) else { return 0 };
    let level = RUNNING.with_borrow(|running| running.iter().rev().find(|entry| entry.0 == hwnd as usize).map_or(1, |entry| entry.1));
    call_level(&window, hwnd, level - 1, umsg, wparam, lparam)
}

pub unsafe fn SetWindowSubclass(hwnd: HWND, pfnsubclass: SUBCLASSPROC, uidsubclass: usize, dwrefdata: usize) -> i32 {
    let Some(window) = find(hwnd) else { return 0 };
    let proc: usize = unsafe { std::mem::transmute(pfnsubclass) };
    let mut list = window.subclasses.borrow_mut();
    match list.iter_mut().find(|entry| entry.0 == proc && entry.1 == uidsubclass) {
        Some(entry) => entry.2 = dwrefdata,
        None => list.push((proc, uidsubclass, dwrefdata)),
    }
    1
}

pub unsafe fn RemoveWindowSubclass(hwnd: HWND, pfnsubclass: SUBCLASSPROC, uidsubclass: usize) -> i32 {
    let Some(window) = find(hwnd) else { return 0 };
    let proc: usize = unsafe { std::mem::transmute(pfnsubclass) };
    let mut list = window.subclasses.borrow_mut();
    let before = list.len();
    list.retain(|entry| !(entry.0 == proc && entry.1 == uidsubclass));
    (list.len() != before) as i32
}

pub unsafe fn GetWindowSubclass(hwnd: HWND, pfnsubclass: SUBCLASSPROC, uidsubclass: usize, pdwrefdata: *mut usize) -> i32 {
    let Some(window) = find(hwnd) else { return 0 };
    let proc: usize = unsafe { std::mem::transmute(pfnsubclass) };
    let found = window.subclasses.borrow().iter().find(|entry| entry.0 == proc && entry.1 == uidsubclass).map(|entry| entry.2);
    match found {
        Some(data) => {
            if let Some(out) = unsafe { pdwrefdata.as_mut() } {
                *out = data;
            }
            1
        }
        None => 0,
    }
}

pub unsafe fn DefWindowProcW(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(window) = find(hwnd) else { return 0 };
    match msg {
        WM_CLOSE => {
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_SETTEXT => {
            let text = unsafe { wide_name(lparam as *const u16) };
            if let Some(frame) = &*window.frame.borrow() {
                frame.setTitle(&NSString::from_str(&text));
            }
            window.text.replace(text);
            1
        }
        WM_GETTEXTLENGTH => window.text.borrow().encode_utf16().count() as isize,
        WM_GETTEXT => {
            let text: Vec<u16> = window.text.borrow().encode_utf16().collect();
            let room = (wparam as usize).saturating_sub(1).min(text.len());
            if wparam > 0 && lparam != 0 {
                let out = lparam as *mut u16;
                unsafe {
                    std::ptr::copy_nonoverlapping(text.as_ptr(), out, room);
                    *out.add(room) = 0;
                }
            }
            room as isize
        }
        WM_SETFONT => {
            window.font.set(wparam as isize);
            0
        }
        WM_GETFONT => window.font.get(),
        WM_NCHITTEST => HTCLIENT as isize,
        WM_SETCURSOR => {
            NSCursor::arrowCursor().set();
            0
        }
        WM_SYSCOMMAND => {
            if let Some(frame) = window.frame.borrow().clone() {
                match (wparam & 0xfff0) as u32 {
                    SC_CLOSE => return send(hwnd, WM_CLOSE, 0, 0),
                    SC_MINIMIZE => frame.miniaturize(None),
                    SC_MAXIMIZE if !frame.isZoomed() => frame.zoom(None),
                    SC_RESTORE if frame.isMiniaturized() => frame.deminiaturize(None),
                    SC_RESTORE if frame.isZoomed() => frame.zoom(None),
                    _ => {}
                }
            }
            0
        }
        WM_RBUTTONUP => {
            let mut point = POINT { x: lparam as i16 as i32, y: (lparam >> 16) as i16 as i32 };
            unsafe { ClientToScreen(hwnd, &mut point) };
            send(hwnd, WM_CONTEXTMENU, hwnd as usize, pack(point.x, point.y))
        }
        // What a child does not want goes to the window around it.
        WM_CONTEXTMENU | WM_MOUSEWHEEL if window.style.get() & WS_CHILD != 0 => {
            let parent = unsafe { GetParent(hwnd) };
            if parent.is_null() { 0 } else { send(parent, msg, wparam, lparam) }
        }
        _ => 0,
    }
}

pub(crate) fn pack(x: i32, y: i32) -> isize {
    ((x as u16 as u32) | ((y as u16 as u32) << 16)) as i32 as isize
}

// --- Making and ending --------------------------------------------------

pub unsafe fn RegisterClassExW(param0: *const WNDCLASSEXW) -> u16 {
    let Some(class) = (unsafe { param0.as_ref() }) else { return 0 };
    let name = unsafe { wide_name(class.lpszClassName) };
    CLASSES.with_borrow_mut(|classes| classes.insert(name, Class { proc: class.lpfnWndProc }));
    1
}

#[allow(clippy::too_many_arguments)]
pub unsafe fn CreateWindowExW(dwexstyle: WINDOW_EX_STYLE, lpclassname: *const u16, lpwindowname: *const u16, dwstyle: WINDOW_STYLE, x: i32, y: i32, nwidth: i32, nheight: i32, hwndparent: HWND, hmenu: HMENU, _: crate::Win32::Foundation::HINSTANCE, lpparam: *const core::ffi::c_void) -> HWND {
    let class = unsafe { wide_name(lpclassname) };
    let title = unsafe { wide_name(lpwindowname) };
    let proc = CLASSES
        .with_borrow(|classes| classes.get(&class).map(|class| class.proc))
        .unwrap_or_else(|| crate::imp::controls::proc_of(&class));
    let mtm = mtm();
    let child = dwstyle & WS_CHILD != 0;
    let (width, height) = (nwidth.max(0) as f64, nheight.max(0) as f64);
    let id = if child { hmenu as isize } else { 0 };
    // An editor asked for by class name is Scintilla's own view, held with
    // the object it reports to.
    let editor = if class == "Scintilla" && child { crate::imp::scintilla::create(mtm, id) } else { None };
    let (view, window) = match editor {
        Some((view, hearing)) => {
            let window = register(view.clone(), "Scintilla", Some(foreign_proc), dwstyle, dwexstyle, id, true);
            window.control.replace(Some(Box::new(hearing)));
            (view, window)
        }
        None => {
            let view: Retained<NSView> = Retained::into_super(LwView::new(mtm));
            let window = register(view.clone(), &class, proc, dwstyle, dwexstyle, id, false);
            (view, window)
        }
    };
    window.text.replace(title.clone());
    if child {
        view.setFrame(NSRect::new(NSPoint::new(x as f64, y as f64), NSSize::new(width, height)));
        view.setHidden(dwstyle & WS_VISIBLE == 0);
        if let Some(parent) = find(hwndparent) {
            unsafe { parent.view.addSubview(&view) };
        }
    } else {
        window.owner.set(hwndparent as usize);
        let popup = dwstyle & WS_POPUP != 0 && dwstyle & WS_CAPTION != WS_CAPTION;
        let mask = if popup {
            NSWindowStyleMask::Borderless
        } else if window.own_caption {
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable | NSWindowStyleMask::Resizable | NSWindowStyleMask::FullSizeContentView
        } else {
            let mut mask = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
            if dwstyle & WS_THICKFRAME != 0 {
                mask |= NSWindowStyleMask::Resizable;
            }
            if dwstyle & WS_MINIMIZEBOX != 0 {
                mask |= NSWindowStyleMask::Miniaturizable;
            }
            mask
        };
        let content = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width.max(1.0), height.max(1.0)));
        let frame = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), content, mask, NSBackingStoreType::Buffered, false)
        };
        unsafe { frame.setReleasedWhenClosed(false) };
        frame.setTitle(&NSString::from_str(&title));
        frame.setAcceptsMouseMovedEvents(true);
        if window.own_caption {
            frame.setTitlebarAppearsTransparent(true);
            frame.setTitleVisibility(objc2_app_kit::NSWindowTitleVisibility::Hidden);
            for button in [objc2_app_kit::NSWindowButton::CloseButton, objc2_app_kit::NSWindowButton::MiniaturizeButton, objc2_app_kit::NSWindowButton::ZoomButton] {
                if let Some(button) = frame.standardWindowButton(button) {
                    button.setHidden(true);
                }
            }
        }
        if popup {
            frame.setOpaque(false);
            frame.setBackgroundColor(Some(&NSColor::clearColor()));
            frame.setHasShadow(dwexstyle & WS_EX_TRANSPARENT == 0);
            frame.setIgnoresMouseEvents(dwexstyle & WS_EX_TRANSPARENT != 0);
            if dwexstyle & WS_EX_TOPMOST != 0 {
                frame.setLevel(3);
            }
        }
        frame.setContentView(Some(&view));
        view::watch(&frame, mtm);
        if dwexstyle & WS_EX_ACCEPTFILES != 0 {
            view.registerForDraggedTypes(&objc2_foundation::NSArray::from_slice(&[&*NSString::from_str("NSFilenamesPboardType")]));
        }
        if x == CW_USEDEFAULT || popup && nwidth == 0 {
            frame.center();
        } else {
            place_frame(&window, &frame, x, y, width, height);
        }
        window.frame.replace(Some(frame));
    }
    let hwnd = handle(&view);
    let created = CREATESTRUCTW {
        lpCreateParams: lpparam as *mut _,
        hInstance: std::ptr::null_mut(),
        hMenu: hmenu,
        hwndParent: hwndparent,
        cy: nheight,
        cx: nwidth,
        y,
        x,
        style: dwstyle as i32,
        lpszName: lpwindowname,
        lpszClass: lpclassname,
        dwExStyle: dwexstyle,
    };
    send(hwnd, WM_NCCREATE, 0, &created as *const _ as isize);
    if send(hwnd, WM_CREATE, 0, &created as *const _ as isize) == -1 {
        unsafe { DestroyWindow(hwnd) };
        return std::ptr::null_mut();
    }
    hwnd
}

pub(crate) fn children(window: &Window) -> Vec<Rc<Window>> {
    let views = unsafe { window.view.subviews() };
    views.iter().filter_map(|view| find(handle(&view))).collect()
}

pub unsafe fn DestroyWindow(hwnd: HWND) -> i32 {
    let Some(window) = find(hwnd) else { return 0 };
    if CAPTURE.get() == hwnd as usize {
        CAPTURE.set(0);
    }
    crate::imp::message::forget_window(hwnd);
    send(hwnd, WM_DESTROY, 0, 0);
    for child in children(&window) {
        // An editor from the editor crate is taken away by its owner; one made
        // here by class name goes with its parent.
        if !child.foreign || child.control.borrow().is_some() {
            unsafe { DestroyWindow(handle(&child.view)) };
        }
    }
    send(hwnd, WM_NCDESTROY, 0, 0);
    WINDOWS.with_borrow_mut(|windows| windows.remove(&(hwnd as usize)));
    match window.frame.take() {
        Some(frame) => {
            frame.setDelegate(None);
            frame.orderOut(None);
            frame.close();
        }
        None => unsafe { window.view.removeFromSuperview() },
    }
    1
}

pub unsafe fn IsWindow(hwnd: HWND) -> i32 {
    find(hwnd).is_some() as i32
}

// --- Place and size -----------------------------------------------------

fn screen_height() -> f64 {
    NSScreen::screens(mtm()).firstObject().map_or(0.0, |screen| screen.frame().size.height)
}

/// What Windows adds around the client area of a window with a caption, on the
/// left, top, right and bottom. The front end asks for a window's whole size,
/// so this much is taken off to leave the client area Windows would leave.
fn edges(window: &Window) -> (i32, i32, i32, i32) {
    let captioned = window.style.get() & WS_CAPTION == WS_CAPTION && window.style.get() & WS_CHILD == 0;
    if captioned && !window.own_caption { (8, 31, 8, 8) } else { (0, 0, 0, 0) }
}

/// Windows counts a screen position from the top left of the main screen.
fn place_frame(window: &Window, frame: &NSWindow, x: i32, y: i32, width: f64, height: f64) {
    let (left, top, right, bottom) = edges(window);
    let (width, height) = ((width - (left + right) as f64).max(1.0), (height - (top + bottom) as f64).max(1.0));
    let content = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height));
    let outer = frame.frameRectForContentRect(content);
    let top = screen_height() - y as f64;
    frame.setFrame_display(NSRect::new(NSPoint::new(x as f64, top - outer.size.height), outer.size), true);
}

/// A window's rectangle on the screen, in Windows' terms.
fn screen_rect(window: &Window) -> RECT {
    let bounds = window.view.bounds();
    let in_window = window.view.convertRect_toView(bounds, None);
    let Some(frame) = window.view.window() else {
        return RECT { left: 0, top: 0, right: bounds.size.width as i32, bottom: bounds.size.height as i32 };
    };
    let on_screen = frame.convertRectToScreen(in_window);
    let top = screen_height() - (on_screen.origin.y + on_screen.size.height);
    RECT {
        left: on_screen.origin.x.round() as i32,
        top: top.round() as i32,
        right: (on_screen.origin.x + on_screen.size.width).round() as i32,
        bottom: (top + on_screen.size.height).round() as i32,
    }
}

pub unsafe fn GetClientRect(hwnd: HWND, lprect: *mut RECT) -> i32 {
    let (Some(window), Some(out)) = (find(hwnd), unsafe { lprect.as_mut() }) else { return 0 };
    let size = window.view.bounds().size;
    *out = RECT { left: 0, top: 0, right: size.width as i32, bottom: size.height as i32 };
    1
}

pub unsafe fn GetWindowRect(hwnd: HWND, lprect: *mut RECT) -> i32 {
    let (Some(window), Some(out)) = (find(hwnd), unsafe { lprect.as_mut() }) else { return 0 };
    let (left, top, right, bottom) = edges(&window);
    let inner = screen_rect(&window);
    *out = RECT { left: inner.left - left, top: inner.top - top, right: inner.right + right, bottom: inner.bottom + bottom };
    1
}

fn origin_on_screen(hwnd: HWND) -> (i32, i32) {
    match find(hwnd) {
        Some(window) => {
            let r = screen_rect(&window);
            (r.left, r.top)
        }
        None => (0, 0),
    }
}

pub unsafe fn MapWindowPoints(hwndfrom: HWND, hwndto: HWND, lppoints: *mut POINT, cpoints: u32) -> i32 {
    let (from, to) = (origin_on_screen(hwndfrom), origin_on_screen(hwndto));
    let (dx, dy) = (from.0 - to.0, from.1 - to.1);
    for point in unsafe { std::slice::from_raw_parts_mut(lppoints, cpoints as usize) } {
        point.x += dx;
        point.y += dy;
    }
    ((dx as u16 as u32) | ((dy as u16 as u32) << 16)) as i32
}

pub unsafe fn ScreenToClient(hwnd: HWND, lppoint: *mut POINT) -> i32 {
    unsafe { MapWindowPoints(std::ptr::null_mut(), hwnd, lppoint, 1) };
    1
}

pub unsafe fn ClientToScreen(hwnd: HWND, lppoint: *mut POINT) -> i32 {
    unsafe { MapWindowPoints(hwnd, std::ptr::null_mut(), lppoint, 1) };
    1
}

pub unsafe fn GetCursorPos(lppoint: *mut POINT) -> i32 {
    let Some(out) = (unsafe { lppoint.as_mut() }) else { return 0 };
    let at = NSEvent::mouseLocation();
    *out = POINT { x: at.x.round() as i32, y: (screen_height() - at.y).round() as i32 };
    1
}

pub(crate) fn set_visible(window: &Window, visible: bool, activate: bool) {
    let style = window.style.get();
    window.style.set(if visible { style | WS_VISIBLE } else { style & !WS_VISIBLE });
    match window.frame.borrow().clone() {
        Some(frame) if !visible => frame.orderOut(None),
        Some(frame) if activate => {
            activated(&frame);
            frame.makeKeyAndOrderFront(None);
        }
        Some(frame) => frame.orderFront(None),
        None => window.view.setHidden(!visible),
    }
}

pub unsafe fn SetWindowPos(hwnd: HWND, _: HWND, x: i32, y: i32, cx: i32, cy: i32, uflags: SET_WINDOW_POS_FLAGS) -> i32 {
    let Some(window) = find(hwnd) else { return 0 };
    let (moves, sizes) = (uflags & SWP_NOMOVE == 0, uflags & SWP_NOSIZE == 0);
    if moves || sizes {
        match window.frame.borrow().clone() {
            Some(frame) => {
                let mut now = RECT { left: 0, top: 0, right: 0, bottom: 0 };
                unsafe { GetWindowRect(hwnd, &mut now) };
                let (x, y) = if moves { (x, y) } else { (now.left, now.top) };
                let (cx, cy) = if sizes { (cx, cy) } else { (now.right - now.left, now.bottom - now.top) };
                place_frame(&window, &frame, x, y, cx.max(1) as f64, cy.max(1) as f64);
            }
            None => {
                let mut bounds = window.view.frame();
                if moves {
                    bounds.origin = NSPoint::new(x as f64, y as f64);
                }
                if sizes {
                    bounds.size = NSSize::new(cx.max(0) as f64, cy.max(0) as f64);
                }
                window.view.setFrame(bounds);
                if window.foreign {
                    // Our own views report a new size themselves.
                    send(hwnd, WM_SIZE, 0, pack(bounds.size.width as i32, bounds.size.height as i32));
                }
            }
        }
    }
    if uflags & SWP_SHOWWINDOW != 0 {
        set_visible(&window, true, uflags & SWP_NOACTIVATE == 0);
    }
    if uflags & SWP_HIDEWINDOW != 0 {
        set_visible(&window, false, false);
    }
    1
}

// A batch of moves is applied one by one: AppKit shows them together anyway,
// at the end of the event being handled.
pub unsafe fn BeginDeferWindowPos(_: i32) -> HDWP {
    1 as HDWP
}

#[allow(clippy::too_many_arguments)]
pub unsafe fn DeferWindowPos(hwinposinfo: HDWP, hwnd: HWND, hwndinsertafter: HWND, x: i32, y: i32, cx: i32, cy: i32, uflags: SET_WINDOW_POS_FLAGS) -> HDWP {
    unsafe { SetWindowPos(hwnd, hwndinsertafter, x, y, cx, cy, uflags) };
    hwinposinfo
}

pub unsafe fn EndDeferWindowPos(_: HDWP) -> i32 {
    1
}

pub unsafe fn ShowWindow(hwnd: HWND, ncmdshow: SHOW_WINDOW_CMD) -> i32 {
    let Some(window) = find(hwnd) else { return 0 };
    let was = window.style.get() & WS_VISIBLE != 0;
    let frame = window.frame.borrow().clone();
    match ncmdshow {
        SW_HIDE => set_visible(&window, false, false),
        SW_MINIMIZE | SW_SHOWMINIMIZED => {
            if let Some(frame) = frame {
                frame.miniaturize(None);
            }
        }
        SW_MAXIMIZE => {
            set_visible(&window, true, true);
            if let Some(frame) = frame.filter(|frame| !frame.isZoomed()) {
                frame.zoom(None);
            }
        }
        SW_RESTORE => {
            set_visible(&window, true, true);
            if let Some(frame) = frame {
                if frame.isMiniaturized() {
                    frame.deminiaturize(None);
                } else if frame.isZoomed() {
                    frame.zoom(None);
                }
            }
        }
        SW_SHOWNA | SW_SHOWNOACTIVATE => set_visible(&window, true, false),
        _ => set_visible(&window, true, true),
    }
    was as i32
}

pub unsafe fn ShowWindowAsync(hwnd: HWND, ncmdshow: SHOW_WINDOW_CMD) -> i32 {
    unsafe { ShowWindow(hwnd, ncmdshow) }
}

pub unsafe fn IsWindowVisible(hwnd: HWND) -> i32 {
    let Some(window) = find(hwnd) else { return 0 };
    let shown = match &*window.frame.borrow() {
        Some(frame) => frame.isVisible(),
        None => !window.view.isHiddenOrHasHiddenAncestor() && window.view.window().is_some_and(|frame| frame.isVisible()),
    };
    shown as i32
}

pub unsafe fn IsZoomed(hwnd: HWND) -> i32 {
    find(hwnd).and_then(|window| window.frame.borrow().clone()).is_some_and(|frame| frame.isZoomed()) as i32
}

pub unsafe fn IsIconic(hwnd: HWND) -> i32 {
    find(hwnd).and_then(|window| window.frame.borrow().clone()).is_some_and(|frame| frame.isMiniaturized()) as i32
}

// --- Family -------------------------------------------------------------

pub unsafe fn GetParent(hwnd: HWND) -> HWND {
    let Some(window) = find(hwnd) else { return std::ptr::null_mut() };
    if window.frame.borrow().is_some() {
        return window.owner.get() as HWND;
    }
    match unsafe { window.view.superview() }.and_then(|view| owner_of(&view)) {
        Some(parent) => handle(&parent.view),
        None => std::ptr::null_mut(),
    }
}

pub(crate) fn control_id(window: &Window) -> isize {
    if window.class == "Scintilla" {
        // The editor keeps its own identifier.
        return unsafe { msg_send![&*window.view, message: 2623u32, wParam: 0usize, lParam: 0isize] };
    }
    window.id.get()
}

pub unsafe fn GetDlgCtrlID(hwnd: HWND) -> i32 {
    find(hwnd).map_or(0, |window| control_id(&window) as i32)
}

pub unsafe fn GetDlgItem(hdlg: HWND, niddlgitem: i32) -> HWND {
    let Some(window) = find(hdlg) else { return std::ptr::null_mut() };
    children(&window).iter().find(|child| control_id(child) == niddlgitem as isize).map_or(std::ptr::null_mut(), |child| handle(&child.view))
}

pub unsafe fn IsChild(hwndparent: HWND, hwnd: HWND) -> i32 {
    let mut at = unsafe { GetParent(hwnd) };
    while !at.is_null() {
        if at == hwndparent {
            return 1;
        }
        if find(at).is_none_or(|window| window.frame.borrow().is_some()) {
            break;
        }
        at = unsafe { GetParent(at) };
    }
    0
}

pub unsafe fn EnableWindow(hwnd: HWND, benable: i32) -> i32 {
    let Some(window) = find(hwnd) else { return 0 };
    let was_disabled = !window.enabled.replace(benable != 0);
    send(hwnd, WM_ENABLE, (benable != 0) as usize, 0);
    window.view.setNeedsDisplay(true);
    was_disabled as i32
}

// --- Numbers and words kept with a window -------------------------------

pub unsafe fn GetWindowLongW(hwnd: HWND, nindex: WINDOW_LONG_PTR_INDEX) -> i32 {
    unsafe { GetWindowLongPtrW(hwnd, nindex) as i32 }
}

pub unsafe fn GetWindowLongPtrW(hwnd: HWND, nindex: WINDOW_LONG_PTR_INDEX) -> isize {
    let Some(window) = find(hwnd) else { return 0 };
    match nindex {
        GWL_STYLE => {
            let style = window.style.get();
            // A view of someone else's is shown or hidden behind our back.
            let shown = if window.frame.borrow().is_some() { style & WS_VISIBLE != 0 } else { !window.view.isHidden() };
            (if shown { style | WS_VISIBLE } else { style & !WS_VISIBLE }) as i32 as isize
        }
        GWL_EXSTYLE => window.ex.get() as i32 as isize,
        GWLP_ID => control_id(&window),
        GWLP_USERDATA => window.user.get(),
        GWLP_WNDPROC => window.proc.get().map_or(0, |proc| proc as usize as isize),
        _ => 0,
    }
}

pub unsafe fn SetWindowLongPtrW(hwnd: HWND, nindex: WINDOW_LONG_PTR_INDEX, dwnewlong: isize) -> isize {
    let Some(window) = find(hwnd) else { return 0 };
    match nindex {
        GWL_STYLE => window.style.replace(dwnewlong as u32) as i32 as isize,
        GWL_EXSTYLE => window.ex.replace(dwnewlong as u32) as i32 as isize,
        GWLP_ID => window.id.replace(dwnewlong),
        GWLP_USERDATA => window.user.replace(dwnewlong),
        GWLP_WNDPROC => {
            let new: Option<Proc> = unsafe { std::mem::transmute(dwnewlong) };
            window.proc.replace(new).map_or(0, |proc| proc as usize as isize)
        }
        _ => 0,
    }
}

pub unsafe fn SetPropW(hwnd: HWND, lpstring: *const u16, hdata: crate::Win32::Foundation::HANDLE) -> i32 {
    let Some(window) = find(hwnd) else { return 0 };
    let name = unsafe { wide_name(lpstring) };
    if window.own_caption {
        crate::imp::remote::marked(&name, true);
    }
    let mut props = window.props.borrow_mut();
    props.retain(|entry| entry.0 != name);
    props.push((name, hdata as isize));
    1
}

pub unsafe fn GetPropW(hwnd: HWND, lpstring: *const u16) -> crate::Win32::Foundation::HANDLE {
    let name = unsafe { wide_name(lpstring) };
    if let Some(other) = crate::imp::remote::window_of(hwnd) {
        return crate::imp::remote::has_mark(other, &name) as usize as _;
    }
    let Some(window) = find(hwnd) else { return std::ptr::null_mut() };
    window.props.borrow().iter().find(|entry| entry.0 == name).map_or(0, |entry| entry.1) as _
}

pub unsafe fn RemovePropW(hwnd: HWND, lpstring: *const u16) -> crate::Win32::Foundation::HANDLE {
    let Some(window) = find(hwnd) else { return std::ptr::null_mut() };
    let name = unsafe { wide_name(lpstring) };
    if window.own_caption {
        crate::imp::remote::marked(&name, false);
    }
    let mut props = window.props.borrow_mut();
    let found = props.iter().position(|entry| entry.0 == name).map(|at| props.remove(at).1);
    found.unwrap_or(0) as _
}

pub unsafe fn SetWindowTextW(hwnd: HWND, lpstring: *const u16) -> i32 {
    send(hwnd, WM_SETTEXT, 0, lpstring as isize) as i32
}

pub unsafe fn GetWindowTextLengthW(hwnd: HWND) -> i32 {
    send(hwnd, WM_GETTEXTLENGTH, 0, 0) as i32
}

pub unsafe fn GetWindowTextW(hwnd: HWND, lpstring: *mut u16, nmaxcount: i32) -> i32 {
    send(hwnd, WM_GETTEXT, nmaxcount.max(0) as usize, lpstring as isize) as i32
}

// --- Keyboard focus and the pointer -------------------------------------

/// The view that takes the keys for a window: the editor's text area inside
/// the editor's view, a control's own text field, or the view itself.
fn key_view(window: &Window) -> Retained<NSView> {
    if window.class == "Scintilla" {
        let content: Option<Retained<NSView>> = unsafe { msg_send![&*window.view, content] };
        if let Some(content) = content {
            return content;
        }
    }
    crate::imp::controls::key_view(window).unwrap_or_else(|| window.view.clone())
}

pub unsafe fn SetFocus(hwnd: HWND) -> HWND {
    let previous = unsafe { GetFocus() };
    let Some(window) = find(hwnd) else { return previous };
    if let Some(frame) = window.view.window() {
        activated(&frame);
        FOCUSING.set(true);
        frame.makeFirstResponder(Some(&key_view(&window)));
        FOCUSING.set(false);
        crate::imp::controls::took_keys(&window);
    }
    previous
}

/// Notes the window the front end is working in.
pub(crate) fn activated(frame: &NSWindow) {
    ACTIVE.replace(Some(frame.retain()));
}

/// The window that has the keys in `frame`.
pub(crate) fn focus_in(frame: &NSWindow) -> HWND {
    let Some(responder) = frame.firstResponder() else { return std::ptr::null_mut() };
    let is_view: bool = unsafe { msg_send![&*responder, isKindOfClass: objc2::class!(NSView)] };
    if !is_view {
        return std::ptr::null_mut();
    }
    let view: &NSView = unsafe { &*(Retained::as_ptr(&responder) as *const NSView) };
    owner_of(view).map_or(std::ptr::null_mut(), |window| handle(&window.view))
}

pub unsafe fn GetFocus() -> HWND {
    let ours = ACTIVE.with_borrow(|active| active.clone().filter(|frame| frame.isVisible()));
    let Some(frame) = app().keyWindow().or(ours).or_else(|| app().mainWindow()) else { return std::ptr::null_mut() };
    let Some(responder) = frame.firstResponder() else { return std::ptr::null_mut() };
    let is_view: bool = unsafe { msg_send![&*responder, isKindOfClass: objc2::class!(NSView)] };
    if !is_view {
        return std::ptr::null_mut();
    }
    let view: &NSView = unsafe { &*(Retained::as_ptr(&responder) as *const NSView) };
    owner_of(view).map_or(std::ptr::null_mut(), |window| handle(&window.view))
}

pub unsafe fn SetActiveWindow(hwnd: HWND) -> HWND {
    if let Some(frame) = find(hwnd).and_then(|window| window.frame.borrow().clone()) {
        frame.makeKeyAndOrderFront(None);
    }
    hwnd
}

pub unsafe fn SetForegroundWindow(hwnd: HWND) -> i32 {
    if let Some(other) = crate::imp::remote::window_of(hwnd) {
        // Another copy's window: that copy is brought to the front.
        let copy: Option<Retained<AnyObject>> = unsafe { msg_send![objc2::class!(NSRunningApplication), runningApplicationWithProcessIdentifier: other as i32] };
        if let Some(copy) = copy {
            let _: bool = unsafe { msg_send![&*copy, activateWithOptions: 2usize] };
        }
        return 1;
    }
    unsafe { SetActiveWindow(hwnd) };
    app().activate();
    1
}

pub(crate) fn capture() -> HWND {
    CAPTURE.get() as HWND
}

pub unsafe fn SetCapture(hwnd: HWND) -> HWND {
    let previous = CAPTURE.replace(hwnd as usize) as HWND;
    if !previous.is_null() && previous != hwnd {
        send(previous, WM_CAPTURECHANGED, 0, hwnd as isize);
    }
    previous
}

pub unsafe fn ReleaseCapture() -> i32 {
    let previous = CAPTURE.replace(0) as HWND;
    if !previous.is_null() {
        send(previous, WM_CAPTURECHANGED, 0, 0);
    }
    1
}

pub unsafe fn GetCapture() -> HWND {
    capture()
}

/// A cursor's handle is its Windows number.
pub unsafe fn LoadCursorW(_: crate::Win32::Foundation::HINSTANCE, lpcursorname: *const u16) -> HCURSOR {
    lpcursorname as HCURSOR
}

pub unsafe fn SetCursor(hcursor: HCURSOR) -> HCURSOR {
    let cursor = match hcursor as usize as *const u16 {
        IDC_HAND => NSCursor::pointingHandCursor(),
        IDC_IBEAM => NSCursor::IBeamCursor(),
        IDC_SIZEWE => NSCursor::resizeLeftRightCursor(),
        IDC_SIZENS => NSCursor::resizeUpDownCursor(),
        IDC_CROSS => NSCursor::crosshairCursor(),
        IDC_NO => NSCursor::operationNotAllowedCursor(),
        _ => NSCursor::arrowCursor(),
    };
    cursor.set();
    hcursor
}

pub unsafe fn LoadIconW(_: crate::Win32::Foundation::HINSTANCE, _: *const u16) -> HICON {
    std::ptr::null_mut()
}

pub unsafe fn WindowFromPoint(point: POINT) -> HWND {
    let on_screen = NSPoint::new(point.x as f64, screen_height() - point.y as f64);
    for frame in app().orderedWindows().iter() {
        if !frame.isVisible() || !objc2_foundation::NSPointInRect(on_screen, frame.frame()) {
            continue;
        }
        let Some(content) = frame.contentView() else { continue };
        let in_window = frame.convertPointFromScreen(on_screen);
        if let Some(hit) = content.hitTest(in_window) {
            return owner_of(&hit).map_or(std::ptr::null_mut(), |window| handle(&window.view));
        }
    }
    std::ptr::null_mut()
}

// --- Painting -----------------------------------------------------------

pub(crate) fn paint(view: &NSView, dirty: NSRect) {
    let hwnd = handle(view);
    // The part asked for can reach past the view; a window is never asked to
    // paint outside itself.
    let dirty = objc2_foundation::NSIntersectionRect(dirty, view.bounds());
    let area = RECT {
        left: dirty.origin.x.floor() as i32,
        top: dirty.origin.y.floor() as i32,
        right: (dirty.origin.x + dirty.size.width).ceil() as i32,
        bottom: (dirty.origin.y + dirty.size.height).ceil() as i32,
    };
    let previous = PAINTING.replace((hwnd as usize, area));
    send(hwnd, WM_PAINT, 0, 0);
    PAINTING.set(previous);
}

pub unsafe fn BeginPaint(hwnd: HWND, lppaint: *mut PAINTSTRUCT) -> HDC {
    let Some(paint) = (unsafe { lppaint.as_mut() }) else { return std::ptr::null_mut() };
    let (painting, area) = PAINTING.get();
    // Asked outside a paint, the context draws nowhere and the area is empty.
    let inside = painting == hwnd as usize;
    paint.hdc = if inside { gdi::painting_dc() } else { gdi::measuring_dc() };
    paint.rcPaint = if inside { area } else { RECT { left: 0, top: 0, right: 0, bottom: 0 } };
    paint.fErase = 0;
    paint.hdc
}

pub unsafe fn EndPaint(_: HWND, lppaint: *const PAINTSTRUCT) -> i32 {
    if let Some(paint) = unsafe { lppaint.as_ref() } {
        gdi::free_dc(paint.hdc);
    }
    1
}

pub unsafe fn InvalidateRect(hwnd: HWND, lprect: *const RECT, _: i32) -> i32 {
    let Some(window) = find(hwnd) else { return 0 };
    match unsafe { lprect.as_ref() } {
        Some(area) => window.view.setNeedsDisplayInRect(ns_rect(area)),
        None => window.view.setNeedsDisplay(true),
    }
    1
}

pub unsafe fn UpdateWindow(hwnd: HWND) -> i32 {
    if let Some(window) = find(hwnd) {
        window.view.displayIfNeeded();
    }
    1
}

pub unsafe fn RedrawWindow(hwnd: HWND, lprcupdate: *const RECT, _: HRGN, flags: REDRAW_WINDOW_FLAGS) -> i32 {
    let Some(window) = find(hwnd) else { return 0 };
    if flags & (RDW_INVALIDATE | RDW_INTERNALPAINT) != 0 {
        unsafe { InvalidateRect(hwnd, lprcupdate, 0) };
    }
    if flags & RDW_UPDATENOW != 0 {
        window.view.displayIfNeeded();
    }
    1
}

/// One line for each window: class, identifier, place in its parent, whether
/// shown, and its text. Written beside the pictures.
pub(crate) fn describe() -> String {
    let mut lines: Vec<String> = WINDOWS.with_borrow(|windows| {
        windows
            .values()
            .map(|window| {
                let frame = window.view.frame();
                format!(
                    "{:<26} id {:<6} at {:>5},{:<5} size {:>5}x{:<5} {} {:?}",
                    window.class,
                    if window.foreign { -1 } else { window.id.get() },
                    frame.origin.x,
                    frame.origin.y,
                    frame.size.width,
                    frame.size.height,
                    if window.frame.borrow().is_some() { "top" } else if window.view.isHidden() { "hidden" } else { "shown" },
                    window.text.borrow()
                )
            })
            .collect()
    });
    lines.sort();
    // Then every view as AppKit has them, ours or not, nested as they are.
    fn walk(view: &NSView, depth: usize, out: &mut Vec<String>) {
        let frame = view.frame();
        let class: Retained<NSString> = unsafe { msg_send![view, className] };
        out.push(format!(
            "{}{} at {},{} size {}x{}{}",
            "  ".repeat(depth),
            class,
            frame.origin.x,
            frame.origin.y,
            frame.size.width,
            frame.size.height,
            if view.isHidden() { " hidden" } else { "" }
        ));
        for child in unsafe { view.subviews() }.iter() {
            walk(&child, depth + 1, out);
        }
    }
    for (_, frame) in frames() {
        if let Some(content) = frame.contentView() {
            lines.push(String::new());
            walk(&content, 0, &mut lines);
        }
    }
    lines.join("\n")
}

/// The program's main window: the one that draws its own caption.
pub(crate) fn main_window() -> Option<HWND> {
    WINDOWS.with_borrow(|windows| windows.values().find(|window| window.own_caption && window.frame.borrow().is_some()).map(|window| handle(&window.view)))
}

/// Every top-level window, for the picture taken when asked (see `message`).
pub(crate) fn frames() -> Vec<(String, Retained<NSWindow>)> {
    WINDOWS.with_borrow(|windows| {
        windows.values().filter_map(|window| window.frame.borrow().clone().map(|frame| (window.class.clone(), frame))).collect()
    })
}

#[allow(dead_code)]
fn unused(_: &AnyObject) {}
