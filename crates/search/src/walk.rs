//! List the files under a folder on several threads, leaving out what the
//! ignore rules and the hidden-file convention leave out.
//!
//! Every folder is read once with one listing call per batch of entries. The
//! listing already carries each entry's size, time and attributes, so no file
//! is opened here except the ignore files themselves.
//!
//! Each thread works down its own stack of folders and takes the shared lock
//! only when it runs dry or sees that another thread is waiting for work, so
//! threads that are all busy never meet.
use crate::account::{self, Step};
use crate::ignore::{Rules, Verdict};
use crate::pick::Pick;
use std::{
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
};
use windows_sys::Win32::{
    Foundation::INVALID_HANDLE_VALUE,
    Storage::FileSystem::{
        FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_REPARSE_POINT, WIN32_FIND_DATAW,
    },
};

#[derive(Debug, Clone, Copy)]
pub struct Options<'a> {
    /// Include names that start with a dot or carry the hidden attribute.
    pub hidden: bool,
    /// Apply `.gitignore`, `.ignore` and `.rgignore` files.
    pub ignore: bool,
    pub threads: usize,
    /// How many files of one folder a thread takes at a time. With 0 a thread
    /// handles every file of a folder itself as it lists it, which suits a
    /// listing. A search sets a small number, so that the files of a large
    /// folder are spread over waiting threads instead of held by one.
    pub batch: usize,
    /// Apply `.gitignore` files outside Git work trees too, as the editor does.
    /// Off, as in ripgrep, they count only inside a Git work tree.
    pub gitignore_anywhere: bool,
    /// Folder names below the root that are never entered, whatever the rules say.
    pub skip_folders: &'static [&'static str],
    /// Only the files these globs and types pick (`-g`, `-t`, `-T`).
    pub pick: Option<&'a Pick>,
}

impl Default for Options<'_> {
    fn default() -> Self {
        let logical = std::thread::available_parallelism().map_or(4, |n| n.get());
        Self {
            hidden: false,
            ignore: true,
            threads: (logical * 3 / 4).clamp(1, 12),
            batch: 0,
            gitignore_anywhere: false,
            skip_folders: &[],
            pick: None,
        }
    }
}

/// One file found by the walk.
#[derive(Debug)]
pub struct Found<'a> {
    /// Relative to the walk's root, with `/` between names.
    pub path: &'a str,
    /// The same file as Windows wants it named, absolute, without a final zero.
    pub wide: &'a [u16],
    pub size: u64,
    /// Last write time as Windows records it: 100 ns steps since 1601.
    pub modified: u64,
}

/// The rules of one folder: its own ignore files, most binding first.
struct Layer {
    /// Length of this folder's path relative to the root, with its trailing
    /// slash; a path below it minus this prefix is relative to the folder.
    /// For a folder above the root, the text to put in front instead.
    strip: usize,
    prepend: String,
    /// `.rgignore`, `.ignore`, then the Git files, each deciding before the next.
    files: Vec<Rules>,
    parent: Option<Arc<Layer>>,
}

struct Job {
    /// The folder as Windows wants it, without a trailing separator.
    wide: Vec<u16>,
    /// Relative to the root with a trailing slash, or empty for the root.
    relative: String,
    layer: Option<Arc<Layer>>,
    in_git: bool,
}

/// Some files of one folder, already past the rules, waiting to be visited.
struct Batch {
    /// The folder as Windows wants it, with its trailing separator.
    wide: Vec<u16>,
    /// Relative to the root with a trailing slash, or empty for the root.
    relative: String,
    names: Vec<u16>,
    files: Vec<Listed>,
}

enum Work {
    Folder(Job),
    Files(Batch),
}

struct Shared {
    spare: Mutex<Vec<Work>>,
    wake: Condvar,
    /// Threads waiting for work right now.
    idle: AtomicUsize,
    /// Folders not yet finished, wherever they are held.
    open: AtomicUsize,
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().collect()
}

