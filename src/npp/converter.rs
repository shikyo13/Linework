//! Quirks of the released x64 Converter 4.7 DLL. Detect the exact file
//! version before adapting its documented ASCII -> HEX command.
use crate::win::wide;
use std::{
    ffi::c_void,
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr::{null_mut, read_unaligned},
};
use windows_sys::Win32::{Storage::FileSystem::*, System::WindowsProgramming::*};

pub(super) fn wrap(path: &Path) -> Option<u64> {
    if !path.file_name()?.eq_ignore_ascii_case("NppConverter.dll") {
        return None;
    }
    let name: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let n = GetFileVersionInfoSizeW(name.as_ptr(), null_mut());
        if n == 0 {
            return None;
        }
        let mut info = vec![0u8; n as usize];
        if GetFileVersionInfoW(name.as_ptr(), 0, n, info.as_mut_ptr().cast()) == 0 {
            return None;
        }
        let mut ptr = null_mut::<c_void>();
        let mut size = 0;
        if VerQueryValueW(
            info.as_ptr().cast(),
            wide("\\").as_ptr(),
            &mut ptr,
            &mut size,
        ) == 0
            || (size as usize) < size_of::<VS_FIXEDFILEINFO>()
        {
            return None;
        }
        let version = read_unaligned(ptr.cast::<VS_FIXEDFILEINFO>());
        if version.dwFileVersionMS != 0x00040007 || version.dwFileVersionLS != 0 {
            return None;
        }
        // The DLL reads these values once during setInfo, immediately before
        // this probe. Use the same Win32 INI functions so encodings agree.
        let ini = super::home().join("Config").join("converter.ini");
        let ini: Vec<_> = ini.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut sections = [0u16; 260];
        GetPrivateProfileSectionNamesW(sections.as_mut_ptr(), 260, ini.as_ptr());
        let first = sections.iter().position(|&c| c == 0).unwrap_or(260);
        let section = wide("ascii2Hex");
        if &sections[..first] != &section[..section.len() - 1] {
            return Some(0);
        }
        Some(GetPrivateProfileIntW(
            section.as_ptr(),
            wide("nbCharPerLine").as_ptr(),
            0,
            ini.as_ptr(),
        ) as u32 as u64)
    }
}
/// In 4.7 the DLL allocates input_len * (stride + total_line_break_bytes).
/// Keep this temporary allocation under 16 MiB. Splitting at complete output
/// lines preserves the exact bytes the original command would produce.
pub(super) fn chunk(wrap: u64, remaining: u64) -> Option<u64> {
    let fits = |n: u64| {
        let breaks = n.checked_div(wrap).unwrap_or(0);
        (n as u128) * (3 + 2 * breaks as u128) + 1 <= 16 << 20
    };
    if fits(remaining) {
        return Some(remaining);
    }
    let mut low = 0;
    let mut high = remaining.min(16 << 20);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if fits(mid) { low = mid } else { high = mid - 1 }
    }
    if wrap != 0 {
        low = low / wrap * wrap;
    }
    (low != 0).then_some(low)
}
