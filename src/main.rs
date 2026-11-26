#![cfg_attr(not(test), windows_subsystem = "windows")]
mod app;
mod appearance;
mod chrome;
mod compare;
mod review;
mod commands;
mod dialogs;
mod events;
mod editor_resize;
mod forms;
mod frame;
mod glide;
mod grammar_ui;
mod launch;
mod macro_manager;
mod macros;
mod mdsmart;
mod minimap;
mod npp;
mod project_ui;
mod preview;
mod sci;
mod selection;
mod sidebar_grip;
mod snippet_ui;
mod state;
mod sublime;
mod surface;
mod win;
use crate::{app::App, win::*};
use std::{
    mem::zeroed,
    ptr::{null, null_mut},
};
use windows_sys::Win32::UI::{HiDpi::*, WindowsAndMessaging::*};
fn main() {
    if let Err(e) = run() {
        alert(0, "Linework", &e)
    }
}
fn run() -> Result<(), String> {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let launch = launch::Launch::parse(std::env::args_os().skip(1));
        if launch::forward(&launch)? { return Ok(()); }
        windows_sys::Win32::System::Com::CoInitializeEx(
            null(),
            windows_sys::Win32::System::Com::COINIT_APARTMENTTHREADED as u32,
        );
        register("LineworkMain", Some(events::proc))?;
        surface::initialize()?;
        forms::init()?;
        let h = CreateWindowExW(
            WS_EX_ACCEPTFILES,
            wide("LineworkMain").as_ptr(),
            wide("Linework").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1250,
            825,
            null_mut(),
            null_mut(),
            module() as _,
            null(),
        ) as H;
        if h == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut app = App::new(h)?;
        sidebar_grip::start(h);
        glide::start(h);
        npp::initialize(&mut app);
        // As the launcher asked (Explorer and shortcuts ask for a normal, active window);
        // a launch that asks for no activation leaves the keyboard where it is.
        ShowWindow(hwnd(h), SW_SHOWDEFAULT);
        app.layout();
        launch::register_window(hwnd(h));
        app.start(launch.targets);
        npp::ready(&mut app);
        SetTimer(hwnd(h), 1, 140, None);
        let mut m: MSG = zeroed();
        loop {
            app.dispatch_pending();
            npp::pump(&mut app);
            if app.exiting {
                break;
            }
            if npp::dispatch(&mut app, || GetMessageW(&mut m, null_mut(), 0, 0)) <= 0 {
                break;
            }
            let altgr =
                matches!(m.message, WM_KEYDOWN | WM_SYSKEYDOWN) && crate::win::altgr_text(&m);
            if !altgr && npp::pretranslate(&mut app, &m) {
                continue;
            }
            if !altgr
                && (m.message == WM_KEYDOWN || m.message == WM_SYSKEYDOWN)
                && app.key(m.wParam as u32)
            {
                continue;
            }
            if m.message == WM_CHAR
                && (app.global_char(m.hwnd as isize, m.wParam as u16)
                    || app.smart_char(m.hwnd as isize, m.wParam as u16))
            {
                continue;
            }
            TranslateMessage(&m);
            npp::dispatch(&mut app, || {
                DispatchMessageW(&m);
            });
        }
        Ok(())
    }
}
