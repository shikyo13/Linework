//! lw split: a file cut into pieces by lines, bytes or count, with the options
//! of GNU split. Pieces of a file on disk are worked out first, from a
//! parallel count of its line ends, then copied on several threads.
use crate::chunks::{self, Output};
use crate::gnu_args::{self, flag, valued, Spec};
use std::{
    fs::File,
    io::{self, Read, Write},
    path::Path,
};

pub const SPECS: &[Spec] = &[
    valued(Some('l'), "lines"),
    valued(Some('b'), "bytes"),
    valued(Some('C'), "line-bytes"),
    valued(Some('n'), "number"),
    valued(Some('a'), "suffix-length"),
    valued(None, "additional-suffix"),
    Spec { short: Some('d'), long: "numeric-suffixes", value: Some(false) },
    Spec { short: Some('x'), long: "hex-suffixes", value: Some(false) },
    flag(Some('e'), "elide-empty-files"),
    valued(Some('t'), "separator"),
    flag(None, "verbose"),
    flag(Some('u'), "unbuffered"),
    valued(Some('j'), "threads"),
];

/// How the input is cut.
enum By {
    Lines(u64),
    Bytes(u64),
    LineBytes(u64),
    /// N pieces of about equal size; `lines` keeps lines whole; `only` writes one of them to the output.
    Number { n: u64, lines: bool, only: Option<u64> },
    RoundRobin { n: u64, only: Option<u64> },
}

#[derive(Clone, Copy)]
enum Suffix {
    Letters,
    Digits(u64),
    Hex(u64),
}

struct Names {
    prefix: String,
    extra: String,
    kind: Suffix,
    length: usize,
    /// GNU split's growing suffixes, used when neither -a nor a first number
    /// is given and the number of pieces is not set by -n: two characters,
    /// and when the first reaches the alphabet's last (z, 9 or f) it stays
    /// and one more is added: yz, zaaa ... zyzz, zzaaaa.
    auto: bool,
}

impl Names {
    /// The length the suffixes need for `count` files, at least `least`.
    fn fit(kind: Suffix, count: u64, least: usize) -> usize {
        let (base, from) = match kind {
            Suffix::Letters => (26u64, 0),
            Suffix::Digits(f) => (10, f),
            Suffix::Hex(f) => (16, f),
        };
        let last = (from + count).saturating_sub(1);
        let mut length = 1;
        let mut room = base;
        while room <= last {
            room = room.saturating_mul(base);
            length += 1;
        }
        length.max(least)
    }

    fn name(&self, index: u64) -> String {
        let (base, from, digits): (u64, u64, &[u8]) = match self.kind {
            Suffix::Letters => (26, 0, b"abcdefghijklmnopqrstuvwxyz"),
            Suffix::Digits(f) => (10, f, b"0123456789"),
            Suffix::Hex(f) => (16, f, b"0123456789abcdef"),
        };
        let mut n = from + index;
        let mut fixed = 0;
        let mut length = self.length;
        if self.auto {
            // Each width holds (base - 1) * base^(width - 1) names before the next.
            loop {
                let room = (base - 1).saturating_mul(base.saturating_pow(length as u32 - 1));
                if n < room {
                    break;
                }
                n -= room;
                fixed += 1;
                length += 1;
            }
        }
        let mut suffix = vec![digits[base as usize - 1]; fixed];
        let mut rest = vec![digits[0]; length];
        for slot in rest.iter_mut().rev() {
            *slot = digits[(n % base) as usize];
            n /= base;
        }
        suffix.extend(rest);
        format!("{}{}{}", self.prefix, String::from_utf8(suffix).expect("ASCII"), self.extra)
    }
}

