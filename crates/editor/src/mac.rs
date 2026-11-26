//! Scintilla on macOS. The control is a ScintillaView, an AppKit view, made and
//! held through the Objective-C runtime. Messages go straight to Scintilla's own
//! entry point, the way a Windows host uses the direct function, so a message
//! costs one call. All of this belongs to the main thread, as AppKit requires.
use crate::Notification;
use crate::sci::{SCI_GETDIRECTFUNCTION, SCI_GETDIRECTPOINTER, SCI_SETIDENTIFIER};
use std::cell::RefCell;
use std::ffi::{CStr, c_char, c_void};
use std::rc::Rc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

type Id = *mut c_void;
type Selector = *const c_void;
type Direct = unsafe extern "C" fn(isize, u32, usize, isize) -> isize;
/// What a view's listener is given: who hears the notifications, and the view
/// they come from.
struct Hearing {
    handler: RefCell<Option<Rc<dyn Fn(&Notification)>>>,
    view: Id,
}

#[repr(C)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_msgSend();
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Selector;
    fn objc_allocateClassPair(parent: Id, name: *const c_char, extra: usize) -> Id;
    fn objc_registerClassPair(class: Id);
    fn class_addIvar(class: Id, name: *const c_char, size: usize, alignment: u8, types: *const c_char) -> bool;
    fn class_addMethod(class: Id, name: Selector, method: unsafe extern "C" fn(), types: *const c_char) -> bool;
    fn class_getInstanceVariable(class: Id, name: *const c_char) -> *const c_void;
    fn ivar_getOffset(variable: *const c_void) -> isize;
    // Naming the class symbol makes the linker take the view out of the static
    // library; a look-up by name at run time would leave it behind.
    #[link_name = "OBJC_CLASS_$_ScintillaView"]
    static SCINTILLA_VIEW: c_void;
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(queue: *const c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
}

unsafe extern "system" {
    /// Lexilla's entry point, linked into the program.
    pub(crate) fn CreateLexer(name: *const i8) -> *mut c_void;
}

fn selector(name: &CStr) -> Selector {
    unsafe { sel_registerName(name.as_ptr()) }
}

/// Sends a message that takes at most one object and returns one.
unsafe fn send(receiver: Id, name: &CStr, argument: Id) -> Id {
    let call: unsafe extern "C" fn(Id, Selector, Id) -> Id =
        unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { call(receiver, selector(name), argument) }
}

unsafe fn send_frame(receiver: Id, name: &CStr, frame: Rect) -> Id {
    let call: unsafe extern "C" fn(Id, Selector, Rect) -> Id =
        unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { call(receiver, selector(name), frame) }
}

/// Sends a Scintilla message to a view known only by its address, as a
/// Windows host does with a window handle. Text sizes are not converted here;
/// use `View::call` for those.
pub(crate) unsafe fn message(view: isize, message: u32, w: usize, l: isize) -> isize {
    let call: unsafe extern "C" fn(Id, Selector, u32, usize, isize) -> isize =
        unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { call(view as Id, selector(c"message:wParam:lParam:"), message, w, l) }
}

/// One ScintillaView with the object that hears its notifications.
pub(crate) struct View {
    view: Id,
    listener: Id,
    hearing: Box<Hearing>,
    /// Scintilla's own object behind the view, which the direct function takes.
    backend: isize,
    direct: Direct,
}

impl View {
    /// `parent` is an NSView to place the editor in, or anything not above zero
    /// for an editor in no window.
    pub(crate) fn create(parent: isize, id: i32) -> Result<Self, String> {
        let frame = Rect { x: 0.0, y: 0.0, width: 1.0, height: 1.0 };
        let view = unsafe {
            // Scintilla takes the system's cursors while it makes a view, and
            // they exist only once the program is an application. In the editor
            // it already is one and this changes nothing.
            send(objc_getClass(c"NSApplication".as_ptr()), c"sharedApplication", std::ptr::null_mut());
            let class = &raw const SCINTILLA_VIEW as Id;
            send_frame(send(class, c"alloc", std::ptr::null_mut()), c"initWithFrame:", frame)
        };
        if view.is_null() {
            return Err("Cannot create text editor".into());
        }
        let message: unsafe extern "C" fn(Id, Selector, u32, usize, isize) -> isize =
            unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
        let ask = |what| unsafe { message(view, selector(c"message:wParam:lParam:"), what, 0, 0) };
        let (entry, backend) = (ask(SCI_GETDIRECTFUNCTION), ask(SCI_GETDIRECTPOINTER));
        if entry == 0 || backend == 0 {
            unsafe { send(view, c"release", std::ptr::null_mut()) };
            return Err("Cannot reach the text editor".into());
        }
        let direct: Direct = unsafe { std::mem::transmute(entry) };
        let hearing = Box::new(Hearing { handler: RefCell::new(None), view });
        let listener = unsafe {
            let class = listener_class();
            let listener = send(send(class, c"alloc", std::ptr::null_mut()), c"init", std::ptr::null_mut());
            listener.byte_offset(hearing_offset(class)).cast::<*const Hearing>().write(&*hearing);
            send(view, c"setDelegate:", listener);
            direct(backend, SCI_SETIDENTIFIER, id as usize, 0);
            if parent > 0 {
                send(parent as Id, c"addSubview:", view);
            }
            listener
        };
        Ok(Self { view, listener, hearing, backend, direct })
    }

