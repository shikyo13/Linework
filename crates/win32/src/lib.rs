//! The Windows calls Linework's front end uses, carried out with AppKit.
//!
//! The front end in `src/` is written against windows-sys. On macOS it is built
//! against this crate under that name, so one front end serves both systems and
//! the two programs look and behave alike. `Win32` holds the declarations, made
//! by tools/win32-gen.py; `imp` holds what is written by hand.
#![cfg(target_os = "macos")]
#![allow(non_snake_case, clippy::missing_safety_doc)]
extern crate self as windows_sys;
pub mod Win32;
mod imp;
pub mod os;

/// The few basic types windows-sys keeps beside `Win32`.
pub mod core {
    pub type PCWSTR = *const u16;
    pub type PWSTR = *mut u16;
    pub type PCSTR = *const u8;
    pub type PSTR = *mut u8;
    pub type BSTR = *const u16;
    pub type HRESULT = i32;
    impl GUID {
        pub const fn from_u128(value: u128) -> Self {
            Self { data1: (value >> 96) as u32, data2: (value >> 80) as u16, data3: (value >> 64) as u16, data4: (value as u64).to_be_bytes() }
        }
    }
    pub use crate::w;

    /// A literal's UTF-16 form with its ending zero, made while compiling.
    /// `N` is the literal's length in bytes plus one; the literals are ASCII.
    pub const fn wide<const N: usize>(text: &str) -> [u16; N] {
        let bytes = text.as_bytes();
        let mut out = [0u16; N];
        let mut at = 0;
        while at < bytes.len() {
            assert!(bytes[at] < 128);
            out[at] = bytes[at] as u16;
            at += 1;
        }
        out
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct GUID {
        pub data1: u32,
        pub data2: u16,
        pub data3: u16,
        pub data4: [u8; 8],
    }
}

/// Says once that the front end called something not written yet.
pub fn missing(name: &'static str) {
    use std::sync::Mutex;
    static SEEN: Mutex<Vec<&str>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap();
    if !seen.contains(&name) {
        seen.push(name);
        eprintln!("linework-win32: {name} is not written yet");
    }
}

/// A literal as a pointer to zero-ended UTF-16, as windows-sys' `w!` gives.
#[macro_export]
macro_rules! w {
    ($text:literal) => {{
        static WIDE: [u16; $text.len() + 1] = $crate::core::wide($text);
        WIDE.as_ptr()
    }};
}
