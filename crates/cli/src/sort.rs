//! lw sort: lines sorted, merged or checked, with the options of GNU sort in
//! the C locale (byte order). Input is read in parts of up to 4 GiB; each part
//! is sorted in memory on every thread (a sample sort), parts wait in memory
//! up to the buffer size (-S) and are then merged into a run file in the
//! temporary folder; at the end everything is merged to the output in key
//! ranges on every thread, written in order.
use crate::chunks::{self, Output};
use crate::gnu_args::{self, flag, valued, Spec};
use crate::sort_order::{Key, Opts, Order};
use std::{
    cmp::Ordering,
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

pub const SPECS: &[Spec] = &[
    flag(Some('b'), "ignore-leading-blanks"),
    flag(Some('d'), "dictionary-order"),
    flag(Some('f'), "ignore-case"),
    flag(Some('g'), "general-numeric-sort"),
    flag(Some('i'), "ignore-nonprinting"),
    flag(Some('M'), "month-sort"),
    flag(Some('h'), "human-numeric-sort"),
    flag(Some('n'), "numeric-sort"),
    flag(Some('R'), "random-sort"),
    valued(None, "random-source"),
    flag(Some('r'), "reverse"),
    valued(None, "sort"),
    flag(Some('V'), "version-sort"),
    valued(None, "batch-size"),
    Spec { short: Some('c'), long: "check", value: Some(false) },
    flag(Some('C'), "check=quiet"),
    flag(None, "debug"),
    valued(None, "files0-from"),
    valued(Some('k'), "key"),
    flag(Some('m'), "merge"),
    valued(Some('o'), "output"),
    flag(Some('s'), "stable"),
    valued(Some('S'), "buffer-size"),
    valued(Some('t'), "field-separator"),
    valued(Some('T'), "temporary-directory"),
    valued(None, "parallel"),
    valued(None, "compress-program"),
    flag(Some('u'), "unique"),
    flag(Some('z'), "zero-terminated"),
    valued(Some('j'), "threads"),
];

/// One line of a part in memory: the first 8 bytes for quick comparison, then where it is.
#[derive(Clone, Copy, Default)]
struct Entry {
    prefix: u64,
    off: u32,
    len: u32,
}

/// Memory one line costs while its part is sorted: the entry, its copy, and its bucket.
const PER_LINE: u64 = 2 * std::mem::size_of::<Entry>() as u64 + 2;
/// The largest part, so an entry's offset fits in 32 bits.
const PART_MAX: u64 = (4 << 30) - (64 << 20);

fn prefix(line: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    let n = line.len().min(8);
    b[..n].copy_from_slice(&line[..n]);
    u64::from_be_bytes(b)
}

struct Part {
    data: Vec<u8>,
    entries: Vec<Entry>,
}

impl Part {
    fn line(&self, e: &Entry) -> &[u8] {
        &self.data[e.off as usize..e.off as usize + e.len as usize]
    }
    fn memory(&self) -> u64 {
        self.data.capacity() as u64 + (self.entries.capacity() * std::mem::size_of::<Entry>()) as u64
    }
}

/// A sorted file: a run written by lw, or an input given to -m. `index` holds
/// a line start every 64 KiB or so, with its line, for finding key ranges.
struct Sorted {
    file: File,
    len: u64,
    index: Vec<(u64, Vec<u8>)>,
}

#[derive(Clone)]
struct Settings {
    order: Order,
    end: u8,
    threads: usize,
    budget: u64,
    temp: Vec<PathBuf>,
}

pub fn run(args: &[String], stdout: &mut dyn Write) -> Result<i32, String> {
    let io = |e: io::Error| e.to_string();
    let g = gnu_args::parse("sort", SPECS, args)?;
    for refused in ["debug", "compress-program"] {
        if g.has(refused) {
            return Err(format!("lw sort does not have --{refused}"));
        }
    }
    let mut global = Opts::default();
    let mut keys = Vec::new();
    for (long, value) in &g.options {
        let letter = match *long {
            "ignore-leading-blanks" => 'b',
            "dictionary-order" => 'd',
            "ignore-case" => 'f',
            "general-numeric-sort" => 'g',
            "ignore-nonprinting" => 'i',
            "month-sort" => 'M',
            "human-numeric-sort" => 'h',
            "numeric-sort" => 'n',
            "random-sort" => 'R',
            "reverse" => 'r',
            "version-sort" => 'V',
            "sort" => match value.as_deref().unwrap_or("") {
                "general-numeric" => 'g',
                "human-numeric" => 'h',
                "month" => 'M',
                "numeric" => 'n',
                "random" => 'R',
                "version" => 'V',
                other => return Err(format!("--sort is general-numeric, human-numeric, month, numeric, random or version, not {other:?}")),
            },
            "key" => {
                keys.push(Key::parse(value.as_deref().unwrap_or(""))?);
                continue;
            }
            _ => continue,
        };
        global.set(letter, false);
        if letter == 'b' {
            global.set('b', true);
        }
    }
    let kinds = [global.general, global.human, global.month, global.numeric, global.random, global.version].iter().filter(|&&k| k).count();
    if kinds > 1 {
        return Err("only one of -g, -h, -M, -n, -R and -V may be given".into());
    }
    let tab = match g.last("field-separator") {
        None => None,
        Some(t) => match linework_search::settings::unescape(t).as_slice() {
            [one] => Some(*one),
            _ => return Err(format!("the field separator must be a single byte, not {t:?}")),
        },
    };
    let salt = match g.last("random-source") {
        Some(path) => {
            let mut bytes = [0u8; 16];
            File::open(path).and_then(|mut f| f.read_exact(&mut bytes)).map_err(|e| format!("{path}: {e}"))?;
            (u64::from_le_bytes(bytes[..8].try_into().unwrap()), u64::from_le_bytes(bytes[8..].try_into().unwrap()))
        }
        None => {
            let s = std::collections::hash_map::RandomState::new();
            use std::hash::BuildHasher;
            (s.hash_one(1u8), s.hash_one(2u8))
        }
    };
    let unique = g.has("unique");
    let order = Order::new(keys, global, tab, g.has("stable"), unique, salt);
    let end = if g.has("zero-terminated") { 0 } else { b'\n' };
    let threads = gnu_args::threads(&g)?;
    let budget = match g.last("buffer-size") {
        Some(s) => buffer_size(s)?,
        None => default_budget(),
    };
    let mut temp: Vec<PathBuf> = g.options.iter().filter(|(l, _)| *l == "temporary-directory").filter_map(|(_, v)| v.clone()).map(PathBuf::from).collect();
    if temp.is_empty() {
        temp.push(env_temp());
    }
    let mut inputs: Vec<String> = g.words.clone();
    if let Some(list) = g.last("files0-from") {
        if !inputs.is_empty() {
            return Err("file names may not be given with --files0-from".into());
        }
        let bytes = if list == "-" {
            let mut b = Vec::new();
            io::stdin().read_to_end(&mut b).map_err(io)?;
            b
        } else {
            std::fs::read(list).map_err(|e| format!("{list}: {e}"))?
        };
        inputs = bytes.split(|&b| b == 0).filter(|n| !n.is_empty()).map(|n| String::from_utf8_lossy(n).into_owned()).collect();
    }
    if inputs.is_empty() {
        inputs.push("-".into());
    }
    let settings = Settings { order, end, threads, budget, temp };
    let check = if g.has("check=quiet") {
        Some(true)
    } else if g.has("check") {
        match g.last("check") {
            None | Some("diagnose-first") => Some(false),
            Some("quiet" | "silent") => Some(true),
            Some(other) => return Err(format!("--check is diagnose-first, quiet or silent, not {other:?}")),
        }
    } else {
        None
    };
    if let Some(quiet) = check {
        if inputs.len() > 1 {
            return Err(format!("-c checks one file, not {}", inputs.len()));
        }
        return check_sorted(&inputs[0], &settings, quiet).map_err(io);
    }
    // The output is written beside its final name and moved there at the end,
    // so an input given as the output is read whole first, and a failed sort
    // leaves the old file as it was.
    let target = g.last("output").map(PathBuf::from);
    let mut staged = None;
    if let Some(path) = &target {
        let folder = path.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        let temp_name = folder.join(format!(".lw-sort-{}-{}.tmp", std::process::id(), path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
        staged = Some(Output::create(&temp_name).map_err(io)?);
    }
    let result = {
        let out: &mut dyn Write = match staged.as_mut() {
            Some(o) => o,
            None => stdout,
        };
        if g.has("merge") {
            merge_inputs(&inputs, &settings, out)
        } else {
            sort_inputs(&inputs, &settings, out)
        }
    };
    match result {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(0),
        Err(e) => return Err(e.to_string()),
    }
    if let (Some(o), Some(path)) = (staged, &target) {
        let temp_name = o.path().to_path_buf();
        o.finish().map_err(io)?;
        if let Err(e) = std::fs::rename(&temp_name, path) {
            let _ = std::fs::remove_file(&temp_name);
            return Err(format!("{}: {e}", path.display()));
        }
    } else {
        stdout.flush().map_err(io)?;
    }
    Ok(0)
}

/// -S: a number of KiB, or with a unit (b, K, M, G, T, P, E) or % of the computer's memory, as GNU sort reads it.
fn buffer_size(text: &str) -> Result<u64, String> {
    if let Some(p) = text.strip_suffix('%') {
        let n: f64 = p.parse().map_err(|_| format!("-S {text:?} is not a size"))?;
        if !(0.0..=100.0).contains(&n) {
            return Err(format!("-S {text}: a share of memory is from 0% to 100%"));
        }
        return Ok((memory().0 as f64 * n / 100.0) as u64);
    }
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let (number, unit) = text.split_at(digits);
    let n: u64 = number.parse().map_err(|_| format!("-S {text:?} is not a size"))?;
    let factor: u64 = match unit {
        "" | "K" | "k" => 1 << 10,
        "b" => 1,
        "M" | "m" => 1 << 20,
        "G" | "g" => 1 << 30,
        "T" | "t" => 1 << 40,
        "P" | "p" => 1 << 50,
        "E" | "e" => 1 << 60,
        _ => return Err(format!("-S {text:?}: the unit is b, K, M, G, T, P, E or %")),
    };
    n.checked_mul(factor).ok_or(format!("-S {text} is too large"))
}

/// The computer's memory and what is available now, in bytes.
fn memory() -> (u64, u64) {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut m: MEMORYSTATUSEX = std::mem::zeroed();
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut m) != 0 {
            return (m.ullTotalPhys, m.ullAvailPhys);
        }
    }
    (8 << 30, 2 << 30)
}

/// Without -S: half the memory available when the sort starts, at least 256 MiB, at most three quarters of all.
fn default_budget() -> u64 {
    let (total, available) = memory();
    (available / 2).max(256 << 20).min(total / 4 * 3)
}

fn env_temp() -> PathBuf {
    std::env::var_os("TMPDIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

/// The inputs read in turn, as parts that end at a line end.
struct Reader<'a> {
    inputs: &'a [String],
    next: usize,
    current: Option<Box<dyn Read + 'a>>,
    file: Option<(File, u64, u64)>,
    carry: Vec<u8>,
    end: u8,
    threads: usize,
}

impl<'a> Reader<'a> {
    fn new(inputs: &'a [String], end: u8, threads: usize) -> Self {
        Self { inputs, next: 0, current: None, file: None, carry: Vec::new(), end, threads }
    }

    /// Opens the next input; false when there is none.
    fn open_next(&mut self) -> io::Result<bool> {
        let Some(name) = self.inputs.get(self.next) else { return Ok(false) };
        self.next += 1;
        if name == "-" {
            self.current = Some(Box::new(io::stdin()));
        } else {
            let (file, len) = chunks::open(name)?;
            self.file = Some((file, len, 0));
        }
        Ok(true)
    }

    /// Up to about `cap` bytes of whole lines, every line with its end byte; None when all is read.
    fn part(&mut self, cap: u64) -> io::Result<Option<Vec<u8>>> {
        let mut data = std::mem::take(&mut self.carry);
        let cap = cap.max(1 << 16) as usize;
        data.reserve(cap.saturating_sub(data.len()));
        loop {
            if data.len() >= cap {
                break;
            }
            if self.current.is_none() && self.file.is_none() && !self.open_next()? {
                break;
            }
            let want = cap - data.len();
            let got = if let Some((file, len, pos)) = &mut self.file {
                let n = want.min((*len - *pos) as usize);
                let start = data.len();
                data.resize(start + n, 0);
                read_parallel(file, *pos, &mut data[start..], self.threads)?;
                *pos += n as u64;
                if *pos == *len {
                    self.file = None;
                }
                n
            } else {
                let reader = self.current.as_mut().expect("an open input");
                let start = data.len();
                data.resize(start + want.min(8 << 20), 0);
                let n = reader.read(&mut data[start..])?;
                data.truncate(start + n);
                if n == 0 {
                    self.current = None;
                }
                n
            };
            // An input that ends without a line end gets one, so its last line stays a line.
            let finished = self.file.is_none() && self.current.is_none();
            if finished && !data.is_empty() && *data.last().unwrap() != self.end {
                data.push(self.end);
            }
            if got == 0 && !finished {
                continue;
            }
        }
        if data.is_empty() {
            return Ok(None);
        }
        // Cut after the last whole line; the rest starts the next part.
        if *data.last().unwrap() != self.end {
            match memchr::memrchr(self.end, &data) {
                Some(i) => self.carry = data.split_off(i + 1),
                None => {
                    // One line longer than the part: read on to its end.
                    let more = self.part(cap as u64 * 2)?;
                    if let Some(m) = more {
                        data.extend_from_slice(&m);
                    }
                }
            }
        }
        Ok(Some(data))
    }

    /// Puts bytes back in front of what is read next.
    fn unread(&mut self, mut bytes: Vec<u8>) {
        bytes.extend_from_slice(&self.carry);
        self.carry = bytes;
    }
}

/// Reads `buf.len()` bytes at `at` on several threads when there are many.
fn read_parallel(file: &File, at: u64, buf: &mut [u8], threads: usize) -> io::Result<()> {
    const SLICE: usize = 64 << 20;
    if buf.len() <= SLICE || threads <= 1 {
        let n = chunks::read_at(file, at, buf)?;
        return if n == buf.len() { Ok(()) } else { Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the file became shorter while it was read")) };
    }
    let slices: Vec<(u64, &mut [u8])> = buf.chunks_mut(SLICE).enumerate().map(|(i, s)| (at + (i * SLICE) as u64, s)).collect();
    let slices = std::sync::Mutex::new(slices);
    let failed: std::sync::Mutex<Option<io::Error>> = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..threads.min(8) {
            scope.spawn(|| loop {
                let Some((pos, slice)) = slices.lock().unwrap().pop() else { break };
                match chunks::read_at(file, pos, slice) {
                    Ok(n) if n == slice.len() => {}
                    Ok(_) => *failed.lock().unwrap() = Some(io::Error::new(io::ErrorKind::UnexpectedEof, "the file became shorter while it was read")),
                    Err(e) => *failed.lock().unwrap() = Some(e),
                }
            });
        }
    });
    failed.into_inner().unwrap().map_or(Ok(()), Err)
}

/// Every line of `data` (each ends with `end`) as an entry, found on several threads.
fn entries(data: &[u8], end: u8, threads: usize) -> Vec<Entry> {
    let pieces = threads.max(1) * 4;
    let mut cuts = vec![0usize];
    for k in 1..pieces {
        let aim = data.len() * k / pieces;
        let at = memchr::memchr(end, &data[aim.max(*cuts.last().unwrap())..]).map_or(data.len(), |i| aim.max(*cuts.last().unwrap()) + i + 1);
        cuts.push(at.min(data.len()));
    }
    cuts.push(data.len());
    cuts.dedup();
    let counts = chunks::each(cuts.len() - 1, threads, &|i| chunks::count_byte(&data[cuts[i]..cuts[i + 1]], end) as usize);
    let total: usize = counts.iter().sum();
    let mut all = vec![Entry::default(); total];
    let mut slices = Vec::new();
    let mut rest: &mut [Entry] = &mut all;
    for &c in &counts {
        let (a, b) = rest.split_at_mut(c);
        slices.push(a);
        rest = b;
    }
    let jobs = std::sync::Mutex::new(slices.into_iter().enumerate().collect::<Vec<_>>());
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| loop {
                let Some((i, slice)) = jobs.lock().unwrap().pop() else { break };
                let mut start = cuts[i];
                for (slot, stop) in slice.iter_mut().zip(memchr::memchr_iter(end, &data[cuts[i]..cuts[i + 1]])) {
                    let stop = cuts[i] + stop;
                    let line = &data[start..stop];
                    *slot = Entry { prefix: prefix(line), off: start as u32, len: line.len() as u32 };
                    start = stop + 1;
                }
            });
        }
    });
    all
}

