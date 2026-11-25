//! Prints the exact account of one folder search: every step each thread
//! performed, how often, for how long, and what the whole process cost.
use linework_search::{
    account::{collect, STEPS},
    sweep::Gathered,
};
use std::time::{Duration, Instant};
use windows_sys::Win32::{
    Foundation::FILETIME,
    System::{
        SystemInformation::GetSystemTimeAsFileTime,
        Threading::{GetCurrentProcess, GetProcessTimes},
        WindowsProgramming::QueryProcessCycleTime,
    },
};

fn ticks(time: &FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

pub fn print(began: Instant, searched: Duration, wrote: Duration, gathered: &[Gathered], threads: usize) {
    let tallies = collect();
    let seconds = |nanos: u64| nanos as f64 / 1e9;
    let alive: u64 = tallies.iter().map(|t| t.alive).sum();
    let files: u64 = gathered.iter().map(|g| g.files).sum();
    let bytes: u64 = gathered.iter().map(|g| g.bytes).sum();
    let lines: u64 = gathered.iter().map(|g| g.matched_lines).sum();
    let out: usize = gathered.iter().map(|g| g.out.len()).sum();

    // The process as Windows saw it: when it was created, what it has used.
    let (mut made, mut gone, mut kernel, mut user, mut now) = ([FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 }; 1], [FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 }; 1], [FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 }; 1], [FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 }; 1], [FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 }; 1]);
    let mut cycles = 0u64;
    unsafe {
        GetProcessTimes(GetCurrentProcess(), made.as_mut_ptr(), gone.as_mut_ptr(), kernel.as_mut_ptr(), user.as_mut_ptr());
        GetSystemTimeAsFileTime(now.as_mut_ptr());
        QueryProcessCycleTime(GetCurrentProcess(), &mut cycles);
    }
    let since_made = (ticks(&now[0]) - ticks(&made[0])) as f64 / 1e7;
    let in_main = began.elapsed().as_secs_f64();

    eprintln!("ACCOUNT  {files} files read, {:.1} MB, {lines} matching lines, {:.1} KB of output, {threads} threads", bytes as f64 / 1e6, out as f64 / 1e3);
    eprintln!();
    eprintln!("Whole process, by the wall clock:");
    eprintln!("  {:9.4} s  from the process being created to this line", since_made);
    eprintln!("  {:9.4} s    of that before the program's own first line ran (Windows loading it)", (since_made - in_main).max(0.0));
    eprintln!("  {:9.4} s    searching (all threads start to all threads done)", searched.as_secs_f64());
    eprintln!("  {:9.4} s    writing the output", wrote.as_secs_f64());
    eprintln!("  {:9.4} s    everything else in the program (arguments, set-up)", (in_main - searched.as_secs_f64() - wrote.as_secs_f64()).max(0.0));
    eprintln!();
    eprintln!("Processor time used by the process (all threads added together):");
    eprintln!("  {:9.4} s  in the program's own instructions", ticks(&user[0]) as f64 / 1e7);
    eprintln!("  {:9.4} s  inside Windows on the program's behalf", ticks(&kernel[0]) as f64 / 1e7);
    eprintln!("  {:9.3} billion processor cycles in all", cycles as f64 / 1e9);
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
