//! An editor made the Windows way, by creating a window of class "Scintilla".
//! The front end's main editors come from the editor crate; its compare and
//! review windows make theirs like this. The class is Scintilla's own view,
//! which the program is linked with, looked up by name.
use crate::Win32::UI::WindowsAndMessaging::WM_NOTIFY;
use crate::imp::window::{handle, send};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBezierPath, NSControl, NSControlSize, NSEvent, NSResponder, NSScrollView, NSScroller, NSScrollerPart, NSScrollerStyle, NSTrackingArea,
    NSTrackingAreaOptions, NSView,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize};
use std::cell::Cell;

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
    dress(&view);
    Some((view, hearing))
}

/// How wide Windows makes a scroll bar at 96 dots to the inch.
const BAR: f64 = 17.0;

define_class!(
    // SAFETY: NSScroller is made to be subclassed for its drawing, and `Bar` has no `Drop`.
    #[unsafe(super(NSScroller, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Cell<bool>]
    struct Bar;

    unsafe impl NSObjectProtocol for Bar {}

    impl Bar {
        // The bar keeps its own strip beside the text, as on Windows, and does
        // not float over it.
        #[unsafe(method(isCompatibleWithOverlayScrollers))]
        fn floats() -> bool {
            false
        }

        #[unsafe(method(scrollerWidthForControlSize:scrollerStyle:))]
        fn width(_: NSControlSize, _: NSScrollerStyle) -> f64 {
            BAR
        }

        #[unsafe(method(drawRect:))]
        fn draw(&self, _: NSRect) {
            paint(self);
        }

        #[unsafe(method(mouseEntered:))]
        fn entered(&self, _: &NSEvent) {
            self.ivars().set(true);
            NSView::setNeedsDisplay(self, true);
        }

        #[unsafe(method(mouseExited:))]
        fn left(&self, _: &NSEvent) {
            self.ivars().set(false);
            NSView::setNeedsDisplay(self, true);
        }
    }
);

/// The scroll bar as Windows 11 draws it: a flat strip with a thin thumb,
/// which thickens while the pointer is over the strip.
fn paint(bar: &Bar) {
    let bounds = bar.bounds();
    let dark = editor_is_dark(bar);
    crate::imp::gdi::color(if dark { 0x171717 } else { 0xF0F0F0 }).set();
    NSBezierPath::fillRect(bounds);
    let knob = bar.rectForPart(NSScrollerPart::Knob);
    if knob.size.width < 1.0 || knob.size.height < 1.0 {
        return;
    }
    let over = bar.ivars().get();
    let thick = if over { 6.0 } else { 2.0 };
    let upright = bounds.size.height >= bounds.size.width;
    let thumb = if upright {
        NSRect::new(NSPoint::new(bounds.origin.x + ((bounds.size.width - thick) / 2.0).ceil(), knob.origin.y), NSSize::new(thick, knob.size.height))
    } else {
        NSRect::new(NSPoint::new(knob.origin.x, bounds.origin.y + ((bounds.size.height - thick) / 2.0).ceil()), NSSize::new(knob.size.width, thick))
    };
    crate::imp::gdi::color(if dark { 0x959595 } else { 0x858585 }).set();
    if over {
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(thumb, 3.0, 3.0).fill();
    } else {
        NSBezierPath::fillRect(thumb);
    }
}

/// Whether the editor the bar belongs to has a dark background.
fn editor_is_dark(bar: &Bar) -> bool {
    let mut above = unsafe { bar.superview() };
    while let Some(view) = above {
        let editor: bool = unsafe { msg_send![&*view, respondsToSelector: sel!(message:wParam:lParam:)] };
        if editor {
            // 2482 reads the background of the default style, blue in the high byte.
            let back: isize = unsafe { msg_send![&*view, message: 2482u32, wParam: 32usize, lParam: 0isize] };
            let (red, green, blue) = (back & 255, back >> 8 & 255, back >> 16 & 255);
            return red * 3 + green * 6 + blue < 1280;
        }
        above = unsafe { view.superview() };
    }
    true
}

/// Gives an editor's view the scroll bars above. Windows hides a bar that has
/// nothing to scroll, so these do too.
pub(crate) fn dress(view: &NSView) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let scroll: Option<Retained<NSScrollView>> = unsafe { msg_send![view, scrollView] };
    let Some(scroll) = scroll else { return };
    // A bar takes its direction from the shape it is made with.
    let bar = |width: f64, height: f64| {
        let made = Bar::alloc(mtm).set_ivars(Cell::new(false));
        let bar: Retained<Bar> = unsafe { msg_send![super(made), initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height))] };
        let watch = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
                NSTrackingAreaOptions::MouseEnteredAndExited | NSTrackingAreaOptions::ActiveAlways | NSTrackingAreaOptions::InVisibleRect,
                Some(&bar),
                None,
            )
        };
        bar.addTrackingArea(&watch);
        bar
    };
    scroll.setScrollerStyle(NSScrollerStyle::Legacy);
    scroll.setVerticalScroller(Some(&bar(BAR, 100.0)));
    scroll.setHorizontalScroller(Some(&bar(100.0, BAR)));
    scroll.setAutohidesScrollers(true);
}