fn entry_cmp(order: &Order, part: &Part, a: &Entry, b: &Entry) -> Ordering {
    if order.plain {
        let o = a.prefix.cmp(&b.prefix).then_with(|| part.line(a).cmp(part.line(b)));
        if order.reversed() {
            o.reverse()
        } else {
            o
        }
    } else {
        order.cmp(part.line(a), part.line(b))
    }
}

struct SendPtr(*mut Entry);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

/// Sorts a part's entries on several threads: samples choose bucket bounds,
/// every thread places its share of entries in buckets (keeping their order),
/// then the buckets are sorted at once. Equal lines share a bucket.
fn sort_part(part: &mut Part, order: &Order, threads: usize) {
    let n = part.entries.len();
    let stable = !order.plain && (order.stable || order.unique);
    let sort = |slice: &mut [Entry], part: &Part| {
        if stable {
            slice.sort_by(|a, b| entry_cmp(order, part, a, b));
        } else {
            slice.sort_unstable_by(|a, b| entry_cmp(order, part, a, b));
        }
    };
    if n < (1 << 16) || threads <= 1 {
        let mut entries = std::mem::take(&mut part.entries);
        sort(&mut entries, part);
        part.entries = entries;
        return;
    }
    let buckets = (threads * 8).min(u16::MAX as usize);
    let samples_wanted = buckets * 32;
    let mut samples: Vec<Entry> = (0..samples_wanted).map(|i| part.entries[i * n / samples_wanted]).collect();
    samples.sort_unstable_by(|a, b| entry_cmp(order, part, a, b));
    let mut bounds: Vec<Entry> = (1..buckets).map(|j| samples[j * samples_wanted / buckets]).collect();
    bounds.dedup_by(|a, b| entry_cmp(order, part, a, b) == Ordering::Equal);
    let buckets = bounds.len() + 1;
    // Each entry's bucket: how many bounds are at or before it.
    let bucket_of = |e: &Entry| -> u16 { bounds.partition_point(|b| entry_cmp(order, part, b, e) != Ordering::Greater) as u16 };
    let shares = threads.max(1);
    let share = n.div_ceil(shares);
    let entries = &part.entries;
    let tagged: Vec<(Vec<u16>, Vec<usize>)> = chunks::each(shares, threads, &|t| {
        let lo = (t * share).min(n);
        let hi = ((t + 1) * share).min(n);
        let mut tags = Vec::with_capacity(hi - lo);
        let mut counts = vec![0usize; buckets];
        for e in &entries[lo..hi] {
            let b = bucket_of(e);
            counts[b as usize] += 1;
            tags.push(b);
        }
        (tags, counts)
    });
    // Where each share's entries of each bucket go.
    let mut starts = vec![vec![0usize; buckets]; shares];
    let mut bucket_start = vec![0usize; buckets + 1];
    let mut at = 0;
    for b in 0..buckets {
        bucket_start[b] = at;
        for t in 0..shares {
            starts[t][b] = at;
            at += tagged[t].1[b];
        }
    }
    bucket_start[buckets] = at;
    let mut placed = vec![Entry::default(); n];
    let target = SendPtr(placed.as_mut_ptr());
    let target = &target;
    let starts = &starts;
    let tagged_ref = &tagged;
    chunks::each(shares, threads, &|t| {
        let lo = (t * share).min(n);
        let hi = ((t + 1) * share).min(n);
        let mut next = starts[t].clone();
        for (e, &b) in entries[lo..hi].iter().zip(&tagged_ref[t].0) {
            // Safety: each share writes only to its own reserved slots of each bucket.
            unsafe { *target.0.add(next[b as usize]) = *e };
            next[b as usize] += 1;
        }
    });
    drop(tagged);
    part.entries = Vec::new();
    // Sort the buckets, largest first, each by one thread.
    let mut regions: Vec<&mut [Entry]> = Vec::with_capacity(buckets);
    let mut rest: &mut [Entry] = &mut placed;
    for b in 0..buckets {
        let (a, r) = rest.split_at_mut(bucket_start[b + 1] - bucket_start[b]);
        regions.push(a);
        rest = r;
    }
    regions.sort_by_key(|r| std::cmp::Reverse(r.len()));
    let jobs = std::sync::Mutex::new(regions);
    let part_ref: &Part = part;
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                let job = {
                    let mut j = jobs.lock().unwrap();
                    if j.is_empty() {
                        None
                    } else {
                        Some(j.remove(0))
                    }
                };
                let Some(region) = job else { break };
                sort(region, part_ref);
            });
        }
    });
    part.entries = placed;
}

