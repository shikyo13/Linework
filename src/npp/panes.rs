use super::*;
use linework_editor::{
    Editor,
    sci::{SCI_SETDOCPOINTER, SCI_SETREADONLY},
};

// Plugins retain both Scintilla HWNDs and direct-call pointers. Keep those
// genuine controls stable, and paint paged documents in separate controls.
// Even a modeless plugin or worker with an old handle cannot edit a page.
thread_local! {
    static SPARE:RefCell<[Option<Editor>;2]>=const{RefCell::new([None,None])};
}
pub(super) fn prepare(app: &mut App) -> Result<(), String> {
    for p in 0..2 {
        if SPARE.with_borrow(|s| s[p].is_none()) {
            let ed = events::quiet(|| Editor::create(app.h, 720 + p as i32))?;
            show(ed.hwnd(), false);
            SPARE.with_borrow_mut(|s| s[p] = Some(ed));
        }
    }
    for p in 0..2 {
        if let Some(i) =
            app.assigned[p].and_then(|id| app.buffers.iter().position(|b| b.id == id && b.large))
        {
            let at = events::point(p, app.editors[p].caret() as u64);
            app.bind(p, i, at);
        }
    }
    app.layout();
    Ok(())
}
pub(super) fn switch(app: &mut App, p: usize, large: bool) {
    let original = HOST.with_borrow(|h| if p == 0 { h.data.main } else { h.data.second });
    if original == 0 || (app.editors[p].hwnd() != original) == large {
        return;
    }
    let Some(next) = SPARE.with_borrow_mut(|s| s[p].take()) else {
        return;
    };
    events::quiet(|| {
        let previous = std::mem::replace(&mut app.editors[p], next);
        let old = previous.hwnd();
        show(old, false);
        unsafe {
            SetWindowLongPtrW(hwnd(old), GWLP_ID, 720 + p as isize);
        }
        // Null builds a fresh Scintilla document. Neither the visible page nor
        // the previous ordinary document remains attached to this control.
        msg(old, SCI_SETDOCPOINTER, 0, 0);
        msg(old, SCI_SETREADONLY, 1, 0);
        let new = app.editors[p].hwnd();
        unsafe {
            SetWindowLongPtrW(hwnd(new), GWLP_ID, 700 + p as isize);
        }
        SPARE.with_borrow_mut(|s| s[p] = Some(previous));
        app.npp_style_view(p);
    });
}
