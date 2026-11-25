//! Search every file under a folder and report the matching lines.
//!
//! This is the plain design: walk, open each file, read it whole, search it.
//! It defines what the right answer is for the designs that avoid the opens.
//! The byte search and the pattern engine are still the `memchr` and `regex`
//! crates here; the walk and the ignore rules are this crate's own.
use crate::account::{self, Step};
use crate::walk::{self, Found};
use std::path::Path;
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{CreateFileW, ReadFile, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
};

pub use crate::matcher::Query;

/// What one thread gathers.
#[derive(Default)]
pub struct Gathered {
    pub out: Vec<u8>,
    /// Each file's part of `out`: its path relative to the root, and the range.
    pub parts: Vec<(String, std::ops::Range<usize>)>,
    pub files: u64,
    pub bytes: u64,
    pub matched_lines: u64,
    /// Matches counted by the caller's visit, and files with a selected line.
    pub matches: u64,
    pub files_matched: u64,
    /// For each part: the file's modified, accessed and created times, for --sort.
    pub times: Vec<(u64, u64, u64)>,
    content: Vec<u8>,
}

/// What is read of a large file before deciding whether it is text.
const HEAD: usize = 64 * 1024;

/// Whether bytes start with a UTF-16 byte-order mark (and not a UTF-32 one).
fn utf16_mark(bytes: &[u8]) -> bool {
    matches!(bytes, [0xFF, 0xFE, ..] | [0xFE, 0xFF, ..]) && !bytes.starts_with(&[0xFF, 0xFE, 0, 0])
}

/// A file that starts with a UTF-16 byte-order mark, as UTF-8 text without
/// the mark; None for any other file. Unpaired halves become U+FFFD.
fn utf16(bytes: &[u8]) -> Option<String> {
    if !utf16_mark(bytes) {
        return None;
    }
    let little = bytes[0] == 0xFF;
    let units = bytes[2..].chunks_exact(2).map(|pair| if little { u16::from_le_bytes([pair[0], pair[1]]) } else { u16::from_be_bytes([pair[0], pair[1]]) });
    Some(char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect())
}

/// Reads a whole file by the name the walk gives, or without `whole` only its
/// head when that already holds a zero byte. False when it cannot be read.
#[cfg(windows)]
fn read_file(found: &Found, wide: &mut Vec<u16>, into: &mut Vec<u8>, whole: bool) -> bool {
    wide.clear();
    wide.extend_from_slice(found.wide);
    wide.push(0);
    let opening = account::begin();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, 0,
            std::ptr::null_mut(),
        )
    };
    account::end(Step::FileOpen, opening);
    if handle == INVALID_HANDLE_VALUE {
        return false;
    }
    into.clear();
    // The listing gave the size; one read usually takes the whole file, and a
    // second, empty one confirms the end only if the file has grown since.
    // Ask for exactly what the listing says is there, never for the whole
    // spare capacity: Windows checks every page of the buffer it is offered,
    // so offering megabytes for a small file costs time for nothing.
    // A large file is read in two steps, its head and then exactly the rest,
    // so a binary one (a zero byte in its head) is left after the head; the
    // caller skips any file with a zero byte, and the rest cannot change that.
    let size = found.size as usize;
    let mut head = size > HEAD;
    let mut want = if head { HEAD } else { size.max(1) };
    let mut ok = true;
    loop {
        into.reserve(want);
        let ask = want.min(u32::MAX as usize) as u32;
        let mut got = 0u32;
        let reading = account::begin();
        let read = unsafe { ReadFile(handle, into.spare_capacity_mut().as_mut_ptr().cast(), ask, &mut got, std::ptr::null_mut()) };
        account::end(Step::FileRead, reading);
        if read == 0 {
            ok = false;
            break;
        }
        unsafe { into.set_len(into.len() + got as usize) };
        if head {
            head = false;
            // UTF-16 text has zero bytes too; a file with its mark is read on and decoded.
            if !whole && memchr::memchr(0, into).is_some() && !utf16_mark(into) {
                break;
            }
            if got as usize == HEAD {
                want = size - HEAD;
                continue;
            }
        }
        // A full first read of the listed size is the whole file. Anything
        // else (a short read, or a file that grew) is followed to its end.
        if got == 0 || (got == ask && into.len() as u64 == found.size) {
            break;
        }
        want = 64 * 1024;
    }
    // Closing here measured faster than handing handles to a closing thread,
    // which became the bottleneck (twice as slow over 29,498 files).
    let closing = account::begin();
    unsafe { CloseHandle(handle) };
    account::end(Step::FileClose, closing);
    ok
}

