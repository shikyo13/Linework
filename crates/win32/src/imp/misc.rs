//! The rest: what Windows is asked about itself, message boxes, files, and
//! calls that have nothing to do here.
use crate::Win32::Foundation::{HWND, RECT};
use crate::Win32::UI::WindowsAndMessaging::*;
use crate::imp::gdi::wide_name;
use crate::imp::window::mtm;
use objc2::{MainThreadOnly, sel};
use objc2_app_kit::{NSAlert, NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem};
use objc2_app_kit::{NSApplicationDelegate, NSWindowDelegate};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString, ns_string};

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

// --- The clipboard ------------------------------------------------------
// The front end puts text on the clipboard the Windows way: a block of memory
// holding UTF-16 with a zero at the end, handed over by its handle.

thread_local! {
    static BLOCKS: std::cell::RefCell<Vec<(usize, Vec<u8>)>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub unsafe fn GlobalAlloc(_: crate::Win32::System::Memory::GLOBAL_ALLOC_FLAGS, dwbytes: usize) -> crate::Win32::Foundation::HGLOBAL {
    BLOCKS.with_borrow_mut(|blocks| {
        // A whole number of 16-bit units, so the text in it can be read as such.
        let block = vec![0u8; dwbytes.max(2).next_multiple_of(2)];
        let handle = block.as_ptr() as usize;
        blocks.push((handle, block));
        handle as _
    })
}

pub unsafe fn GlobalLock(hmem: crate::Win32::Foundation::HGLOBAL) -> *mut core::ffi::c_void {
    hmem as _
}

pub unsafe fn GlobalUnlock(_: crate::Win32::Foundation::HGLOBAL) -> i32 {
    1
}

pub unsafe fn GlobalFree(hmem: crate::Win32::Foundation::HGLOBAL) -> crate::Win32::Foundation::HGLOBAL {
    BLOCKS.with_borrow_mut(|blocks| blocks.retain(|block| block.0 != hmem as usize));
    std::ptr::null_mut()
}

pub unsafe fn OpenClipboard(_: HWND) -> i32 {
    1
}

pub unsafe fn EmptyClipboard() -> i32 {
    objc2_app_kit::NSPasteboard::generalPasteboard().clearContents();
    1
}

pub unsafe fn CloseClipboard() -> i32 {
    1
}

pub unsafe fn SetClipboardData(uformat: u32, hmem: crate::Win32::Foundation::HANDLE) -> crate::Win32::Foundation::HANDLE {
    // 13 is Unicode text, the only kind the front end sets.
    if uformat != 13 {
        return std::ptr::null_mut();
    }
    let text = BLOCKS.with_borrow_mut(|blocks| {
        let at = blocks.iter().position(|block| block.0 == hmem as usize)?;
        let (_, block) = blocks.remove(at);
        let units: Vec<u16> = block.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).take_while(|unit| *unit != 0).collect();
        Some(String::from_utf16_lossy(&units))
    });
    let Some(text) = text else { return std::ptr::null_mut() };
    let board = objc2_app_kit::NSPasteboard::generalPasteboard();
    board.clearContents();
    board.setString_forType(&NSString::from_str(&text), unsafe { objc2_app_kit::NSPasteboardTypeString });
    hmem
}

// --- What Windows is asked about itself ---------------------------------

pub unsafe fn SystemParametersInfoW(uiaction: SYSTEM_PARAMETERS_INFO_ACTION, _: u32, pvparam: *mut core::ffi::c_void, _: SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS) -> i32 {
    if pvparam.is_null() {
        return 0;
    }
    match uiaction {
        // High contrast: the structure stays as given, with no flag set.
        SPI_GETHIGHCONTRAST => 1,
        // Whether windows animate: not when the person has asked for less motion.
        SPI_GETCLIENTAREAANIMATION => {
            let still = objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion();
            unsafe { *(pvparam as *mut i32) = !still as i32 };
            1
        }
        SPI_GETWHEELSCROLLLINES => {
            unsafe { *(pvparam as *mut u32) = 3 };
            1
        }
        _ => 0,
    }
}