fn read(folder: &[u16], name: &str) -> Option<Rules> {
    let mut path = OsString::from_wide(folder);
    path.push("\\");
    path.push(name);
    let rules = Rules::parse(&std::fs::read(PathBuf::from(path)).ok()?);
    (!rules.is_empty()).then_some(rules)
}

/// The ignore files of one folder, in the order they bind.
fn files(folder: &[u16], names: &[&str], in_git: bool, git_root: bool) -> Vec<Rules> {
    let mut out = Vec::new();
    for name in [".rgignore", ".ignore"] {
        if names.contains(&name) {
            out.extend(read(folder, name));
        }
    }
    if in_git {
        if names.contains(&".gitignore") {
            out.extend(read(folder, ".gitignore"));
        }
        if git_root {
            out.extend(read(folder, ".git\\info\\exclude"));
        }
    }
    out
}

fn verdict(layer: &Option<Arc<Layer>>, relative: &str, is_folder: bool, joined: &mut String) -> Option<Verdict> {
    let mut next = layer.as_ref();
    while let Some(layer) = next {
        let path: &str = if layer.prepend.is_empty() {
            &relative[layer.strip..]
        } else {
            joined.clear();
            joined.push_str(&layer.prepend);
            joined.push_str(relative);
            joined
        };
        for rules in &layer.files {
            if let Some(verdict) = rules.verdict(path.as_bytes(), is_folder) {
                return Some(verdict);
            }
        }
        next = layer.parent.as_ref();
    }
    None
}

/// Walks `root`. Every thread gets its own state from `state` and calls
/// `visit` with it for each file it finds, in no fixed order. Returns the
/// states, one per thread.
pub fn walk<S: Send>(
    root: &Path, options: Options, state: impl Fn() -> S + Sync, visit: impl Fn(&mut S, &Found) + Sync,
) -> std::io::Result<Vec<S>> {
    let absolute = std::path::absolute(root)?;
    // Folders above the root also speak: their ignore files, and whether the
    // root lies inside a Git work tree at all.
    let mut layer: Option<Arc<Layer>> = None;
    let mut in_git = false;
    if options.ignore {
        let above: Vec<&Path> = absolute.ancestors().skip(1).collect();
        in_git = options.gitignore_anywhere || absolute.ancestors().any(|folder| folder.join(".git").exists());
        for folder in above.iter().rev() {
            let present: Vec<&str> =
                [".rgignore", ".ignore", ".gitignore"].into_iter().filter(|name| folder.join(name).is_file()).collect();
            let found = files(&wide(folder), &present, in_git, folder.join(".git").exists());
            if found.is_empty() {
                continue;
            }
            let below = absolute.strip_prefix(folder).unwrap_or(Path::new(""));
            let mut prepend = below.to_string_lossy().replace('\\', "/");
            prepend.push('/');
            layer = Some(Arc::new(Layer { strip: 0, prepend, files: found, parent: layer }));
        }
    }
    let first = Job { wide: wide(&absolute), relative: String::new(), layer, in_git };
    let shared = Shared { spare: Mutex::new(vec![Work::Folder(first)]), wake: Condvar::new(), idle: AtomicUsize::new(0), open: AtomicUsize::new(1) };
    let (shared, state, visit) = (&shared, &state, &visit);
    let states = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..options.threads.max(1)).map(|_| scope.spawn(move || work(shared, options, state(), visit))).collect();
        workers.into_iter().map(|worker| worker.join().expect("a walking thread stopped")).collect()
    });
    Ok(states)
}

fn work<S>(shared: &Shared, options: Options, state: S, visit: &(impl Fn(&mut S, &Found) + Sync)) -> S {
    account::thread_begins();
    let state = labour(shared, options, state, visit);
    account::thread_ends();
    state
}

