//! The view behind every window of ours, and what AppKit tells it: draw, the
//! pointer, focus, a new size. Each becomes the Windows message for it.
use crate::Win32::Foundation::{HWND, POINT};
use crate::Win32::UI::Controls::WM_MOUSELEAVE;
use crate::Win32::UI::WindowsAndMessaging::*;
use crate::imp::window::{self, find, handle, pack, send};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindow, NSWindowDelegate};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize};
use std::cell::Cell;

// The buttons and keys Windows names in a pointer message.
const MK_LBUTTON: u32 = 1;
const MK_RBUTTON: u32 = 2;
const MK_SHIFT: u32 = 4;
const MK_CONTROL: u32 = 8;
const MK_MBUTTON: u32 = 16;

#[derive(Clone, Copy, PartialEq)]
enum Pointer {
    Down(u32, u32),
    Up(u32),
    Move,
}

thread_local! {
    /// Whether the pointer was last seen over the caption area of a window
    /// that draws its own, so leaving it can be told.
    static OVER_CAPTION: Cell<bool> = const { Cell::new(false) };
}

define_class!(
    // SAFETY: NSView allows subclassing, and `LwView` has no `Drop`.
    #[unsafe(super(NSView, objc2_app_kit::NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    pub(crate) struct LwView;

    impl LwView {
        // Windows counts down from the top left; so does every window of ours.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw(&self, dirty: NSRect) {
            window::paint(self, dirty);
        }

        // On Windows a click does not move the keyboard focus; only the front
        // end's own request does. So a view takes it only while one is made.
        #[unsafe(method(acceptsFirstResponder))]
        fn takes_keys(&self) -> bool {
            window::focusing()
        }

        #[unsafe(method(becomeFirstResponder))]
        fn got_keys(&self) -> bool {
            send(handle(self), WM_SETFOCUS, 0, 0);
            true
        }

        #[unsafe(method(resignFirstResponder))]
        fn lost_keys(&self) -> bool {
            send(handle(self), WM_KILLFOCUS, 0, 0);
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn first_click_counts(&self, _: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(setFrameSize:))]
        fn sized(&self, size: NSSize) {
            let before = self.frame().size;
            let _: () = unsafe { msg_send![super(self), setFrameSize: size] };
            if before.width != size.width || before.height != size.height {
                send(handle(self), WM_SIZE, 0, pack(size.width as i32, size.height as i32));
            }
        }

        #[unsafe(method(didAddSubview:))]
        fn gained(&self, view: &NSView) {
            window::adopt(view);
        }

        #[unsafe(method(willRemoveSubview:))]
        fn losing(&self, view: &NSView) {
            if find(handle(view)).is_some_and(|window| window.foreign) {
                window::forget(view);
            }
        }

        #[unsafe(method(updateTrackingAreas))]
        fn track(&self) {
            for area in self.trackingAreas().iter() {
                self.removeTrackingArea(&area);
            }
            let options = NSTrackingAreaOptions::MouseMoved
                | NSTrackingAreaOptions::MouseEnteredAndExited
                | NSTrackingAreaOptions::ActiveAlways
                | NSTrackingAreaOptions::InVisibleRect;
            let area = unsafe {
                NSTrackingArea::initWithRect_options_owner_userInfo(NSTrackingArea::alloc(), self.bounds(), options, Some(self), None)
            };
            self.addTrackingArea(&area);
            let _: () = unsafe { msg_send![super(self), updateTrackingAreas] };
        }

        #[unsafe(method(mouseDown:))]
        fn left_down(&self, event: &NSEvent) {
            let double = event.clickCount() % 2 == 0;
            pointer(self, event, Pointer::Down(if double { WM_LBUTTONDBLCLK } else { WM_LBUTTONDOWN }, WM_NCLBUTTONDOWN));
        }

        #[unsafe(method(mouseUp:))]
        fn left_up(&self, event: &NSEvent) {
            pointer(self, event, Pointer::Up(WM_LBUTTONUP));
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_down(&self, event: &NSEvent) {
            pointer(self, event, Pointer::Down(WM_RBUTTONDOWN, WM_NCRBUTTONDOWN));
        }

        #[unsafe(method(rightMouseUp:))]
        fn right_up(&self, event: &NSEvent) {
            pointer(self, event, Pointer::Up(WM_RBUTTONUP));
        }

        #[unsafe(method(otherMouseDown:))]
        fn middle_down(&self, event: &NSEvent) {
            pointer(self, event, Pointer::Down(WM_MBUTTONDOWN, WM_NCMBUTTONDOWN));
        }

        #[unsafe(method(otherMouseUp:))]
        fn middle_up(&self, event: &NSEvent) {
            pointer(self, event, Pointer::Up(WM_MBUTTONUP));
        }

        #[unsafe(method(mouseMoved:))]
        fn moved(&self, event: &NSEvent) {
            pointer(self, event, Pointer::Move);
        }

        #[unsafe(method(mouseDragged:))]
        fn dragged(&self, event: &NSEvent) {
            pointer(self, event, Pointer::Move);
        }

        #[unsafe(method(rightMouseDragged:))]
        fn right_dragged(&self, event: &NSEvent) {
            pointer(self, event, Pointer::Move);
        }

        #[unsafe(method(mouseExited:))]
        fn left_view(&self, _: &NSEvent) {
            let hwnd = handle(self);
            if OVER_CAPTION.replace(false) {
                send(hwnd, WM_NCMOUSELEAVE, 0, 0);
            }
            send(hwnd, WM_MOUSELEAVE, 0, 0);
        }

        #[unsafe(method(scrollWheel:))]
        fn wheel(&self, event: &NSEvent) {
            // A notch of a wheel is 120 on Windows. A trackpad reports points;
            // sixty of them are taken as a notch.
            let turn = event.scrollingDeltaY() * if event.hasPreciseScrollingDeltas() { 2.0 } else { 120.0 };
            if turn == 0.0 {
                return;
            }
            let at = screen_point(event);
            let keys = buttons(event) & 0xffff;
            send(handle(self), WM_MOUSEWHEEL, (((turn as i32 as i16 as u16 as usize) << 16) | keys) as usize, pack(at.x, at.y));
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            crate::imp::message::key_to(handle(self), event);
        }

        // The editor tells the window around it what happened in it.
        #[unsafe(method(linework_notify:))]
        fn editor_says(&self, notification: *const core::ffi::c_void) {
            // The identifier of the sender is the second field of every notification.
            let id = unsafe { *(notification as *const usize).add(1) };
            send(handle(self), WM_NOTIFY, id, notification as isize);
        }
    }
);