/// A part sorted: its entries found and put in order.
fn sorted_part(data: Vec<u8>, s: &Settings) -> io::Result<Part> {
    if data.len() as u64 > u32::MAX as u64 {
        return Err(io::Error::other("a line is longer than 4 GiB"));
    }
    let entries = entries(&data, s.end, s.threads);
    let mut part = Part { data, entries };
    sort_part(&mut part, &s.order, s.threads);
    Ok(part)
}

/// One source of a merge: a part in memory or a sorted file.
enum Source<'a> {
    Memory(&'a Part),
    File(&'a Sorted),
}

impl Source<'_> {
    fn bytes(&self) -> u64 {
        match self {
            Source::Memory(p) => p.data.len() as u64,
            Source::File(f) => f.len,
        }
    }
}

/// Where the lines at or after `key` begin in a source: an entry index, or a byte offset.
fn lower_bound(source: &Source, key: &[u8], order: &Order, end: u8) -> io::Result<u64> {
    match source {
        Source::Memory(part) => Ok(part.entries.partition_point(|e| order.cmp(part.line(e), key) == Ordering::Less) as u64),
        Source::File(sorted) => file_lower_bound(sorted, key, order, end),
    }
}

/// The offset of the first line at or after `key` in a sorted file, found from its index and one short read.
fn file_lower_bound(sorted: &Sorted, key: &[u8], order: &Order, end: u8) -> io::Result<u64> {
    let i = sorted.index.partition_point(|(_, line)| order.cmp(line, key) == Ordering::Less);
    if i == 0 {
        return Ok(0);
    }
    let from = sorted.index[i - 1].0;
    let to = sorted.index.get(i).map_or(sorted.len, |s| s.0);
    let mut buf = vec![0u8; (to - from) as usize];
    chunks::read_at(&sorted.file, from, &mut buf)?;
    let mut at = 0;
    while at < buf.len() {
        let stop = memchr::memchr(end, &buf[at..]).map_or(buf.len(), |e| at + e);
        if order.cmp(&buf[at..stop], key) != Ordering::Less {
            return Ok(from + at as u64);
        }
        at = stop + 1;
    }
    Ok(to)
}

