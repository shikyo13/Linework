#![cfg(windows)]
use linework_editor::Editor;
use std::{path::Path, ptr};
use windows_sys::Win32::{System::LibraryLoader::GetModuleHandleW, UI::WindowsAndMessaging::*};
mod workflows;

// Create and test the real DLL control on an isolated, never-switched desktop.
// A completion list opened during this test cannot appear on the user's desktop.
#[test]
fn native_editor_workflows() {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn CreateDesktopW(
            name: *const u16,
            device: *const u16,
            devmode: *const std::ffi::c_void,
            flags: u32,
            access: u32,
            sa: *const std::ffi::c_void,
        ) -> isize;
        fn SetThreadDesktop(desktop: isize) -> i32;
        fn GetThreadDesktop(thread: u32) -> isize;
        fn CloseDesktop(desktop: isize) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    let desktop_name: Vec<_> = format!("LineworkEditorTest{}", std::process::id())
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let old = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
    let desktop = unsafe {
        CreateDesktopW(
            desktop_name.as_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            0x1ff,
            ptr::null(),
        )
    };
    assert_ne!(desktop, 0);
    assert_ne!(unsafe { SetThreadDesktop(desktop) }, 0);
    let base = Path::new(env!("CARGO_MANIFEST_DIR"));
    if std::env::var_os("LINEWORK_RUNTIME").is_none() {
        unsafe {
            std::env::set_var("LINEWORK_RUNTIME", base.join("../../runtime"));
        }
    }
    let class = "STATIC".encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let parent = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            ptr::null(),
            WS_POPUP,
            0,
            0,
            1000,
            600,
            ptr::null_mut(),
            ptr::null_mut(),
            GetModuleHandleW(ptr::null()),
            ptr::null(),
        )
    };
    assert!(!parent.is_null());
    let e = Editor::create(parent as isize, 100).expect("native editor");
    workflows::run(&e, || unsafe {
        MoveWindow(e.hwnd() as _, 0, 0, 840, 550, 0);
    });
    drop(e);
    unsafe {
        DestroyWindow(parent);
        SetThreadDesktop(old);
        CloseDesktop(desktop);
    }
}
