use crate::win::{H, wide};
use linework_editor::Notification;
use std::{ffi::CStr, os::windows::ffi::OsStrExt, path::Path, ptr::null_mut};
use windows_sys::Win32::{Foundation::FreeLibrary, System::LibraryLoader::*};

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct Data {
    pub host: H,
    pub main: H,
    pub second: H,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct Shortcut {
    pub ctrl: u8,
    pub alt: u8,
    pub shift: u8,
    pub key: u8,
}
pub(super) type Command = unsafe extern "C" fn();
pub(super) type Notify = unsafe extern "C" fn(*mut Notification);
pub(super) type Message = unsafe extern "C" fn(u32, usize, isize) -> isize;
#[repr(C)]
struct Item {
    name: [u16; 64],
    func: Option<Command>,
    id: i32,
    checked: u8,
    shortcut: *const Shortcut,
}
pub(super) struct Action {
    pub label: String,
    pub callback: Option<Command>,
    pub checked: bool,
    pub shortcut: Shortcut,
    pub id: i32,
}
pub(super) struct Plugin {
    // Pinned until process exit. A plugin can subclass windows or run worker threads.
    pub _module: H,
    pub file: String,
    pub name: String,
    pub converter_wrap: Option<u64>,
    pub actions: Vec<Action>,
    pub notify: Notify,
    pub message: Message,
}
fn export(module: H, name: &CStr) -> Result<unsafe extern "system" fn() -> isize, String> {
    unsafe { GetProcAddress(module as _, name.as_ptr().cast()) }
        .ok_or_else(|| format!("Missing {} export", name.to_string_lossy()))
}
pub(super) unsafe fn read_string(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while len < 32768 && unsafe { *ptr.add(len) } != 0 {
        len += 1
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr, len) })
}
pub(super) fn load(path: &Path, data: Data) -> Result<Plugin, String> {
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let module = LoadLibraryExW(
            name.as_ptr(),
            null_mut(),
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR
                | LOAD_LIBRARY_SEARCH_SYSTEM32
                | LOAD_LIBRARY_SEARCH_APPLICATION_DIR,
        ) as H;
        if module == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        macro_rules! symbol {
            ($name:expr,$ty:ty) => {
                match export(module, $name) {
                    Ok(f) => std::mem::transmute::<unsafe extern "system" fn() -> isize, $ty>(f),
                    Err(e) => {
                        FreeLibrary(module as _);
                        return Err(e);
                    }
                }
            };
        }
        let unicode = symbol!(c"isUnicode", unsafe extern "C" fn() -> i32);
        let set = symbol!(c"setInfo", unsafe extern "C" fn(Data));
        let get_name = symbol!(c"getName", unsafe extern "C" fn() -> *const u16);
        let get_items = symbol!(
            c"getFuncsArray",
            unsafe extern "C" fn(*mut i32) -> *mut Item
        );
        let notify = symbol!(c"beNotified", Notify);
        let message = symbol!(c"messageProc", Message);
        if unicode() == 0 {
            FreeLibrary(module as _);
            return Err("Requires a Unicode x64 plugin".into());
        }
        set(data);
        let mut count = 0;
        let raw = get_items(&mut count);
        if raw.is_null() || !(1..=4096).contains(&count) {
            // setInfo has run and may have registered external callbacks.
            return Err("Invalid plugin command table".into());
        }
        let start =
            super::reserve_commands(count as usize).ok_or("Plugin command space exhausted")?;
        let mut actions = Vec::with_capacity(count as usize);
        for (i, item) in std::slice::from_raw_parts_mut(raw, count as usize)
            .iter_mut()
            .enumerate()
        {
            let len = item
                .name
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(item.name.len());
            item.id = start + i as i32;
            actions.push(Action {
                label: String::from_utf16_lossy(&item.name[..len]),
                callback: item.func,
                id: item.id,
                checked: item.checked != 0,
                shortcut: item.shortcut.as_ref().copied().unwrap_or_default(),
            });
        }
        Ok(Plugin {
            _module: module,
            file: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            name: read_string(get_name()),
            converter_wrap: super::converter::wrap(path),
            actions,
            notify,
            message,
        })
    }
}
pub(super) unsafe fn write_string(value: &str, ptr: isize, capacity: Option<usize>) -> isize {
    let s = wide(value);
    let len = (s.len() - 1) as isize;
    if ptr == 0 {
        return len;
    }
    if capacity.is_some_and(|n| n < s.len()) {
        return 0;
    }
    unsafe { std::ptr::copy_nonoverlapping(s.as_ptr(), ptr as *mut u16, s.len()) };
    if capacity.is_some() { 1 } else { len }
}