/// Lines of one source within a key range, one after another.
enum Cursor<'a> {
    Memory { part: &'a Part, entries: &'a [Entry] },
    Bytes { bytes: &'a [u8], at: usize, stop: usize },
}

impl<'a> Cursor<'a> {
    fn bytes(bytes: &'a [u8], end: u8) -> Self {
        let stop = memchr::memchr(end, bytes).unwrap_or(bytes.len());
        Cursor::Bytes { bytes, at: 0, stop }
    }

    fn head(&self) -> Option<(u64, &'a [u8])> {
        match self {
            Cursor::Memory { part, entries } => entries.first().map(|e| (e.prefix, part.line(e))),
            Cursor::Bytes { bytes, at, stop } => (*at < bytes.len()).then(|| {
                let line = &bytes[*at..*stop];
                (prefix(line), line)
            }),
        }
    }

    fn advance(&mut self, end: u8) {
        match self {
            Cursor::Memory { entries, .. } => *entries = &entries[1..],
            Cursor::Bytes { bytes, at, stop } => {
                *at = *stop + 1;
                if *at < bytes.len() {
                    *stop = memchr::memchr(end, &bytes[*at..]).map_or(bytes.len(), |e| *at + e);
                }
            }
        }
    }
}

fn head_cmp(order: &Order, a: (u64, &[u8]), b: (u64, &[u8])) -> Ordering {
    if order.plain {
        let o = a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1));
        if order.reversed() {
            o.reverse()
        } else {
            o
        }
    } else {
        order.cmp(a.1, b.1)
    }
}