pub fn run(args: &[String], out: &mut dyn Write) -> Result<i32, String> {
    let io = |e: io::Error| e.to_string();
    let g = gnu_args::parse("split", SPECS, args)?;
    let threads = gnu_args::threads(&g)?;
    let end = match g.last("separator") {
        None => b'\n',
        Some(t) => match linework_search::settings::unescape(t).as_slice() {
            [one] => *one,
            _ => return Err(format!("-t takes one byte, such as : or \\0, not {t:?}")),
        },
    };
    // The last of -l, -b, -C and -n decides, as in GNU split.
    let mut by = By::Lines(1000);
    for (long, value) in &g.options {
        let v = value.as_deref().unwrap_or("");
        by = match *long {
            "lines" => By::Lines(gnu_args::number(v, "-l")?),
            "bytes" => By::Bytes(gnu_args::size(v, "-b")?),
            "line-bytes" => By::LineBytes(gnu_args::size(v, "-C")?),
            "number" => chunk_spec(v)?,
            _ => continue,
        };
    }
    if matches!(by, By::Lines(0) | By::Bytes(0) | By::LineBytes(0) | By::Number { n: 0, .. } | By::RoundRobin { n: 0, .. }) {
        return Err("the size of a piece, or the number of pieces, must be more than 0".into());
    }
    let mut kind = Suffix::Letters;
    for (long, value) in &g.options {
        let from = |v: &Option<String>| -> Result<u64, String> { v.as_deref().map_or(Ok(0), |f| gnu_args::number(f, "the first suffix")) };
        kind = match *long {
            "numeric-suffixes" => Suffix::Digits(from(value)?),
            "hex-suffixes" => Suffix::Hex(from(value)?),
            _ => continue,
        };
    }
    let fixed = g.last("suffix-length").map(|v| gnu_args::number(v, "-a")).transpose()?.map(|n| n as usize);
    if fixed == Some(0) {
        return Err("-a must be at least 1".into());
    }
    let (input, prefix) = match g.words.as_slice() {
        [] => ("-".to_owned(), "x".to_owned()),
        [i] => (i.clone(), "x".to_owned()),
        [i, p] => (i.clone(), p.clone()),
        _ => return Err("lw split takes at most a FILE and a PREFIX".into()),
    };
    let elide = g.has("elide-empty-files");
    let verbose = g.has("verbose");
    let from_given = matches!(kind, Suffix::Digits(f) | Suffix::Hex(f) if f > 0);
    let counted = matches!(by, By::Number { .. } | By::RoundRobin { .. });
    // GNU split's growing suffixes apply unless -a, a first number or -n fixes the length.
    let auto = fixed.is_none() && !from_given && !counted;
    let names = |count: u64| -> Result<Names, String> {
        let extra = g.last("additional-suffix").unwrap_or("").to_owned();
        if extra.contains(['/', '\\']) {
            return Err(format!("--additional-suffix {extra:?} contains a folder separator"));
        }
        if auto {
            return Ok(Names { prefix: prefix.clone(), extra, kind, length: 2, auto });
        }
        let length = Names::fit(kind, count, fixed.unwrap_or(2));
        if let Some(f) = fixed {
            if length > f {
                return Err(format!("{count} pieces need suffixes of {length} characters, but -a is {f}; nothing was written"));
            }
        }
        Ok(Names { prefix: prefix.clone(), extra, kind, length, auto })
    };
    if input == "-" {
        if matches!(by, By::Number { .. } | By::RoundRobin { .. }) {
            return Err("-n needs a file, since the pieces depend on its size; what is piped in can be cut with -l, -b or -C".into());
        }
        return streamed(io::stdin().lock(), &by, end, &names, verbose).map_err(io);
    }
    let (file, len) = chunks::open(&input).map_err(io)?;
    // Where each piece starts and ends, in bytes.
    let cuts: Vec<u64> = match by {
        By::Bytes(size) => (0..len.div_ceil(size)).map(|k| k * size).chain(std::iter::once(len)).collect(),
        By::LineBytes(size) => line_bytes(&file, len, size, end).map_err(io)?,
        By::Lines(n) => by_lines(&file, len, n, end, threads).map_err(io)?,
        By::Number { n, lines, only } => {
            // As GNU split: pieces of len / n bytes, the first len % n of them one byte longer.
            let mut cuts = vec![0];
            for k in 1..n {
                let nominal = k * (len / n) + k.min(len % n);
                let at = if lines && nominal > 0 { chunks::line_end_after(&file, len, nominal - 1, end).map_err(io)? } else { nominal };
                cuts.push(at.max(*cuts.last().unwrap()));
            }
            cuts.push(len);
            if let Some(k) = only {
                let (a, b) = (cuts[k as usize - 1], cuts[k as usize]);
                copy_to(&file, a, b, out).map_err(io)?;
                out.flush().map_err(io)?;
                return Ok(0);
            }
            cuts
        }
        By::RoundRobin { n, only } => return round_robin(&file, len, n, only, end, threads, &names, elide, verbose, &input, out).map_err(io),
    };
    // An empty input makes no pieces, except with -n, which makes N empty ones (as GNU split).
    if len == 0 && !matches!(by, By::Number { .. }) {
        return Ok(0);
    }
    let all = cuts.len() as u64 - 1;
    let pieces: Vec<(u64, u64)> = cuts.windows(2).map(|w| (w[0], w[1])).filter(|(a, b)| !elide || a < b).collect();
    let names = names(all)?;
    for k in 0..pieces.len() {
        if chunks::same_file(Path::new(&names.name(k as u64)), Path::new(&input)) {
            return Err(format!("{} is the input; give another PREFIX", names.name(k as u64)));
        }
    }
    copy_pieces(&file, &pieces, &names, threads, verbose).map_err(io)?;
    Ok(0)
}

