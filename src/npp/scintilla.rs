use super::H;
use linework_editor::sci::*;
use windows_sys::Win32::{
    Foundation::HWND,
    UI::{
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{GetPropW, RemovePropW, SendMessageW, SetPropW, WM_NCDESTROY},
    },
};

type Direct = unsafe extern "C" fn(isize, u32, usize, isize) -> isize;
type DirectStatus = unsafe extern "C" fn(isize, u32, usize, isize, *mut i32) -> isize;
struct Native {
    hwnd: H,
    data: isize,
    call: Direct,
    status: DirectStatus,
}
const SUBCLASS: usize = 0x4e505058;

// Notepad++ x64 uses pointer-width Sci_PositionCR for the four historical
// range structures. Stock Windows Scintilla uses LONG there, and exposes
// pointer-width versions under the FULL message IDs.
fn translate(m: u32) -> u32 {
    match m {
        SCI_GETSTYLEDTEXT => SCI_GETSTYLEDTEXTFULL,
        SCI_GETTEXTRANGE => SCI_GETTEXTRANGEFULL,
        SCI_FINDTEXT => SCI_FINDTEXTFULL,
        SCI_FORMATRANGE => SCI_FORMATRANGEFULL,
        _ => m,
    }
}
fn entry(ctx: isize, m: u32) -> Option<isize> {
    match m {
        SCI_GETDIRECTPOINTER => Some(ctx),
        SCI_GETDIRECTFUNCTION => Some(direct as *const () as isize),
        SCI_GETDIRECTSTATUSFUNCTION => Some(direct_status as *const () as isize),
        _ => None,
    }
}
unsafe extern "C" fn direct(ctx: isize, m: u32, w: usize, l: isize) -> isize {
    if let Some(value) = entry(ctx, m) {
        return value;
    }
    let native = unsafe { &*(ctx as *const Native) };
    if let Some(value) = super::paged::request(native.hwnd, translate(m), w, l) {
        return value;
    }
    unsafe { (native.call)(native.data, translate(m), w, l) }
}
unsafe extern "C" fn direct_status(
    ctx: isize,
    m: u32,
    w: usize,
    l: isize,
    status: *mut i32,
) -> isize {
    if let Some(value) = entry(ctx, m) {
        if !status.is_null() {
            unsafe { *status = 0 }
        }
        return value;
    }
    let native = unsafe { &*(ctx as *const Native) };
    if let Some(value) = super::paged::request(native.hwnd, translate(m), w, l) {
        if !status.is_null() {
            unsafe {
                *status =
                    super::paged::request(native.hwnd, SCI_GETSTATUS, 0, 0).unwrap_or(1) as i32;
            }
        }
        return value;
    }
    unsafe { (native.status)(native.data, translate(m), w, l, status) }
}
unsafe extern "system" fn proc(
    h: HWND,
    m: u32,
    w: usize,
    l: isize,
    _id: usize,
    ctx: usize,
) -> isize {
    if let Some(value) = entry(ctx as isize, m) {
        return value;
    }
    if let Some(value) = super::paged::request(h as H, translate(m), w, l) {
        return value;
    }
    let value = unsafe { DefSubclassProc(h, translate(m), w, l) };
    if m == WM_NCDESTROY {
        unsafe {
            RemovePropW(h, crate::win::wide("Linework.NppRangeABI").as_ptr());
            drop(Box::from_raw(ctx as *mut Native))
        }
    }
    value
}
pub(super) fn install(handle: H) -> Result<(), String> {
    unsafe {
        let h = handle as HWND;
        let property = crate::win::wide("Linework.NppRangeABI");
        if !GetPropW(h, property.as_ptr()).is_null() {
            return Ok(());
        }
        let data = SendMessageW(h, SCI_GETDIRECTPOINTER, 0, 0);
        let call = SendMessageW(h, SCI_GETDIRECTFUNCTION, 0, 0);
        let status = SendMessageW(h, SCI_GETDIRECTSTATUSFUNCTION, 0, 0);
        if data == 0 || call == 0 || status == 0 {
            return Err("Incompatible Scintilla direct-call ABI".into());
        }
        let ctx = Box::into_raw(Box::new(Native {
            hwnd: handle,
            data,
            call: std::mem::transmute::<isize, Direct>(call),
            status: std::mem::transmute::<isize, DirectStatus>(status),
        }));
        if SetWindowSubclass(h, Some(proc), SUBCLASS, ctx as usize) == 0 {
            drop(Box::from_raw(ctx));
            return Err(std::io::Error::last_os_error().to_string());
        }
        if SetPropW(h, property.as_ptr(), ctx.cast()) == 0 {
            RemoveWindowSubclass(h, Some(proc), SUBCLASS);
            drop(Box::from_raw(ctx));
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }
}