/// What one key range of a merge became: its bytes, and line starts about every 64 KiB within them.
struct Merged {
    bytes: Vec<u8>,
    marks: Vec<usize>,
}

/// Merges the cursors into lines with end bytes; for -u, only the first of equal lines. Ties go to the earlier source, so the merge is stable.
fn merge_cursors<'a>(mut cursors: Vec<Cursor<'a>>, order: &Order, end: u8, size_hint: usize) -> Merged {
    let mut out = Vec::with_capacity(size_hint + size_hint / 8);
    let mut marks = vec![0usize];
    let mut next_mark = 64 << 10;
    let mut last: Option<&'a [u8]> = None;
    macro_rules! emit {
        ($line:expr) => {{
            let line: &'a [u8] = $line;
            let repeated = order.unique && last.is_some_and(|prev| order.same(prev, line));
            if !repeated {
                if out.len() >= next_mark {
                    marks.push(out.len());
                    next_mark = out.len() + (64 << 10);
                }
                out.extend_from_slice(line);
                out.push(end);
                last = Some(line);
            }
        }};
    }
    cursors.retain(|c| c.head().is_some());
    if cursors.len() == 1 {
        let c = &mut cursors[0];
        while let Some((_, line)) = c.head() {
            emit!(line);
            c.advance(end);
        }
        return Merged { bytes: out, marks };
    }
    // A binary heap of cursor numbers, the smallest head on top.
    let less = |cursors: &[Cursor], a: usize, b: usize| -> bool {
        let (ha, hb) = (cursors[a].head().unwrap(), cursors[b].head().unwrap());
        match head_cmp(order, ha, hb) {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => a < b,
        }
    };
    let mut heap: Vec<usize> = (0..cursors.len()).collect();
    let sift = |heap: &mut Vec<usize>, cursors: &[Cursor], mut i: usize| {
        loop {
            let l = 2 * i + 1;
            if l >= heap.len() {
                break;
            }
            let r = l + 1;
            let m = if r < heap.len() && less(cursors, heap[r], heap[l]) { r } else { l };
            if less(cursors, heap[m], heap[i]) {
                heap.swap(m, i);
                i = m;
            } else {
                break;
            }
        }
    };
    for i in (0..heap.len() / 2).rev() {
        sift(&mut heap, &cursors, i);
    }
    while !heap.is_empty() {
        let top = heap[0];
        let (_, line) = cursors[top].head().unwrap();
        emit!(line);
        cursors[top].advance(end);
        if cursors[top].head().is_none() {
            let last_slot = heap.pop().unwrap();
            if heap.is_empty() {
                break;
            }
            heap[0] = last_slot;
        }
        sift(&mut heap, &cursors, 0);
    }
    Merged { bytes: out, marks }
}

