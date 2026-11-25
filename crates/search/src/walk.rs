//! List the files under a folder on several threads, leaving out what the
//! ignore rules, the file choices and the hidden-file convention leave out.
//!
//! Every folder is read once with one listing call per batch of entries. The
//! listing already carries each entry's size, times and attributes, so no file
//! is opened here except the ignore files themselves, and with `follow` each
//! folder once, to know it again if a link leads back to it.
//!
//! Each thread works down its own stack of folders and takes the shared lock
//! only when it runs dry or sees that another thread is waiting for work, so
//! threads that are all busy never meet.
//!
//! The order of the rules is ripgrep's: a glob that picks or leaves out
//! decides first; then hidden names; then ignore files, nearest folder first
//! (in each, `.rgignore`, `.ignore`, `.gitignore`, `.git/info/exclude`), then
//! Git's global ignore file, then the files given with `--ignore-file`; then
//! file types.
use crate::account::{self, Step};
use crate::ignore::{Rules, Verdict};
use crate::pick::{Pick, Said};
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
    Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
        CreateFileW, FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, GetFileInformationByHandle,
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, WIN32_FIND_DATAW,
    },
};

/// Told the path and the reason for each path left out (`--debug`).
pub type Report<'a> = &'a (dyn Fn(&str, &str) + Sync);

/// Told about each path that could not be walked: a link that leads nowhere,
/// a loop, a folder that cannot be listed. The search goes on without it.
pub type Failure<'a> = &'a (dyn Fn(&str, &str) + Sync);

#[derive(Clone, Copy)]
pub struct Options<'a> {
    /// Include names that start with a dot or carry the hidden attribute.
    pub hidden: bool,
    /// Apply ignore files at all (`--no-ignore` turns this off).
    pub ignore: bool,
    /// `.ignore` and `.rgignore` (`--no-ignore-dot`).
    pub ignore_dot: bool,
    /// `.gitignore`, `.git/info/exclude` and Git's global file (`--no-ignore-vcs`).
    pub ignore_vcs: bool,
    /// `.git/info/exclude` (`--no-ignore-exclude`).
    pub ignore_exclude: bool,
    /// Git's global ignore file (`--no-ignore-global`).
    pub ignore_global: bool,
    /// Ignore files in folders above the searched one (`--no-ignore-parent`).
    pub ignore_parent: bool,
    /// Ignore files named with `--ignore-file`, matched from the current folder, applied last.
    pub extra: &'a [PathBuf],
    /// Ignore files match ignoring letter case (`--ignore-file-case-insensitive`).
    pub ignore_fold: bool,
    pub threads: usize,
    /// How many files of one folder a thread takes at a time. With 0 a thread
    /// handles every file of a folder itself as it lists it, which suits a
    /// listing. A search sets a small number, so that the files of a large
    /// folder are spread over waiting threads instead of held by one.
    pub batch: usize,
    /// Apply `.gitignore` files outside Git work trees too (`--no-require-git`;
    /// the editor's choice). Off, as in ripgrep, they count only inside one.
    pub gitignore_anywhere: bool,
    /// Folder names below the root that are never entered, whatever the rules say.
    pub skip_folders: &'static [&'static str],
    /// The globs and types that choose files (`-g`, `-t`, `-T` and grep's options).
    pub pick: Option<&'a Pick>,
    /// The deepest level reported: 1 is the root's own entries (`--max-depth`).
    pub max_depth: Option<usize>,
    /// Files larger than this are left out (`--max-filesize`).
    pub max_filesize: Option<u64>,
    /// Follow links to files and folders (`-L`).
    pub follow: bool,
    /// Do not follow a link to another volume (`--one-file-system`).
    pub one_file_system: bool,
    /// The folder that `-g` globs and `--ignore-file` files are matched from.
    /// None is the current folder, as in ripgrep; the editor gives its workspace folder.
    pub base: Option<&'a Path>,
    pub report: Option<Report<'a>>,
    pub failure: Option<Failure<'a>>,
}