    /// The view's address: the window, in a Windows host's terms.
    pub(crate) fn view(&self) -> isize {
        self.view as isize
    }

    pub(crate) unsafe fn call(&self, message: u32, w: usize, l: isize) -> isize {
        // Text sizes are given in points. On Windows a point is 96/72 of a
        // pixel; here a point is a pixel, and the rest of the program lays
        // itself out in Windows pixels. So sizes go in a third larger and come
        // back as they went in: SCI_STYLESETSIZE, SCI_STYLESETSIZEFRACTIONAL
        // and SCI_SETZOOM, and the three that read them.
        let (message, w, l, read) = match message {
            2055 => (2061, w, l * 400 / 3, false),
            2061 => (2061, w, l * 4 / 3, false),
            2373 => (2373, ((w as isize * 4 + 1) / 3) as usize, 0, false),
            2485 | 2062 | 2374 => (message, w, l, true),
            _ => (message, w, l, false),
        };
        let result = unsafe { (self.direct)(self.backend, message, w, l) };
        if read { result * 3 / 4 } else { result }
    }

    pub(crate) fn listen(&self, handler: Option<Rc<dyn Fn(&Notification)>>) {
        self.hearing.handler.replace(handler);
    }

    pub(crate) fn set_frame(&self, x: f64, y: f64, width: f64, height: f64) {
        unsafe { send_frame(self.view, c"setFrame:", Rect { x, y, width, height }) };
    }
}

impl Drop for View {
    fn drop(&mut self) {
        unsafe {
            send(self.view, c"setDelegate:", std::ptr::null_mut());
            send(self.view, c"removeFromSuperview", std::ptr::null_mut());
            send(self.view, c"release", std::ptr::null_mut());
            send(self.listener, c"release", std::ptr::null_mut());
        }
    }
}

/// The class of the object each view reports to. It holds one address: where
/// that view's hearing is kept.
fn listener_class() -> Id {
    static CLASS: OnceLock<usize> = OnceLock::new();
    *CLASS.get_or_init(|| unsafe {
        let class = objc_allocateClassPair(objc_getClass(c"NSObject".as_ptr()), c"LineworkEditorListener".as_ptr(), 0);
        class_addIvar(class, c"hearing".as_ptr(), size_of::<usize>(), 3, c"^v".as_ptr());
        class_addMethod(
            class,
            selector(c"notification:"),
            std::mem::transmute(notified as unsafe extern "C" fn(Id, Selector, *const Notification)),
            c"v@:^v".as_ptr(),
        );
        objc_registerClassPair(class);
        class as usize
    }) as Id
}

fn hearing_offset(class: Id) -> isize {
    unsafe { ivar_getOffset(class_getInstanceVariable(class, c"hearing".as_ptr())) }
}

unsafe extern "C" fn notified(listener: Id, _: Selector, notification: *const Notification) {
    let hearing = unsafe { &*listener.byte_offset(hearing_offset(listener_class())).cast::<*const Hearing>().read() };
    // Scintilla names its own inner object as the sender. To the host the
    // sender is the view, as the window is on Windows.
    let mut notice = unsafe { *notification };
    notice.nmhdr.hwnd_from = hearing.view as isize;
    // The handler may send messages that notify again, or replace itself, so it
    // is taken out of its cell before it runs.
    let handler = hearing.handler.borrow().clone();
    if let Some(handler) = handler {
        return handler(&notice);
    }
    // With no handler named, the view around this one is told, if it listens:
    // the place a Windows host receives WM_NOTIFY.
    unsafe {
        let parent = send(hearing.view, c"superview", std::ptr::null_mut());
        let name = selector(c"linework_notify:");
        let responds: unsafe extern "C" fn(Id, Selector, Selector) -> bool = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        if !parent.is_null() && responds(parent, selector(c"respondsToSelector:"), name) {
            let tell: unsafe extern "C" fn(Id, Selector, *const Notification) = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            tell(parent, name, &notice);
        }
    }
}

static WAKE: AtomicUsize = AtomicUsize::new(0);

/// Names what the host runs on the main thread when a grammar worker has styles
/// ready, the part a timer message plays on Windows. The host answers by calling
/// `Editor::pump_grammar` on its editors.
pub fn set_wake(wake: fn()) {
    WAKE.store(wake as usize, Ordering::Relaxed);
}

/// Called on a worker thread.
pub(crate) fn wake() {
    extern "C" fn run(_: *mut c_void) {
        let wake = WAKE.load(Ordering::Relaxed);
        if wake != 0 {
            unsafe { std::mem::transmute::<usize, fn()>(wake)() };
        }
    }
    if WAKE.load(Ordering::Relaxed) != 0 {
        unsafe { dispatch_async_f(&raw const _dispatch_main_q, std::ptr::null_mut(), run) };
    }
}