/// Merges the sources in key ranges on every thread; `sink` gets each range's result in order.
fn merge(sources: &[Source], s: &Settings, sink: &mut dyn FnMut(Merged) -> io::Result<()>) -> io::Result<()> {
    let total: u64 = sources.iter().map(Source::bytes).sum();
    if total == 0 {
        return Ok(());
    }
    let order = &s.order;
    // Range bounds: lines sampled about every MiB of every source, at even steps of the whole.
    let ranges = ((total / (32 << 20)) as usize).max(s.threads * 4).min(1 << 16);
    let mut samples: Vec<Vec<u8>> = Vec::new();
    for source in sources {
        match source {
            Source::Memory(part) => {
                let n = part.entries.len();
                let m = ((part.data.len() >> 20) + 1).min(n);
                for k in 0..m {
                    samples.push(part.line(&part.entries[k * n / m]).to_vec());
                }
            }
            Source::File(sorted) => {
                for (_, line) in sorted.index.iter().step_by(16) {
                    samples.push(line.clone());
                }
            }
        }
    }
    samples.sort_unstable_by(|a, b| order.cmp(a, b));
    let mut bounds: Vec<Vec<u8>> = if samples.len() < 2 || total < (8 << 20) {
        Vec::new()
    } else {
        (1..ranges).map(|j| samples[j * samples.len() / ranges].clone()).collect()
    };
    bounds.dedup_by(|a, b| order.cmp(a, b) == Ordering::Equal);
    drop(samples);
    // For each source, where each range starts, and its end.
    let mut starts: Vec<Vec<u64>> = Vec::with_capacity(sources.len());
    for source in sources {
        let mut at = vec![0u64];
        let found = chunks::each(bounds.len(), s.threads, &|i| lower_bound(source, &bounds[i], order, s.end));
        for f in found {
            at.push(f?.max(*at.last().unwrap()));
        }
        at.push(match source {
            Source::Memory(p) => p.entries.len() as u64,
            Source::File(f) => f.len,
        });
        starts.push(at);
    }
    let count = bounds.len() + 1;
    let work = |r: usize| -> io::Result<Merged> {
        let mut loaded: Vec<Vec<u8>> = Vec::new();
        for (k, source) in sources.iter().enumerate() {
            if let Source::File(sorted) = source {
                let (a, b) = (starts[k][r], starts[k][r + 1]);
                let mut buf = vec![0u8; (b - a) as usize];
                let n = chunks::read_at(&sorted.file, a, &mut buf)?;
                if n != buf.len() {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "a sorted file became shorter while it was read"));
                }
                loaded.push(buf);
            }
        }
        let mut cursors = Vec::with_capacity(sources.len());
        let mut files = loaded.iter();
        let mut hint = 0usize;
        for (k, source) in sources.iter().enumerate() {
            let (a, b) = (starts[k][r] as usize, starts[k][r + 1] as usize);
            match source {
                Source::Memory(part) => {
                    let entries = &part.entries[a..b];
                    hint += entries.iter().map(|e| e.len as usize + 1).sum::<usize>();
                    cursors.push(Cursor::Memory { part, entries });
                }
                Source::File(_) => {
                    let bytes = files.next().expect("loaded");
                    hint += bytes.len();
                    cursors.push(Cursor::bytes(bytes, s.end));
                }
            }
        }
        Ok(merge_cursors(cursors, order, s.end, hint))
    };
    chunks::tasks(count, s.threads, &work, &mut |_, merged| sink(merged))
}

/// A temporary file in one of the temporary folders, deleted by Windows when it is closed, even if lw is stopped.
fn temp_file(s: &Settings, serial: usize) -> io::Result<(File, PathBuf)> {
    let folder = &s.temp[serial % s.temp.len()];
    let path = folder.join(format!("lw-sort-{}-{serial}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0400_0000); // FILE_FLAG_DELETE_ON_CLOSE
        options.share_mode(0x1 | 0x2 | 0x4);
    }
    let file = options.open(&path).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    Ok((file, path))
}

/// Free space in a folder's volume, if Windows says.
fn free_space(folder: &Path) -> Option<u64> {
    #[cfg(windows)]
    unsafe {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = folder.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let mut free = 0u64;
        if windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) != 0 {
            return Some(free);
        }
    }
    let _ = folder;
    None
}

