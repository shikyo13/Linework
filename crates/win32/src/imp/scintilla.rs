//! An editor made the Windows way, by creating a window of class "Scintilla".
//! The front end's main editors come from the editor crate; its compare and
//! review windows make theirs like this. The class is Scintilla's own view,
//! which the program is linked with, looked up by name.
use crate::Win32::UI::WindowsAndMessaging::WM_NOTIFY;
use crate::imp::window::{handle, send};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::NSView;
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize};

/// The size of Scintilla's notification record, with room to spare.
const RECORD: usize = 256;

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `Hearing` has no `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = usize]
    pub(crate) struct Hearing;

    unsafe impl NSObjectProtocol for Hearing {}

    impl Hearing {
        // Scintilla reports to this object; the window around the editor is told,
        // with the editor's view named as the sender, as its window is on Windows.
        #[unsafe(method(notification:))]
        fn notified(&self, notification: *const usize) {
            let view = *self.ivars() as *const NSView;
            let mut copy = [0usize; RECORD / size_of::<usize>()];
            unsafe { std::ptr::copy_nonoverlapping(notification, copy.as_mut_ptr(), 22) };
            copy[0] = view as usize;
            let Some(parent) = (unsafe { (*view).superview() }) else { return };
            send(handle(&parent), WM_NOTIFY, copy[1], copy.as_ptr() as isize);
        }
    }
);

/// A Scintilla view and the object it reports to, or nothing when the program
/// was built without Scintilla.
pub(crate) fn create(mtm: MainThreadMarker, id: isize) -> Option<(Retained<NSView>, Retained<Hearing>)> {
    let class = objc2::runtime::AnyClass::get(c"ScintillaView")?;
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
    let view: Retained<NSView> = unsafe {
        let made: objc2::rc::Allocated<NSView> = msg_send![class, alloc];
        msg_send![made, initWithFrame: frame]
    };
    let hearing: Retained<Hearing> = unsafe { msg_send![super(Hearing::alloc(mtm).set_ivars(Retained::as_ptr(&view) as usize)), init] };
    unsafe {
        let _: () = msg_send![&*view, setDelegate: &*hearing];
        // 2622 gives the editor the identifier its notifications carry.
        let _: isize = msg_send![&*view, message: 2622u32, wParam: id as usize, lParam: 0isize];
    }
    let _ = std::ptr::null::<AnyObject>();
    Some((view, hearing))
}