impl Default for Options<'_> {
    fn default() -> Self {
        let logical = std::thread::available_parallelism().map_or(4, |n| n.get());
        Self {
            hidden: false,
            ignore: true,
            ignore_dot: true,
            ignore_vcs: true,
            ignore_exclude: true,
            ignore_global: true,
            ignore_parent: true,
            extra: &[],
            ignore_fold: false,
            threads: (logical * 3 / 4).clamp(1, 12),
            batch: 0,
            gitignore_anywhere: false,
            skip_folders: &[],
            pick: None,
            max_depth: None,
            max_filesize: None,
            follow: false,
            one_file_system: false,
            base: None,
            report: None,
            failure: None,
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
    /// Times as Windows records them: 100 ns steps since 1601.
    pub modified: u64,
    pub accessed: u64,
    pub created: u64,
}

/// The rules of one folder: its own ignore files, most binding first.
struct Layer {
    /// Length of this folder's path relative to the root, with its trailing
    /// slash; a path below it minus this prefix is relative to the folder.
    /// For rules from elsewhere, the text to put in front instead.
    strip: usize,
    prepend: String,
    /// `.rgignore`, `.ignore`, then the Git files, each deciding before the next.
    files: Vec<Rules>,
    parent: Option<Arc<Layer>>,
}

/// A folder on the way down, known by its volume and file number, for finding a link back to it.
struct Ancestor {
    id: (u32, u64),
    parent: Option<Arc<Ancestor>>,
}

impl Ancestor {
    fn holds(this: &Option<Arc<Ancestor>>, id: (u32, u64)) -> bool {
        let mut next = this.as_ref();
        while let Some(a) = next {
            if a.id == id {
                return true;
            }
            next = a.parent.as_ref();
        }
        false
    }
}

struct Job {
    /// The folder as Windows wants it, without a trailing separator.
    wide: Vec<u16>,
    /// Relative to the root with a trailing slash, or empty for the root.
    relative: String,
    layer: Option<Arc<Layer>>,
    in_git: bool,
    /// How deep the folder is: 0 for the root.
    depth: usize,
    /// With `follow`: this folder and those above it.
    ancestors: Option<Arc<Ancestor>>,
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
    /// With `one_file_system`: the root's volume.
    volume: Option<u32>,
    /// What goes in front of a path for globs: the root as seen from the
    /// current folder, since ripgrep matches -g globs from there.
    glob_prefix: String,
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().collect()
}

fn read_rules(path: &Path, fold: bool) -> Option<Rules> {
    let rules = Rules::parse_from(&std::fs::read(path).ok()?, &path.to_string_lossy(), fold);
    (!rules.is_empty()).then_some(rules)
}

fn read(folder: &[u16], name: &str, fold: bool) -> Option<Rules> {
    let mut path = OsString::from_wide(folder);
    path.push("\\");
    path.push(name);
    read_rules(&PathBuf::from(path), fold)
}

/// The ignore files of one folder that the options apply, in the order they bind.
fn files(folder: &[u16], names: &[&str], in_git: bool, git_root: bool, options: &Options) -> Vec<Rules> {
    let mut out = Vec::new();
    if options.ignore_dot {
        for name in [".rgignore", ".ignore"] {
            if names.contains(&name) {
                out.extend(read(folder, name, options.ignore_fold));
            }
        }
    }
    if in_git && options.ignore_vcs {
        if names.contains(&".gitignore") {
            out.extend(read(folder, ".gitignore", options.ignore_fold));
        }
        if git_root && options.ignore_exclude {
            out.extend(read(folder, ".git\\info\\exclude", options.ignore_fold));
        }
    }
    out
}

/// What the ignore files say about a path, and which rule said it.
fn verdict(layer: &Option<Arc<Layer>>, relative: &str, is_folder: bool, joined: &mut String) -> Option<(Verdict, String)> {
    let mut next = layer.as_ref();
    while let Some(layer) = next {
        let path: &str = if layer.prepend.is_empty() {
            &relative[layer.strip.min(relative.len())..]
        } else {
            joined.clear();
            joined.push_str(&layer.prepend);
            joined.push_str(relative);
            joined
        };
        for rules in &layer.files {
            if let Some(decided) = rules.decide(path.as_bytes(), is_folder) {
                return Some((decided.verdict, format!("{} in {}", decided.rule, decided.source)));
            }
        }
        next = layer.parent.as_ref();
    }
    None
}

/// Git's global ignore file: `core.excludesFile` in the user's Git
/// configuration, or else `.config/git/ignore` in the home folder.
fn global_ignore(fold: bool) -> Option<Rules> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from)?;
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    let mut path = xdg.join("git").join("ignore");
    for config in [xdg.join("git").join("config"), home.join(".gitconfig")] {
        let Ok(text) = std::fs::read_to_string(&config) else { continue };
        let mut in_core = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                in_core = line.trim_start_matches('[').trim_end_matches(']').trim().eq_ignore_ascii_case("core");
            } else if in_core {
                if let Some((key, value)) = line.split_once('=') {
                    if key.trim().eq_ignore_ascii_case("excludesfile") {
                        let value = value.trim().trim_matches('"');
                        path = match value.strip_prefix("~/").or_else(|| value.strip_prefix("~\\")) {
                            Some(rest) => home.join(rest),
                            None => PathBuf::from(value),
                        };
                    }
                }
            }
        }
    }
    read_rules(&path, fold)
}

