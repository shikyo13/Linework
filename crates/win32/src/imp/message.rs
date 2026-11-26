//! The message loop. The front end runs its own loop, asking for the next
//! message and handing it back to be delivered, as every Windows program does.
//! Here the next message is a posted one, a timer that has come due, or the
//! next AppKit event. A key press is offered to the front end as WM_KEYDOWN and
//! then WM_CHAR, and reaches the editor or text field it was typed into only if
//! the front end lets it through, which is how its shortcuts work on Windows.
use crate::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use crate::Win32::UI::Input::KeyboardAndMouse::*;
use crate::Win32::UI::WindowsAndMessaging::*;
use crate::imp::window::{app, find, send};
use objc2::rc::{Retained, autoreleasepool};
use objc2::{msg_send, sel};
use objc2_app_kit::{NSBitmapImageFileType, NSEvent, NSEventMask, NSEventModifierFlags, NSEventType};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSDictionary, NSPoint, NSString};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// An AppKit event with no Windows meaning of its own, passing through the loop.
const WM_NATIVE: u32 = 0xC3A0;

struct Posted {
    hwnd: usize,
    message: u32,
    w: WPARAM,
    l: LPARAM,
    held: u32,
}

struct Timer {
    hwnd: usize,
    id: usize,
    every: Duration,
    due: Instant,
}

/// An event kept while its message goes round the loop.
struct Held {
    id: u32,
    event: Retained<NSEvent>,
    /// WM_CHAR messages still to come for it; the event is delivered with the last.
    chars: u32,
}

static POSTED: Mutex<VecDeque<Posted>> = Mutex::new(VecDeque::new());
static WAKING: AtomicBool = AtomicBool::new(false);
static PICTURE: AtomicBool = AtomicBool::new(false);

thread_local! {
    static CHARS: RefCell<VecDeque<Posted>> = const { RefCell::new(VecDeque::new()) };
    static TIMERS: RefCell<Vec<Timer>> = const { RefCell::new(Vec::new()) };
    static HELD: RefCell<Vec<Held>> = const { RefCell::new(Vec::new()) };
    static NEXT: Cell<u32> = const { Cell::new(1) };
    static DOWN: RefCell<Vec<u16>> = const { RefCell::new(Vec::new()) };
    static READY: Cell<bool> = const { Cell::new(false) };
    /// The modifier keys held when the event now being handled was made: what
    /// Windows' key state means, and true of an event posted by a script too.
    static FLAGS: Cell<NSEventModifierFlags> = const { Cell::new(NSEventModifierFlags::empty()) };
}

unsafe extern "C" {
    static _dispatch_main_q: core::ffi::c_void;
    fn dispatch_async_f(queue: *const core::ffi::c_void, context: *mut core::ffi::c_void, work: extern "C" fn(*mut core::ffi::c_void));
    fn signal(number: i32, handler: extern "C" fn(i32)) -> usize;
}

/// Before the first message: the program becomes an application with a menu
/// bar, and a signal is set up that asks for a picture of every window.
fn ready() {
    if READY.replace(true) {
        return;
    }
    let app = app();
    app.setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Regular);
    crate::imp::misc::install_menu(&app);
    crate::imp::misc::watch_application(&app);
    app.finishLaunching();
    app.activate();
    extern "C" fn asked(_: i32) {
        PICTURE.store(true, Ordering::Relaxed);
    }
    // 30 is SIGUSR1.
    unsafe { signal(30, asked) };
    crate::imp::script::load();
}

/// Makes the loop come round when it is waiting for an event.
pub(crate) fn wake() {
    extern "C" fn post(_: *mut core::ffi::c_void) {
        WAKING.store(false, Ordering::Release);
        let event: Option<Retained<NSEvent>> = unsafe {
            msg_send![objc2::class!(NSEvent), otherEventWithType: 15usize, location: NSPoint::new(0.0, 0.0), modifierFlags: 0usize,
                timestamp: 0.0f64, windowNumber: 0isize, context: std::ptr::null::<objc2::runtime::AnyObject>(), subtype: 0i16, data1: 0isize, data2: 0isize]
        };
        if let Some(event) = event {
            let app: Option<Retained<objc2::runtime::AnyObject>> = unsafe { msg_send![objc2::class!(NSApplication), sharedApplication] };
            if let Some(app) = app {
                let _: () = unsafe { msg_send![&*app, postEvent: &*event, atStart: false] };
            }
        }
    }
    if !WAKING.swap(true, Ordering::AcqRel) {
        unsafe { dispatch_async_f(&raw const _dispatch_main_q, std::ptr::null_mut(), post) };
    }
}

