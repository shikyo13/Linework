//! Long stretches of zero bytes in a large file, as a damaged download or
//! copy leaves them in place of the data that never arrived. When the part of
//! the file on screen holds one, it is measured to both ends in the
//! background, and a bar above the text says how long it is and where, with
//! buttons that move to the text just before it or just after it.
use super::*;

/// Runs of zero bytes shorter than this are left alone.
pub(crate) const LONG_ZEROS: usize = 4096;
/// How much of the file is read at a time while measuring a run.
const CHUNK: usize = 1 << 20;
/// How often a run still being measured is reported, in chunks read.
const REPORT_EVERY: usize = 64;

/// A run of zero bytes and the document it is in.
#[derive(Clone, PartialEq)]
pub(crate) struct Run {
    pub id: u64,
    pub rev: u64,
    pub pane: usize,
    pub bytes: Range<u64>,
    /// Both ends are known; false while they are still being read.
    pub measured: bool,
}

#[derive(Default)]
pub(crate) struct Zeros {
    /// Runs measured to both ends, by document and revision.
    known: Vec<(u64, u64, Range<u64>)>,
    /// The run in each pane's part of the file, if any.
    pub shown: [Option<Run>; 2],
    /// The measurement under way: its number and the flag that stops it.
    scan: Option<(u64, Arc<AtomicBool>)>,
    next: u64,
}

/// The first run of at least `LONG_ZEROS` zero bytes in `bytes`, at or after
/// `from` if there is one there, or else the last one before it.
pub(crate) fn long_zeros(bytes: &[u8], from: usize) -> Option<Range<usize>> {
    let mut runs = Vec::new();
    let mut at = 0;
    while let Some(start) = memchr::memchr(0, &bytes[at..]).map(|i| at + i) {
        let end = bytes[start..].iter().position(|&b| b != 0).map_or(bytes.len(), |i| start + i);
        if end - start >= LONG_ZEROS {
            runs.push(start..end);
        }
        at = end;
        if at >= bytes.len() {
            break;
        }
    }
    let after = runs.iter().position(|r| r.end > from);
    match after {
        Some(i) => runs.get(i).cloned(),
        None => runs.last().cloned(),
    }
}

/// Whether every byte is zero, a word at a time.
fn all_zero(bytes: &[u8]) -> bool {
    let (head, words, tail) = unsafe { bytes.align_to::<u64>() };
    head.iter().all(|&b| b == 0) && words.iter().all(|&w| w == 0) && tail.iter().all(|&b| b == 0)
}

/// Measures the run of zero bytes holding `seed` to both ends, reading `doc`
/// a chunk at a time, and reports the run as it grows.
fn measure(doc: &Document, seed: Range<u64>, cancel: &AtomicBool, mut report: impl FnMut(Range<u64>, bool)) -> io::Result<()> {
    let (mut start, mut end) = (seed.start, seed.end);
    let len = doc.len();
    let mut chunks = 0usize;
    // Back towards the start of the file.
    while start > 0 {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let from = start.saturating_sub(CHUNK as u64);
        let bytes = doc.read(from, (start - from) as usize)?;
        match bytes.iter().rposition(|&b| b != 0) {
            Some(last) => {
                start = from + last as u64 + 1;
                break;
            }
            None => start = from,
        }
        chunks += 1;
        if chunks % REPORT_EVERY == 0 {
            report(start..end, false);
        }
    }
    // On towards the end.
    while end < len {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let bytes = doc.read(end, CHUNK.min((len - end) as usize))?;
        if all_zero(&bytes) {
            end += bytes.len() as u64;
        } else {
            end += bytes.iter().position(|&b| b != 0).unwrap_or(0) as u64;
            break;
        }
        chunks += 1;
        if chunks % REPORT_EVERY == 0 {
            report(start..end, false);
        }
    }
    report(start..end, true);
    Ok(())
}