/// -n's CHUNKS: N, K/N, l/N, l/K/N, r/N or r/K/N.
fn chunk_spec(text: &str) -> Result<By, String> {
    let parts: Vec<&str> = text.split('/').collect();
    let num = |t: &str| gnu_args::number(t, "-n");
    let (mode, rest): (Option<&str>, &[&str]) = match parts.first() {
        Some(&m @ ("l" | "r")) => (Some(m), &parts[1..]),
        _ => (None, &parts[..]),
    };
    let (n, only) = match rest {
        [n] => (num(n)?, None),
        [k, n] => (num(n)?, Some(num(k)?)),
        _ => return Err(format!("-n is N, K/N, l/N, l/K/N, r/N or r/K/N, not {text:?}")),
    };
    if let Some(k) = only {
        if k == 0 || k > n {
            return Err(format!("in -n {text}, K must be from 1 to N"));
        }
    }
    Ok(match mode {
        Some("r") => By::RoundRobin { n, only },
        Some(_) => By::Number { n, lines: true, only },
        None => By::Number { n, lines: false, only },
    })
}

/// Pieces of at most `size` bytes of whole lines; a line longer than `size` is cut.
fn line_bytes(file: &File, len: u64, size: u64, end: u8) -> io::Result<Vec<u64>> {
    let mut cuts = vec![0];
    let mut at = 0;
    let mut buf = vec![0u8; 64 << 10];
    while len - at > size {
        let limit = at + size;
        // The last line end at or before the limit, looking back from it.
        let mut cut = None;
        let mut hi = limit;
        while hi > at && cut.is_none() {
            let lo = hi.saturating_sub(buf.len() as u64).max(at);
            let n = (hi - lo) as usize;
            chunks::read_at(file, lo, &mut buf[..n])?;
            cut = memchr::memrchr(end, &buf[..n]).map(|i| lo + i as u64 + 1);
            hi = lo;
        }
        at = cut.filter(|&c| c > at).unwrap_or(limit);
        cuts.push(at);
    }
    cuts.push(len);
    Ok(cuts)
}

use chunks::nth_end;