/// Reads a whole file by the name the walk gives, or without `whole` only its
/// head when that already holds a zero byte. False when it cannot be read.
#[cfg(target_os = "macos")]
fn read_file(found: &Found, name: &mut Vec<u8>, into: &mut Vec<u8>, whole: bool) -> bool {
    name.clear();
    name.extend_from_slice(found.native);
    name.push(0);
    let opening = account::begin();
    let fd = unsafe { libc::open(name.as_ptr().cast(), libc::O_RDONLY | libc::O_CLOEXEC) };
    account::end(Step::FileOpen, opening);
    if fd < 0 {
        return false;
    }
    into.clear();
    // The listing gave the size, so one read usually takes the whole file and
    // no second read is needed to learn that it has ended. A large file is
    // read in two steps, its head and then exactly the rest, so a binary one
    // (a zero byte in its head) is left after the head; the caller skips any
    // file with a zero byte, and the rest cannot change that.
    let size = found.size as usize;
    let mut head = size > HEAD;
    let mut want = if head { HEAD } else { size.max(1) };
    let mut ok = true;
    loop {
        into.reserve(want);
        let reading = account::begin();
        let got = unsafe { libc::read(fd, into.spare_capacity_mut().as_mut_ptr().cast(), want) };
        account::end(Step::FileRead, reading);
        if got < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            ok = false;
            break;
        }
        let got = got as usize;
        unsafe { into.set_len(into.len() + got) };
        if head {
            head = false;
            // UTF-16 text has zero bytes too; a file with its mark is read on and decoded.
            if !whole && memchr::memchr(0, into).is_some() && !utf16_mark(into) {
                break;
            }
            if got == HEAD {
                want = size - HEAD;
                continue;
            }
        }
        // A full first read of the listed size is the whole file. Anything
        // else (a short read, or a file that grew) is followed to its end.
        if got == 0 || (got == want && into.len() as u64 == found.size) {
            break;
        }
        want = 64 * 1024;
    }
    let closing = account::begin();
    unsafe { libc::close(fd) };
    account::end(Step::FileClose, closing);
    ok
}

/// One matching line of a file's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineHit {
    /// Counted from 1; 0 when the lines were not numbered.
    pub number: u64,
    /// The line is `bytes[start..end]`, without its line break.
    pub start: usize,
    pub end: usize,
}

/// Every line of `bytes` the query matches, in order, passed to `visit`.
/// A match may touch several lines; each is judged on its own, because a line
/// matches only if the pattern matches inside that line. With `numbered`, each
/// line carries its number, counted as the search moves forward.
pub fn matching_lines(query: &Query, bytes: &[u8], numbered: bool, mut visit: impl FnMut(LineHit)) -> u64 {
    let mut found = 0u64;
    let (mut counted, mut number) = (0usize, 1u64);
    let mut from = 0usize;
    while from <= bytes.len() {
        let Some((start, end)) = query.next(bytes, from) else { break };
        let mut line_start = memchr::memrchr(b'\n', &bytes[..start]).map_or(0, |at| at + 1);
        let last = end.max(start + 1).min(bytes.len());
        loop {
            let line_end = memchr::memchr(b'\n', &bytes[line_start..]).map_or(bytes.len(), |at| line_start + at);
            if line_start < bytes.len() && query.within(&bytes[line_start..line_end]) {
                found += 1;
                if numbered {
                    number += memchr::memchr_iter(b'\n', &bytes[counted..line_start]).count() as u64;
                    counted = line_start;
                }
                visit(LineHit { number: if numbered { number } else { 0 }, start: line_start, end: line_end });
            }
            from = line_end + 1;
            line_start = line_end + 1;
            if line_start >= last {
                break;
            }
        }
    }
    found
}

/// A line longer than this many bytes is shown as a part around its first
/// match, so one minified line cannot fill a screen or an agent's context.
pub const LONG_LINE: usize = 1000;
/// Of a long line: the bytes shown before its first match, and in all.
const SHOWN_BEFORE: usize = 150;
const SHOWN: usize = 800;

static CLIPPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Whether long lines are shown in part (the default) or whole.
pub fn set_clipping(on: bool) {
    CLIPPING.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// The part of `line` to show when its first match starts at `at`: all of it
/// unless it is longer than `LONG_LINE`, otherwise about `SHOWN` bytes from a
/// little before the match, cut between characters.
pub fn clip(line: &[u8], at: usize) -> std::ops::Range<usize> {
    if line.len() <= LONG_LINE || !CLIPPING.load(std::sync::atomic::Ordering::Relaxed) {
        return 0..line.len();
    }
    let follows = |i: usize| i < line.len() && (0x80..0xC0).contains(&line[i]);
    let mut start = at.min(line.len()).saturating_sub(SHOWN_BEFORE);
    while follows(start) {
        start += 1;
    }
    let mut end = (start + SHOWN).min(line.len());
    while follows(end) {
        end -= 1;
    }
    start..end
}

/// Appends the part of `line` that `clip` shows, with how many bytes were left out on either side.
pub fn write_clipped(out: &mut Vec<u8>, line: &[u8], at: usize) {
    let shown = clip(line, at);
    if shown.start != 0 {
        out.extend_from_slice(format!("[{} bytes omitted] ", shown.start).as_bytes());
    }
    out.extend_from_slice(&line[shown.clone()]);
    if shown.end != line.len() {
        out.extend_from_slice(format!(" [{} bytes omitted]", line.len() - shown.end).as_bytes());
    }
}

/// Where the first match in a line starts, for showing a long line in part.
pub fn first_match(query: &Query, line: &[u8]) -> usize {
    if line.len() <= LONG_LINE {
        return 0;
    }
    query.next(line, 0).map_or(0, |(start, _)| start)
}

/// Formats one file's matching lines: called with the file's path relative to
/// the root, its bytes and its numbered matching lines, on the searching thread.
/// Returns how many of the lines it kept, which may be fewer when it filters.
pub type Format<'a> = dyn Fn(&str, &[u8], &[LineHit], &mut Vec<u8>) -> u64 + Sync + 'a;

/// Searches every text file the walk reports under `root` and hands each file
/// with matching lines to `format`, which writes that file's part of the
/// output. A file with a zero byte is passed over; UTF-16 text is searched as
/// UTF-8, and positions in the output are positions in that UTF-8 text.
pub fn tree_with(root: &Path, options: walk::Options, query: &Query, format: &Format<'_>) -> std::io::Result<Vec<Gathered>> {
    files(root, options, false, &|read: &FileRead, out: &mut Vec<u8>| {
        let checking = account::begin();
        let decoded = utf16(read.bytes);
        let content: &[u8] = decoded.as_ref().map_or(read.bytes, |text| text.as_bytes());
        let text = read.complete && memchr::memchr(0, content).is_none();
        account::end(Step::Binary, checking);
        if !text {
            return Outcome::default();
        }
        let mut hits = Vec::new();
        matching_lines(query, content, true, |hit| hits.push(hit));
        if hits.is_empty() {
            return Outcome::default();
        }
        Outcome { lines: format(read.found.path, content, &hits, out), matches: 0 }
    })
}

/// What one searching thread keeps: its results and the buffer it names files in.
#[cfg(windows)]
type Scratch = (Gathered, Vec<u16>);
#[cfg(target_os = "macos")]
type Scratch = (Gathered, Vec<u8>, crate::kept::Fresh);

/// One file of a walk as read: where it is, and its bytes, which are all of it
/// unless reading stopped after a head that held a zero byte.
pub struct FileRead<'a> {
    pub found: &'a Found<'a>,
    pub bytes: &'a [u8],
    pub complete: bool,
}

/// What searching one file gave: selected lines and matches.
#[derive(Clone, Copy, Debug, Default)]
pub struct Outcome {
    pub lines: u64,
    pub matches: u64,
}

/// Searches one file read by `files`, appending its output.
pub type Visit<'a> = dyn Fn(&FileRead, &mut Vec<u8>) -> Outcome + Sync + 'a;

/// Reads every file the walk reports under `root` and hands it to `visit`
/// on the reading thread. With `whole`, a file whose head holds a zero byte
/// is read to its end too (binary files are then searched, not passed over).
#[cfg(windows)]
pub fn files(root: &Path, options: walk::Options, whole: bool, visit: &Visit<'_>) -> std::io::Result<Vec<Gathered>> {
    walk::walk(root, options, Scratch::default, |scratch, found| visit_one(scratch, found, whole, visit))
        .map(|states| states.into_iter().map(|(gathered, ..)| gathered).collect())
}