/// A path's volume and file number, attributes and size, following links; None when it cannot be opened.
fn identity(wide_path: &[u16]) -> Option<((u32, u64), u32, u64)> {
    let mut name = wide_path.to_vec();
    name.push(0);
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(), FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS, std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) } != 0;
    unsafe { CloseHandle(handle) };
    ok.then(|| {
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        let size = (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow);
        ((info.dwVolumeSerialNumber, index), info.dwFileAttributes, size)
    })
}

/// A path below `outer` relative to it with `/` and a trailing slash; empty when they are the same.
fn below(path: &Path, outer: &Path) -> Option<String> {
    let rest = path.strip_prefix(outer).ok()?.to_string_lossy().replace('\\', "/");
    Some(if rest.is_empty() { rest } else { rest + "/" })
}

/// Walks `root`. Every thread gets its own state from `state` and calls
/// `visit` with it for each file it finds, in no fixed order. Returns the
/// states, one per thread.
pub fn walk<S: Send>(
    root: &Path, options: Options, state: impl Fn() -> S + Sync, visit: impl Fn(&mut S, &Found) + Sync,
) -> std::io::Result<Vec<S>> {
    let absolute = std::path::absolute(root)?;
    // Where globs and --ignore-file files are matched from.
    let base = match options.base {
        Some(base) => std::path::absolute(base).ok(),
        None => std::env::current_dir().ok(),
    };
    let git_top = absolute.ancestors().find(|folder| folder.join(".git").exists()).map(Path::to_path_buf);
    let in_git = options.ignore && (options.gitignore_anywhere || git_top.is_some());
    // Rules for the whole walk, least binding first: the --ignore-file files,
    // Git's global file, then the ignore files of the folders above the root.
    let mut layer: Option<Arc<Layer>> = None;
    if options.ignore {
        let extra: Vec<Rules> = options.extra.iter().filter_map(|path| read_rules(path, options.ignore_fold)).collect();
        if !extra.is_empty() {
            // Matched from the base folder (the current one, as in ripgrep).
            let prepend = base.as_deref().and_then(|from| below(&absolute, from)).unwrap_or_default();
            layer = Some(Arc::new(Layer { strip: 0, prepend, files: extra, parent: None }));
        }
        if in_git && options.ignore_vcs && options.ignore_global {
            if let Some(global) = global_ignore(options.ignore_fold) {
                let prepend = git_top.as_deref().and_then(|top| below(&absolute, top)).unwrap_or_default();
                layer = Some(Arc::new(Layer { strip: 0, prepend, files: vec![global], parent: layer }));
            }
        }
        if options.ignore_parent {
            let above: Vec<&Path> = absolute.ancestors().skip(1).collect();
            for folder in above.iter().rev() {
                let present: Vec<&str> =
                    [".rgignore", ".ignore", ".gitignore"].into_iter().filter(|name| folder.join(name).is_file()).collect();
                let found = files(&wide(folder), &present, in_git, folder.join(".git").exists(), &options);
                if found.is_empty() {
                    continue;
                }
                let prepend = below(&absolute, folder).unwrap_or_default();
                layer = Some(Arc::new(Layer { strip: 0, prepend, files: found, parent: layer }));
            }
        }
    }
    let root_wide = wide(&absolute);
    let root_id = if options.follow || options.one_file_system { identity(&root_wide) } else { None };
    let ancestors = if options.follow { root_id.map(|(id, ..)| Arc::new(Ancestor { id, parent: None })) } else { None };
    let first = Job { wide: root_wide, relative: String::new(), layer, in_git, depth: 0, ancestors };
    let shared = Shared {
        spare: Mutex::new(vec![Work::Folder(first)]),
        wake: Condvar::new(),
        idle: AtomicUsize::new(0),
        open: AtomicUsize::new(1),
        volume: if options.one_file_system { root_id.map(|((volume, _), ..)| volume) } else { None },
        glob_prefix: base
            .as_deref()
            .and_then(|from| below(&absolute, from))
            .unwrap_or_else(|| format!("{}/", absolute.to_string_lossy().replace('\\', "/").trim_end_matches('/'))),
    };
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
            Work::Folder(job) => folder(job, shared, options, &mut state, visit, &mut own, &mut scratch),
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
                    visit(&mut state, &Found { path: relative, wide, size: file.size, modified: file.modified, accessed: file.accessed, created: file.created });
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
    /// A path as globs see it.
    globbed: String,
    wide: Vec<u16>,
}

