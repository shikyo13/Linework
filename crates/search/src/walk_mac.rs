//! List the files under a folder on several threads, leaving out what the
//! ignore rules, the file choices and the hidden-file convention leave out.
//! This is the macOS walk; `walk.rs` is the Windows one, with the same rules
//! in the same order and the same way of sharing work between threads.
//!
//! Every folder is read once. A walk that needs sizes and times asks for them
//! with the names in one bulk call per batch of entries (`getattrlistbulk`); a
//! walk that needs only names reads names and kinds (`readdir`), which
//! measured faster still. No file is opened here except the ignore files
//! themselves, and with `follow` or `one_file_system` a folder is looked at
//! once more, to know it again or to know its volume.
//!
//! Four threads by default: every open of a file or folder on macOS passes
//! through one lock shared by the whole system, and past four threads a walk
//! measured slower, not faster (0.18 s at 4 threads, 0.45 s at 12, for
//! 222,625 entries).
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
    ffi::{c_char, c_int, c_void, OsStr},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
};

/// Told the path and the reason for each path left out (`--debug`).
pub type Report<'a> = &'a (dyn Fn(&str, &str) + Sync);

/// Told about each path that could not be walked: a link that leads nowhere,
/// a loop, a folder that cannot be listed. The search goes on without it.
pub type Failure<'a> = &'a (dyn Fn(&str, &str) + Sync);

#[derive(Clone, Copy)]
pub struct Options<'a> {
    /// Include names that start with a dot.
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
    /// Do not go onto another volume (`--one-file-system`).
    pub one_file_system: bool,
    /// The folder that `-g` globs and `--ignore-file` files are matched from.
    /// None is the current folder, as in ripgrep; the editor gives its workspace folder.
    pub base: Option<&'a Path>,
    /// Whether each file's size and times are wanted. Without them (and with no
    /// size limit) a folder is read for names and kinds only, and every file is
    /// reported with size and times of zero.
    pub details: bool,
    pub report: Option<Report<'a>>,
    pub failure: Option<Failure<'a>>,
}

/// The most threads a walk uses unless told otherwise.
pub const THREADS: usize = 4;

impl Default for Options<'_> {
    fn default() -> Self {
        let logical = std::thread::available_parallelism().map_or(THREADS, |n| n.get());
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
            threads: logical.clamp(1, THREADS),
            batch: 0,
            gitignore_anywhere: false,
            skip_folders: &[],
            pick: None,
            max_depth: None,
            max_filesize: None,
            follow: false,
            one_file_system: false,
            base: None,
            details: true,
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
    /// The same file as the system names it: its absolute path as bytes, without a final zero.
    pub native: &'a [u8],
    pub size: u64,
    /// Times in nanoseconds since 1970; zero in a walk without `details`.
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
    /// Where the Git files (`.gitignore`, `.git/info/exclude`) start in `files`.
    git_from: usize,
    /// The folder holds a `.git`: it is the top of a repository. As in ripgrep,
    /// the Git files of folders above it say nothing about what is inside it,
    /// so a repository inside another is judged by its own.
    git_root: bool,
    parent: Option<Arc<Layer>>,
}

/// A folder on the way down, known by its volume and file number, for finding a link back to it.
struct Ancestor {
    id: (u64, u64),
    parent: Option<Arc<Ancestor>>,
}