/// Every `n`th line start, from a parallel count of the line ends.
fn by_lines(file: &File, len: u64, n: u64, end: u8, threads: usize) -> io::Result<Vec<u64>> {
    let parts = chunks::boundaries(file, 0, len, 64 << 20, end)?;
    let mut counts = Vec::with_capacity(parts.len());
    chunks::ordered(file, &parts, threads, &|_, bytes| Ok(chunks::count_byte(bytes, end)), &mut |_, c| {
        counts.push(c);
        Ok(())
    })?;
    // The line ends after which a piece ends, numbered over the whole file, from 1.
    let total: u64 = counts.iter().sum();
    let mut wanted: Vec<Vec<u64>> = vec![Vec::new(); counts.len()];
    let mut before = 0u64;
    let mut target = n;
    for (i, &c) in counts.iter().enumerate() {
        while target <= before + c && target < total + 1 {
            wanted[i].push(target - before);
            target += n;
        }
        before += c;
    }
    let mut cuts = vec![0];
    // Only the parts holding a cut are read again; each is a pair in `busy`.
    let pairs: Vec<usize> = (0..counts.len()).filter(|&i| !wanted[i].is_empty()).collect();
    let busy: Vec<u64> = pairs.iter().flat_map(|&i| [parts[i], parts[i + 1]]).collect();
    let found = std::sync::Mutex::new(Vec::new());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failed: std::sync::Mutex<Option<io::Error>> = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..threads.clamp(1, pairs.len().max(1)) {
            scope.spawn(|| {
                let mut buf = Vec::new();
                loop {
                    let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if k >= pairs.len() {
                        break;
                    }
                    let i = pairs[k];
                    let (a, b) = (busy[2 * k], busy[2 * k + 1]);
                    buf.resize((b - a) as usize, 0);
                    if let Err(e) = chunks::read_at(file, a, &mut buf) {
                        *failed.lock().unwrap() = Some(e);
                        break;
                    }
                    let mut offsets = Vec::new();
                    for &w in &wanted[i] {
                        if let Some(o) = nth_end(&buf, end, w) {
                            offsets.push(a + o as u64);
                        }
                    }
                    found.lock().unwrap().extend(offsets);
                }
            });
        }
    });
    if let Some(e) = failed.into_inner().unwrap() {
        return Err(e);
    }
    let mut found = found.into_inner().unwrap();
    found.sort_unstable();
    cuts.extend(found.into_iter().filter(|&o| o < len));
    cuts.push(len);
    cuts.dedup();
    Ok(cuts)
}

fn copy_to(file: &File, a: u64, b: u64, out: &mut dyn Write) -> io::Result<()> {
    let mut buf = vec![0u8; (8u64 << 20).min(b - a) as usize];
    let mut at = a;
    while at < b {
        let n = (b - at).min(buf.len() as u64) as usize;
        let got = chunks::read_at(file, at, &mut buf[..n])?;
        if got != n {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the file became shorter while it was read"));
        }
        out.write_all(&buf[..n])?;
        at += n as u64;
    }
    Ok(())
}

/// Each piece copied to its own file, several at once.
fn copy_pieces(file: &File, pieces: &[(u64, u64)], names: &Names, threads: usize, verbose: bool) -> io::Result<()> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failed: std::sync::Mutex<Option<io::Error>> = std::sync::Mutex::new(None);
    let written = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..threads.clamp(1, pieces.len().max(1)) {
            scope.spawn(|| loop {
                let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if k >= pieces.len() || failed.lock().unwrap().is_some() {
                    break;
                }
                let name = names.name(k as u64);
                let (a, b) = pieces[k];
                let result = Output::create(Path::new(&name)).and_then(|mut o| {
                    copy_to(file, a, b, &mut o)?;
                    o.finish()
                });
                match result {
                    Ok(()) => written.lock().unwrap().push(k),
                    Err(e) => *failed.lock().unwrap() = Some(e),
                }
            });
        }
    });
    if let Some(e) = failed.into_inner().unwrap() {
        // Pieces already written stay; the message says where it stopped.
        let done = written.into_inner().unwrap().len();
        return Err(io::Error::new(e.kind(), format!("{e} ({done} of {} pieces written)", pieces.len())));
    }
    if verbose {
        for k in 0..pieces.len() {
            crate::note(format!("creating file '{}'", names.name(k as u64)));
        }
    }
    Ok(())
}

