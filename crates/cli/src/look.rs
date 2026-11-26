//! lw look: the lines of a sorted file that begin with a string, found by
//! bisection in a few dozen small reads, with the options of util-linux look;
//! and with --keys, the lines that begin with any of many strings.
use crate::chunks;
use crate::gnu_args::{self, flag, valued, Spec};
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{self, Read, Write},
};

pub const SPECS: &[Spec] = &[
    flag(Some('d'), "alphanum"),
    flag(Some('f'), "ignore-case"),
    valued(Some('t'), "terminate"),
    flag(Some('b'), "binary"),
    flag(Some('a'), "alternative"),
    valued(None, "keys"),
    valued(Some('j'), "threads"),
];

#[derive(Clone, Copy)]
struct Fold {
    alphanum: bool,
    case: bool,
}

impl Fold {
    /// A byte as compared: None when -d leaves it out.
    fn byte(&self, c: u8) -> Option<u8> {
        if self.alphanum && !(c.is_ascii_alphanumeric() || c == b' ' || c == b'\t') {
            return None;
        }
        Some(if self.case { c.to_ascii_lowercase() } else { c })
    }

    fn key(&self, key: &[u8], terminate: Option<u8>) -> Vec<u8> {
        let key = match terminate.and_then(|t| memchr::memchr(t, key)) {
            Some(i) => &key[..=i],
            None => key,
        };
        key.iter().filter_map(|&c| self.byte(c)).collect()
    }
}

/// A sorted file read through a small cache of 64 KiB blocks, since bisection rereads the same places.
struct Text {
    file: File,
    len: u64,
    blocks: std::cell::RefCell<HashMap<u64, Box<[u8]>>>,
}

const BLOCK: u64 = 64 << 10;

impl Text {
    fn byte(&self, at: u64) -> io::Result<u8> {
        let number = at / BLOCK;
        let mut blocks = self.blocks.borrow_mut();
        if !blocks.contains_key(&number) {
            if blocks.len() > 4096 {
                blocks.clear();
            }
            let start = number * BLOCK;
            let mut buf = vec![0u8; (self.len - start).min(BLOCK) as usize];
            chunks::read_at(&self.file, start, &mut buf)?;
            blocks.insert(number, buf.into_boxed_slice());
        }
        Ok(blocks[&number][(at - number * BLOCK) as usize])
    }

    /// util-linux look's SKIP_PAST_NEWLINE.
    fn skip_line(&self, mut p: u64, back: u64) -> io::Result<u64> {
        while p < back {
            let c = self.byte(p)?;
            p += 1;
            if c == b'\n' {
                break;
            }
        }
        Ok(p)
    }

    /// How `key` compares with the start of the line at `p` (read no further than `back`), as util-linux look compares.
    fn compare(&self, key: &[u8], fold: Fold, mut p: u64, back: u64) -> io::Result<std::cmp::Ordering> {
        let mut taken = Vec::with_capacity(key.len());
        while p < back && taken.len() < key.len() {
            let c = self.byte(p)?;
            if c == b'\n' {
                break;
            }
            p += 1;
            if let Some(c) = fold.byte(c) {
                taken.push(c);
            }
        }
        // strcmp stops at a zero byte.
        if let Some(z) = memchr::memchr(0, &taken) {
            taken.truncate(z);
        }
        Ok(key.cmp(&taken[..]))
    }
}

/// util-linux look's bisection, linear search and printing, step for step, so even a file
/// not sorted the way the options compare gives the lines util-linux look gives.
fn look_one(text: &Text, key: &[u8], fold: Fold, out: &mut Vec<(u64, u64)>) -> io::Result<()> {
    use std::cmp::Ordering::*;
    let (mut front, mut back) = (0u64, text.len);
    let mut p = text.skip_line(front + (back - front) / 2, back)?;
    while p < back && back > front {
        if text.compare(key, fold, p, back)? == Greater {
            front = p;
        } else {
            back = p;
        }
        p = text.skip_line(front + (back - front) / 2, back)?;
    }
    let back = text.len;
    loop {
        if front >= back {
            return Ok(());
        }
        match text.compare(key, fold, front, back)? {
            Equal => break,
            Less => return Ok(()),
            Greater => front = text.skip_line(front, back)?,
        }
    }
    while front < back && text.compare(key, fold, front, back)? == Equal {
        let end = text.skip_line(front, back)?;
        out.push((front, end));
        front = end;
    }
    Ok(())
}