/// Reads every file the walk reports under `root` and hands it to `visit`
/// on the reading thread. With `whole`, a file whose head holds a zero byte
/// is read to its end too (binary files are then searched, not passed over).
/// Where a kept copy of the project's text is on, a file that has not changed
/// since it was kept is answered from it without being opened.
#[cfg(target_os = "macos")]
pub fn files(root: &Path, options: walk::Options, whole: bool, visit: &Visit<'_>) -> std::io::Result<Vec<Gathered>> {
    let kept = if options.details { crate::kept::Kept::for_walk(root) } else { None };
    let states = walk::walk(root, options, Scratch::default, |scratch, found| visit_kept(scratch, found, whole, visit, kept.as_ref()))?;
    let mut gathered = Vec::with_capacity(states.len());
    let mut fresh = Vec::with_capacity(states.len());
    for (found, _, kept) in states {
        gathered.push(found);
        fresh.push(kept);
    }
    if let Some(kept) = kept {
        kept.settle(fresh);
    }
    Ok(gathered)
}

/// `files` for a given list of files under `root` (paths relative to it, with their sizes).
#[cfg(target_os = "macos")]
pub fn files_listed(root: &Path, list: &[(String, u64)], threads: usize, whole: bool, visit: &Visit<'_>) -> std::io::Result<Vec<Gathered>> {
    use std::os::unix::ffi::OsStrExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mut base: Vec<u8> = std::path::absolute(root)?.as_os_str().as_bytes().to_vec();
    while base.last() == Some(&b'/') {
        base.pop();
    }
    let next = AtomicUsize::new(0);
    Ok(std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads.clamp(1, list.len().max(1)))
            .map(|_| {
                scope.spawn(|| {
                    let mut scratch = Scratch::default();
                    let mut native = Vec::new();
                    loop {
                        let Some((path, size)) = list.get(next.fetch_add(1, Ordering::Relaxed)) else { return scratch.0 };
                        native.clear();
                        native.extend_from_slice(&base);
                        native.push(b'/');
                        native.extend_from_slice(path.as_bytes());
                        let found = Found { path, native: &native, size: *size, modified: 0, accessed: 0, created: 0, changed: 0 };
                        visit_one(&mut scratch, &found, whole, visit);
                    }
                })
            })
            .collect();
        workers.into_iter().map(|worker| worker.join().expect("a searching thread stopped")).collect()
    }))
}

/// `files` for a given list of files under `root` (paths relative to it, with their sizes).
#[cfg(windows)]
pub fn files_listed(root: &Path, list: &[(String, u64)], threads: usize, whole: bool, visit: &Visit<'_>) -> std::io::Result<Vec<Gathered>> {
    use std::os::windows::ffi::OsStrExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let base: Vec<u16> = std::path::absolute(root)?.as_os_str().encode_wide().collect();
    let next = AtomicUsize::new(0);
    Ok(std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads.clamp(1, list.len().max(1)))
            .map(|_| {
                scope.spawn(|| {
                    let mut scratch = Scratch::default();
                    let mut wide = Vec::new();
                    loop {
                        let Some((path, size)) = list.get(next.fetch_add(1, Ordering::Relaxed)) else { return scratch.0 };
                        wide.clear();
                        wide.extend_from_slice(&base);
                        wide.push(u16::from(b'\\'));
                        wide.extend(path.encode_utf16().map(|unit| if unit == u16::from(b'/') { u16::from(b'\\') } else { unit }));
                        let found = Found { path, wide: &wide, size: *size, modified: 0, accessed: 0, created: 0 };
                        visit_one(&mut scratch, &found, whole, visit);
                    }
                })
            })
            .collect();
        workers.into_iter().map(|worker| worker.join().expect("a searching thread stopped")).collect()
    }))
}

#[cfg(target_os = "macos")]
fn visit_one(scratch: &mut Scratch, found: &Found, whole: bool, visit: &Visit<'_>) {
    visit_kept(scratch, found, whole, visit, None)
}

