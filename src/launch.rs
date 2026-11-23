use crate::{events, state, win::wide};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{OsStr, OsString},
    hash::{DefaultHasher, Hash, Hasher},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HWND, WAIT_OBJECT_0},
    System::{
        DataExchange::COPYDATASTRUCT,
        Threading::{INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    },
    UI::WindowsAndMessaging::*,
};

pub struct Target {
    pub path: PathBuf,
    pub position: Option<(u64, u64)>,
}

#[derive(Default)]
pub struct Launch {
    pub targets: Vec<Target>,
    reuse: bool,
    wait: bool,
}

fn trailing_number(s: &[u16]) -> Option<(&[u16], u64)> {
    let colon = s.iter().rposition(|v| *v == b':' as u16)?;
    // A drive designator alone is never a filename and coordinate.
    if colon == 0 || (colon == 1 && s[0] <= 127 && (s[0] as u8).is_ascii_alphabetic()) {
        return None;
    }
    let digits = &s[colon + 1..];
    if digits.is_empty() {
        return None;
    }
    let mut n = 0u64;
    for &d in digits {
        let digit = d.checked_sub(48)?;
        if digit > 9 {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(u64::from(digit))?;
    }
    (n > 0).then_some((&s[..colon], n))
}

impl Target {
    fn parse(arg: OsString) -> Self {
        let literal = PathBuf::from(arg);
        let raw: Vec<u16> = literal.as_os_str().encode_wide().collect();
        let mut path = literal;
        let mut position = None;
        if let Some((head, last)) = trailing_number(&raw) {
            // Existing literal names, including NTFS streams, take precedence.
            if !path.exists() {
                let (name, at) = match trailing_number(head) {
                    Some((name, line)) => (name, (line, last)),
                    None => (head, (last, 1)),
                };
                path = OsString::from_wide(name).into();
                position = Some(at);
            }
        }
        Self {
            path: std::path::absolute(&path).unwrap_or(path),
            position,
        }
    }
}

impl Launch {
    pub fn parse(args: impl Iterator<Item = OsString>) -> Self {
        let mut req = Self::default();
        let mut flags = true;
        for arg in args {
            if flags && arg == OsStr::new("--") {
                flags = false;
            } else if flags && arg == OsStr::new("--reuse-window") {
                req.reuse = true;
            } else if flags && arg == OsStr::new("--new-window") {
                req.reuse = false;
            } else if flags && arg == OsStr::new("--wait") {
                req.wait = true;
            } else {
                req.targets.push(Target::parse(arg));
            }
        }
        req
    }
}

// WM_COPYDATA copies a bounded, read-only request into the receiver. UTF-16 keeps
// Windows filenames lossless and absolute paths keep the sender's working directory.
const OPEN_TAG: usize = 0x4C574F31;
const MAX_REQUEST: usize = 1024 * 1024;
#[derive(Serialize, Deserialize)]
struct WireTarget {
    path: Vec<u16>,
    position: Option<(u64, u64)>,
}

pub fn receive(address: isize) -> isize {
    if address == 0 {
        return 0;
    }
    let msg = unsafe { &*(address as *const COPYDATASTRUCT) };
    if msg.dwData != OPEN_TAG || msg.lpData.is_null() || msg.cbData as usize > MAX_REQUEST {
        return 0;
    }
    let data = unsafe { std::slice::from_raw_parts(msg.lpData.cast::<u8>(), msg.cbData as usize) };
    let Ok(wire) = serde_json::from_slice::<Vec<WireTarget>>(data) else {
        return 0;
    };
    let mut targets = Vec::with_capacity(wire.len());
    for t in wire {
        if t.path.contains(&0) || t.position.is_some_and(|(l, c)| l == 0 || c == 0) {
            return 0;
        }
        let path = PathBuf::from(OsString::from_wide(&t.path));
        if !path.is_absolute() {
            return 0;
        }
        targets.push(Target {
            path,
            position: t.position,
        });
    }
    events::push(events::Event::Launch(targets));
    1
}

fn profile_property() -> Vec<u16> {
    let mut hash = DefaultHasher::new();
    let path = state::profile();
    state::key(&std::path::absolute(&path).unwrap_or(path)).hash(&mut hash);
    wide(&format!("Linework.Open.v1.{:016x}", hash.finish()))
}

pub fn register_window(h: HWND) {
    unsafe {
        SetPropW(h, profile_property().as_ptr(), 1usize as _);
    }
}

pub fn unregister_window(h: HWND) {
    unsafe { RemovePropW(h, profile_property().as_ptr()); }
}

pub fn forward(req: &Launch) -> Result<bool, String> {
    if !req.reuse {
        return Ok(false);
    }
    let class = wide("LineworkMain");
    let prop = profile_property();
    unsafe {
        let mut h = null_mut();
        loop {
            h = FindWindowExW(null_mut(), h, class.as_ptr(), std::ptr::null());
            if h.is_null() {
                return Ok(false);
            }
            if !GetPropW(h, prop.as_ptr()).is_null() {
                break;
            }
        }
        let wire: Vec<_> = req
            .targets
            .iter()
            .map(|t| WireTarget {
                path: t.path.as_os_str().encode_wide().collect(),
                position: t.position,
            })
            .collect();
        let bytes = serde_json::to_vec(&wire).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_REQUEST {
            return Err("Too many file paths for one window request.".into());
        }
        let msg = COPYDATASTRUCT {
            dwData: OPEN_TAG,
            cbData: bytes.len() as u32,
            lpData: bytes.as_ptr() as _,
        };
        let mut pid = 0;
        GetWindowThreadProcessId(h, &mut pid);
        let proc = if req.wait {
            OpenProcess(PROCESS_SYNCHRONIZE, 0, pid)
        } else {
            null_mut()
        };
        if req.wait && proc.is_null() {
            return Err("Could not wait for the selected Linework window.".into());
        }
        AllowSetForegroundWindow(pid);
        let mut accepted = 0;
        let sent = SendMessageTimeoutW(
            h,
            WM_COPYDATA,
            0,
            &msg as *const _ as isize,
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            3000,
            &mut accepted,
        );
        let ok = sent != 0 && accepted == 1;
        if ok {
            if IsIconic(h) != 0 {
                ShowWindowAsync(h, SW_RESTORE);
            }
            SetForegroundWindow(h);
        }
        let mut waited = true;
        if !proc.is_null() {
            if ok {
                waited = WaitForSingleObject(proc, INFINITE) == WAIT_OBJECT_0;
            }
            CloseHandle(proc);
        }
        if !ok {
            return Ok(false); // Continue startup and open the requested files here.
        }
        if !waited {
            return Err("The wait for Linework failed.".into());
        }
        Ok(true)
    }
}

// Scintilla columns count one Unicode scalar and expand tabs. A position
// inside a tab resolves to its leading edge. Walk the logical document so
// the viewport's page boundary cannot truncate a requested column.
pub fn column_offset(
    doc: &linework_storage::Document,
    mut at: u64,
    goal: u64,
    tabs: u64,
    cancelled: impl Fn() -> bool,
) -> std::io::Result<u64> {
    let mut column = 0;
    while at < doc.len() && column < goal {
        if cancelled() {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        let bytes = doc.read(
            at,
            (doc.len() - at).min((goal - column).saturating_mul(4).clamp(4, 65536)) as usize,
        )?;
        let mut i = 0;
        while i < bytes.len() && column < goal {
            let ch = bytes[i];
            if ch == b'\r' || ch == b'\n' {
                return Ok(at + i as u64);
            }
            let advance = if ch == b'\t' { tabs - column % tabs } else { 1 };
            if advance > goal - column {
                return Ok(at + i as u64);
            }
            let width = match ch {
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => 1,
            };
            if i + width > bytes.len() && at + (bytes.len() as u64) < doc.len() {
                break;
            }
            let valid = width == 1
                || bytes
                    .get(i..i + width)
                    .is_some_and(|s| std::str::from_utf8(s).is_ok());
            i += if valid { width } else { 1 };
            column += advance;
        }
        at += i as u64;
    }
    Ok(at)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paged_columns_obey_tabs_utf8_and_crlf_at_buffer_boundaries() {
        let mut d = linework_storage::Document::empty();
        let mut data = vec![b'a'; 65535];
        data.extend_from_slice("🦀\tΩ\r\n".as_bytes());
        d.replace(0, 0, &data).unwrap();
        assert_eq!(column_offset(&d, 0, 65536, 4, || false).unwrap(), 65539);
        assert_eq!(column_offset(&d, 0, 65538, 4, || false).unwrap(), 65539);
        assert_eq!(column_offset(&d, 0, 65540, 4, || false).unwrap(), 65540);
        assert_eq!(column_offset(&d, 0, u64::MAX, 4, || false).unwrap(), 65542);
    }
    #[test]
    fn parses_diagnostic_paths_without_consuming_drive_or_literals() {
        for (arg, tail, pos) in [
            (r"C:\source dir\café.rs:17:8", "café.rs", Some((17, 8))),
            (r"C:\source dir\test.rs:17", "test.rs", Some((17, 1))),
            ("file.rs", "file.rs", None),
        ] {
            let t = Target::parse(arg.into());
            assert!(t.path.ends_with(tail));
            assert_eq!(t.position, pos);
        }
        assert!(trailing_number(&"C:23".encode_utf16().collect::<Vec<_>>()).is_none());
        assert!(
            trailing_number(
                &"x.txt:18446744073709551616"
                    .encode_utf16()
                    .collect::<Vec<_>>()
            )
            .is_none()
        );
        let r = Launch::parse(
            ["--reuse-window", "--wait", "--", "--new-window"]
                .map(OsString::from)
                .into_iter(),
        );
        assert!(r.reuse && r.wait && r.targets[0].path.ends_with("--new-window"));
    }
}