impl LwView {
    pub(crate) fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let view: Retained<Self> = unsafe { msg_send![Self::alloc(mtm), initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0))] };
        // A window draws inside itself and nowhere else. Since macOS 14 a view
        // is no longer held to its own bounds unless it asks to be.
        let can: bool = unsafe { msg_send![&*view, respondsToSelector: objc2::sel!(setClipsToBounds:)] };
        if can {
            let _: () = unsafe { msg_send![&*view, setClipsToBounds: true] };
        }
        view
    }
}

fn screen_point(_: &NSEvent) -> POINT {
    let mut at = POINT { x: 0, y: 0 };
    unsafe { window::GetCursorPos(&mut at) };
    at
}

/// The buttons and keys held, as Windows passes them with a pointer message.
fn buttons(event: &NSEvent) -> usize {
    let (held, keys) = (NSEvent::pressedMouseButtons(), event.modifierFlags());
    let mut flags = 0;
    for (bit, flag) in [(1, MK_LBUTTON), (2, MK_RBUTTON), (4, MK_MBUTTON)] {
        if held & bit != 0 {
            flags |= flag;
        }
    }
    if keys.contains(NSEventModifierFlags::Shift) {
        flags |= MK_SHIFT;
    }
    if keys.contains(NSEventModifierFlags::Control) {
        flags |= MK_CONTROL;
    }
    flags as usize
}