/// Lines dealt to N files in turn, line 1 to the first: counted first, so each part of the file knows its first line's number.
#[allow(clippy::too_many_arguments)]
fn round_robin(
    file: &File, len: u64, n: u64, only: Option<u64>, end: u8, threads: usize, names: &dyn Fn(u64) -> Result<Names, String>, elide: bool, verbose: bool, input: &str,
    out: &mut dyn Write,
) -> io::Result<i32> {
    let parts = chunks::boundaries(file, 0, len, 32 << 20, end)?;
    let mut firsts = Vec::with_capacity(parts.len());
    let mut lines = 0u64;
    chunks::ordered(file, &parts, threads, &|_, bytes| Ok(chunks::count_byte(bytes, end) + u64::from(bytes.last().is_some_and(|&b| b != end))), &mut |_, c| {
        firsts.push(lines);
        lines += c;
        Ok(())
    })?;
    let firsts = &firsts;
    let deal = |i: usize, bytes: &[u8]| -> io::Result<Vec<Vec<u8>>> {
        let mut piles = vec![Vec::new(); n as usize];
        let mut line = firsts[i];
        let mut at = 0;
        while at < bytes.len() {
            let stop = memchr::memchr(end, &bytes[at..]).map_or(bytes.len(), |e| at + e + 1);
            let pile = (line % n) as usize;
            if only.is_none_or(|k| k as usize == pile + 1) {
                piles[pile].extend_from_slice(&bytes[at..stop]);
            }
            line += 1;
            at = stop;
        }
        Ok(piles)
    };
    if let Some(k) = only {
        chunks::ordered(file, &parts, threads, &deal, &mut |_, piles| out.write_all(&piles[k as usize - 1]))?;
        out.flush()?;
        return Ok(0);
    }
    let count = if elide { n.min(lines) } else { n };
    let names = names(count).map_err(io::Error::other)?;
    let mut outputs = Vec::new();
    for k in 0..count {
        let name = names.name(k);
        if chunks::same_file(Path::new(&name), Path::new(input)) {
            return Err(io::Error::other(format!("{name} is the input; give another PREFIX")));
        }
        outputs.push(Output::create(Path::new(&name))?);
    }
    chunks::ordered(file, &parts, threads, &deal, &mut |_, piles| {
        for (o, pile) in outputs.iter_mut().zip(&piles) {
            o.write_all(pile)?;
        }
        Ok(())
    })?;
    for (k, o) in outputs.into_iter().enumerate() {
        o.finish()?;
        if verbose {
            crate::note(format!("creating file '{}'", names.name(k as u64)));
        }
    }
    Ok(0)
}

