use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawWindowHandle,
    Win32WindowHandle, WindowHandle,
};
use std::num::NonZeroIsize;

struct Owner(NonZeroIsize);
impl HasWindowHandle for Owner {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        // The editor window remains alive for the entire modal picker.
        Ok(unsafe {
            WindowHandle::borrow_raw(RawWindowHandle::Win32(Win32WindowHandle::new(self.0)))
        })
    }
}
impl HasDisplayHandle for Owner {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::windows())
    }
}
pub fn files(hwnd: isize) -> rfd::FileDialog {
    let dlg = rfd::FileDialog::new();
    if let Some(owner) = NonZeroIsize::new(hwnd) {
        dlg.set_parent(&Owner(owner))
    } else {
        dlg
    }
}

pub enum ConflictAction {
    Copy,
    Compare,
    Reload,
    Overwrite,
    Keep,
}
pub fn save_conflict(owner: isize, filename: &str, exists: bool, writable: bool) -> ConflictAction {
    use crate::win::{hwnd, wide};
    use windows_sys::Win32::UI::Controls::*;
    let mut actions = vec![(101, "Save As…")];
    if exists {
        actions.extend([
            (102, "Compare with disk"),
            (103, "Reload from disk and discard my edits"),
        ]);
    }
    if writable {
        actions.push((
            104,
            if exists {
                "Overwrite disk with my draft"
            } else {
                "Recreate the deleted file"
            },
        ));
    }
    actions.push((105, "Keep editing"));
    let names: Vec<_> = actions.iter().map(|(_, s)| wide(s)).collect();
    let buttons: Vec<_> = actions
        .iter()
        .zip(&names)
        .map(|((id, _), s)| TASKDIALOG_BUTTON {
            nButtonID: *id,
            pszButtonText: s.as_ptr(),
        })
        .collect();
    let title = wide("File changed on disk");
    let heading = wide(&format!("{filename} changed outside Linework"));
    let body = wide("Your draft is still open. Choose which version to keep.");
    unsafe {
        let mut config: TASKDIALOGCONFIG = std::mem::zeroed();
        config.cbSize = std::mem::size_of_val(&config) as u32;
        config.hwndParent = hwnd(owner);
        config.dwFlags = TDF_ALLOW_DIALOG_CANCELLATION | TDF_USE_COMMAND_LINKS;
        config.pszWindowTitle = title.as_ptr();
        config.pszMainInstruction = heading.as_ptr();
        config.pszContent = body.as_ptr();
        config.cButtons = buttons.len() as u32;
        config.pButtons = buttons.as_ptr();
        config.nDefaultButton = 101;
        let mut pressed = 0;
        TaskDialogIndirect(
            &config,
            &mut pressed,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        match pressed {
            101 => ConflictAction::Copy,
            102 => ConflictAction::Compare,
            103 => ConflictAction::Reload,
            104 => ConflictAction::Overwrite,
            _ => ConflictAction::Keep,
        }
    }
}