pub unsafe fn SystemParametersInfoForDpi(uiaction: u32, _: u32, pvparam: *mut core::ffi::c_void, _: u32, _: u32) -> i32 {
    // Asked for the menu font: Segoe UI at 9 points, 12 pixels, on Windows.
    if uiaction != SPI_GETNONCLIENTMETRICS || pvparam.is_null() {
        return 0;
    }
    let metrics = unsafe { &mut *(pvparam as *mut NONCLIENTMETRICSW) };
    metrics.iMenuHeight = 19;
    metrics.lfMenuFont.lfHeight = -12;
    metrics.lfMenuFont.lfWeight = 400;
    for (at, unit) in "Segoe UI".encode_utf16().enumerate() {
        metrics.lfMenuFont.lfFaceName[at] = unit;
    }
    1
}

pub unsafe fn GetSysColor(nindex: crate::Win32::Graphics::Gdi::SYS_COLOR_INDEX) -> u32 {
    use crate::Win32::Graphics::Gdi::*;
    let dark = crate::imp::controls::DARK.get();
    match nindex {
        COLOR_WINDOWTEXT | COLOR_INFOTEXT | COLOR_BTNTEXT | COLOR_MENUTEXT => if dark { 0xF7EDF0 } else { 0x322326 },
        COLOR_HIGHLIGHT | COLOR_HOTLIGHT => if dark { 0xF5B2CC } else { 0x974C68 },
        COLOR_HIGHLIGHTTEXT => if dark { 0x1F1918 } else { 0xFFFFFF },
        COLOR_GRAYTEXT => 0xB2A1A3,
        _ => if dark { 0x282120 } else { 0xFFFFFF },
    }
}

// AppKit already draws a window into a buffer and shows it whole, so a control
// asking Windows for a buffer of its own is told to draw straight away.
pub unsafe fn BufferedPaintInit() -> crate::core::HRESULT {
    0
}

pub unsafe fn BufferedPaintUnInit() -> crate::core::HRESULT {
    0
}

pub unsafe fn BeginBufferedPaint(_: crate::Win32::Graphics::Gdi::HDC, _: *const RECT, _: crate::Win32::UI::Controls::BP_BUFFERFORMAT, _: *const crate::Win32::UI::Controls::BP_PAINTPARAMS, _: *mut crate::Win32::Graphics::Gdi::HDC) -> isize {
    0
}

pub unsafe fn EndBufferedPaint(_: isize, _: i32) -> crate::core::HRESULT {
    0
}

// Windows' own pictures of scroll bar parts: there are none here, and the
// front end draws its own when told so.
pub unsafe fn OpenThemeData(_: HWND, _: *const u16) -> crate::Win32::UI::Controls::HTHEME {
    0
}

pub unsafe fn CloseThemeData(_: crate::Win32::UI::Controls::HTHEME) -> crate::core::HRESULT {
    0
}

pub unsafe fn DrawThemeBackground(_: crate::Win32::UI::Controls::HTHEME, _: crate::Win32::Graphics::Gdi::HDC, _: i32, _: i32, _: *const RECT, _: *const RECT) -> crate::core::HRESULT {
    -1
}

pub unsafe fn GetScrollBarInfo(_: HWND, _: OBJECT_IDENTIFIER, _: *mut SCROLLBARINFO) -> i32 {
    0
}

pub unsafe fn MonitorFromWindow(_: HWND, _: crate::Win32::Graphics::Gdi::MONITOR_FROM_FLAGS) -> crate::Win32::Graphics::Gdi::HMONITOR {
    1 as _
}

pub unsafe fn GetMonitorInfoW(_: crate::Win32::Graphics::Gdi::HMONITOR, lpmi: *mut crate::Win32::Graphics::Gdi::MONITORINFO) -> i32 {
    let Some(info) = (unsafe { lpmi.as_mut() }) else { return 0 };
    let Some(screen) = objc2_app_kit::NSScreen::mainScreen(mtm()) else { return 0 };
    let (whole, usable) = (screen.frame(), screen.visibleFrame());
    let rect = |part: objc2_foundation::NSRect| RECT {
        left: part.origin.x as i32,
        top: (whole.origin.y + whole.size.height - part.origin.y - part.size.height) as i32,
        right: (part.origin.x + part.size.width) as i32,
        bottom: (whole.origin.y + whole.size.height - part.origin.y) as i32,
    };
    (info.rcMonitor, info.rcWork) = (rect(whole), rect(usable));
    1
}