impl Ancestor {
    fn holds(this: &Option<Arc<Ancestor>>, id: (u64, u64)) -> bool {
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
    /// The folder's absolute path, without a trailing slash (the root folder `/` is empty).
    native: Vec<u8>,
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
    /// The folder's absolute path, with its trailing slash.
    native: Vec<u8>,
    /// Relative to the root with a trailing slash, or empty for the root.
    relative: String,
    names: Vec<u8>,
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
    volume: Option<u64>,
    /// What goes in front of a path for globs: the root as seen from the
    /// current folder, since ripgrep matches -g globs from there.
    glob_prefix: String,
}

fn native(path: &Path) -> Vec<u8> {
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    while bytes.last() == Some(&b'/') {
        bytes.pop();
    }
    bytes
}

fn read_rules(path: &Path, fold: bool) -> Option<Rules> {
    let rules = Rules::parse_from(&std::fs::read(path).ok()?, &path.to_string_lossy(), fold);
    (!rules.is_empty()).then_some(rules)
}

fn read(folder: &[u8], name: &str, fold: bool) -> Option<Rules> {
    let mut path = folder.to_vec();
    path.push(b'/');
    path.extend_from_slice(name.as_bytes());
    read_rules(Path::new(OsStr::from_bytes(&path)), fold)
}

/// The ignore files of one folder that the options apply, in the order they
/// bind, and where the Git files start among them.
fn files(folder: &[u8], names: &[&str], in_git: bool, git_root: bool, options: &Options) -> (Vec<Rules>, usize) {
    let mut out = Vec::new();
    if options.ignore_dot {
        for name in [".rgignore", ".ignore"] {
            if names.contains(&name) {
                out.extend(read(folder, name, options.ignore_fold));
            }
        }
    }
    let git_from = out.len();
    if in_git && options.ignore_vcs {
        if names.contains(&".gitignore") {
            out.extend(read(folder, ".gitignore", options.ignore_fold));
        }
        if git_root && options.ignore_exclude {
            out.extend(read(folder, ".git/info/exclude", options.ignore_fold));
        }
    }
    (out, git_from)
}

/// What the ignore files say about a path, and which rule said it.
fn verdict(layer: &Option<Arc<Layer>>, relative: &str, is_folder: bool, joined: &mut String) -> Option<(Verdict, String)> {
    let mut next = layer.as_ref();
    // Set once the top of the nearest repository has been passed on the way up.
    let mut left_repository = false;
    while let Some(layer) = next {
        let path: &str = if layer.prepend.is_empty() {
            &relative[layer.strip.min(relative.len())..]
        } else {
            joined.clear();
            joined.push_str(&layer.prepend);
            joined.push_str(relative);
            joined
        };
        let heard = if left_repository { &layer.files[..layer.git_from] } else { &layer.files[..] };
        for rules in heard {
            if let Some(decided) = rules.decide(path.as_bytes(), is_folder) {
                return Some((decided.verdict, format!("{} in {}", decided.rule, decided.source)));
            }
        }
        left_repository |= layer.git_root;
        next = layer.parent.as_ref();
    }
    None
}

/// Git's global ignore file: `core.excludesFile` in the user's Git
/// configuration, or else `.config/git/ignore` in the home folder.
fn global_ignore(fold: bool) -> Option<Rules> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let xdg = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
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
                        path = match value.strip_prefix("~/") {
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

/// What kind of thing a name in a folder is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Folder,
    Link,
    /// A pipe, socket or device: never searched or listed.
    Other,
}

fn kind_of_mode(mode: libc::mode_t) -> Kind {
    match mode & libc::S_IFMT {
        libc::S_IFREG => Kind::File,
        libc::S_IFDIR => Kind::Folder,
        libc::S_IFLNK => Kind::Link,
        _ => Kind::Other,
    }
}

fn nanos(seconds: i64, nanoseconds: i64) -> u64 {
    if seconds < 0 {
        0
    } else {
        (seconds as u64).saturating_mul(1_000_000_000).saturating_add(nanoseconds as u64)
    }
}

/// A path's volume and file number, kind and size, following links; None when it leads nowhere.
fn identity(path: &[u8], scratch: &mut Vec<u8>) -> Option<((u64, u64), Kind, u64)> {
    scratch.clear();
    scratch.extend_from_slice(if path.is_empty() { b"/" } else { path });
    scratch.push(0);
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::stat(scratch.as_ptr().cast::<c_char>(), &mut st) } == 0)
        .then(|| ((st.st_dev as u64, st.st_ino), kind_of_mode(st.st_mode), st.st_size.max(0) as u64))
}

