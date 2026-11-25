//! Search every file under a folder and report the matching lines.
//!
//! This is the plain design: walk, open each file, read it whole, search it.
//! It defines what the right answer is for the designs that avoid the opens.
//! The byte search and the pattern engine are still the `memchr` and `regex`
//! crates here; the walk and the ignore rules are this crate's own.
use crate::account::{self, Step};
use crate::walk::{self, Found};
use std::path::Path;
use windows_sys::Win32::{
    Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{CreateFileW, ReadFile, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
};

pub struct Query {
    text: Option<memchr::memmem::Finder<'static>>,
    pattern: Option<regex::bytes::Regex>,
}

impl Query {
    /// `pattern` is plain text when `plain`, otherwise a regular expression.
    pub fn new(pattern: &str, plain: bool, ignore_case: bool, whole_word: bool) -> Result<Self, String> {
        if plain && !ignore_case && !whole_word {
            return Ok(Self { text: Some(memchr::memmem::Finder::new(pattern.as_bytes()).into_owned()), pattern: None });
        }
        let mut source = if plain { regex::escape(pattern) } else { pattern.to_owned() };
        if whole_word {
            source = format!(r"\b(?:{source})\b");
        }
        let built = regex::bytes::RegexBuilder::new(&source).case_insensitive(ignore_case).multi_line(true).unicode(true).build();
        Ok(Self { text: None, pattern: Some(built.map_err(|e| e.to_string())?) })
    }

    /// The next match at or after `from`, as a byte range.
    fn next(&self, bytes: &[u8], from: usize) -> Option<(usize, usize)> {
        match (&self.text, &self.pattern) {
            (Some(text), _) => text.find(&bytes[from..]).map(|at| (from + at, from + at + text.needle().len())),
            (_, Some(pattern)) => pattern.find_at(bytes, from).map(|m| (m.start(), m.end())),
            _ => None,
        }
    }

    fn within(&self, line: &[u8]) -> bool {
        match (&self.text, &self.pattern) {
            (Some(text), _) => text.find(line).is_some(),
            (_, Some(pattern)) => pattern.is_match(line),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Report {
    /// `path:line:text` for every matching line.
    Lines,
    /// `path:count` for every file with matching lines.
    Counts,
}

/// What one thread gathers.
#[derive(Default)]
pub struct Gathered {
    pub out: Vec<u8>,
    pub files: u64,
    pub bytes: u64,
    pub matched_lines: u64,
    content: Vec<u8>,
}

/// What is read of a large file before deciding whether it is text.
const HEAD: usize = 64 * 1024;

/// Reads a whole file by the name the walk gives, or only its head when that
/// already holds a zero byte. False when it cannot be read.
fn read(found: &Found, wide: &mut Vec<u16>, into: &mut Vec<u8>) -> bool {
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
            if memchr::memchr(0, into).is_some() {
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

/// The matching lines of one file's bytes, appended to `out`.
fn lines(query: &Query, name: &[u8], bytes: &[u8], report: Report, out: &mut Vec<u8>) -> u64 {
    let mut found = 0u64;
    // Line number of the byte at `counted`, kept as the search moves forward.
    let (mut counted, mut number) = (0usize, 1u64);
    let mut from = 0usize;
    while from <= bytes.len() {
        let Some((start, end)) = query.next(bytes, from) else { break };
        // A match may touch several lines; each is judged on its own, because
        // a line matches only if the pattern matches inside that line.
        let mut line_start = memchr::memrchr(b'\n', &bytes[..start]).map_or(0, |at| at + 1);
        let last = end.max(start + 1).min(bytes.len());
        loop {
            let line_end = memchr::memchr(b'\n', &bytes[line_start..]).map_or(bytes.len(), |at| line_start + at);
            let line = &bytes[line_start..line_end];
            if line_start < bytes.len() && query.within(line) {
                found += 1;
                if report == Report::Lines {
                    number += memchr::memchr_iter(b'\n', &bytes[counted..line_start]).count() as u64;
                    counted = line_start;
                    out.extend_from_slice(name);
                    out.push(b':');
                    let mut digits = [0u8; 20];
                    let mut at = digits.len();
                    let mut n = number;
                    loop {
                        at -= 1;
                        digits[at] = b'0' + (n % 10) as u8;
                        n /= 10;
                        if n == 0 {
                            break;
                        }
                    }
                    out.extend_from_slice(&digits[at..]);
                    out.push(b':');
                    out.extend_from_slice(line);
                    out.push(b'\n');
                }
            }
            from = line_end + 1;
            line_start = line_end + 1;
            if line_start >= last {
                break;
            }
        }
    }
    if report == Report::Counts && found != 0 {
        out.extend_from_slice(name);
        out.push(b':');
        out.extend_from_slice(found.to_string().as_bytes());
        out.push(b'\n');
    }
    found
}

/// Searches every file the walk reports under `root`. `shown` is put in front
/// of each relative path in the output.
pub fn tree(root: &Path, shown: &str, options: walk::Options, query: &Query, report: Report) -> std::io::Result<Vec<Gathered>> {
    walk::walk(
        root,
        options,
        || (Gathered::default(), Vec::<u16>::new(), Vec::<u8>::new()),
        |(gathered, wide, name), found| {
            let mut content = std::mem::take(&mut gathered.content);
            if read(found, wide, &mut content) {
                gathered.files += 1;
                gathered.bytes += content.len() as u64;
                // A file with a zero byte anywhere is taken to be binary and
                // left out, in every kind of report. ripgrep's answer for such a
                // file depends on which of its threads reads it; this one does not.
                let checking = account::begin();
                let text = memchr::memchr(0, &content).is_none();
                account::end(Step::Binary, checking);
                if text {
                    name.clear();
                    name.extend_from_slice(shown.as_bytes());
                    name.push(b'/');
                    name.extend_from_slice(found.path.as_bytes());
                    let matching = account::begin();
                    gathered.matched_lines += lines(query, name, &content, report, &mut gathered.out);
                    account::end(Step::Match, matching);
                }
            }
            gathered.content = content;
        },
    )
    .map(|states| states.into_iter().map(|(gathered, _, _)| gathered).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(pattern: &str, plain: bool, text: &str) -> String {
        let query = Query::new(pattern, plain, false, false).unwrap();
        let mut out = Vec::new();
        lines(&query, b"f", text.as_bytes(), Report::Lines, &mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn each_matching_line_is_reported_once_with_its_number() {
        assert_eq!(found("ab", true, "ab ab\nx\nab"), "f:1:ab ab\nf:3:ab\n");
        assert_eq!(found("x", true, "a\n\nx\n"), "f:3:x\n");
        assert_eq!(found("zz", true, "a\nb\n"), "");
        // A pattern that can run across a line break still reports whole lines,
        // and only lines it matches by themselves.
        assert_eq!(found(r"a\s+b", false, "a\nb\na b\n"), "f:3:a b\n");
        assert_eq!(found(r"^b", false, "ab\nb\n"), "f:2:b\n");
        assert_eq!(found(r"b$", false, "ab\r\nb"), "f:2:b\n");
        // The last line needs no line break.
        assert_eq!(found("end", true, "x\nthe end"), "f:2:the end\n");
    }
}
