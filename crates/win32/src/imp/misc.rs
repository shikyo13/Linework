//! The rest: what Windows is asked about itself, message boxes, files, and
//! calls that have nothing to do here.
use crate::Win32::Foundation::{HWND, RECT};
use crate::Win32::UI::WindowsAndMessaging::*;
use crate::imp::gdi::wide_name;
use crate::imp::window::mtm;
use objc2::{MainThreadOnly, sel};
use objc2_app_kit::{NSAlert, NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem};
use objc2_foundation::{NSString, ns_string};

/// macOS always shows a menu bar with the program's own menu first. Linework's
/// menus are inside its window, as on Windows, so this one holds only what the
/// system expects there.
pub(crate) fn install_menu(app: &NSApplication) {
    let mtm = mtm();
    let bar = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!(""));
    let own = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Linework"));
    for (title, action, key) in [("Hide Linework", sel!(hide:), "h"), ("Quit Linework", sel!(terminate:), "q")] {
        let item = unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::from_str(title), Some(action), &NSString::from_str(key)) };
        item.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);
        own.addItem(&item);
    }
    let holder = NSMenuItem::new(mtm);
    holder.setSubmenu(Some(&own));
    bar.addItem(&holder);
    app.setMainMenu(Some(&bar));
}

// One Windows pixel is one point here, so the scale never changes.
pub unsafe fn GetDpiForWindow(_: HWND) -> u32 {
    96
}

pub unsafe fn SetProcessDpiAwarenessContext(_: crate::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT) -> i32 {
    1
}

pub unsafe fn GetSystemMetricsForDpi(_: SYSTEM_METRICS_INDEX, _: u32) -> i32 {
    0
}

pub unsafe fn CoInitializeEx(_: *const core::ffi::c_void, _: u32) -> crate::core::HRESULT {
    0
}

pub unsafe fn CoUninitialize() {}

pub unsafe fn GetModuleHandleW(_: *const u16) -> crate::Win32::Foundation::HMODULE {
    1 as _
}

pub unsafe fn GetLastError() -> crate::Win32::Foundation::WIN32_ERROR {
    0
}

pub unsafe fn DwmSetWindowAttribute(_: HWND, _: u32, _: *const core::ffi::c_void, _: u32) -> crate::core::HRESULT {
    0
}

pub unsafe fn DwmFlush() -> crate::core::HRESULT {
    0
}

pub unsafe fn SetWindowTheme(_: HWND, _: *const u16, _: *const u16) -> crate::core::HRESULT {
    0
}

pub unsafe fn AllowSetForegroundWindow(_: u32) -> i32 {
    1
}

pub unsafe fn MessageBoxW(_: HWND, lptext: *const u16, lpcaption: *const u16, utype: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    let alert = NSAlert::new(mtm());
    alert.setMessageText(&NSString::from_str(&unsafe { wide_name(lpcaption) }));
    alert.setInformativeText(&NSString::from_str(&unsafe { wide_name(lptext) }));
    let answers: &[(&str, MESSAGEBOX_RESULT)] = match utype & 0xf {
        MB_YESNOCANCEL => &[("Yes", IDYES), ("No", IDNO), ("Cancel", IDCANCEL)],
        MB_YESNO => &[("Yes", IDYES), ("No", IDNO)],
        MB_OKCANCEL => &[("OK", IDOK), ("Cancel", IDCANCEL)],
        _ => &[("OK", IDOK)],
    };
    for (title, _) in answers {
        alert.addButtonWithTitle(&NSString::from_str(title));
    }
    // The first button answers 1000, the next 1001, and so on.
    let chosen = (alert.runModal() - 1000).clamp(0, answers.len() as isize - 1) as usize;
    answers[chosen].1
}

pub unsafe fn MoveFileExW(lpexistingfilename: *const u16, lpnewfilename: *const u16, _: crate::Win32::Storage::FileSystem::MOVE_FILE_FLAGS) -> i32 {
    let (from, to) = unsafe { (wide_name(lpexistingfilename), wide_name(lpnewfilename)) };
    // A rename replaces what is there in one step, which is all that is asked.
    std::fs::rename(from, to).is_ok() as i32
}

#[allow(dead_code)]
fn unused(_: RECT) {}