pub fn run(args: &[String], out: &mut dyn Write) -> Result<i32, String> {
    let io = |e: io::Error| e.to_string();
    let g = gnu_args::parse("look", SPECS, args)?;
    if g.has("alternative") {
        return Err("-a names a dictionary of words that Windows does not have; give the FILE to search".into());
    }
    let threads = gnu_args::threads(&g)?;
    let fold = Fold { alphanum: g.has("alphanum"), case: g.has("ignore-case") };
    let terminate = match g.last("terminate") {
        None => None,
        Some(t) => match linework_search::settings::unescape(t).as_slice() {
            [one] => Some(*one),
            _ => return Err(format!("-t takes one character, not {t:?}")),
        },
    };
    let (keys, path): (Vec<Vec<u8>>, &str) = match (g.last("keys"), g.words.as_slice()) {
        (Some(list), [file]) => {
            let mut bytes = Vec::new();
            if list == "-" {
                io::stdin().read_to_end(&mut bytes).map_err(io)?;
            } else {
                bytes = std::fs::read(list).map_err(|e| format!("{list}: {e}"))?;
            }
            let keys = bytes.split(|&b| b == b'\n').map(|k| k.strip_suffix(b"\r").unwrap_or(k)).filter(|k| !k.is_empty()).map(|k| fold.key(k, terminate)).collect();
            (keys, file.as_str())
        }
        (None, [key, file]) => (vec![fold.key(key.as_bytes(), terminate)], file.as_str()),
        (None, [_]) => return Err("lw look needs the FILE to search; Windows has no dictionary of words to use instead".into()),
        _ => return Err("usage: lw look [-df] [-t CHAR] STRING FILE, or lw look --keys KEYS FILE".into()),
    };
    let (file, len) = chunks::open(path).map_err(io)?;
    let mut found: Vec<(u64, u64)> = Vec::new();
    if g.has("keys") {
        let mut keys = keys;
        keys.sort_unstable();
        keys.dedup();
        // Many keys: one pass over the whole file costs less than bisecting for each.
        let probes = keys.len() as u64 * 48 * BLOCK;
        if probes > len {
            found = scan(&file, len, &keys, fold, threads).map_err(io)?;
        } else {
            let groups: Vec<&[Vec<u8>]> = keys.chunks(keys.len().div_ceil(threads.max(1)).max(1)).collect();
            let parts = chunks::each(groups.len(), threads, &|i| -> io::Result<Vec<(u64, u64)>> {
                let text = Text { file: file.try_clone()?, len, blocks: Default::default() };
                let mut part = Vec::new();
                for key in groups[i] {
                    look_one(&text, key, fold, &mut part)?;
                }
                Ok(part)
            });
            for p in parts {
                found.extend(p.map_err(io)?);
            }
        }
        found.sort_unstable();
        found.dedup();
    } else {
        let text = Text { file, len, blocks: Default::default() };
        look_one(&text, &keys[0], fold, &mut found).map_err(io)?;
        return write_lines(&text.file, &found, out).map_err(io).map(|()| i32::from(found.is_empty()));
    }
    write_lines(&file, &found, out).map_err(io)?;
    Ok(i32::from(found.is_empty()))
}

/// Every line that begins with one of the keys, by reading the file once on every thread; the file need not be sorted.
fn scan(file: &File, len: u64, keys: &[Vec<u8>], fold: Fold, threads: usize) -> io::Result<Vec<(u64, u64)>> {
    let mut by_length: HashMap<usize, HashSet<&[u8]>> = HashMap::new();
    for k in keys {
        by_length.entry(k.len()).or_default().insert(k);
    }
    let mut lengths: Vec<(usize, HashSet<&[u8]>)> = by_length.into_iter().collect();
    lengths.sort_by_key(|(l, _)| *l);
    let longest = lengths.last().map_or(0, |(l, _)| *l);
    let cuts = chunks::boundaries(file, 0, len, 32 << 20, b'\n')?;
    let mut found = Vec::new();
    chunks::ordered(
        file,
        &cuts,
        threads,
        &|i, bytes| {
            let base = cuts[i];
            let mut hits = Vec::new();
            let mut folded = Vec::with_capacity(longest);
            let mut at = 0;
            while at < bytes.len() {
                let stop = memchr::memchr(b'\n', &bytes[at..]).map_or(bytes.len(), |e| at + e + 1);
                let line = &bytes[at..stop];
                folded.clear();
                for &c in line {
                    if folded.len() == longest || c == b'\n' {
                        break;
                    }
                    if let Some(c) = fold.byte(c) {
                        folded.push(c);
                    }
                }
                if let Some(z) = memchr::memchr(0, &folded) {
                    folded.truncate(z);
                }
                if lengths.iter().any(|(l, set)| folded.len() >= *l && set.contains(&folded[..*l])) {
                    hits.push((base + at as u64, base + stop as u64));
                }
                at = stop;
            }
            Ok(hits)
        },
        &mut |_, hits| {
            found.extend(hits);
            Ok(())
        },
    )?;
    Ok(found)
}

fn write_lines(file: &File, found: &[(u64, u64)], out: &mut dyn Write) -> io::Result<()> {
    let mut buf = Vec::new();
    for &(a, b) in found {
        buf.resize((b - a) as usize, 0);
        chunks::read_at(file, a, &mut buf)?;
        out.write_all(&buf)?;
    }
    out.flush()
}