pub unsafe fn SetLayeredWindowAttributes(hwnd: HWND, _: u32, balpha: u8, dwflags: LAYERED_WINDOW_ATTRIBUTES_FLAGS) -> i32 {
    if dwflags & LWA_ALPHA != 0 {
        if let Some(frame) = crate::imp::window::find(hwnd).and_then(|window| window.frame.borrow().clone()) {
            frame.setAlphaValue(balpha as f64 / 255.0);
        }
    }
    1
}

// --- Other programs -----------------------------------------------------
// A second copy handing its files to the first is not written yet, so no
// running copy is ever found.

pub unsafe fn FindWindowExW(_: HWND, _: HWND, _: *const u16, _: *const u16) -> HWND {
    std::ptr::null_mut()
}

pub unsafe fn GetWindowThreadProcessId(_: HWND, lpdwprocessid: *mut u32) -> u32 {
    if let Some(id) = unsafe { lpdwprocessid.as_mut() } {
        *id = std::process::id();
    }
    1
}

pub unsafe fn OpenProcess(_: crate::Win32::System::Threading::PROCESS_ACCESS_RIGHTS, _: i32, _: u32) -> crate::Win32::Foundation::HANDLE {
    std::ptr::null_mut()
}

pub unsafe fn CreateEventW(_: *const crate::Win32::Security::SECURITY_ATTRIBUTES, _: i32, _: i32, _: *const u16) -> crate::Win32::Foundation::HANDLE {
    1 as _
}

pub unsafe fn CloseHandle(_: crate::Win32::Foundation::HANDLE) -> i32 {
    1
}

/// Nothing here is ever signalled, so the wait lasts its time, or a twentieth
/// of a second when it has none, and reports that the time ran out.
pub unsafe fn WaitForSingleObject(_: crate::Win32::Foundation::HANDLE, dwmilliseconds: u32) -> crate::Win32::Foundation::WAIT_EVENT {
    std::thread::sleep(std::time::Duration::from_millis(dwmilliseconds.min(50) as u64));
    258
}

pub unsafe fn SendMessageTimeoutW(hwnd: HWND, msg: u32, wparam: usize, lparam: isize, _: SEND_MESSAGE_TIMEOUT_FLAGS, _: u32, lpdwresult: *mut usize) -> isize {
    let result = crate::imp::window::send(hwnd, msg, wparam, lparam);
    if let Some(out) = unsafe { lpdwresult.as_mut() } {
        *out = result as usize;
    }
    1
}

// --- Files handed to the program ----------------------------------------
// Dropped on a window, or opened with the program from the Finder: either way
// the window is sent WM_DROPFILES with a handle to the list of paths.

pub(crate) fn drop_files(hwnd: HWND, paths: Vec<String>) {
    if paths.is_empty() {
        return;
    }
    let handle = Box::into_raw(Box::new(paths));
    unsafe { crate::imp::message::PostMessageW(hwnd, WM_DROPFILES, handle as usize, 0) };
}

pub unsafe fn DragQueryFileW(hdrop: crate::Win32::UI::Shell::HDROP, ifile: u32, lpszfile: *mut u16, cch: u32) -> u32 {
    let Some(paths) = (unsafe { (hdrop as *const Vec<String>).as_ref() }) else { return 0 };
    if ifile == u32::MAX {
        return paths.len() as u32;
    }
    let Some(path) = paths.get(ifile as usize) else { return 0 };
    let units: Vec<u16> = path.encode_utf16().collect();
    if lpszfile.is_null() {
        return units.len() as u32;
    }
    let room = (cch as usize).saturating_sub(1).min(units.len());
    unsafe {
        std::ptr::copy_nonoverlapping(units.as_ptr(), lpszfile, room);
        *lpszfile.add(room) = 0;
    }
    room as u32
}

pub unsafe fn DragFinish(hdrop: crate::Win32::UI::Shell::HDROP) {
    if !hdrop.is_null() {
        drop(unsafe { Box::from_raw(hdrop as *mut Vec<String>) });
    }
}