fn labour<S>(shared: &Shared, options: Options, mut state: S, visit: &(impl Fn(&mut S, &Found) + Sync)) -> S {
    let mut own: Vec<Work> = Vec::new();
    let mut scratch = Scratch::default();
    loop {
        let job = match own.pop() {
            Some(job) => job,
            None => {
                let mut spare = shared.spare.lock().unwrap();
                loop {
                    if let Some(job) = spare.pop() {
                        break job;
                    }
                    if shared.open.load(Ordering::Acquire) == 0 {
                        shared.wake.notify_all();
                        return state;
                    }
                    shared.idle.fetch_add(1, Ordering::AcqRel);
                    let waited = account::begin();
                    spare = shared.wake.wait(spare).unwrap();
                    account::end(Step::Wait, waited);
                    shared.idle.fetch_sub(1, Ordering::AcqRel);
                }
            }
        };
        let before = own.len();
        match job {
            Work::Folder(job) => folder(job, options, &mut state, visit, &mut own, &mut scratch),
            Work::Files(batch) => {
                let Scratch { relative, wide, .. } = &mut scratch;
                for file in &batch.files {
                    let name = &batch.names[file.name.clone()];
                    wide.clear();
                    wide.extend_from_slice(&batch.wide);
                    wide.extend_from_slice(name);
                    relative.clear();
                    relative.push_str(&batch.relative);
                    relative.extend(char::decode_utf16(name.iter().copied()).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)));
                    visit(&mut state, &Found { path: relative, wide, size: file.size, modified: file.modified });
                }
            }
        }
        shared.open.fetch_add(own.len() - before, Ordering::AcqRel);
        // Hand the oldest folders, which tend to be the largest, to waiting threads.
        let waiting = shared.idle.load(Ordering::Acquire);
        if waiting > 0 && own.len() > 1 {
            let give = waiting.min(own.len() - 1);
            let shared_at = account::begin();
            let mut spare = shared.spare.lock().unwrap();
            spare.extend(own.drain(..give));
            drop(spare);
            for _ in 0..give {
                shared.wake.notify_one();
            }
            account::end(Step::Share, shared_at);
        }
        if shared.open.fetch_sub(1, Ordering::AcqRel) == 1 {
            // The last folder is done: wake everyone, under the lock so that
            // no thread can check and then sleep in between.
            let _spare = shared.spare.lock().unwrap();
            shared.wake.notify_all();
        }
    }
}

/// Buffers a thread reuses for every folder.
#[derive(Default)]
struct Scratch {
    entries: Vec<Listed>,
    names: Vec<u16>,
    relative: String,
    joined: String,
    wide: Vec<u16>,
}

struct Listed {
    /// Where the name sits in `Scratch::names`.
    name: std::ops::Range<usize>,
    attributes: u32,
    size: u64,
    modified: u64,
}

fn list(folder: &[u16], scratch: &mut Scratch) {
    scratch.entries.clear();
    scratch.names.clear();
    scratch.wide.clear();
    scratch.wide.extend_from_slice(folder);
    scratch.wide.extend_from_slice(&[b'\\' as u16, b'*' as u16, 0]);
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let first = account::begin();
    // Without FIND_FIRST_EX_LARGE_FETCH: its 64 KiB buffer made a listing of
    // 40,219 mostly small folders 1.7 times slower on 12 threads, as offering
    // Windows a large buffer costs whether or not it is filled.
    let handle = unsafe {
        FindFirstFileExW(
            scratch.wide.as_ptr(), FindExInfoBasic, (&mut data as *mut WIN32_FIND_DATAW).cast(), FindExSearchNameMatch,
            std::ptr::null(), 0,
        )
    };
    account::end(Step::ListFirst, first);
    if handle == INVALID_HANDLE_VALUE {
        return;
    }
    loop {
        let len = data.cFileName.iter().position(|&c| c == 0).unwrap_or(data.cFileName.len());
        let name = &data.cFileName[..len];
        if name != [b'.' as u16] && name != [b'.' as u16, b'.' as u16] {
            let at = scratch.names.len();
            scratch.names.extend_from_slice(name);
            scratch.entries.push(Listed {
                name: at..at + len,
                attributes: data.dwFileAttributes,
                size: (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow),
                modified: (u64::from(data.ftLastWriteTime.dwHighDateTime) << 32) | u64::from(data.ftLastWriteTime.dwLowDateTime),
            });
        }
        let next = account::begin();
        let more = unsafe { FindNextFileW(handle, &mut data) };
        account::end(Step::ListNext, next);
        if more == 0 {
            break;
        }
    }
    let closing = account::begin();
    unsafe { FindClose(handle) };
    account::end(Step::ListClose, closing);
}