#[derive(Clone)]
struct Listed {
    /// Where the name sits in `Scratch::names`.
    name: std::ops::Range<usize>,
    attributes: u32,
    size: u64,
    modified: u64,
    accessed: u64,
    created: u64,
}

fn time(t: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

/// Lists a folder into `scratch`; the Windows error when it cannot be listed.
fn list(folder: &[u16], scratch: &mut Scratch) -> Result<(), std::io::Error> {
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
        let error = std::io::Error::last_os_error();
        // An empty folder has nothing to list, which is not a failure.
        return if error.raw_os_error() == Some(2) || error.raw_os_error() == Some(18) { Ok(()) } else { Err(error) };
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
                modified: time(data.ftLastWriteTime),
                accessed: time(data.ftLastAccessTime),
                created: time(data.ftCreationTime),
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
    Ok(())
}

fn is(name: &[u16], text: &str) -> bool {
    name.len() == text.len() && name.iter().zip(text.bytes()).all(|(&unit, byte)| unit == u16::from(byte))
}

/// Reads one folder, reports its files and pushes its subfolders onto `more`.
fn folder<S>(
    job: Job, shared: &Shared, options: Options, state: &mut S, visit: &(impl Fn(&mut S, &Found) + Sync), more: &mut Vec<Work>,
    scratch: &mut Scratch,
) {
    // The entries of this folder are one level deeper than it.
    if options.max_depth.is_some_and(|most| job.depth + 1 > most) {
        return;
    }
    if let Err(error) = list(&job.wide, scratch) {
        if let Some(failure) = options.failure {
            failure(job.relative.trim_end_matches('/'), &error.to_string());
        }
        return;
    }
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
            let found = files(&job.wide, &special, in_git, git_root, &options);
            account::end(Step::IgnoreFiles, reading);
            if !found.is_empty() {
                layer = Some(Arc::new(Layer { strip: job.relative.len(), prepend: String::new(), files: found, parent: layer }));
            }
        }
    }
    let Scratch { entries, names, relative, joined, globbed, wide } = scratch;
    wide.clear();
    wide.extend_from_slice(&job.wide);
    wide.push(b'\\' as u16);
    let stem = wide.len();
    let fresh = |wide: &[u16]| Batch { wide: wide[..stem].to_vec(), relative: job.relative.clone(), names: Vec::new(), files: Vec::new() };
    let mut batch = (options.batch != 0).then(|| fresh(wide));
    let skip = |relative: &str, why: &str| {
        if let Some(report) = options.report {
            report(relative, why);
        }
    };
    let fail = |relative: &str, why: &str| {
        if let Some(failure) = options.failure {
            failure(relative, why);
        }
    };
    for entry in entries.iter_mut() {
        let name = &names[entry.name.clone()];
        let mut is_folder = entry.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        relative.clear();
        relative.push_str(&job.relative);
        relative.extend(char::decode_utf16(name.iter().copied()).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)));
        if is_folder && options.skip_folders.iter().any(|skipped| is(name, skipped)) {
            continue;
        }
        // A glob that picks or leaves out decides before anything else.
        let said = match options.pick {
            Some(pick) => {
                globbed.clear();
                globbed.push_str(&shared.glob_prefix);
                globbed.push_str(relative);
                pick.globs_say(globbed, is_folder)
            }
            None => Said::Silent,
        };
        if said == Said::LeftOut {
            skip(relative, "left out by a glob");
            continue;
        }
        if said == Said::Silent {
            let ruled = if options.ignore && layer.is_some() {
                let judging = account::begin();
                let ruled = verdict(&layer, relative, is_folder, joined);
                account::end(Step::Rules, judging);
                ruled
            } else {
                None
            };
            if let Some((Verdict::Ignored, rule)) = &ruled {
                skip(relative, &format!("ignored by {rule}"));
                continue;
            }
            // Types come after the ignore files and cannot bring a file back;
            // a type that picks a file lifts the hidden rule for it, as an
            // exception in an ignore file (`!.gitkeep`) does.
            let typed = match options.pick {
                Some(pick) if !is_folder && entry.attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 => pick.types_say(relative),
                _ => Said::Silent,
            };
            if typed == Said::LeftOut {
                skip(relative, "not of the chosen file types");
                continue;
            }
            let hidden = name.first() == Some(&(b'.' as u16)) || entry.attributes & FILE_ATTRIBUTE_HIDDEN != 0;
            if hidden && !options.hidden && !matches!(ruled, Some((Verdict::Kept, _))) && typed != Said::Picked {
                skip(relative, "hidden");
                continue;
            }
        }
        wide.truncate(stem);
        wide.extend_from_slice(name);
        let mut ancestors = None;
        if entry.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            // A link: followed only with `follow`, and then to what it leads to.
            if !options.follow {
                skip(relative, "a link, not followed without -L");
                continue;
            }
            let Some((id, attributes, size)) = identity(wide) else {
                fail(relative, "the link leads to nothing that can be opened");
                continue;
            };
            if shared.volume.is_some_and(|volume| volume != id.0) {
                skip(relative, "on another volume (--one-file-system)");
                continue;
            }
            is_folder = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
            entry.size = size;
            if is_folder {
                if Ancestor::holds(&job.ancestors, id) {
                    fail(relative, "file system loop found: the link points to a folder above it");
                    continue;
                }
                ancestors = Some(Arc::new(Ancestor { id, parent: job.ancestors.clone() }));
            }
        } else if is_folder && options.follow {
            // Known by number, so a link further down that leads back here is seen.
            ancestors = identity(wide).map(|(id, ..)| Arc::new(Ancestor { id, parent: job.ancestors.clone() }));
        }
        if is_folder {
            if options.max_depth.is_some_and(|most| job.depth + 2 > most) {
                continue;
            }
            relative.push('/');
            let ancestors = ancestors.or_else(|| job.ancestors.clone());
            more.push(Work::Folder(Job { wide: wide.clone(), relative: relative.clone(), layer: layer.clone(), in_git, depth: job.depth + 1, ancestors }));
            continue;
        }
        // A followed link to a file is judged by type here, now that it is known to be one.
        if said == Said::Silent && entry.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && options.pick.is_some_and(|pick| !pick.types_keep(relative)) {
            skip(relative, "not of the chosen file types");
            continue;
        }
        if options.max_filesize.is_some_and(|most| entry.size > most) {
            skip(relative, "larger than --max-filesize");
            continue;
        }
        if let Some(pending) = &mut batch {
            let at = pending.names.len();
            pending.names.extend_from_slice(name);
            pending.files.push(Listed { name: at..at + name.len(), ..entry.clone() });
            if pending.files.len() == options.batch {
                more.push(Work::Files(std::mem::replace(pending, fresh(wide))));
            }
        } else {
            visit(state, &Found { path: relative, wide, size: entry.size, modified: entry.modified, accessed: entry.accessed, created: entry.created });
        }
    }
    if let Some(pending) = batch {
        if !pending.files.is_empty() {
            more.push(Work::Files(pending));
        }
    }
}
