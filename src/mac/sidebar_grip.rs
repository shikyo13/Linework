//! On Windows a second thread owns the sidebar's edge while it is dragged, so
//! the drag keeps up when the editor holds the main thread for a frame. macOS
//! draws on one thread and has no such stall, so the window handles the drag
//! itself, which is also what Windows does when that thread cannot start.
use crate::win::H;

pub(crate) fn owns_edge() -> bool {
    false
}
pub(crate) fn captured() -> bool {
    false
}
pub(crate) fn start(_: H) {}
pub(crate) fn layout(_: H, _: bool, _: i32, _: bool) {}
pub(crate) fn host_message(_: H, _: u32, _: usize, _: isize) -> bool {
    false
}
pub(crate) fn shutdown() {}
pub(crate) fn editor_changed(_: H) {}
pub(crate) fn artwork_changed() {}
pub(crate) fn editor_painted(_: H) {}
