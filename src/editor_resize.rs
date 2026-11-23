//! Scintilla resizes its Direct2D target synchronously in WM_SIZE. During a
//! divider drag, commit the actual HWND geometry immediately and defer that
//! GPU resize until the editor is about to paint its new viewport.
use crate::{events, win::*};
use std::cell::Cell;
use windows_sys::Win32::{
    Foundation::HWND,
    UI::{Shell::*, WindowsAndMessaging::*},
};

pub(crate) const FLUSH: u32 = WM_APP + 167;
#[derive(Default)]
struct State {
    size: Cell<Option<(usize, isize)>>,
}
thread_local! { static APPLYING: Cell<H> = const { Cell::new(0) }; }
pub(crate) fn install(h: H) {
    let data = Box::into_raw(Box::<State>::default());
    unsafe {
        if SetWindowSubclass(hwnd(h), Some(proc), 1, data as usize) == 0 {
            drop(Box::from_raw(data));
        }
    }
}
unsafe extern "system" fn proc(
    h: HWND,
    m: u32,
    w: usize,
    l: isize,
    id: usize,
    data: usize,
) -> isize {
    unsafe {
        let s = &*(data as *const State);
        match m {
            WM_SIZE if events::divider_dragging() && APPLYING.get() != h as H => {
                s.size.set(Some((w, l)));
                invalidate(h as H);
                return 0;
            }
            WM_PAINT | FLUSH => {
                if let Some((flags, size)) = s.size.take() {
                    let previous = APPLYING.replace(h as H);
                    SendMessageW(h, WM_SIZE, flags, size);
                    APPLYING.set(previous);
                }
                if m == FLUSH {
                    return 0;
                }
            }
            WM_SIZE => s.size.set(None),
            WM_NCDESTROY => {
                RemoveWindowSubclass(h, Some(proc), id);
                drop(Box::from_raw(data as *mut State));
            }
            _ => {}
        }
        let result = DefSubclassProc(h, m, w, l);
        if m == WM_PAINT {
            crate::sidebar_grip::editor_painted(h as H);
        }
        result
    }
}