/// Hands one file to `visit`: from the kept copy when the file is as it was
/// when kept, else read from the disk, and then kept for the next search.
#[cfg(target_os = "macos")]
fn visit_kept((gathered, name, fresh): &mut Scratch, found: &Found, whole: bool, visit: &Visit<'_>, kept: Option<&crate::kept::Kept>) {
    use crate::kept::Kind;
    let mut content = std::mem::take(&mut gathered.content);
    let known = kept.and_then(|kept| {
        kept.name_into(found.path, &mut fresh.key);
        kept.bytes(&fresh.key, found.size, found.modified, found.changed, whole)
    });
    let read = known.is_some() || read_file(found, name, &mut content, whole);
    if read {
        if let (None, Some(kept)) = (known, kept.filter(|kept| !kept.has(&fresh.key, found.size, found.modified, found.changed))) {
            // Kept as a read without `whole` would give it: all of the file, or
            // the head of a larger file when the head holds a zero byte.
            let binary_head = memchr::memchr(0, &content[..content.len().min(HEAD)]).is_some() && !utf16_mark(&content);
            if binary_head && found.size > HEAD as u64 {
                if content.len() >= HEAD {
                    kept.keep(fresh, found.size, found.modified, found.changed, &content[..HEAD], Kind::Head);
                }
            } else if content.len() as u64 == found.size {
                kept.keep(fresh, found.size, found.modified, found.changed, &content, Kind::Whole);
            }
        }
        let bytes: &[u8] = known.unwrap_or(&content);
        gathered.files += 1;
        let complete = whole || bytes.len() as u64 >= found.size;
        gathered.bytes += bytes.len() as u64;
        let from = gathered.out.len();
        let matching = account::begin();
        let outcome = visit(&FileRead { found, bytes, complete }, &mut gathered.out);
        account::end(Step::Match, matching);
        gathered.matched_lines += outcome.lines;
        gathered.matches += outcome.matches;
        gathered.files_matched += u64::from(outcome.lines > 0);
        if gathered.out.len() > from {
            gathered.parts.push((found.path.to_owned(), from..gathered.out.len()));
            gathered.times.push((found.modified, found.accessed, found.created));
        }
    }
    gathered.content = content;
}

#[cfg(windows)]
fn visit_one((gathered, wide): &mut Scratch, found: &Found, whole: bool, visit: &Visit<'_>) {
    let mut content = std::mem::take(&mut gathered.content);
    if read_file(found, wide, &mut content, whole) {
        gathered.files += 1;
        let complete = whole || content.len() as u64 >= found.size;
        gathered.bytes += content.len() as u64;
        let from = gathered.out.len();
        let matching = account::begin();
        let outcome = visit(&FileRead { found, bytes: &content, complete }, &mut gathered.out);
        account::end(Step::Match, matching);
        gathered.matched_lines += outcome.lines;
        gathered.matches += outcome.matches;
        gathered.files_matched += u64::from(outcome.lines > 0);
        if gathered.out.len() > from {
            gathered.parts.push((found.path.to_owned(), from..gathered.out.len()));
            gathered.times.push((found.modified, found.accessed, found.created));
        }
    }
    gathered.content = content;
}

/// How the files of a folder search are put in order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Order {
    /// Path order ignoring letter case (lw's own).
    Path,
    /// Names compared as bytes, folder by folder (ripgrep's --sort path).
    Bytes,
    Modified,
    Accessed,
    Created,
}

/// Every file's part of the output in the order asked for, reversed with `reverse`.
pub fn ordered(gathered: &[Gathered], order: Order, reverse: bool) -> Vec<(&str, &[u8])> {
    let mut parts: Vec<(&str, &[u8], (u64, u64, u64))> = gathered
        .iter()
        .flat_map(|g| g.parts.iter().zip(&g.times).map(move |((path, range), times)| (path.as_str(), &g.out[range.clone()], *times)))
        .collect();
    match order {
        Order::Path => by_path(&mut parts, |part| part.0),
        // Name by name as bytes: the same as whole paths compared with `/` below every other byte.
        Order::Bytes => parts.sort_unstable_by(|a, b| by_names(a.0.as_bytes(), b.0.as_bytes())),
        // Ties keep path order, so the result is the same every time.
        Order::Modified => parts.sort_by(|a, b| a.2 .0.cmp(&b.2 .0).then_with(|| a.0.cmp(b.0))),
        Order::Accessed => parts.sort_by(|a, b| a.2 .1.cmp(&b.2 .1).then_with(|| a.0.cmp(b.0))),
        Order::Created => parts.sort_by(|a, b| a.2 .2.cmp(&b.2 .2).then_with(|| a.0.cmp(b.0))),
    }
    if reverse {
        parts.reverse();
    }
    parts.into_iter().map(|(p, out, _)| (p, out)).collect()
}