pub unsafe fn PostMessageW(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> i32 {
    POSTED.lock().unwrap().push_back(Posted { hwnd: hwnd as usize, message: msg, w: wparam, l: lparam, held: 0 });
    wake();
    1
}

pub unsafe fn PostQuitMessage(nexitcode: i32) {
    unsafe { PostMessageW(std::ptr::null_mut(), WM_QUIT, nexitcode as usize, 0) };
}

pub(crate) fn forget_window(hwnd: HWND) {
    POSTED.lock().unwrap().retain(|posted| posted.hwnd != hwnd as usize);
    CHARS.with_borrow_mut(|chars| chars.retain(|posted| posted.hwnd != hwnd as usize));
    TIMERS.with_borrow_mut(|timers| timers.retain(|timer| timer.hwnd != hwnd as usize));
}

pub unsafe fn SetTimer(hwnd: HWND, nidevent: usize, uelapse: u32, _: TIMERPROC) -> usize {
    let every = Duration::from_millis(uelapse.max(1) as u64);
    TIMERS.with_borrow_mut(|timers| {
        timers.retain(|timer| !(timer.hwnd == hwnd as usize && timer.id == nidevent));
        timers.push(Timer { hwnd: hwnd as usize, id: nidevent, every, due: Instant::now() + every });
    });
    nidevent.max(1)
}

pub unsafe fn KillTimer(hwnd: HWND, uidevent: usize) -> i32 {
    TIMERS.with_borrow_mut(|timers| {
        let before = timers.len();
        timers.retain(|timer| !(timer.hwnd == hwnd as usize && timer.id == uidevent));
        (timers.len() != before) as i32
    })
}

fn fill(out: &mut MSG, posted: Posted) {
    out.hwnd = posted.hwnd as HWND;
    out.message = posted.message;
    out.wParam = posted.w;
    out.lParam = posted.l;
    out.time = posted.held;
}

/// A message that is ready without waiting, if there is one.
fn waiting() -> Option<Posted> {
    if let Some(char) = CHARS.with_borrow_mut(|chars| chars.pop_front()) {
        return Some(char);
    }
    if let Some(posted) = POSTED.lock().unwrap().pop_front() {
        return Some(posted);
    }
    TIMERS.with_borrow_mut(|timers| {
        let now = Instant::now();
        let timer = timers.iter_mut().find(|timer| timer.due <= now)?;
        timer.due = now + timer.every;
        Some(Posted { hwnd: timer.hwnd, message: WM_TIMER, w: timer.id, l: 0, held: 0 })
    })
}

fn hold(event: Retained<NSEvent>) -> u32 {
    let id = NEXT.get();
    NEXT.set(id.wrapping_add(1).max(1));
    HELD.with_borrow_mut(|held| {
        if held.len() > 32 {
            held.remove(0);
        }
        held.push(Held { id, event, chars: 0 });
    });
    id
}

fn held(id: u32) -> Option<(Retained<NSEvent>, u32)> {
    HELD.with_borrow(|held| held.iter().find(|entry| entry.id == id).map(|entry| (entry.event.clone(), entry.chars)))
}

/// The Windows key for a Mac key's number.
pub(crate) fn virtual_key(code: u16) -> u16 {
    const LETTERS: [(u16, u8); 36] = [
        (0x00, b'A'), (0x01, b'S'), (0x02, b'D'), (0x03, b'F'), (0x04, b'H'), (0x05, b'G'), (0x06, b'Z'), (0x07, b'X'), (0x08, b'C'),
        (0x09, b'V'), (0x0B, b'B'), (0x0C, b'Q'), (0x0D, b'W'), (0x0E, b'E'), (0x0F, b'R'), (0x10, b'Y'), (0x11, b'T'), (0x12, b'1'),
        (0x13, b'2'), (0x14, b'3'), (0x15, b'4'), (0x16, b'6'), (0x17, b'5'), (0x19, b'9'), (0x1A, b'7'), (0x1C, b'8'), (0x1D, b'0'),
        (0x1F, b'O'), (0x20, b'U'), (0x22, b'I'), (0x23, b'P'), (0x25, b'L'), (0x26, b'J'), (0x28, b'K'), (0x2D, b'N'), (0x2E, b'M'),
    ];
    if let Some(found) = LETTERS.iter().find(|entry| entry.0 == code) {
        return found.1 as u16;
    }
    match code {
        0x18 => VK_OEM_PLUS, 0x1B => VK_OEM_MINUS, 0x1E => VK_OEM_6, 0x21 => VK_OEM_4, 0x27 => VK_OEM_7, 0x29 => VK_OEM_1,
        0x2A => VK_OEM_5, 0x2B => VK_OEM_COMMA, 0x2C => VK_OEM_2, 0x2F => VK_OEM_PERIOD, 0x32 => VK_OEM_3,
        0x24 | 0x4C => VK_RETURN, 0x30 => VK_TAB, 0x31 => VK_SPACE, 0x33 => VK_BACK, 0x35 => VK_ESCAPE, 0x75 => VK_DELETE,
        0x73 => VK_HOME, 0x77 => VK_END, 0x74 => VK_PRIOR, 0x79 => VK_NEXT,
        0x7B => VK_LEFT, 0x7C => VK_RIGHT, 0x7D => VK_DOWN, 0x7E => VK_UP,
        0x7A => VK_F1, 0x78 => VK_F2, 0x63 => VK_F3, 0x76 => VK_F4, 0x60 => VK_F5, 0x61 => VK_F6, 0x62 => VK_F7, 0x64 => VK_F8,
        0x65 => VK_F9, 0x6D => VK_F10, 0x67 => VK_F11, 0x6F => VK_F12,
        0x52..=0x59 => VK_NUMPAD0 + (code - 0x52), 0x5B => VK_NUMPAD8, 0x5C => VK_NUMPAD9,
        0x41 => VK_DECIMAL, 0x43 => VK_MULTIPLY, 0x45 => VK_ADD, 0x4E => VK_SUBTRACT, 0x4B => VK_DIVIDE,
        _ => 0,
    }
}

/// A key press or release as the message Windows would post for it.
fn key_message(event: &NSEvent, down: bool) -> Option<Posted> {
    let key = virtual_key(event.keyCode());
    if key == 0 {
        return None;
    }
    DOWN.with_borrow_mut(|keys| {
        keys.retain(|held| *held != key);
        if down {
            keys.push(key);
        }
    });
    // A key belongs to the window its event names, when that is one of ours.
    let named = event.window(crate::imp::window::mtm()).map(|frame| crate::imp::window::focus_in(&frame)).filter(|hwnd| !hwnd.is_null());
    let hwnd = named.unwrap_or_else(|| unsafe { crate::imp::window::GetFocus() });
    if std::env::var_os("LINEWORK_TRACE").is_some() {
        eprintln!("key {key:#x} down={down} to {:?}", find(hwnd).map(|window| (window.class.clone(), window.id.get())));
    }
    let repeat = if down && event.isARepeat() { 1 << 30 } else { 0 };
    Some(Posted {
        hwnd: hwnd as usize,
        message: if down { WM_KEYDOWN } else { WM_KEYUP },
        w: key as usize,
        l: 1 | ((event.keyCode() as isize) << 16) | repeat,
        held: 0,
    })
}

/// Turns the next AppKit event into a message. False when none came in time.
fn next_event(out: &mut MSG, wait: bool) -> bool {
    let until = if !wait {
        NSDate::distantPast()
    } else {
        match TIMERS.with_borrow(|timers| timers.iter().map(|timer| timer.due).min()).into_iter().chain(crate::imp::script::due()).min() {
            Some(due) => NSDate::dateWithTimeIntervalSinceNow(due.saturating_duration_since(Instant::now()).as_secs_f64()),
            None => NSDate::distantFuture(),
        }
    };
    let event = app().nextEventMatchingMask_untilDate_inMode_dequeue(NSEventMask::Any, Some(&until), unsafe { NSDefaultRunLoopMode }, true);
    let Some(event) = event else { return false };
    let kind = event.r#type();
    if kind == NSEventType::ApplicationDefined {
        return false;
    }
    FLAGS.set(event.modifierFlags());
    let keys = kind == NSEventType::KeyDown || kind == NSEventType::KeyUp;
    // Command chords belong to the Mac: the menu bar and the editor's own keys.
    let ours = keys && !event.modifierFlags().contains(NSEventModifierFlags::Command);
    let mut posted = ours
        .then(|| key_message(&event, kind == NSEventType::KeyDown))
        .flatten()
        .unwrap_or(Posted { hwnd: 0, message: WM_NATIVE, w: 0, l: 0, held: 0 });
    posted.held = hold(event);
    fill(out, posted);
    true
}

fn next(out: &mut MSG, wait: bool) -> bool {
    ready();
    loop {
        if PICTURE.swap(false, Ordering::Relaxed) {
            pictures("");
        }
        crate::imp::script::run();
        crate::imp::remote::serve();
        if let Some(posted) = waiting() {
            fill(out, posted);
            return true;
        }
        let found = autoreleasepool(|_| next_event(out, wait));
        if found {
            return true;
        }
        if !wait {
            if let Some(posted) = waiting() {
                fill(out, posted);
                return true;
            }
            return false;
        }
    }
}

pub unsafe fn GetMessageW(lpmsg: *mut MSG, _: HWND, _: u32, _: u32) -> i32 {
    let Some(out) = (unsafe { lpmsg.as_mut() }) else { return -1 };
    next(out, true);
    (out.message != WM_QUIT) as i32
}

pub unsafe fn PeekMessageW(lpmsg: *mut MSG, _: HWND, _: u32, _: u32, _: PEEK_MESSAGE_REMOVE_TYPE) -> i32 {
    let Some(out) = (unsafe { lpmsg.as_mut() }) else { return 0 };
    next(out, false) as i32
}

/// Whether the text being typed is still being composed (an accent waiting for
/// its letter, an input method's candidates), when no character is final yet.
fn composing() -> bool {
    let Some(responder) = app().keyWindow().and_then(|frame| frame.firstResponder()) else { return false };
    let can: bool = unsafe { msg_send![&*responder, respondsToSelector: sel!(hasMarkedText)] };
    can && unsafe { msg_send![&*responder, hasMarkedText] }
}

pub unsafe fn TranslateMessage(lpmsg: *const MSG) -> i32 {
    let Some(message) = (unsafe { lpmsg.as_ref() }) else { return 0 };
    if message.message != WM_KEYDOWN || message.time == 0 {
        return 0;
    }
    let Some((event, _)) = held(message.time) else { return 0 };
    if composing() {
        return 0;
    }
    let text = event.characters().map(|text| text.to_string()).unwrap_or_default();
    // Arrow and function keys report characters from a private range.
    let units: Vec<u16> = text.encode_utf16().filter(|unit| !(0xF700..=0xF8FF).contains(unit)).collect();
    if units.is_empty() {
        return 0;
    }
    HELD.with_borrow_mut(|held| {
        if let Some(entry) = held.iter_mut().find(|entry| entry.id == message.time) {
            entry.chars = units.len() as u32;
        }
    });
    CHARS.with_borrow_mut(|chars| {
        for unit in units {
            chars.push_back(Posted { hwnd: message.hwnd as usize, message: WM_CHAR, w: unit as usize, l: message.lParam, held: message.time });
        }
    });
    1
}

pub unsafe fn DispatchMessageW(lpmsg: *const MSG) -> LRESULT {
    let Some(message) = (unsafe { lpmsg.as_ref() }) else { return 0 };
    let carried = if message.time == 0 { None } else { held(message.time) };
    let Some((event, chars)) = carried else {
        return send(message.hwnd, message.message, message.wParam, message.lParam);
    };
    if message.message == WM_NATIVE {
        autoreleasepool(|_| app().sendEvent(&event));
        return 0;
    }
    // The view takes the key itself: the editor, a text field, or a window that
    // is not one of ours at all.
    let native = find(message.hwnd).is_none_or(|window| window.native_keys.get());
    if !native {
        return send(message.hwnd, message.message, message.wParam, message.lParam);
    }
    if message.message == WM_CHAR {
        let left = HELD.with_borrow_mut(|held| {
            held.iter_mut().find(|entry| entry.id == message.time).map_or(0, |entry| {
                entry.chars = entry.chars.saturating_sub(1);
                entry.chars
            })
        });
        if left == 0 {
            autoreleasepool(|_| app().sendEvent(&event));
        }
    } else if chars == 0 {
        autoreleasepool(|_| app().sendEvent(&event));
    }
    0
}

/// A key press that reached a view of ours without passing through the loop.
pub(crate) fn key_to(hwnd: HWND, event: &NSEvent) {
    if let Some(posted) = key_message(event, true) {
        send(hwnd, posted.message, posted.w, posted.l);
    }
    if let Some(text) = event.characters() {
        for unit in text.to_string().encode_utf16().filter(|unit| !(0xF700..=0xF8FF).contains(unit)) {
            send(hwnd, WM_CHAR, unit as usize, 0);
        }
    }
}

fn pressed(key: i32) -> bool {
    let flags = FLAGS.get();
    match key as u16 {
        VK_SHIFT | VK_LSHIFT | VK_RSHIFT => flags.contains(NSEventModifierFlags::Shift),
        VK_CONTROL | VK_LCONTROL | VK_RCONTROL => flags.contains(NSEventModifierFlags::Control),
        // Alt is the Option key. The right-hand one is never reported alone, so
        // a layout's third level is typed as on a Mac and not taken for AltGr.
        VK_MENU | VK_LMENU => flags.contains(NSEventModifierFlags::Option),
        VK_RMENU => false,
        VK_LBUTTON => NSEvent::pressedMouseButtons() & 1 != 0,
        VK_RBUTTON => NSEvent::pressedMouseButtons() & 2 != 0,
        VK_CAPITAL => flags.contains(NSEventModifierFlags::CapsLock),
        other => DOWN.with_borrow(|keys| keys.contains(&other)),
    }
}

pub unsafe fn GetKeyState(nvirtkey: i32) -> i16 {
    if pressed(nvirtkey) { i16::MIN } else { 0 }
}

pub unsafe fn GetAsyncKeyState(vkey: i32) -> i16 {
    unsafe { GetKeyState(vkey) }
}

pub unsafe fn GetKeyboardState(lpkeystate: *mut u8) -> i32 {
    let keys = unsafe { std::slice::from_raw_parts_mut(lpkeystate, 256) };
    for (key, state) in keys.iter_mut().enumerate() {
        *state = if pressed(key as i32) { 0x80 } else { 0 };
    }
    1
}

pub unsafe fn GetKeyboardLayout(_: u32) -> HKL {
    std::ptr::null_mut()
}

/// Asked only to tell an AltGr letter from a shortcut, which does not arise here.
pub unsafe fn ToUnicodeEx(_: u32, _: u32, _: *const u8, _: *mut u16, _: i32, _: u32, _: HKL) -> i32 {
    0
}

/// Writes a picture of every window into the folder LINEWORK_PICTURES names:
/// what the window would show, drawn again into an image. Asked for with
/// `kill -USR1`, so the program can be looked at without the screen.
pub(crate) fn pictures(name: &str) {
    let Some(folder) = std::env::var_os("LINEWORK_PICTURES").map(std::path::PathBuf::from) else { return };
    let _ = std::fs::create_dir_all(&folder);
    for (number, (class, frame)) in crate::imp::window::frames().iter().enumerate() {
        if !frame.isVisible() {
            continue;
        }
        let Some(view) = frame.contentView() else { continue };
        let bounds = view.bounds();
        // The layers are drawn, which is what the screen shows: the editor
        // paints its own layer and does not draw rightly when asked to draw
        // into an image as one view among others.
        let Some(image) = view.bitmapImageRepForCachingDisplayInRect(bounds) else { continue };
        let layer: Option<Retained<objc2::runtime::AnyObject>> = unsafe { msg_send![&*view, layer] };
        match (layer, objc2_app_kit::NSGraphicsContext::graphicsContextWithBitmapImageRep(&image)) {
            (Some(layer), Some(context)) => {
                view.displayIfNeeded();
                // A view that counts down from the top has its layer the same way up.
                let port = context.CGContext();
                objc2_core_graphics::CGContext::translate_ctm(Some(&port), 0.0, bounds.size.height);
                objc2_core_graphics::CGContext::scale_ctm(Some(&port), 1.0, -1.0);
                let _: () = unsafe { msg_send![&*layer, renderInContext: &*port] };
                context.flushGraphics();
            }
            _ => view.cacheDisplayInRect_toBitmapImageRep(bounds, &image),
        }
        let empty = NSDictionary::new();
        let Some(data) = (unsafe { image.representationUsingType_properties(NSBitmapImageFileType::PNG, &empty) }) else { continue };
        let path = folder.join(format!("{name}{class}-{number}.png"));
        let _: bool = unsafe { msg_send![&*data, writeToFile: &*NSString::from_str(&path.to_string_lossy()), atomically: true] };
    }
    let _ = std::fs::write(folder.join(format!("{name}windows.txt")), crate::imp::window::describe());
    crate::imp::gdi::save_picture(crate::imp::gdi::LAST_SOURCE.get() as _, &folder.join("source.png"));
    let _ = std::fs::write(folder.join("done"), b"");
}