/// A path below `outer` relative to it with a trailing slash; empty when they are the same.
fn below(path: &Path, outer: &Path) -> Option<String> {
    let rest = path.strip_prefix(outer).ok()?.to_string_lossy().into_owned();
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
            layer = Some(Arc::new(Layer { strip: 0, prepend, git_from: extra.len(), git_root: false, files: extra, parent: None }));
        }
        if in_git && options.ignore_vcs && options.ignore_global {
            if let Some(global) = global_ignore(options.ignore_fold) {
                let prepend = git_top.as_deref().and_then(|top| below(&absolute, top)).unwrap_or_default();
                layer = Some(Arc::new(Layer { strip: 0, prepend, files: vec![global], git_from: 1, git_root: false, parent: layer }));
            }
        }
        if options.ignore_parent {
            let above: Vec<&Path> = absolute.ancestors().skip(1).collect();
            for folder in above.iter().rev() {
                let present: Vec<&str> =
                    [".rgignore", ".ignore", ".gitignore"].into_iter().filter(|name| folder.join(name).is_file()).collect();
                let git_root = folder.join(".git").exists();
                let (found, git_from) = files(&native(folder), &present, in_git, git_root, &options);
                if found.is_empty() && !git_root {
                    continue;
                }
                let prepend = below(&absolute, folder).unwrap_or_default();
                layer = Some(Arc::new(Layer { strip: 0, prepend, files: found, git_from, git_root, parent: layer }));
            }
        }
    }
    let root_native = native(&absolute);
    let mut spare_path = Vec::new();
    let root_id = if options.follow || options.one_file_system { identity(&root_native, &mut spare_path) } else { None };
    let ancestors = if options.follow { root_id.map(|(id, ..)| Arc::new(Ancestor { id, parent: None })) } else { None };
    let first = Job { native: root_native, relative: String::new(), layer, in_git, depth: 0, ancestors };
    let shared = Shared {
        spare: Mutex::new(vec![Work::Folder(first)]),
        wake: Condvar::new(),
        idle: AtomicUsize::new(0),
        open: AtomicUsize::new(1),
        volume: if options.one_file_system { root_id.map(|((volume, _), ..)| volume) } else { None },
        glob_prefix: base
            .as_deref()
            .and_then(|from| below(&absolute, from))
            .unwrap_or_else(|| format!("{}/", absolute.to_string_lossy().trim_end_matches('/'))),
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
                let Scratch { relative, native, .. } = &mut scratch;
                for file in &batch.files {
                    let name = &batch.names[file.name.clone()];
                    native.clear();
                    native.extend_from_slice(&batch.native);
                    native.extend_from_slice(name);
                    relative.clear();
                    relative.push_str(&batch.relative);
                    relative.push_str(&String::from_utf8_lossy(name));
                    visit(&mut state, &Found { path: relative, native, size: file.size, modified: file.modified, accessed: file.accessed, created: file.created });
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
    names: Vec<u8>,
    relative: String,
    joined: String,
    /// A path as globs see it.
    globbed: String,
    native: Vec<u8>,
    /// A path with its final zero, for a system call.
    call: Vec<u8>,
    /// Where a bulk listing puts a batch of entries.
    bulk: Vec<u8>,
}

#[derive(Clone)]
struct Listed {
    /// Where the name sits in `Scratch::names`.
    name: std::ops::Range<usize>,
    kind: Kind,
    size: u64,
    modified: u64,
    accessed: u64,
    created: u64,
}

// The few names of <sys/attr.h> and <sys/vnode.h> the bulk listing needs.
const ATTR_BIT_MAP_COUNT: u16 = 5;
const ATTR_CMN_NAME: u32 = 0x0000_0001;
const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
const ATTR_CMN_CRTIME: u32 = 0x0000_0200;
const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
const ATTR_CMN_ACCTIME: u32 = 0x0000_1000;
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
const ATTR_FILE_DATALENGTH: u32 = 0x0000_0200;
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

#[repr(C)]
struct AttrList {
    bitmapcount: u16,
    reserved: u16,
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

extern "C" {
    fn getattrlistbulk(dirfd: c_int, attr_list: *mut AttrList, attr_buf: *mut c_void, attr_buf_size: usize, options: u64) -> c_int;
}

/// How much room a bulk listing offers the system for one batch of entries.
const BULK: usize = 64 * 1024;

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn i64_at(bytes: &[u8], at: usize) -> i64 {
    i64::from_ne_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// Lists a folder into `scratch` with each entry's size and times, in one call
/// per batch. `Ok(false)` when this volume cannot answer a bulk listing.
fn list_bulk(scratch: &mut Scratch) -> Result<bool, std::io::Error> {
    let first = account::begin();
    let fd = unsafe { libc::open(scratch.call.as_ptr().cast::<c_char>(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        account::end(Step::ListFirst, first);
        return Err(std::io::Error::last_os_error());
    }
    let mut asked = AttrList {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_ERROR | ATTR_CMN_NAME | ATTR_CMN_OBJTYPE | ATTR_CMN_CRTIME | ATTR_CMN_MODTIME | ATTR_CMN_ACCTIME,
        volattr: 0,
        dirattr: 0,
        fileattr: ATTR_FILE_DATALENGTH,
        forkattr: 0,
    };
    scratch.bulk.resize(BULK, 0);
    let mut began = first;
    let mut step = Step::ListFirst;
    let mut result = Ok(true);
    loop {
        let count = unsafe { getattrlistbulk(fd, &mut asked, scratch.bulk.as_mut_ptr().cast(), BULK, 0) };
        account::end(step, began);
        if count < 0 {
            let error = std::io::Error::last_os_error();
            // A volume without this call: the caller reads names and asks about each.
            result = if scratch.entries.is_empty() && matches!(error.raw_os_error(), Some(libc::ENOTSUP) | Some(libc::EINVAL)) { Ok(false) } else { Err(error) };
            break;
        }
        if count == 0 {
            break;
        }
        let buffer = &scratch.bulk;
        let mut at = 0usize;
        for _ in 0..count {
            let length = u32_at(buffer, at) as usize;
            let entry = &buffer[at..at + length];
            // After the length: which attributes came back (five 32-bit sets), then each in the order of its bit.
            let common = u32_at(entry, 4);
            let file = u32_at(entry, 4 + 12);
            let mut field = 4 + 20;
            let mut failed = false;
            if common & ATTR_CMN_ERROR != 0 {
                failed = u32_at(entry, field) != 0;
                field += 4;
            }
            let mut name: &[u8] = &[];
            if common & ATTR_CMN_NAME != 0 {
                // An offset from this field to the name, and the name's length with its final zero.
                let offset = u32_at(entry, field) as usize;
                let len = u32_at(entry, field + 4) as usize;
                name = &entry[field + offset..field + offset + len.saturating_sub(1)];
                field += 8;
            }
            let mut kind = Kind::Other;
            let mut typed = false;
            if common & ATTR_CMN_OBJTYPE != 0 {
                kind = match u32_at(entry, field) {
                    VREG => Kind::File,
                    VDIR => Kind::Folder,
                    VLNK => Kind::Link,
                    _ => Kind::Other,
                };
                typed = true;
                field += 4;
            }
            let (mut created, mut modified, mut accessed, mut size) = (0, 0, 0, 0);
            if common & ATTR_CMN_CRTIME != 0 {
                created = nanos(i64_at(entry, field), i64_at(entry, field + 8));
                field += 16;
            }
            if common & ATTR_CMN_MODTIME != 0 {
                modified = nanos(i64_at(entry, field), i64_at(entry, field + 8));
                field += 16;
            }
            if common & ATTR_CMN_ACCTIME != 0 {
                accessed = nanos(i64_at(entry, field), i64_at(entry, field + 8));
                field += 16;
            }
            if file & ATTR_FILE_DATALENGTH != 0 {
                size = i64_at(entry, field).max(0) as u64;
            }
            at += length;
            if name.is_empty() || name == b"." || name == b".." {
                continue;
            }
            let start = scratch.names.len();
            scratch.names.extend_from_slice(name);
            let mut listed = Listed { name: start..scratch.names.len(), kind, size, modified, accessed, created };
            if failed || !typed {
                // The system could not describe this one in the batch: ask about it alone.
                ask(fd, &mut scratch.names, &mut listed);
            }
            scratch.entries.push(listed);
        }
        began = account::begin();
        step = Step::ListNext;
    }
    let closing = account::begin();
    unsafe { libc::close(fd) };
    account::end(Step::ListClose, closing);
    result
}

/// Fills in one entry's kind, size and times by asking about it alone, without following a link.
fn ask(folder: c_int, names: &mut Vec<u8>, listed: &mut Listed) {
    let mut name = names[listed.name.clone()].to_vec();
    name.push(0);
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(folder, name.as_ptr().cast::<c_char>(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } == 0 {
        listed.kind = kind_of_mode(st.st_mode);
        listed.size = st.st_size.max(0) as u64;
        listed.modified = nanos(st.st_mtime, st.st_mtime_nsec);
        listed.accessed = nanos(st.st_atime, st.st_atime_nsec);
        listed.created = nanos(st.st_birthtime, st.st_birthtime_nsec);
    } else {
        listed.kind = Kind::Other;
    }
}

/// Lists a folder into `scratch` by names and kinds; with `details`, asks about each entry as well.
fn list_names(scratch: &mut Scratch, details: bool) -> Result<(), std::io::Error> {
    let first = account::begin();
    let dir = unsafe { libc::opendir(scratch.call.as_ptr().cast::<c_char>()) };
    if dir.is_null() {
        account::end(Step::ListFirst, first);
        return Err(std::io::Error::last_os_error());
    }
    let fd = unsafe { libc::dirfd(dir) };
    let mut began = first;
    let mut step = Step::ListFirst;
    loop {
        let entry = unsafe { libc::readdir(dir) };
        account::end(step, began);
        if entry.is_null() {
            break;
        }
        let entry = unsafe { &*entry };
        let name = unsafe { std::slice::from_raw_parts(entry.d_name.as_ptr().cast::<u8>(), entry.d_namlen as usize) };
        if name != b"." && name != b".." {
            let start = scratch.names.len();
            scratch.names.extend_from_slice(name);
            let kind = match entry.d_type {
                libc::DT_REG => Some(Kind::File),
                libc::DT_DIR => Some(Kind::Folder),
                libc::DT_LNK => Some(Kind::Link),
                libc::DT_UNKNOWN => None,
                _ => Some(Kind::Other),
            };
            let mut listed = Listed { name: start..scratch.names.len(), kind: kind.unwrap_or(Kind::Other), size: 0, modified: 0, accessed: 0, created: 0 };
            if details || kind.is_none() {
                ask(fd, &mut scratch.names, &mut listed);
            }
            scratch.entries.push(listed);
        }
        began = account::begin();
        step = Step::ListNext;
    }
    let closing = account::begin();
    unsafe { libc::closedir(dir) };
    account::end(Step::ListClose, closing);
    Ok(())
}

/// Lists a folder into `scratch`; the system's error when it cannot be listed.
fn list(folder: &[u8], details: bool, scratch: &mut Scratch) -> Result<(), std::io::Error> {
    scratch.entries.clear();
    scratch.names.clear();
    scratch.call.clear();
    scratch.call.extend_from_slice(if folder.is_empty() { b"/" } else { folder });
    scratch.call.push(0);
    if details && list_bulk(scratch)? {
        return Ok(());
    }
    scratch.entries.clear();
    scratch.names.clear();
    list_names(scratch, details)
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
    let details = options.details || options.max_filesize.is_some();
    if let Err(error) = list(&job.native, details, scratch) {
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
            if name.first() == Some(&b'.') {
                special.extend([".rgignore", ".ignore", ".gitignore", ".git"].into_iter().find(|known| name == known.as_bytes()));
            }
        }
        let git_root = special.contains(&".git");
        in_git |= git_root;
        if !special.is_empty() {
            let reading = account::begin();
            let (found, git_from) = files(&job.native, &special, in_git, git_root, &options);
            account::end(Step::IgnoreFiles, reading);
            if !found.is_empty() || git_root {
                layer = Some(Arc::new(Layer { strip: job.relative.len(), prepend: String::new(), files: found, git_from, git_root, parent: layer }));
            }
        }
    }
    let Scratch { entries, names, relative, joined, globbed, native, call, .. } = scratch;
    native.clear();
    native.extend_from_slice(&job.native);
    native.push(b'/');
    let stem = native.len();
    let fresh = |native: &[u8]| Batch { native: native[..stem].to_vec(), relative: job.relative.clone(), names: Vec::new(), files: Vec::new() };
    let mut batch = (options.batch != 0).then(|| fresh(native));
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
        let mut is_folder = entry.kind == Kind::Folder;
        let is_link = entry.kind == Kind::Link;
        relative.clear();
        relative.push_str(&job.relative);
        relative.push_str(&String::from_utf8_lossy(name));
        if is_folder && options.skip_folders.iter().any(|skipped| name == skipped.as_bytes()) {
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
                Some(pick) if !is_folder && !is_link => pick.types_say(relative),
                _ => Said::Silent,
            };
            if typed == Said::LeftOut {
                skip(relative, "not of the chosen file types");
                continue;
            }
            let hidden = name.first() == Some(&b'.');
            if hidden && !options.hidden && !matches!(ruled, Some((Verdict::Kept, _))) && typed != Said::Picked {
                skip(relative, "hidden");
                continue;
            }
        }
        native.truncate(stem);
        native.extend_from_slice(name);
        let mut ancestors = None;
        if is_link {
            // A link: followed only with `follow`, and then to what it leads to.
            if !options.follow {
                skip(relative, "a link, not followed without -L");
                continue;
            }
            let Some((id, kind, size)) = identity(native, call) else {
                fail(relative, "the link leads to nothing that can be opened");
                continue;
            };
            if shared.volume.is_some_and(|volume| volume != id.0) {
                skip(relative, "on another volume (--one-file-system)");
                continue;
            }
            if kind == Kind::Other {
                continue;
            }
            is_folder = kind == Kind::Folder;
            entry.size = size;
            if is_folder {
                if Ancestor::holds(&job.ancestors, id) {
                    fail(relative, "file system loop found: the link points to a folder above it");
                    continue;
                }
                ancestors = Some(Arc::new(Ancestor { id, parent: job.ancestors.clone() }));
            }
        } else if is_folder && (options.follow || shared.volume.is_some()) {
            // Known by number, so a link further down that leads back here is
            // seen; and by volume, since another volume is mounted on a plain folder.
            let known = identity(native, call);
            if let (Some(volume), Some((id, ..))) = (shared.volume, known) {
                if volume != id.0 {
                    skip(relative, "on another volume (--one-file-system)");
                    continue;
                }
            }
            if options.follow {
                ancestors = known.map(|(id, ..)| Arc::new(Ancestor { id, parent: job.ancestors.clone() }));
            }
        } else if entry.kind == Kind::Other {
            continue;
        }
        if is_folder {
            if options.max_depth.is_some_and(|most| job.depth + 2 > most) {
                continue;
            }
            relative.push('/');
            let ancestors = ancestors.or_else(|| job.ancestors.clone());
            more.push(Work::Folder(Job { native: native.clone(), relative: relative.clone(), layer: layer.clone(), in_git, depth: job.depth + 1, ancestors }));
            continue;
        }
        // A followed link to a file is judged by type here, now that it is known to be one.
        if said == Said::Silent && is_link && options.pick.is_some_and(|pick| !pick.types_keep(relative)) {
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
                more.push(Work::Files(std::mem::replace(pending, fresh(native))));
            }
        } else {
            visit(state, &Found { path: relative, native, size: entry.size, modified: entry.modified, accessed: entry.accessed, created: entry.created });
        }
    }
    if let Some(pending) = batch {
        if !pending.files.is_empty() {
            more.push(Work::Files(pending));
        }
    }
}
