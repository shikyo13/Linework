//! The standard controls: text fields, lists, buttons, labels, scroll bars.
use crate::imp::window::{Proc, Window};
use objc2::rc::Retained;
use objc2_app_kit::NSView;

/// The window procedure of a class Windows itself provides.
pub(crate) fn proc_of(_class: &str) -> Option<Proc> {
    None
}

/// The view inside a control that takes its keys, when it is not the control's own.
pub(crate) fn key_view(_window: &Window) -> Option<Retained<NSView>> {
    None
}