objc2::define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `Launching` has no `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    struct Launching;

    unsafe impl NSObjectProtocol for Launching {}

    unsafe impl NSApplicationDelegate for Launching {
        #[unsafe(method(application:openFiles:))]
        fn open_files(&self, _: &NSApplication, files: &objc2_foundation::NSArray<NSString>) {
            if let Some(main) = crate::imp::window::main_window() {
                drop_files(main, files.iter().map(|file| file.to_string()).collect());
            }
        }

        // Quitting from the menu or the Dock closes the window the way its own
        // close button does, so unsaved work is asked about; the program ends
        // when its loop does.
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _: &NSApplication) -> objc2_app_kit::NSApplicationTerminateReply {
            match crate::imp::window::main_window() {
                Some(main) => {
                    unsafe { crate::imp::message::PostMessageW(main, WM_CLOSE, 0, 0) };
                    objc2_app_kit::NSApplicationTerminateReply::TerminateCancel
                }
                None => objc2_app_kit::NSApplicationTerminateReply::TerminateNow,
            }
        }
    }
);

thread_local! {
    static LAUNCHING: std::cell::OnceCell<objc2::rc::Retained<Launching>> = const { std::cell::OnceCell::new() };
}

/// Has the application report files opened with it and requests to quit.
pub(crate) fn watch_application(app: &NSApplication) {
    LAUNCHING.with(|cell| {
        let watcher = cell.get_or_init(|| unsafe { objc2::msg_send![Launching::alloc(mtm()), init] });
        app.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&**watcher)));
    });
}

// --- Questions with named answers ---------------------------------------

pub unsafe fn TaskDialogIndirect(ptaskconfig: *const crate::Win32::UI::Controls::TASKDIALOGCONFIG, pnbutton: *mut i32, _: *mut i32, _: *mut i32) -> crate::core::HRESULT {
    if ptaskconfig.is_null() {
        return -1;
    }
    // The structure is packed, so each field is copied out before it is used.
    let config = unsafe { std::ptr::read_unaligned(ptaskconfig) };
    let (heading, body, count, buttons) = (config.pszMainInstruction, config.pszContent, config.cButtons, config.pButtons);
    let alert = NSAlert::new(mtm());
    alert.setMessageText(&NSString::from_str(&unsafe { wide_name(heading) }));
    alert.setInformativeText(&NSString::from_str(&unsafe { wide_name(body) }));
    let mut ids = Vec::new();
    for at in 0..count as usize {
        let button = unsafe { std::ptr::read_unaligned(buttons.add(at)) };
        let (id, text) = (button.nButtonID, button.pszButtonText);
        // A command link's second line is its explanation; the button takes the first.
        let title = unsafe { wide_name(text) };
        alert.addButtonWithTitle(&NSString::from_str(title.lines().next().unwrap_or("")));
        ids.push(id);
    }
    if ids.is_empty() {
        alert.addButtonWithTitle(ns_string!("OK"));
        ids.push(IDOK);
    }
    let chosen = (alert.runModal() - 1000).clamp(0, ids.len() as isize - 1) as usize;
    if let Some(out) = unsafe { pnbutton.as_mut() } {
        *out = ids[chosen];
    }
    0
}

objc2::define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `Closing` has no `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    struct Closing;

    unsafe impl NSObjectProtocol for Closing {}

    unsafe impl NSWindowDelegate for Closing {
        #[unsafe(method(windowWillClose:))]
        fn closing(&self, _: &objc2_foundation::NSNotification) {
            crate::imp::window::app().stopModal();
        }
    }
);

/// The Mac's colour panel, shown until it is closed; the colour it then holds
/// is the answer.
pub unsafe fn ChooseColorW(param0: *mut crate::Win32::UI::Controls::Dialogs::CHOOSECOLORW) -> i32 {
    let Some(asked) = (unsafe { param0.as_mut() }) else { return 0 };
    let mtm = mtm();
    let panel = objc2_app_kit::NSColorPanel::sharedColorPanel(mtm);
    panel.setShowsAlpha(false);
    panel.setColor(&crate::imp::gdi::color(asked.rgbResult));
    let closing: objc2::rc::Retained<Closing> = unsafe { objc2::msg_send![Closing::alloc(mtm), init] };
    panel.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*closing)));
    crate::imp::window::app().runModalForWindow(&panel);
    panel.setDelegate(None);
    let Some(color) = panel.color().colorUsingColorSpace(&objc2_app_kit::NSColorSpace::sRGBColorSpace()) else { return 0 };
    let part = |value: f64| (value.clamp(0.0, 1.0) * 255.0).round() as u32;
    asked.rgbResult = part(color.redComponent()) | (part(color.greenComponent()) << 8) | (part(color.blueComponent()) << 16);
    1
}