/// Every file's part of the output, ordered by path, so the same search gives
/// the same output whichever threads found what.
pub fn in_order(gathered: &[Gathered]) -> Vec<(&str, &[u8])> {
    let mut parts: Vec<(&str, &[u8])> =
        gathered.iter().flat_map(|g| g.parts.iter().map(move |(path, range)| (path.as_str(), &g.out[range.clone()]))).collect();
    by_path(&mut parts, |part| part.0);
    parts
}

fn slash_first(byte: u8) -> u8 {
    if byte == b'/' {
        0
    } else {
        byte
    }
}

/// Two paths compared name by name as bytes. Paths in one tree share long
/// beginnings, so the shared part is passed over eight bytes at a time and
/// only the first bytes that differ are weighed, `/` below every other byte.
fn by_names(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let shared = a.len().min(b.len());
    let mut at = 0;
    while at + 8 <= shared && a[at..at + 8] == b[at..at + 8] {
        at += 8;
    }
    while at < shared && a[at] == b[at] {
        at += 1;
    }
    match (a.get(at), b.get(at)) {
        (Some(&x), Some(&y)) => slash_first(x).cmp(&slash_first(y)),
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// Sorts by path ignoring letter case, paths that differ only in case by their bytes.
/// Each path is lower-cased once, and not at all when it is already lower-case
/// ASCII: comparing made two lower-case copies per comparison before, which
/// was half the time of listing 183,083 files.
fn by_path<'a, T: Copy>(parts: &mut Vec<T>, path: impl Fn(&T) -> &'a str) {
    use std::borrow::Cow;
    let mut keyed: Vec<(Cow<'a, str>, &'a str, T)> = parts
        .iter()
        .map(|part| {
            let path = path(part);
            let plain = path.bytes().all(|b| b.is_ascii() && !b.is_ascii_uppercase());
            (if plain { Cow::Borrowed(path) } else { Cow::Owned(path.to_lowercase()) }, path, *part)
        })
        .collect();
    keyed.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    parts.clear();
    parts.extend(keyed.into_iter().map(|(_, _, part)| part));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_are_the_ones_defined_by_plain_comparison() {
        let names = ["b/A.txt", "b/a.txt", "B/a.txt", "a-b/x", "a/b/x", "a/b", "a.b/x", "É/x", "é/x", "Σας", "σας", "a", "A", "Zeta", "alpha/Beta/gamma", "alpha/beta/Gamma", "ΌΣΟΣ", "ὈΔΥΣΣΕΎΣ"];
        let mut expected = names.to_vec();
        expected.sort_unstable_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b)));
        let mut sorted = names.to_vec();
        by_path(&mut sorted, |name| name);
        assert_eq!(sorted, expected);
        let mut expected = names.to_vec();
        expected.sort_unstable_by(|a, b| a.split('/').cmp(b.split('/')));
        let mut sorted = names.to_vec();
        sorted.sort_unstable_by(|a, b| by_names(a.as_bytes(), b.as_bytes()));
        assert_eq!(sorted, expected);
    }

    fn found(pattern: &str, plain: bool, text: &str) -> Vec<(u64, String)> {
        let query = Query::new(pattern, plain, false, false).unwrap();
        let mut lines = Vec::new();
        matching_lines(&query, text.as_bytes(), true, |hit| lines.push((hit.number, text[hit.start..hit.end].to_owned())));
        lines
    }

    #[test]
    fn each_matching_line_is_reported_once_with_its_number() {
        let pair = |n: u64, t: &str| (n, t.to_owned());
        assert_eq!(found("ab", true, "ab ab\nx\nab"), [pair(1, "ab ab"), pair(3, "ab")]);
        assert_eq!(found("x", true, "a\n\nx\n"), [pair(3, "x")]);
        assert_eq!(found("zz", true, "a\nb\n"), []);
        // A pattern never matches across a line end: \s stops at it, as in ripgrep.
        assert_eq!(found(r"a\s+b", false, "a\nb\na b\n"), [pair(3, "a b")]);
        assert_eq!(found(r"^b", false, "ab\nb\n"), [pair(2, "b")]);
        assert_eq!(found(r"b$", false, "ab\r\nb"), [pair(2, "b")]);
        // The last line needs no line break.
        assert_eq!(found("end", true, "x\nthe end"), [pair(2, "the end")]);
    }
}
