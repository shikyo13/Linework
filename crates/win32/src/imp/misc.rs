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

/// The first thing the front end calls, so the fonts the program carries are
/// made known here, before any window asks for one.
pub unsafe fn SetProcessDpiAwarenessContext(_: crate::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT) -> i32 {
    fonts();
    1
}

#[link(name = "CoreText", kind = "framework")]
unsafe extern "C" {
    fn CTFontManagerRegisterFontsForURL(url: *const core::ffi::c_void, scope: u32, error: *mut *mut core::ffi::c_void) -> bool;
}

/// Makes the program's own font files usable by name, for this process only.
/// In the application bundle they are in Resources/fonts; in a build run from
/// the source tree they are in assets/mac/fonts.
fn fonts() {
    let beside = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.parent()?.join("Resources/fonts")));
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/mac/fonts");
    let Some(folder) = beside.into_iter().chain(Some(source)).find(|folder| folder.is_dir()) else {
        return eprintln!("linework-win32: the program's fonts were not found");
    };
    let Ok(files) = std::fs::read_dir(folder) else { return };
    for file in files.flatten().map(|file| file.path()).filter(|file| file.extension().is_some_and(|kind| kind == "ttf")) {
        let url = objc2_foundation::NSURL::fileURLWithPath(&NSString::from_str(&file.to_string_lossy()));
        // 1: for this process, not installed for the person.
        let known = unsafe { CTFontManagerRegisterFontsForURL(objc2::rc::Retained::as_ptr(&url).cast(), 1, std::ptr::null_mut()) };
        if !known {
            eprintln!("linework-win32: could not make {} known as a font", file.display());
        }
    }
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

pub unsafe fn DwmSetWindowAttribute(hwnd: HWND, dwattribute: u32, pvattribute: *const core::ffi::c_void, _: u32) -> crate::core::HRESULT {
    // 20 asks for a dark title bar: here, the dark look for the window and for
    // the controls drawn in it.
    if dwattribute == 20 && !pvattribute.is_null() {
        let dark = unsafe { *(pvattribute as *const i32) } != 0;
        crate::imp::controls::DARK.set(dark);
        if let Some(frame) = crate::imp::window::find(hwnd).and_then(|window| window.frame.borrow().clone()) {
            use objc2_app_kit::NSAppearanceCustomization;
            let name = unsafe { if dark { objc2_app_kit::NSAppearanceNameDarkAqua } else { objc2_app_kit::NSAppearanceNameAqua } };
            frame.setAppearance(objc2_app_kit::NSAppearance::appearanceNamed(name).as_deref());
        }
    }
    0
}

/// Asked so that leaving a control can be told; every window of ours is told anyway.
pub unsafe fn TrackMouseEvent(_: *mut crate::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT) -> i32 {
    1
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
