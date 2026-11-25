//! Prints the exact account of one folder search: every step each thread
//! performed, how often, for how long, and what the whole process cost.
use linework_search::{
    account::{collect, STEPS},
    sweep::Gathered,
};
use std::time::{Duration, Instant};

/// The process as the system saw it.
struct Process {
    /// Seconds since the process was created.
    since_made: f64,
    /// Processor seconds in the program's own instructions, and inside the system on its behalf.
    user: f64,
    kernel: f64,
    cycles: u64,
}

#[cfg(windows)]
const SYSTEM: &str = "Windows";
#[cfg(target_os = "macos")]
const SYSTEM: &str = "macOS";

#[cfg(windows)]
fn process() -> Process {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::{
            SystemInformation::GetSystemTimeAsFileTime,
            Threading::{GetCurrentProcess, GetProcessTimes},
            WindowsProgramming::QueryProcessCycleTime,
        },
    };
    let ticks = |time: &FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut made, mut gone, mut kernel, mut user, mut now) = (zero, zero, zero, zero, zero);
    let mut cycles = 0u64;
    unsafe {
        GetProcessTimes(GetCurrentProcess(), &mut made, &mut gone, &mut kernel, &mut user);
        GetSystemTimeAsFileTime(&mut now);
        QueryProcessCycleTime(GetCurrentProcess(), &mut cycles);
    }
    Process { since_made: (ticks(&now) - ticks(&made)) as f64 / 1e7, user: ticks(&user) as f64 / 1e7, kernel: ticks(&kernel) as f64 / 1e7, cycles }
}

#[cfg(target_os = "macos")]
fn process() -> Process {
    let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    let mut used: libc::rusage = unsafe { std::mem::zeroed() };
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let mut now: libc::timeval = unsafe { std::mem::zeroed() };
    let mut detail: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let since_made = unsafe {
        libc::getrusage(libc::RUSAGE_SELF, &mut used);
        libc::gettimeofday(&mut now, std::ptr::null_mut());
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let got = libc::proc_pidinfo(libc::getpid(), libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size);
        libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V4, (&mut detail as *mut libc::rusage_info_v4).cast());
        if got == size { seconds(now) - (info.pbi_start_tvsec as f64 + info.pbi_start_tvusec as f64 / 1e6) } else { 0.0 }
    };
    Process { since_made, user: seconds(used.ru_utime), kernel: seconds(used.ru_stime), cycles: detail.ri_cycles }
}

pub fn print(began: Instant, searched: Duration, wrote: Duration, gathered: &[Gathered], threads: usize) {
    let tallies = collect();
    let seconds = |nanos: u64| nanos as f64 / 1e9;
    let alive: u64 = tallies.iter().map(|t| t.alive).sum();
    let files: u64 = gathered.iter().map(|g| g.files).sum();
    let bytes: u64 = gathered.iter().map(|g| g.bytes).sum();
    let lines: u64 = gathered.iter().map(|g| g.matched_lines).sum();
    let out: usize = gathered.iter().map(|g| g.out.len()).sum();

    let process = process();
    let since_made = process.since_made;
    let in_main = began.elapsed().as_secs_f64();

    eprintln!("ACCOUNT  {files} files read, {:.1} MB, {lines} matching lines, {:.1} KB of output, {threads} threads", bytes as f64 / 1e6, out as f64 / 1e3);
    eprintln!();
    eprintln!("Whole process, by the wall clock:");
    eprintln!("  {:9.4} s  from the process being created to this line", since_made);
    eprintln!("  {:9.4} s    of that before the program's own first line ran ({SYSTEM} loading it)", (since_made - in_main).max(0.0));
    eprintln!("  {:9.4} s    searching (all threads start to all threads done)", searched.as_secs_f64());
    eprintln!("  {:9.4} s    writing the output", wrote.as_secs_f64());
    eprintln!("  {:9.4} s    everything else in the program (arguments, set-up)", (in_main - searched.as_secs_f64() - wrote.as_secs_f64()).max(0.0));
    eprintln!();
    eprintln!("Processor time used by the process (all threads added together):");
    eprintln!("  {:9.4} s  in the program's own instructions", process.user);
    eprintln!("  {:9.4} s  inside {SYSTEM} on the program's behalf", process.kernel);
    eprintln!("  {:9.3} billion processor cycles in all", process.cycles as f64 / 1e9);
    eprintln!();
    eprintln!("Where the threads' time went ({} threads, {:.4} s of thread time in all):", tallies.len(), seconds(alive));
    eprintln!("  {:>10}  {:>6}  {:>9}  {:>11}  step", "seconds", "share", "calls", "each, in us");
    let mut covered = 0u64;
    let mut order: Vec<usize> = (0..STEPS.len()).collect();
    let total = |i: usize| tallies.iter().map(|t| t.nanos[i]).sum::<u64>();
    order.sort_by_key(|&i| std::cmp::Reverse(total(i)));
    for i in order {
        let nanos = total(i);
        let calls: u64 = tallies.iter().map(|t| t.calls[i]).sum();
        covered += nanos;
        if calls == 0 {
            continue;
        }
        eprintln!(
            "  {:10.4}  {:5.1}%  {:9}  {:11.2}  {}",
            seconds(nanos),
            nanos as f64 / alive as f64 * 100.0,
            calls,
            nanos as f64 / calls as f64 / 1e3,
            STEPS[i].1
        );
    }
    let rest = alive.saturating_sub(covered);
    eprintln!("  {:10.4}  {:5.1}%  {:>9}  {:>11}  the program's own work between those steps", seconds(rest), rest as f64 / alive as f64 * 100.0, "", "");
    eprintln!();
    eprintln!("Per file read: {:.2} us of thread time; per megabyte: {:.2} ms of thread time.", seconds(alive) / files.max(1) as f64 * 1e6, seconds(alive) / (bytes.max(1) as f64 / 1e6) * 1e3);
}
