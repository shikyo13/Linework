//! lw cut: fields, bytes or characters from each line, with the options of
//! GNU cut. A file is cut in pieces on several threads and written in order.
use crate::chunks::{self, Output};
use crate::gnu_args::{self, flag, valued, Spec};
use std::io::{self, Read, Write};

pub const SPECS: &[Spec] = &[
    valued(Some('b'), "bytes"),
    valued(Some('c'), "characters"),
    valued(Some('f'), "fields"),
    valued(Some('d'), "delimiter"),
    flag(Some('s'), "only-delimited"),
    flag(None, "complement"),
    valued(None, "output-delimiter"),
    flag(Some('z'), "zero-terminated"),
    flag(Some('n'), "no-split-multibyte"),
    valued(Some('o'), "output"),
    valued(Some('j'), "threads"),
];

/// Which positions (fields or bytes, from 1) are kept.
#[derive(Clone)]
struct List {
    /// Merged, sorted, inclusive ranges.
    ranges: Vec<(u64, u64)>,
}

impl List {
    fn parse(text: &str, what: &str) -> Result<Self, String> {
        let mut ranges = Vec::new();
        for part in text.split([',', ' ']).filter(|p| !p.is_empty()) {
            let num = |t: &str| -> Result<u64, String> {
                let n: u64 = t.parse().map_err(|_| format!("{what} {text:?}: {t:?} is not a position"))?;
                if n == 0 {
                    return Err(format!("{what} {text:?}: positions are counted from 1"));
                }
                Ok(n)
            };
            let range = match part.split_once('-') {
                None => {
                    let n = num(part)?;
                    (n, n)
                }
                Some(("", "")) => return Err(format!("{what} {text:?}: a lone - is not a range")),
                Some(("", b)) => (1, num(b)?),
                Some((a, "")) => (num(a)?, u64::MAX),
                Some((a, b)) => {
                    let (a, b) = (num(a)?, num(b)?);
                    if b < a {
                        return Err(format!("{what} {text:?}: {part} runs backwards"));
                    }
                    (a, b)
                }
            };
            ranges.push(range);
        }
        if ranges.is_empty() {
            return Err(format!("{what} needs a list of positions, such as 1,3-5,7-"));
        }
        ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (a, b) in ranges {
            match merged.last_mut() {
                Some(last) if a <= last.1.saturating_add(1) => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        Ok(Self { ranges: merged })
    }

    /// The positions not in the list.
    fn complement(&self) -> Self {
        let mut out = Vec::new();
        let mut next = 1u64;
        for &(a, b) in &self.ranges {
            if a > next {
                out.push((next, a - 1));
            }
            next = b.saturating_add(1);
            if b == u64::MAX {
                next = u64::MAX;
                break;
            }
        }
        if next != u64::MAX {
            out.push((next, u64::MAX));
        }
        Self { ranges: out }
    }

    /// Whether every position from `n` on is kept.
    fn open_from(&self) -> u64 {
        self.ranges.last().filter(|r| r.1 == u64::MAX).map_or(u64::MAX, |r| r.0)
    }
}

#[derive(Clone)]
enum Mode {
    Fields { list: List, delimiter: u8, only_delimited: bool },
    Bytes(List),
}

#[derive(Clone)]
struct Cutter {
    mode: Mode,
    /// Between fields, or between byte ranges when given.
    joiner: Option<Vec<u8>>,
    end: u8,
}

impl Cutter {
    /// One line, without its end byte, cut and written with an end byte.
    fn line(&self, line: &[u8], out: &mut Vec<u8>) {
        match &self.mode {
            Mode::Bytes(list) => {
                let mut first = true;
                for &(a, b) in &list.ranges {
                    let start = (a - 1) as usize;
                    if start >= line.len() {
                        break;
                    }
                    let stop = b.min(line.len() as u64) as usize;
                    if !first {
                        if let Some(j) = &self.joiner {
                            out.extend_from_slice(j);
                        }
                    }
                    out.extend_from_slice(&line[start..stop]);
                    first = false;
                }
                out.push(self.end);
            }
            Mode::Fields { list, delimiter, only_delimited } => {
                let d = *delimiter;
                let Some(_) = memchr::memchr(d, line) else {
                    // A line with no delimiter is passed whole, or left out with -s.
                    if !*only_delimited {
                        out.extend_from_slice(line);
                        out.push(self.end);
                    }
                    return;
                };
                let joiner: &[u8] = self.joiner.as_deref().unwrap_or(std::slice::from_ref(delimiter));
                let open = list.open_from();
                let mut field = 1u64;
                let mut start = 0usize;
                let mut range = 0usize;
                let mut first = true;
                loop {
                    // From the first kept field of an open range on, the rest of the line goes as it is when the joiner is the delimiter.
                    while range < list.ranges.len() && list.ranges[range].1 < field {
                        range += 1;
                    }
                    if range == list.ranges.len() {
                        break;
                    }
                    let keep = list.ranges[range].0 <= field;
                    if keep && field >= open && joiner == [d] {
                        if !first {
                            out.extend_from_slice(joiner);
                        }
                        out.extend_from_slice(&line[start..]);
                        break;
                    }
                    let stop = memchr::memchr(d, &line[start..]).map(|i| start + i);
                    let end = stop.unwrap_or(line.len());
                    if keep {
                        if !first {
                            out.extend_from_slice(joiner);
                        }
                        out.extend_from_slice(&line[start..end]);
                        first = false;
                    }
                    match stop {
                        Some(s) => {
                            start = s + 1;
                            field += 1;
                        }
                        None => break,
                    }
                }
                out.push(self.end);
            }
        }
    }

    /// Whole lines (the last may lack its end byte) cut into `out`.
    fn block(&self, bytes: &[u8], out: &mut Vec<u8>) {
        out.reserve(bytes.len() / 2);
        let mut at = 0;
        while at < bytes.len() {
            let stop = memchr::memchr(self.end, &bytes[at..]).map_or(bytes.len(), |i| at + i);
            self.line(&bytes[at..stop], out);
            at = stop + 1;
        }
    }
}

pub fn run(args: &[String], stdout: &mut dyn Write) -> Result<i32, String> {
    let io = |e: io::Error| e.to_string();
    let g = gnu_args::parse("cut", SPECS, args)?;
    let threads = gnu_args::threads(&g)?;
    let end = if g.has("zero-terminated") { 0 } else { b'\n' };
    let lists: Vec<(&str, &str)> = g.options.iter().filter(|(l, _)| matches!(*l, "bytes" | "characters" | "fields")).map(|(l, v)| (*l, v.as_deref().unwrap_or(""))).collect();
    let (kind, text) = match lists.as_slice() {
        [one] => *one,
        [] => return Err("lw cut needs one of -b, -c or -f".into()),
        _ => return Err("only one of -b, -c and -f may be given".into()),
    };
    let mut list = List::parse(text, &format!("-{}", &kind[..1]))?;
    if g.has("complement") {
        list = list.complement();
    }
    let joiner = g.last("output-delimiter").map(|d| d.as_bytes().to_vec());
    let mode = if kind == "fields" {
        let delimiter = match g.last("delimiter") {
            None => b'\t',
            Some(d) => match linework_search::settings::unescape(d).as_slice() {
                [one] => *one,
                _ => return Err(format!("the delimiter must be a single byte, not {d:?}")),
            },
        };
        Mode::Fields { list, delimiter, only_delimited: g.has("only-delimited") }
    } else {
        if g.has("delimiter") {
            return Err("-d applies to fields (-f), not to bytes or characters".into());
        }
        if g.has("only-delimited") {
            return Err("-s applies to fields (-f), not to bytes or characters".into());
        }
        Mode::Bytes(list)
    };
    let cutter = Cutter { mode, joiner, end };
    let inputs: Vec<String> = if g.words.is_empty() { vec!["-".into()] } else { g.words.clone() };
    let mut output = match g.last("output") {
        Some(path) => {
            for i in &inputs {
                if i != "-" && chunks::same_file(std::path::Path::new(i), std::path::Path::new(path)) {
                    return Err(format!("{path} is also an input; write to another file"));
                }
            }
            Some(Output::create(std::path::Path::new(path)).map_err(io)?)
        }
        None => None,
    };
    let to_file = output.is_some();
    let out: &mut dyn Write = match output.as_mut() {
        Some(o) => o,
        None => stdout,
    };
    let mut failed = false;
    for input in &inputs {
        let result = if input == "-" { piped(io::stdin().lock(), &cutter, out) } else { from_file(input, &cutter, threads, out) };
        if let Err(e) = result {
            if e.kind() == io::ErrorKind::BrokenPipe {
                return Ok(0);
            }
            crate::note(format!("lw: {input}: {e}"));
            failed = true;
            if to_file {
                // A partial output file is not left behind.
                return Ok(2);
            }
        }
    }
    out.flush().map_err(io)?;
    if let Some(o) = output {
        o.finish().map_err(io)?;
    }
    Ok(if failed { 1 } else { 0 })
}

fn from_file(path: &str, cutter: &Cutter, threads: usize, out: &mut dyn Write) -> io::Result<()> {
    let (file, len) = chunks::open(path)?;
    let cuts = chunks::boundaries(&file, 0, len, 16 << 20, cutter.end)?;
    chunks::ordered(
        &file,
        &cuts,
        threads,
        &|_, bytes| {
            let mut part = Vec::new();
            cutter.block(bytes, &mut part);
            Ok(part)
        },
        &mut |_, part: Vec<u8>| out.write_all(&part),
    )
}

fn piped(mut input: impl Read, cutter: &Cutter, out: &mut dyn Write) -> io::Result<()> {
    let mut buf = vec![0u8; 8 << 20];
    let mut carry: Vec<u8> = Vec::new();
    let mut part = Vec::new();
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        carry.extend_from_slice(&buf[..n]);
        if let Some(last) = memchr::memrchr(cutter.end, &carry) {
            part.clear();
            cutter.block(&carry[..=last], &mut part);
            out.write_all(&part)?;
            carry.drain(..=last);
        }
    }
    if !carry.is_empty() {
        part.clear();
        cutter.block(&carry, &mut part);
        out.write_all(&part)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cut(mode: Mode, joiner: Option<&str>, text: &[u8]) -> Vec<u8> {
        let c = Cutter { mode, joiner: joiner.map(|j| j.as_bytes().to_vec()), end: b'\n' };
        let mut out = Vec::new();
        c.block(text, &mut out);
        out
    }

    #[test]
    fn fields_and_bytes_as_gnu_cut() {
        let f = |list: &str, s| Mode::Fields { list: List::parse(list, "-f").unwrap(), delimiter: b':', only_delimited: s };
        assert_eq!(cut(f("1", false), None, b"a:b:c\nplain\n"), b"a\nplain\n");
        assert_eq!(cut(f("2-", false), None, b"a:b:c\n"), b"b:c\n");
        assert_eq!(cut(f("3,1", false), Some("|"), b"a:b:c\n"), b"a|c\n");
        assert_eq!(cut(f("2", true), None, b"a:b\nplain\nx:\n"), b"b\n\n");
        assert_eq!(cut(f("5", false), None, b"a:b\n"), b"\n");
        let comp = List::parse("2", "-f").unwrap().complement();
        assert_eq!(cut(Mode::Fields { list: comp, delimiter: b':', only_delimited: false }, None, b"a:b:c:d"), b"a:c:d\n");
        let b = |list: &str| Mode::Bytes(List::parse(list, "-b").unwrap());
        assert_eq!(cut(b("1-3,5"), None, b"abcdefg\nab\n"), b"abce\nab\n");
        assert_eq!(cut(b("1,3"), Some("-"), b"abc\n"), b"a-c\n");
        assert_eq!(cut(b("1-2,2-4"), Some("-"), b"abcdef\n"), b"abcd\n");
        assert!(List::parse("0", "-f").is_err() && List::parse("3-1", "-f").is_err() && List::parse("-", "-f").is_err());
    }
}