/// The parts in memory merged into one sorted run file.
fn spill(parts: &[Part], s: &Settings, serial: usize) -> io::Result<Sorted> {
    let bytes: u64 = parts.iter().map(|p| p.data.len() as u64).sum();
    let folder = &s.temp[serial % s.temp.len()];
    if let Some(free) = free_space(folder) {
        if free < bytes + (256 << 20) {
            return Err(io::Error::other(format!(
                "{} has {:.1} GB free, and the next temporary file needs up to {:.1} GB; give another folder with -T, or more memory with -S",
                folder.display(),
                free as f64 / 1e9,
                bytes as f64 / 1e9
            )));
        }
    }
    let (file, _path) = temp_file(s, serial)?;
    let sources: Vec<Source> = parts.iter().map(Source::Memory).collect();
    let mut index = Vec::new();
    let mut written = 0u64;
    {
        let mut w = io::BufWriter::with_capacity(8 << 20, &file);
        merge(&sources, s, &mut |m: Merged| {
            for &mark in &m.marks {
                if mark < m.bytes.len() {
                    let stop = memchr::memchr(s.end, &m.bytes[mark..]).map_or(m.bytes.len(), |e| mark + e);
                    index.push((written + mark as u64, m.bytes[mark..stop].to_vec()));
                }
            }
            w.write_all(&m.bytes)?;
            written += m.bytes.len() as u64;
            Ok(())
        })?;
        w.flush()?;
    }
    Ok(Sorted { file, len: written, index })
}

fn sort_inputs(inputs: &[String], s: &Settings, out: &mut dyn Write) -> io::Result<()> {
    let mut reader = Reader::new(inputs, s.end, s.threads);
    let mut parts: Vec<Part> = Vec::new();
    let mut runs: Vec<Sorted> = Vec::new();
    // The memory one part may use while it is sorted (its text, entries and their copy).
    let part_budget = (s.budget / 2).max(32 << 20);
    // The bytes of text per line, from the last part; a guess for the first.
    let mut average_line = 32f64;
    loop {
        let in_use: u64 = parts.iter().map(Part::memory).sum();
        if !parts.is_empty() && in_use + part_budget > s.budget.max(part_budget * 2) {
            // Memory is full: the parts so far become one run file.
            runs.push(spill(&parts, s, runs.len())?);
            parts.clear();
            continue;
        }
        let text_share = |budget: u64, line: f64| ((budget as f64 / (1.0 + PER_LINE as f64 / line)) as u64).min(PART_MAX);
        let cap = text_share(part_budget, average_line).max(1 << 20);
        let Some(mut data) = reader.part(cap)? else { break };
        let lines = chunks::count_byte(&data, s.end).max(1);
        average_line = data.len() as f64 / lines as f64;
        // Shorter lines than guessed: keep what fits, and read the rest again next.
        if data.len() as u64 + lines * PER_LINE > part_budget && lines > 1 {
            let fits = text_share(part_budget, average_line).max(1) as usize;
            if let Some(i) = memchr::memrchr(s.end, &data[..fits.min(data.len())]) {
                if i + 1 < data.len() {
                    reader.unread(data.split_off(i + 1));
                }
            }
        }
        parts.push(sorted_part(data, s)?);
    }
    drop(reader);
    let mut sources: Vec<Source> = runs.iter().map(Source::File).collect();
    sources.extend(parts.iter().map(Source::Memory));
    merge(&sources, s, &mut |m: Merged| out.write_all(&m.bytes))?;
    out.flush()
}

/// An index of a sorted input for -m: a line start about every MiB, with its line.
fn index_file(file: File, len: u64, end: u8) -> io::Result<Sorted> {
    let mut index = Vec::new();
    let mut buf = vec![0u8; 64 << 10];
    let step = 1u64 << 20;
    let mut aim = 0u64;
    while aim < len {
        let at = if aim == 0 { 0 } else { chunks::line_end_after(&file, len, aim - 1, end)? };
        if at >= len {
            break;
        }
        if index.last().is_some_and(|(o, _): &(u64, Vec<u8>)| *o >= at) {
            aim += step;
            continue;
        }
        // The line at `at`, however long.
        let mut line = Vec::new();
        let mut pos = at;
        loop {
            let n = chunks::read_at(&file, pos, &mut buf)?;
            if n == 0 {
                break;
            }
            match memchr::memchr(end, &buf[..n]) {
                Some(i) => {
                    line.extend_from_slice(&buf[..i]);
                    break;
                }
                None => line.extend_from_slice(&buf[..n]),
            }
            pos += n as u64;
        }
        index.push((at, line));
        aim = at.max(aim) + step;
    }
    Ok(Sorted { file, len, index })
}