fn is(name: &[u16], text: &str) -> bool {
    name.len() == text.len() && name.iter().zip(text.bytes()).all(|(&unit, byte)| unit == u16::from(byte))
}

/// Reads one folder, reports its files and pushes its subfolders onto `more`.
fn folder<S>(
    job: Job, options: Options, state: &mut S, visit: &(impl Fn(&mut S, &Found) + Sync), more: &mut Vec<Work>,
    scratch: &mut Scratch,
) {
    list(&job.wide, scratch);
    let mut layer = job.layer.clone();
    let mut in_git = job.in_git;
    if options.ignore {
        let mut special: Vec<&str> = Vec::new();
        for entry in &scratch.entries {
            let name = &scratch.names[entry.name.clone()];
            if name.first() == Some(&(b'.' as u16)) {
                special.extend([".rgignore", ".ignore", ".gitignore", ".git"].into_iter().find(|known| is(name, known)));
            }
        }
        let git_root = special.contains(&".git");
        in_git |= git_root;
        if !special.is_empty() {
            let reading = account::begin();
            let found = files(&job.wide, &special, in_git, git_root);
            account::end(Step::IgnoreFiles, reading);
            if !found.is_empty() {
                layer = Some(Arc::new(Layer { strip: job.relative.len(), prepend: String::new(), files: found, parent: layer }));
            }
        }
    }
    let Scratch { entries, names, relative, joined, wide } = scratch;
    wide.clear();
    wide.extend_from_slice(&job.wide);
    wide.push(b'\\' as u16);
    let stem = wide.len();
    let fresh = |wide: &[u16]| Batch { wide: wide[..stem].to_vec(), relative: job.relative.clone(), names: Vec::new(), files: Vec::new() };
    let mut batch = (options.batch != 0).then(|| fresh(wide));
    for entry in entries.iter() {
        let name = &names[entry.name.clone()];
        let is_folder = entry.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        relative.clear();
        relative.push_str(&job.relative);
        relative.extend(char::decode_utf16(name.iter().copied()).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)));
        let ruled = if options.ignore && layer.is_some() {
            let judging = account::begin();
            let ruled = verdict(&layer, relative, is_folder, joined);
            account::end(Step::Rules, judging);
            ruled
        } else {
            None
        };
        if ruled == Some(Verdict::Ignored) {
            continue;
        }
        // An exception written in an ignore file (`!.gitkeep`) also brings a
        // hidden name back; nothing else does.
        let hidden = name.first() == Some(&(b'.' as u16)) || entry.attributes & FILE_ATTRIBUTE_HIDDEN != 0;
        if hidden && !options.hidden && ruled != Some(Verdict::Kept) {
            continue;
        }
        // A link is neither followed nor reported.
        if entry.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            continue;
        }
        if is_folder && options.skip_folders.iter().any(|skipped| is(name, skipped)) {
            continue;
        }
        if !is_folder && options.pick.is_some_and(|pick| !pick.keeps(relative)) {
            continue;
        }
        wide.truncate(stem);
        wide.extend_from_slice(name);
        if is_folder {
            relative.push('/');
            more.push(Work::Folder(Job { wide: wide.clone(), relative: relative.clone(), layer: layer.clone(), in_git }));
        } else if let Some(pending) = &mut batch {
            let at = pending.names.len();
            pending.names.extend_from_slice(name);
            pending.files.push(Listed { name: at..at + name.len(), attributes: entry.attributes, size: entry.size, modified: entry.modified });
            if pending.files.len() == options.batch {
                more.push(Work::Files(std::mem::replace(pending, fresh(wide))));
            }
        } else {
            visit(state, &Found { path: relative, wide, size: entry.size, modified: entry.modified });
        }
    }
    if let Some(pending) = batch {
        if !pending.files.is_empty() {
            more.push(Work::Files(pending));
        }
    }
}
