//! An exact account of where a search's time goes.
//!
//! When switched on, every thread times each step it performs with the
//! processor's own clock and adds it to its tally; at the end the tallies are
//! collected. What a thread's steps do not cover is its own bookkeeping
//! between them. Switched off, each call costs one load of a flag.
use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::Instant,
};

static ON: AtomicBool = AtomicBool::new(false);
static DONE: Mutex<Vec<Tally>> = Mutex::new(Vec::new());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Step {
    /// Opening a folder and getting its first batch of entries.
    ListFirst,
    /// Each further entry of a folder.
    ListNext,
    ListClose,
    /// Reading and parsing ignore files.
    IgnoreFiles,
    /// Testing names against the ignore rules.
    Rules,
    /// A thread with nothing to do, waiting for work from another.
    Wait,
    /// Taking the shared lock to hand work over.
    Share,
    FileOpen,
    FileRead,
    FileClose,
    /// Looking for a zero byte.
    Binary,
    /// Finding matches, cutting lines, counting line numbers, formatting.
    Match,
}

pub const STEPS: [(Step, &str); 12] = [
    (Step::ListFirst, "open a folder and read its first entries"),
    (Step::ListNext, "read each further entry of a folder"),
    (Step::ListClose, "close a folder"),
    (Step::IgnoreFiles, "read and parse ignore files"),
    (Step::Rules, "test names against ignore rules"),
    (Step::Wait, "threads waiting for work"),
    (Step::Share, "handing work to another thread"),
    (Step::FileOpen, "open a file"),
    (Step::FileRead, "read a file"),
    (Step::FileClose, "close a file"),
    (Step::Binary, "look for a zero byte"),
    (Step::Match, "find matches and build output lines"),
];

#[derive(Clone, Debug, Default)]
pub struct Tally {
    pub nanos: [u64; STEPS.len()],
    pub calls: [u64; STEPS.len()],
    /// From the thread's first step to its last.
    pub alive: u64,
}

thread_local! {
    static MINE: RefCell<(Tally, Option<Instant>)> = RefCell::new((Tally::default(), None));
}

pub fn switch_on() {
    ON.store(true, Ordering::Release);
}

#[inline]
pub fn begin() -> Option<Instant> {
    ON.load(Ordering::Relaxed).then(Instant::now)
}

#[inline]
pub fn end(step: Step, began: Option<Instant>) {
    if let Some(began) = began {
        let spent = began.elapsed().as_nanos() as u64;
        MINE.with(|mine| {
            let mut mine = mine.borrow_mut();
            mine.0.nanos[step as usize] += spent;
            mine.0.calls[step as usize] += 1;
        });
    }
}

/// Thread identifiers of the working threads, for a sampler that wants to
/// look at where each one is executing.
pub static WORKERS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Call when a working thread starts.
pub fn thread_begins() {
    if ON.load(Ordering::Relaxed) {
        MINE.with(|mine| mine.borrow_mut().1 = Some(Instant::now()));
        #[cfg(windows)]
        WORKERS.lock().unwrap().push(unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() });
    }
}

/// Call when a working thread ends: its tally joins the others.
pub fn thread_ends() {
    if ON.load(Ordering::Relaxed) {
        MINE.with(|mine| {
            let mut mine = mine.borrow_mut();
            if let Some(began) = mine.1.take() {
                mine.0.alive = began.elapsed().as_nanos() as u64;
            }
            DONE.lock().unwrap().push(std::mem::take(&mut mine.0));
        });
    }
}

/// One tally per finished thread.
pub fn collect() -> Vec<Tally> {
    std::mem::take(&mut *DONE.lock().unwrap())
}
