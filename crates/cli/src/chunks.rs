//! Reading a large file in line-aligned pieces on several threads, and writing
//! what each piece becomes in the file's order: the shared machinery of lw
//! split, cut and sort.
use std::{
    fs::File,
    io::{self, Write},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Condvar, Mutex,
    },
};

/// Reads `buf.len()` bytes at `at`, or fewer at the end of the file; returns how many.
pub fn read_at(file: &File, at: u64, buf: &mut [u8]) -> io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        #[cfg(windows)]
        let n = std::os::windows::fs::FileExt::seek_read(file, &mut buf[done..], at + done as u64);
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::read_at(file, &mut buf[done..], at + done as u64);
        match n {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

/// Where the line holding byte `at` ends: the offset just after the next
/// `end` byte at or after `at`, or the file's length.
pub fn line_end_after(file: &File, len: u64, at: u64, end: u8) -> io::Result<u64> {
    let mut buf = vec![0u8; 64 << 10];
    let mut pos = at;
    while pos < len {
        let n = read_at(file, pos, &mut buf)?;
        if n == 0 {
            break;
        }
        if let Some(i) = memchr::memchr(end, &buf[..n]) {
            return Ok(pos + i as u64 + 1);
        }
        pos += n as u64;
    }
    Ok(len)
}

/// Line starts that cut `from..to` into pieces of about `piece` bytes each:
/// the first is `from`, the last `to`, each one just after an `end` byte. A
/// line longer than `piece` makes its piece longer.
pub fn boundaries(file: &File, from: u64, to: u64, piece: u64, end: u8) -> io::Result<Vec<u64>> {
    let mut cuts = vec![from];
    let mut at = from;
    while at < to {
        let aim = at.saturating_add(piece.max(1));
        if aim >= to {
            break;
        }
        let next = line_end_after(file, to, aim - 1, end)?;
        if next >= to {
            break;
        }
        cuts.push(next);
        at = next;
    }
    cuts.push(to);
    cuts.dedup();
    Ok(cuts)
}

/// Runs `work(i)` for i in 0..count on up to `threads` threads, results in order.
pub fn each<T: Send>(count: usize, threads: usize, work: &(dyn Fn(usize) -> T + Sync)) -> Vec<T> {
    let slots: Vec<Mutex<Option<T>>> = (0..count).map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads.clamp(1, count.max(1)) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= count {
                    break;
                }
                let v = work(i);
                *slots[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
            });
        }
    });
    slots.into_iter().map(|s| s.into_inner().unwrap_or_else(|e| e.into_inner()).expect("every task ran")).collect()
}

/// The offset just after the `n`th `end` byte (from 1) in `bytes`.
pub fn nth_end(bytes: &[u8], end: u8, mut n: u64) -> Option<usize> {
    const BLOCK: usize = 4096;
    let mut at = 0;
    while at + BLOCK <= bytes.len() {
        let c = count_byte(&bytes[at..at + BLOCK], end);
        if c >= n {
            break;
        }
        n -= c;
        at += BLOCK;
    }
    memchr::memchr_iter(end, &bytes[at..]).nth(n as usize - 1).map(|i| at + i + 1)
}

/// The number of `end` bytes in `bytes`, with AVX2 where the processor has it.
pub fn count_byte(bytes: &[u8], end: u8) -> u64 {
    #[cfg(target_arch = "x86_64")]
    if bytes.len() >= 64 && std::is_x86_feature_detected!("avx2") {
        return unsafe { count_avx2(bytes, end) };
    }
    memchr::memchr_iter(end, bytes).count() as u64
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn count_avx2(bytes: &[u8], end: u8) -> u64 {
    use std::arch::x86_64::*;
    let want = _mm256_set1_epi8(end as i8);
    let mut i = 0;
    let mut n = 0u64;
    while i + 128 <= bytes.len() {
        let p = bytes.as_ptr().add(i);
        let a = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(p.cast()), want)) as u32;
        let b = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(p.add(32).cast()), want)) as u32;
        let c = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(p.add(64).cast()), want)) as u32;
        let d = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(p.add(96).cast()), want)) as u32;
        n += (a.count_ones() + b.count_ones() + c.count_ones() + d.count_ones()) as u64;
        i += 128;
    }
    while i + 32 <= bytes.len() {
        let m = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(bytes.as_ptr().add(i).cast()), want)) as u32;
        n += m.count_ones() as u64;
        i += 32;
    }
    n + memchr::memchr_iter(end, &bytes[i..]).count() as u64
}

/// Runs `work` on each piece `cuts[i]..cuts[i+1]` of `file` on `threads`
/// threads, and hands what each returns to `sink` in piece order on the
/// calling thread. At most a few pieces per thread are in memory at once.
/// The first error from any side stops the rest and is returned.
pub fn ordered<T: Send>(
    file: &File,
    cuts: &[u64],
    threads: usize,
    work: &(dyn Fn(usize, &[u8]) -> io::Result<T> + Sync),
    sink: &mut dyn FnMut(usize, T) -> io::Result<()>,
) -> io::Result<()> {
    let pieces = cuts.len().saturating_sub(1);
    let read = |i: usize| -> io::Result<T> {
        let (a, b) = (cuts[i], cuts[i + 1]);
        let mut buf = vec![0u8; (b - a) as usize];
        let n = read_at(file, a, &mut buf)?;
        if n as u64 != b - a {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the file became shorter while it was read"));
        }
        work(i, &buf)
    };
    tasks(pieces, threads, &read, sink)
}

