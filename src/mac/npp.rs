//! Notepad++ plugins are Windows programs, so macOS has no host for them. The
//! front end's calls into the host are kept, and all but the one that lends the
//! application to a nested call do nothing.
use crate::{app::App, events, win::H};
use std::{
    cell::Cell,
    path::{Path, PathBuf},
    ptr::null_mut,
};
use windows_sys::Win32::{Foundation::HWND, UI::WindowsAndMessaging::MSG};

thread_local! { static LIVE: Cell<*mut App> = const { Cell::new(null_mut()) }; }
struct Restore(*mut App);
impl Drop for Restore {
    fn drop(&mut self) {
        LIVE.set(self.0);
    }
}
/// A call into the system may come back into the window's handler before it
/// returns. The application is lent to one such nested request at a time.
pub fn dispatch<T>(app: &mut App, f: impl FnOnce() -> T) -> T {
    let _restore = Restore(LIVE.replace(app as *mut App));
    f()
}
/// A window being resized runs inside the system's own loop, before the outer
/// event queue can drain.
pub(crate) fn live_resize(host: H) -> bool {
    let ptr = LIVE.replace(null_mut());
    let _restore = Restore(ptr);
    if ptr.is_null() {
        return false;
    }
    let app = unsafe { &mut *ptr };
    if app.h != host {
        return false;
    }
    app.resize_client();
    true
}
pub(crate) fn home() -> PathBuf {
    crate::state::profile().join("plugins")
}
pub fn initialize(_: &mut App) {}
pub fn ready(_: &mut App) {}
pub fn pump(_: &mut App) {}
pub fn shutdown(_: &mut App) {}
pub(crate) fn invoke(_: &mut App, _: i32) -> bool {
    false
}
pub fn pretranslate(_: &mut App, _: &MSG) -> bool {
    false
}
pub struct NotifyAfter;
pub fn after_notification(_: isize, _: events::Binding) -> NotifyAfter {
    NotifyAfter
}
pub fn message(_: HWND, _: u32, _: usize, _: isize) -> Option<isize> {
    None
}
pub(crate) fn rows() -> Vec<(i32, String)> {
    Vec::new()
}
pub(crate) fn attach_new(_: &mut App, _: &Path) -> Result<(), String> {
    Err("Notepad++ plugins run on Windows only.".into())
}
pub fn file_event(_: &mut App, _: u32, _: u64) {}
pub(crate) fn switch_pane(_: &mut App, _: usize, _: bool) {}
pub(crate) fn checkpoint_pages(_: &mut App) -> bool {
    true
}