/// What is piped in, cut by lines, bytes or line-bytes as it arrives.
fn streamed(mut input: impl Read, by: &By, end: u8, names: &dyn Fn(u64) -> Result<Names, String>, verbose: bool) -> io::Result<i32> {
    // The number of pieces is not known ahead: with -a or a first number, more
    // pieces than the suffixes allow stops with a message, as in GNU split.
    let names = names(1).map_err(io::Error::other)?;
    let room = if names.auto { Some(u64::MAX) } else { match names.kind {
        Suffix::Letters => 26u64.checked_pow(names.length as u32),
        Suffix::Digits(f) => 10u64.checked_pow(names.length as u32).map(|r| r.saturating_sub(f)),
        Suffix::Hex(f) => 16u64.checked_pow(names.length as u32).map(|r| r.saturating_sub(f)),
    } }
    .unwrap_or(u64::MAX);
    let mut buf = vec![0u8; 8 << 20];
    let mut piece: Option<Output> = None;
    let mut index = 0u64;
    let mut used = 0u64; // lines or bytes in the current piece
    let open = |index: &mut u64, piece: &mut Option<Output>| -> io::Result<()> {
        if let Some(p) = piece.take() {
            p.finish()?;
        }
        if *index >= room {
            return Err(io::Error::other(format!("more pieces than suffixes of {} characters allow; give a larger -a", names.length)));
        }
        let name = names.name(*index);
        if verbose {
            crate::note(format!("creating file '{name}'"));
        }
        *piece = Some(Output::create(Path::new(&name))?);
        *index += 1;
        Ok(())
    };
    let mut pending: Vec<u8> = Vec::new(); // for -C: the start of a line not yet placed
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let mut bytes = &buf[..n];
        match *by {
            By::Lines(limit) => {
                while !bytes.is_empty() {
                    if piece.is_none() || used == limit {
                        open(&mut index, &mut piece)?;
                        used = 0;
                    }
                    let want = limit - used;
                    let (take, ends) = match nth_end(bytes, end, want) {
                        Some(at) => (at, want),
                        None => (bytes.len(), chunks::count_byte(bytes, end)),
                    };
                    piece.as_mut().unwrap().write_all(&bytes[..take])?;
                    used += ends;
                    bytes = &bytes[take..];
                }
            }
            By::Bytes(limit) => {
                while !bytes.is_empty() {
                    if piece.is_none() || used == limit {
                        open(&mut index, &mut piece)?;
                        used = 0;
                    }
                    let take = ((limit - used) as usize).min(bytes.len());
                    piece.as_mut().unwrap().write_all(&bytes[..take])?;
                    used += take as u64;
                    bytes = &bytes[take..];
                }
            }
            By::LineBytes(limit) => {
                pending.extend_from_slice(bytes);
                // Place whole lines while a piece's worth is known.
                // Each piece is placed whole, so each placement opens the next file.
                while pending.len() as u64 > limit {
                    open(&mut index, &mut piece)?;
                    let cut = memchr::memrchr(end, &pending[..limit as usize]).map_or(limit as usize, |i| i + 1);
                    piece.as_mut().unwrap().write_all(&pending[..cut])?;
                    pending.drain(..cut);
                }
            }
            _ => unreachable!("-n is refused for piped input"),
        }
    }
    if matches!(by, By::LineBytes(_)) && !pending.is_empty() {
        open(&mut index, &mut piece)?;
        piece.as_mut().unwrap().write_all(&pending)?;
    }
    if let Some(p) = piece {
        p.finish()?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffixes_follow_gnu_split() {
        let n = Names { prefix: "x".into(), extra: ".txt".into(), kind: Suffix::Letters, length: 2, auto: false };
        assert_eq!(n.name(0), "xaa.txt");
        assert_eq!(n.name(27), "xbb.txt");
        let d = Names { prefix: "p_".into(), extra: String::new(), kind: Suffix::Digits(1), length: 4, auto: false };
        assert_eq!(d.name(0), "p_0001");
        // GNU split's own names at each widening (from split -l1 on 20,000 lines).
        let a = Names { prefix: "x".into(), extra: String::new(), kind: Suffix::Letters, length: 2, auto: true };
        for (line, name) in [(650, "xyz"), (651, "xzaaa"), (17550, "xzyzz"), (17551, "xzzaaaa"), (20000, "xzzadqf")] {
            assert_eq!(a.name(line - 1), name);
        }
        let d = Names { prefix: "x".into(), extra: String::new(), kind: Suffix::Digits(0), length: 2, auto: true };
        for (line, name) in [(90, "x89"), (91, "x9000"), (17550, "x99907559"), (20000, "x99910009")] {
            assert_eq!(d.name(line - 1), name);
        }
        let h = Names { prefix: "x".into(), extra: String::new(), kind: Suffix::Hex(0), length: 2, auto: true };
        for (line, name) in [(240, "xef"), (241, "xf000"), (17551, "xff349e"), (20000, "xff3e2f")] {
            assert_eq!(h.name(line - 1), name);
        }
        assert_eq!(Names::fit(Suffix::Letters, 676, 2), 2);
        assert_eq!(Names::fit(Suffix::Letters, 677, 2), 3);
        assert_eq!(Names::fit(Suffix::Digits(1), 2966, 4), 4);
        assert_eq!(Names::fit(Suffix::Digits(0), 100, 2), 2);
        assert_eq!(Names::fit(Suffix::Digits(0), 101, 2), 3);
    }

    #[test]
    fn nth_end_finds_the_right_line() {
        let text: Vec<u8> = (0..10_000).flat_map(|i| format!("{i}\n").into_bytes()).collect();
        let at = nth_end(&text, b'\n', 5000).unwrap();
        assert!(text[..at].ends_with(b"4999\n"));
        assert_eq!(nth_end(&text, b'\n', 10_001), None);
    }
}