/// -m: inputs already sorted, merged. Piped input is read into a temporary file first.
fn merge_inputs(inputs: &[String], s: &Settings, out: &mut dyn Write) -> io::Result<()> {
    let mut sorted = Vec::new();
    for (k, name) in inputs.iter().enumerate() {
        let (file, len) = if name == "-" {
            let (mut file, _) = temp_file(s, 1_000_000 + k)?;
            io::copy(&mut io::stdin().lock(), &mut file)?;
            let len = file.metadata()?.len();
            (file, len)
        } else {
            chunks::open(name)?
        };
        // A last line without its end byte gets one, as the merge expects.
        let mut file = file;
        let mut len = len;
        if len > 0 {
            let mut last = [0u8];
            chunks::read_at(&file, len - 1, &mut last)?;
            if last[0] != s.end {
                let (mut copy, _) = temp_file(s, 2_000_000 + k)?;
                let mut source = &file;
                // Positional reads move a Windows file's position: start from the beginning.
                io::Seek::seek(&mut source, io::SeekFrom::Start(0))?;
                io::copy(&mut source, &mut copy)?;
                copy.write_all(&[s.end])?;
                file = copy;
                len += 1;
            }
        }
        sorted.push(index_file(file, len, s.end)?);
    }
    let sources: Vec<Source> = sorted.iter().map(Source::File).collect();
    merge(&sources, s, &mut |m: Merged| out.write_all(&m.bytes))?;
    out.flush()
}

/// -c and -C: whether a file is in order (strictly, with -u); the first line out of order is reported unless quiet.
fn check_sorted(name: &str, s: &Settings, quiet: bool) -> io::Result<i32> {
    let order = &s.order;
    let bad = |a: &[u8], b: &[u8]| -> bool {
        match order.cmp(a, b) {
            Ordering::Greater => true,
            Ordering::Equal => order.unique,
            Ordering::Less => false,
        }
    };
    struct Piece {
        first: Vec<u8>,
        last: Vec<u8>,
        lines: u64,
        disorder: Option<(u64, Vec<u8>)>,
    }
    let look = |bytes: &[u8]| -> Piece {
        let mut lines = bytes.split(|&b| b == s.end);
        let mut count = 0u64;
        let mut prev: Option<&[u8]> = None;
        let mut first: &[u8] = &[];
        let mut disorder = None;
        let ends_with_end = bytes.last() == Some(&s.end);
        let total = chunks::count_byte(bytes, s.end) + u64::from(!ends_with_end && !bytes.is_empty());
        for line in lines.by_ref() {
            if count == total {
                break;
            }
            if count == 0 {
                first = line;
            }
            if let Some(p) = prev {
                if disorder.is_none() && bad(p, line) {
                    disorder = Some((count, line.to_vec()));
                }
            }
            prev = Some(line);
            count += 1;
        }
        Piece { first: first.to_vec(), last: prev.unwrap_or(&[]).to_vec(), lines: count, disorder }
    };
    let mut seen = 0u64;
    let mut previous: Option<Vec<u8>> = None;
    let mut found: Option<(u64, Vec<u8>)> = None;
    let mut sink = |p: Piece| -> io::Result<()> {
        if found.is_some() || p.lines == 0 {
            return Ok(());
        }
        if let Some(prev) = &previous {
            if bad(prev, &p.first) {
                found = Some((seen + 1, p.first.clone()));
                return Ok(());
            }
        }
        if let Some((k, line)) = p.disorder {
            found = Some((seen + k + 1, line));
            return Ok(());
        }
        seen += p.lines;
        previous = Some(p.last);
        Ok(())
    };
    if name == "-" {
        let mut all = Vec::new();
        io::stdin().read_to_end(&mut all)?;
        sink(look(&all))?;
    } else {
        let (file, len) = chunks::open(name)?;
        let cuts = chunks::boundaries(&file, 0, len, 32 << 20, s.end)?;
        chunks::ordered(&file, &cuts, s.threads, &|_, bytes| Ok(look(bytes)), &mut |_, p| sink(p))?;
    }
    match found {
        None => Ok(0),
        Some((line, text)) => {
            if !quiet {
                crate::note(format!("lw: {name}:{line}: disorder: {}", String::from_utf8_lossy(&text)));
            }
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(order: Order, budget: u64) -> Settings {
        Settings { order, end: b'\n', threads: 4, budget, temp: vec![std::env::temp_dir()] }
    }

    #[test]
    fn sorts_in_memory_and_through_run_files_alike() {
        let mut text = Vec::new();
        let mut x = 12345u64;
        for _ in 0..400_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            text.extend_from_slice(format!("{:x}\n", x % 50_000).as_bytes());
        }
        let dir = std::env::temp_dir().join(format!("lw-sort-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("in.txt");
        std::fs::write(&path, &text).unwrap();
        let mut want: Vec<&[u8]> = text.split(|&b| b == b'\n').filter(|l| !l.is_empty()).collect();
        want.sort();
        let mut expected = Vec::new();
        for l in &want {
            expected.extend_from_slice(l);
            expected.push(b'\n');
        }
        want.dedup();
        let mut unique = Vec::new();
        for l in &want {
            unique.extend_from_slice(l);
            unique.push(b'\n');
        }
        let inputs = vec![path.to_string_lossy().into_owned()];
        for budget in [1u64 << 30, 40 << 20] {
            for u in [false, true] {
                let order = Order::new(Vec::new(), Opts::default(), None, false, u, (0, 0));
                let mut out = Vec::new();
                sort_inputs(&inputs, &settings(order, budget), &mut out).unwrap();
                assert!(out == *if u { &unique } else { &expected }, "budget {budget} unique {u}");
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