/// Runs `work(i)` for i in 0..count on `threads` threads, handing each result
/// to `sink` in order on the calling thread; at most a few results per thread
/// wait in memory. The first error from any side stops the rest.
pub fn tasks<T: Send>(
    count: usize,
    threads: usize,
    work: &(dyn Fn(usize) -> io::Result<T> + Sync),
    sink: &mut dyn FnMut(usize, T) -> io::Result<()>,
) -> io::Result<()> {
    let pieces = count;
    if pieces == 0 {
        return Ok(());
    }
    let threads = threads.clamp(1, pieces);
    let window = threads * 2;
    let next = AtomicUsize::new(0);
    // How many pieces the sink has taken; a piece is only started within `window` of it.
    let taken = Mutex::new(0usize);
    let moved = Condvar::new();
    let done: Mutex<Vec<Option<io::Result<T>>>> = Mutex::new((0..pieces).map(|_| None).collect());
    let ready = Condvar::new();
    let failed = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| -> io::Result<()> {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= pieces || failed.load(Ordering::Relaxed) {
                        break;
                    }
                    {
                        let mut t = taken.lock().unwrap_or_else(|e| e.into_inner());
                        while i >= *t + window && !failed.load(Ordering::Relaxed) {
                            t = moved.wait(t).unwrap_or_else(|e| e.into_inner());
                        }
                    }
                    let result = work(i);
                    if result.is_err() {
                        failed.store(true, Ordering::Relaxed);
                    }
                    done.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(result);
                    ready.notify_all();
                    moved.notify_all();
                }
            });
        }
        let mut outcome = Ok(());
        for i in 0..pieces {
            let result = {
                let mut d = done.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(r) = d[i].take() {
                        break r;
                    }
                    d = ready.wait(d).unwrap_or_else(|e| e.into_inner());
                }
            };
            let step = result.and_then(|value| sink(i, value));
            *taken.lock().unwrap_or_else(|e| e.into_inner()) = i + 1;
            moved.notify_all();
            if let Err(e) = step {
                outcome = Err(e);
                failed.store(true, Ordering::Relaxed);
                moved.notify_all();
                break;
            }
        }
        outcome
    })
}

/// A file being written, removed again if it is dropped before `finish`, so a
/// command that fails or is stopped leaves no half-written output behind.
pub struct Output {
    path: std::path::PathBuf,
    file: Option<io::BufWriter<File>>,
}

impl Output {
    pub fn create(path: &std::path::Path) -> io::Result<Self> {
        let file = File::create(path).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
        Ok(Self { path: path.to_owned(), file: Some(io::BufWriter::with_capacity(8 << 20, file)) })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn finish(mut self) -> io::Result<()> {
        if let Some(f) = self.file.take() {
            f.into_inner().map_err(|e| e.into_error())?;
        }
        Ok(())
    }
}

impl Write for Output {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.as_mut().expect("written after finish").write(buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.file.as_mut().expect("written after finish").write_all(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().expect("written after finish").flush()
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        if self.file.take().is_some() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Opens an input file, naming it in any error.
pub fn open(path: &str) -> io::Result<(File, u64)> {
    let file = File::open(path).map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))?;
    let len = file.metadata()?.len();
    Ok((file, len))
}

/// The same file, by identity rather than by name, so an output never overwrites an input it is still reading.
pub fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_cuts_agree_with_a_plain_scan() {
        let mut text = Vec::new();
        for i in 0..5000u32 {
            text.extend_from_slice(format!("line {i} {}\n", "x".repeat((i % 97) as usize)).as_bytes());
        }
        text.extend_from_slice(b"no end");
        assert_eq!(count_byte(&text, b'\n'), text.iter().filter(|&&b| b == b'\n').count() as u64);
        let dir = std::env::temp_dir().join(format!("lw-chunks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.txt");
        std::fs::write(&path, &text).unwrap();
        let file = File::open(&path).unwrap();
        let cuts = boundaries(&file, 0, text.len() as u64, 4096, b'\n').unwrap();
        assert_eq!(*cuts.first().unwrap(), 0);
        assert_eq!(*cuts.last().unwrap(), text.len() as u64);
        for c in &cuts[1..cuts.len() - 1] {
            assert_eq!(text[*c as usize - 1], b'\n');
        }
        let mut joined = Vec::new();
        ordered(&file, &cuts, 4, &|_, bytes| Ok(bytes.to_vec()), &mut |_, part: Vec<u8>| {
            joined.extend_from_slice(&part);
            Ok(())
        })
        .unwrap();
        assert_eq!(joined, text);
        drop(file);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
