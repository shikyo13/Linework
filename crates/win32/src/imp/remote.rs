//! One copy of the program finding another and handing it a request, which on
//! Windows is a search for the other's window, a property read from it, and a
//! WM_COPYDATA message sent across. Here each copy that marks its main window
//! keeps a socket and a list of the marks in a folder of the person's temporary
//! files, and another copy's window is known by the number of its process.
use crate::Win32::Foundation::HWND;
use crate::Win32::System::DataExchange::COPYDATASTRUCT;
use crate::Win32::UI::WindowsAndMessaging::WM_COPYDATA;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

/// A window of another copy: this mark with the copy's process number.
const OTHER: usize = 0x4C57_0000_0000_0000;
/// A handle on another copy's process, to wait for its end.
const PROCESS: usize = 0x5052_0000_0000_0000;
const NUMBER: usize = 0xFFFF_FFFF;

struct Request {
    tag: usize,
    bytes: Vec<u8>,
    from: UnixStream,
}

static WAITING: Mutex<Vec<Request>> = Mutex::new(Vec::new());
static MARKS: Mutex<Vec<String>> = Mutex::new(Vec::new());

unsafe extern "C" {
    fn kill(process: i32, signal: i32) -> i32;
    fn atexit(run: extern "C" fn()) -> i32;
}

fn folder() -> PathBuf {
    std::env::temp_dir().join("linework-copies")
}

fn file(process: u32, kind: &str) -> PathBuf {
    folder().join(format!("{process}.{kind}"))
}

fn alive(process: u32) -> bool {
    unsafe { kill(process as i32, 0) == 0 }
}

pub(crate) fn window_of(hwnd: HWND) -> Option<u32> {
    let value = hwnd as usize;
    (value & !NUMBER == OTHER).then_some((value & NUMBER) as u32)
}

pub(crate) fn process_of(handle: usize) -> Option<u32> {
    (handle & !NUMBER == PROCESS).then_some((handle & NUMBER) as u32)
}

pub(crate) fn process_handle(process: u32) -> usize {
    PROCESS | process as usize
}

extern "C" fn leave() {
    let _ = std::fs::remove_file(file(std::process::id(), "sock"));
    let _ = std::fs::remove_file(file(std::process::id(), "marks"));
}

/// This copy's main window was given or lost a mark. The first mark starts the
/// listening; the marks are written where other copies read them.
pub(crate) fn marked(name: &str, set: bool) {
    let mut marks = MARKS.lock().unwrap();
    marks.retain(|mark| mark != name);
    if set {
        marks.push(name.to_owned());
    }
    let _ = std::fs::create_dir_all(folder());
    let _ = std::fs::write(file(std::process::id(), "marks"), marks.join("\n"));
    let socket = file(std::process::id(), "sock");
    if !set || socket.exists() {
        return;
    }
    let Ok(listener) = UnixListener::bind(&socket) else { return };
    unsafe { atexit(leave) };
    let _ = std::thread::Builder::new().name("Other copies".into()).spawn(move || {
        for from in listener.incoming().flatten() {
            let _ = from.set_read_timeout(Some(Duration::from_secs(3)));
            let mut from = from;
            let mut head = [0u8; 12];
            if from.read_exact(&mut head).is_err() {
                continue;
            }
            let tag = u64::from_le_bytes(head[..8].try_into().unwrap()) as usize;
            let length = u32::from_le_bytes(head[8..].try_into().unwrap()) as usize;
            let mut bytes = vec![0u8; length.min(2 << 20)];
            if from.read_exact(&mut bytes).is_err() {
                continue;
            }
            WAITING.lock().unwrap().push(Request { tag, bytes, from });
            crate::imp::message::wake();
        }
    });
}

/// Gives the main window what other copies have sent. Called as the message
/// loop comes round.
pub(crate) fn serve() {
    let requests = std::mem::take(&mut *WAITING.lock().unwrap());
    for mut request in requests {
        let answer = match crate::imp::window::main_window() {
            Some(main) => {
                let copied = COPYDATASTRUCT { dwData: request.tag, cbData: request.bytes.len() as u32, lpData: request.bytes.as_mut_ptr().cast() };
                crate::imp::window::send(main, WM_COPYDATA, 0, &copied as *const _ as isize)
            }
            None => 0,
        };
        let _ = request.from.write_all(&(answer as u64).to_le_bytes());
    }
}

/// The next other copy after `after`, for a search by window class.
pub(crate) fn next_window(after: HWND) -> HWND {
    let after = window_of(after).unwrap_or(0);
    let Ok(entries) = std::fs::read_dir(folder()) else { return std::ptr::null_mut() };
    let mut copies: Vec<u32> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_string_lossy().strip_suffix(".sock")?.parse().ok())
        .filter(|process| *process != std::process::id() && *process > after)
        .collect();
    copies.sort_unstable();
    for process in copies {
        if alive(process) {
            return (OTHER | process as usize) as HWND;
        }
        // Left behind by a copy that ended without tidying up.
        let _ = std::fs::remove_file(file(process, "sock"));
        let _ = std::fs::remove_file(file(process, "marks"));
    }
    std::ptr::null_mut()
}

pub(crate) fn has_mark(process: u32, name: &str) -> bool {
    std::fs::read_to_string(file(process, "marks")).is_ok_and(|marks| marks.lines().any(|mark| mark == name))
}

/// Sends WM_COPYDATA to another copy and waits for its answer.
pub(crate) fn send(process: u32, message: u32, data: isize, wait: Duration) -> Option<usize> {
    if message != WM_COPYDATA || data == 0 {
        return None;
    }
    let copied = unsafe { &*(data as *const COPYDATASTRUCT) };
    let bytes = unsafe { std::slice::from_raw_parts(copied.lpData as *const u8, copied.cbData as usize) };
    let mut to = UnixStream::connect(file(process, "sock")).ok()?;
    to.set_read_timeout(Some(wait)).ok()?;
    to.write_all(&(copied.dwData as u64).to_le_bytes()).ok()?;
    to.write_all(&copied.cbData.to_le_bytes()).ok()?;
    to.write_all(bytes).ok()?;
    let mut answer = [0u8; 8];
    to.read_exact(&mut answer).ok()?;
    Some(u64::from_le_bytes(answer) as usize)
}

/// Waits for another copy to end. True if it did within the time given.
pub(crate) fn ended(process: u32, milliseconds: u32) -> bool {
    let until = (milliseconds != u32::MAX).then(|| std::time::Instant::now() + Duration::from_millis(milliseconds as u64));
    while alive(process) {
        if until.is_some_and(|until| std::time::Instant::now() >= until) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}