fn pointer(view: &NSView, event: &NSEvent, what: Pointer) {
    let own = handle(view);
    let captured = window::capture();
    let target: HWND = if captured.is_null() { own } else { captured };
    let Some(window) = find(target) else { return };
    if !window.enabled.get() {
        return;
    }
    let screen = screen_point(event);
    let mut local = screen;
    unsafe { window::ScreenToClient(target, &mut local) };
    let keys = buttons(event);
    // A window that draws its own caption says which part the pointer is on.
    if window.own_caption && captured.is_null() {
        let part = send(target, WM_NCHITTEST, 0, pack(screen.x, screen.y)) as u32;
        if part != HTCLIENT {
            OVER_CAPTION.set(true);
            match what {
                Pointer::Down(_, outside) if part == HTCAPTION && outside == WM_NCLBUTTONDOWN => {
                    if let Some(frame) = view.window() {
                        if event.clickCount() == 2 { frame.zoom(None) } else { frame.performWindowDragWithEvent(event) }
                    }
                }
                Pointer::Down(_, outside) => drop(send(target, outside, part as usize, pack(screen.x, screen.y))),
                Pointer::Up(inside) => {
                    let outside = match inside {
                        WM_LBUTTONUP => WM_NCLBUTTONUP,
                        WM_RBUTTONUP => WM_NCRBUTTONUP,
                        _ => WM_NCMBUTTONUP,
                    };
                    send(target, outside, part as usize, pack(screen.x, screen.y));
                }
                Pointer::Move => drop(send(target, WM_NCMOUSEMOVE, part as usize, pack(screen.x, screen.y))),
            }
            return;
        }
        if OVER_CAPTION.replace(false) {
            send(target, WM_NCMOUSELEAVE, 0, 0);
        }
    }
    let at = pack(local.x, local.y);
    match what {
        Pointer::Down(message, _) => drop(send(target, message, keys, at)),
        Pointer::Up(message) => drop(send(target, message, keys, at)),
        Pointer::Move => {
            if captured.is_null() {
                send(target, WM_SETCURSOR, target as usize, (HTCLIENT | (WM_MOUSEMOVE << 16)) as isize);
            }
            send(target, WM_MOUSEMOVE, keys, at);
        }
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `Watcher` has no `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    struct Watcher;

    unsafe impl NSObjectProtocol for Watcher {}

    unsafe impl NSWindowDelegate for Watcher {
        #[unsafe(method(windowShouldClose:))]
        fn asked_to_close(&self, frame: &NSWindow) -> bool {
            if let Some(view) = frame.contentView() {
                send(handle(&view), WM_CLOSE, 0, 0);
            }
            false
        }

        #[unsafe(method(windowDidBecomeKey:))]
        fn became_key(&self, notice: &NSNotification) {
            tell(notice, WM_ACTIVATE, WA_ACTIVE as usize);
        }

        #[unsafe(method(windowDidResignKey:))]
        fn resigned_key(&self, notice: &NSNotification) {
            tell(notice, WM_ACTIVATE, WA_INACTIVE as usize);
        }

        #[unsafe(method(windowDidMove:))]
        fn moved(&self, notice: &NSNotification) {
            tell(notice, WM_MOVE, 0);
        }

        #[unsafe(method(windowWillStartLiveResize:))]
        fn resize_began(&self, notice: &NSNotification) {
            tell(notice, WM_ENTERSIZEMOVE, 0);
        }

        #[unsafe(method(windowDidEndLiveResize:))]
        fn resize_ended(&self, notice: &NSNotification) {
            tell(notice, WM_EXITSIZEMOVE, 0);
        }

        #[unsafe(method(windowWillResize:toSize:))]
        fn resizing(&self, frame: &NSWindow, wanted: NSSize) -> NSSize {
            let Some(view) = frame.contentView() else { return wanted };
            let mut limits: MINMAXINFO = unsafe { std::mem::zeroed() };
            send(handle(&view), WM_GETMINMAXINFO, 0, &mut limits as *mut _ as isize);
            // The limits are for the content; the system's frame is added back.
            let chrome = frame.frame().size.height - view.frame().size.height;
            NSSize::new(wanted.width.max(limits.ptMinTrackSize.x as f64), wanted.height.max(limits.ptMinTrackSize.y as f64 + chrome))
        }
    }
);

fn tell(notice: &NSNotification, message: u32, w: usize) {
    let frame: Option<Retained<AnyObject>> = unsafe { notice.object() };
    let Some(frame) = frame else { return };
    let view: Option<Retained<NSView>> = unsafe { msg_send![&*frame, contentView] };
    if let Some(view) = view {
        send(handle(&view), message, w, 0);
    }
}

thread_local! {
    static WATCHER: std::cell::OnceCell<Retained<Watcher>> = const { std::cell::OnceCell::new() };
}

/// Has a top-level window report closing, activation, moving and resizing.
pub(crate) fn watch(frame: &NSWindow, mtm: MainThreadMarker) {
    WATCHER.with(|cell| {
        let watcher = cell.get_or_init(|| unsafe { msg_send![Watcher::alloc(mtm), init] });
        frame.setDelegate(Some(ProtocolObject::from_ref(&**watcher)));
    });
}
