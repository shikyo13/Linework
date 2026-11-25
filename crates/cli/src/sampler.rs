//! A sampler for the program's own threads: several thousand times a second
//! it stops each working thread for an instant, notes the address of the
//! instruction it was about to execute, and lets it go on. The addresses are
//! written to a file as offsets into the module they lie in, to be turned
//! into function names and machine code afterwards.
//!
//! It sees only the program's side. While a thread is inside a Windows call,
//! the address is the call's return point in ntdll, which says which call.
use std::{
    io::Write,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, HMODULE},
    System::{
        Diagnostics::Debug::{GetThreadContext, CONTEXT},
        LibraryLoader::GetModuleFileNameW,
        ProcessStatus::{EnumProcessModules, GetModuleInformation, MODULEINFO},
        Threading::{GetCurrentProcess, OpenThread, ResumeThread, SuspendThread, THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME},
    },
};

/// CONTEXT_CONTROL for x64: only the instruction and stack pointers are wanted.
const CONTROL: u32 = 0x0010_0001;

#[repr(align(16))]
struct Aligned(CONTEXT);

fn threads_of_this_process() -> Vec<u32> {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32};
    let mut found = Vec::new();
    unsafe {
        let me = windows_sys::Win32::System::Threading::GetCurrentProcessId();
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return found;
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        if Thread32First(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32OwnerProcessID == me {
                    found.push(entry.th32ThreadID);
                }
                if Thread32Next(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    found
}

pub struct Sampler {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<Vec<u64>>,
}

impl Sampler {
    pub fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut addresses = Vec::with_capacity(1 << 20);
            let mut handles: Vec<(u32, HANDLE)> = Vec::new();
            let mut context: Aligned = unsafe { std::mem::zeroed() };
            let own = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
            let mut rounds = 0u64;
            while !flag.load(Ordering::Relaxed) {
                // Pick up threads that have started since the last round: the
                // ones the folder search registered, and, every so often, every
                // other thread of this process (the storage engine's workers).
                rounds += 1;
                let mut ids: Vec<u32> = linework_search::account::WORKERS.try_lock().map(|ids| ids.clone()).unwrap_or_default();
                if rounds % 64 == 1 {
                    ids.extend(threads_of_this_process());
                }
                {
                    for &id in ids.iter() {
                        if id == own {
                            continue;
                        }
                        if !handles.iter().any(|&(known, _)| known == id) {
                            let handle = unsafe { OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION, 0, id) };
                            if !handle.is_null() {
                                handles.push((id, handle));
                            }
                        }
                    }
                }
                for &(_, handle) in &handles {
                    if unsafe { SuspendThread(handle) } == u32::MAX {
                        continue;
                    }
                    context.0.ContextFlags = CONTROL;
                    if unsafe { GetThreadContext(handle, &mut context.0) } != 0 {
                        addresses.push(context.0.Rip);
                    }
                    unsafe { ResumeThread(handle) };
                }
                std::thread::sleep(std::time::Duration::from_micros(100));
            }
            for (_, handle) in handles {
                unsafe { CloseHandle(handle) };
            }
            addresses
        });
        Self { stop, thread }
    }

    /// Stops sampling and writes `module offset` lines to `path`.
    pub fn finish(self, path: &str) -> std::io::Result<usize> {
        self.stop.store(true, Ordering::Release);
        let addresses = self.thread.join().unwrap_or_default();
        let mut modules = [std::ptr::null_mut::<core::ffi::c_void>() as HMODULE; 256];
        let mut needed = 0u32;
        let mut loaded: Vec<(u64, u64, String)> = Vec::new();
        unsafe {
            let process = GetCurrentProcess();
            if EnumProcessModules(process, modules.as_mut_ptr(), std::mem::size_of_val(&modules) as u32, &mut needed) != 0 {
                for &module in &modules[..(needed as usize / std::mem::size_of::<HMODULE>()).min(modules.len())] {
                    let mut info: MODULEINFO = std::mem::zeroed();
                    let mut name = [0u16; 520];
                    GetModuleInformation(process, module, &mut info, std::mem::size_of::<MODULEINFO>() as u32);
                    let len = GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) as usize;
                    loaded.push((info.lpBaseOfDll as u64, u64::from(info.SizeOfImage), String::from_utf16_lossy(&name[..len])));
                }
            }
        }
        let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
        for address in &addresses {
            match loaded.iter().find(|(base, size, _)| address >= base && *address < base + size) {
                Some((base, _, name)) => writeln!(out, "{name}\t{:x}", address - base)?,
                None => writeln!(out, "?\t{address:x}")?,
            }
        }
        out.flush()?;
        Ok(addresses.len())
    }
}