/// A count of bytes as a person reads it: 24.0 MiB.
pub(crate) fn size(bytes: u64) -> String {
    let units = [("GiB", 1u64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
    for (name, unit) in units {
        if bytes >= unit {
            return format!("{:.1} {name}", bytes as f64 / unit as f64);
        }
    }
    format!("{bytes} bytes")
}

impl App {
    /// Called when pane `p` shows a part of document `i`: `found` is a long
    /// run of zero bytes in it, in the document's own positions, if any.
    pub(super) fn zeros_in_view(&mut self, p: usize, i: usize, found: Option<Range<u64>>) {
        let (id, rev) = (self.buffers[i].id, self.buffers[i].rev);
        let Some(seed) = found else {
            self.zeros.shown[p] = None;
            return;
        };
        // A run already shown or measured that holds this one.
        let holds = |r: &Range<u64>| r.start <= seed.start && seed.end <= r.end;
        if let Some(run) = &self.zeros.shown[p] {
            if run.id == id && run.rev == rev && holds(&run.bytes) {
                return;
            }
        }
        if let Some((_, _, known)) = self.zeros.known.iter().find(|(kid, krev, r)| *kid == id && *krev == rev && holds(r)) {
            self.zeros.shown[p] = Some(Run { id, rev, pane: p, bytes: known.clone(), measured: true });
            return;
        }
        // Measure it in the background; the bar shows what is known so far.
        if let Some((_, flag)) = self.zeros.scan.take() {
            flag.store(true, Ordering::Relaxed);
        }
        self.zeros.next += 1;
        let job = self.zeros.next;
        let cancel = Arc::new(AtomicBool::new(false));
        self.zeros.scan = Some((job, cancel.clone()));
        self.zeros.shown[p] = Some(Run { id, rev, pane: p, bytes: seed.clone(), measured: false });
        let doc = self.buffers[i].doc.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let sent = measure(&doc, seed, &cancel, |bytes, done| {
                let _ = tx.send(Worker::Zeros(job, id, rev, bytes, done));
            });
            if let Err(e) = sent {
                let _ = tx.send(Worker::ZerosFailed(job, e.to_string()));
            }
        });
    }

    /// A measurement reported how far the run reaches.
    pub(super) fn zeros_measured(&mut self, job: u64, id: u64, rev: u64, bytes: Range<u64>, done: bool) {
        if self.zeros.scan.as_ref().is_none_or(|(current, _)| *current != job) {
            return;
        }
        if done {
            self.zeros.scan = None;
            self.zeros.known.retain(|(kid, krev, _)| *kid != id || *krev == rev);
            self.zeros.known.push((id, rev, bytes.clone()));
        }
        for run in self.zeros.shown.iter_mut().flatten() {
            if run.id == id && run.rev == rev && run.bytes.start >= bytes.start && run.bytes.end <= bytes.end {
                run.bytes = bytes.clone();
                run.measured = done;
            }
        }
        self.layout();
    }

    pub(super) fn zeros_failed(&mut self, job: u64, error: String) {
        if self.zeros.scan.as_ref().is_some_and(|(current, _)| *current == job) {
            self.zeros.scan = None;
            self.notice(format!("The zero bytes could not be measured: {error}"));
        }
    }

    /// The run of zero bytes in the active pane, if its document is still the one it was found in.
    pub(crate) fn active_zeros(&self) -> Option<&Run> {
        let run = self.zeros.shown[self.active].as_ref()?;
        let b = self.assigned[self.active].and_then(|id| self.idx(id)).map(|i| &self.buffers[i])?;
        (b.id == run.id && b.rev == run.rev).then_some(run)
    }

    /// The bar's text: how long the run is and where.
    pub(crate) fn zeros_caption(&self) -> Option<(String, bool)> {
        let run = self.active_zeros()?;
        let length = run.bytes.end - run.bytes.start;
        let text = if run.measured {
            format!(
                "{} of zero bytes here, from byte {} to {}: data missing from the file itself",
                size(length),
                state::pretty(run.bytes.start),
                state::pretty(run.bytes.end)
            )
        } else {
            format!("Zero bytes here, at least {} so far: measuring how far they reach…", size(length))
        };
        Some((text, run.measured))
    }

    /// Moves to the text just after the run (`after`) or just before it.
    pub(super) fn zeros_jump(&mut self, after: bool) {
        let Some(run) = self.active_zeros().cloned() else {
            self.notice("There are no long zero bytes here");
            return;
        };
        if !run.measured {
            self.notice("Still measuring the zero bytes");
            return;
        }
        let Some(i) = self.current() else { return };
        let len = self.buffers[i].doc.len();
        if after && run.bytes.end >= len {
            self.notice("The zero bytes run to the end of the file");
            return;
        }
        if !after && run.bytes.start == 0 {
            self.notice("The zero bytes start at the beginning of the file");
            return;
        }
        // Zeros left by a damaged copy usually cut a line in two. After them, the
        // cut line's end is a stray piece; the first whole line follows it.
        let at = if after {
            let doc = &self.buffers[i].doc;
            let near = doc.read(run.bytes.end, LONG_ZEROS.min((len - run.bytes.end) as usize)).unwrap_or_default();
            match memchr::memchr(b'\n', &near) {
                Some(n) if run.bytes.end + (n as u64) + 1 < len => run.bytes.end + n as u64 + 1,
                _ => run.bytes.end,
            }
        } else {
            run.bytes.start
        };
        self.remember_location();
        self.go(at, 0);
        // The line before may be the zeros, one line millions of characters wide: start at the left.
        self.edit().send(linework_editor::sci::SCI_SETXOFFSET, 0, 0);
        self.notice(if after {
            format!("Skipped {} of zero bytes", size(run.bytes.end - run.bytes.start))
        } else {
            "Moved to the text before the zero bytes".into()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_runs_are_found_and_short_ones_left() {
        let mut bytes = b"text\n".repeat(10);
        bytes.extend(std::iter::repeat_n(0u8, 100));
        bytes.extend(b"more\n");
        let short_end = bytes.len();
        bytes.extend(std::iter::repeat_n(0u8, LONG_ZEROS));
        bytes.extend(b"end");
        assert_eq!(long_zeros(&bytes, 0), Some(short_end..short_end + LONG_ZEROS));
        assert_eq!(long_zeros(&bytes[..short_end], 0), None);
        // A view that starts and ends inside zeros.
        assert_eq!(long_zeros(&[0u8; 8192], 100), Some(0..8192));
    }

    #[test]
    fn a_run_is_measured_to_both_ends_beyond_the_part_shown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gaps.txt");
        let mut bytes = b"line before\n".repeat(200_000);
        let start = bytes.len() as u64 - 3; // the run starts inside a line, as in a damaged copy
        bytes.truncate(start as usize);
        bytes.extend(std::iter::repeat_n(0u8, 3 * CHUNK + 12345));
        let end = bytes.len() as u64;
        bytes.extend(b"_is\nline after\n".repeat(1000));
        std::fs::write(&path, &bytes).unwrap();
        let doc = Document::open(&path).unwrap();
        let seed = start + CHUNK as u64..start + CHUNK as u64 + 8192;
        let mut last = None;
        measure(&doc, seed, &AtomicBool::new(false), |r, done| last = Some((r, done))).unwrap();
        assert_eq!(last, Some((start..end, true)));
        assert_eq!(size(end - start), "3.0 MiB");
    }
}
