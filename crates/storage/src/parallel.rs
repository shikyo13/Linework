//! Scan a large document on several threads and deliver the results in order.
use crate::{
    invalid,
    source::{Lane, Lead, Source},
    Document,
};
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Condvar, Mutex},
    thread,
};

/// Reading from the file cache is limited by memory bandwidth well before every
/// logical processor is busy, and the rest of the desktop needs some of them.
pub(crate) fn threads() -> usize {
    let logical = thread::available_parallelism().map_or(1, |n| n.get());
    (logical * 3 / 4).clamp(1, 12)
}

/// One thread's view of a document. Disk sources are read through file
/// objects owned by this reader, so several readers do not queue on one handle.
pub(crate) struct Reader<'a> {
    doc: &'a Document,
    lanes: Vec<(usize, Lane)>,
    /// The scan's lead, when its document is one file read in order (see `lead.rs`).
    lead: Option<&'a Lead>,
}
impl<'a> Reader<'a> {
    /// A reader for one thread of a scan, with the scan's lead if it has one.
    pub(crate) fn led(doc: &'a Document, lead: Option<&'a Lead>) -> Self {
        Self {
            doc,
            lanes: Vec::new(),
            lead,
        }
    }
    fn lane(lanes: &mut Vec<(usize, Lane)>, source: &Arc<Source>) -> usize {
        let key = Arc::as_ptr(source) as usize;
        match lanes.iter().position(|(k, _)| *k == key) {
            Some(slot) => slot,
            None => {
                lanes.push((key, Lane::default()));
                lanes.len() - 1
            }
        }
    }
    /// Before reading the chunk at `offset`: keep the lead ahead of it. The
    /// document of a scan with a lead is one file, so its offsets are the file's.
    pub(crate) fn ahead(&mut self, offset: u64) -> io::Result<()> {
        let (Some(lead), Some(source)) = (self.lead, self.doc.sole_source()) else {
            return Ok(());
        };
        let slot = Self::lane(&mut self.lanes, source);
        lead.advance(&mut self.lanes[slot].1, offset)
    }
    pub(crate) fn read_into(&mut self, offset: u64, dst: &mut [u8]) -> io::Result<()> {
        let len = self.doc.len();
        if offset > len || dst.len() as u64 > len - offset {
            return Err(invalid("Read outside document"));
        }
        let Some(root) = &self.doc.root else {
            return Ok(());
        };
        let (lanes, lead) = (&mut self.lanes, self.lead);
        root.read_via(offset, dst, &mut |source: &Arc<Source>, at, out| {
            if !source.on_disk() {
                return source.read(at, out);
            }
            let slot = Self::lane(lanes, source);
            match lead.filter(|lead| lead.is(source)) {
                Some(lead) => lead.read(source, &mut lanes[slot].1, at, out),
                None => source.read_lane(&mut lanes[slot].1, at, out),
            }
        })
    }
}

/// Write the whole document to `out` in order, read on `threads` threads with a
/// handle each. One thread copying from the file cache is the limit of a large
/// save: alone it reads about 10 GB/s, beside the save's hash and write about
/// 6 to 7. Blocks are reused, so at most about `3 * threads` MiB are held.
pub(crate) fn copy_out(doc: &Document, threads: usize, out: &mut impl io::Write) -> io::Result<()> {
    const CHUNK: u64 = 1024 * 1024;
    let len = doc.len();
    let chunks = len.div_ceil(CHUNK) as usize;
    let lead = doc.lead(0);
    let spare = Mutex::new(Vec::<Vec<u8>>::new());
    ordered(
        chunks,
        threads.min(chunks).max(1),
        || Reader::led(doc, lead.as_ref()),
        |k, reader| {
            let left = k as u64 * CHUNK;
            let mut bytes = spare.lock().unwrap().pop().unwrap_or_default();
            bytes.resize((len - left).min(CHUNK) as usize, 0);
            reader.ahead(left)?;
            reader.read_into(left, &mut bytes)?;
            Ok(bytes)
        },
        |_, bytes| {
            out.write_all(&bytes)?;
            spare.lock().unwrap().push(bytes);
            Ok(false)
        },
    )?;
    Ok(())
}

struct Queue<T> {
    next: usize,
    taken: usize,
    ready: BTreeMap<usize, io::Result<T>>,
    stop: bool,
    lost: bool,
}
struct Shared<T> {
    queue: Mutex<Queue<T>>,
    filled: Condvar,
    space: Condvar,
}
/// A worker that unwinds must not leave the consumer waiting for its result.
struct Alive<'a, T>(&'a Shared<T>);
impl<T> Drop for Alive<'_, T> {
    fn drop(&mut self) {
        if thread::panicking() {
            let mut queue = self.0.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.lost = true;
            queue.stop = true;
            drop(queue);
            self.0.filled.notify_all();
            self.0.space.notify_all();
        }
    }
}

/// Compute `work(index)` for `0..count` on `threads` threads and pass each
/// result to `take` on the calling thread, strictly in index order. `take`
/// returns true to stop early. Workers run at most `3 * threads` items ahead
/// of the consumer, which bounds the results held in memory.
pub(crate) fn ordered<T: Send, S>(
    count: usize,
    threads: usize,
    state: impl Fn() -> S + Sync,
    work: impl Fn(usize, &mut S) -> io::Result<T> + Sync,
    mut take: impl FnMut(usize, T) -> io::Result<bool>,
) -> io::Result<bool> {
    let shared = Shared {
        queue: Mutex::new(Queue {
            next: 0,
            taken: 0,
            ready: BTreeMap::new(),
            stop: false,
            lost: false,
        }),
        filled: Condvar::new(),
        space: Condvar::new(),
    };
    let window = threads * 3;
    thread::scope(|scope| {
        for _ in 0..threads.min(count) {
            scope.spawn(|| {
                let _alive = Alive(&shared);
                let mut local = state();
                loop {
                    let index = {
                        let mut queue = shared.queue.lock().unwrap();
                        loop {
                            if queue.stop || queue.next == count {
                                return;
                            }
                            if queue.next < queue.taken + window {
                                break;
                            }
                            queue = shared.space.wait(queue).unwrap();
                        }
                        queue.next += 1;
                        queue.next - 1
                    };
                    let result = work(index, &mut local);
                    let failed = result.is_err();
                    let mut queue = shared.queue.lock().unwrap();
                    queue.ready.insert(index, result);
                    // Every earlier item is already claimed, so the consumer
                    // still reaches this error in order.
                    queue.stop |= failed;
                    drop(queue);
                    shared.filled.notify_all();
                    if failed {
                        shared.space.notify_all();
                    }
                }
            });
        }
        let finish = |result| {
            shared.queue.lock().unwrap().stop = true;
            shared.space.notify_all();
            result
        };
        for index in 0..count {
            let result = {
                let mut queue = shared.queue.lock().unwrap();
                loop {
                    if let Some(result) = queue.ready.remove(&index) {
                        queue.taken = index + 1;
                        break result;
                    }
                    if queue.lost {
                        // The scope repeats the worker's panic on exit.
                        return Err(io::Error::other("A search thread stopped unexpectedly"));
                    }
                    queue = shared.filled.wait(queue).unwrap();
                }
            };
            shared.space.notify_all();
            match result.and_then(|value| take(index, value)) {
                Ok(false) => {}
                other => return finish(other),
            }
        }
        finish(Ok(false))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn results_arrive_in_order_and_an_early_stop_ends_the_work() -> io::Result<()> {
        let mut seen = Vec::new();
        let stopped = ordered(
            1000,
            6,
            || (),
            |i, _| {
                if i % 7 == 0 {
                    thread::yield_now();
                }
                Ok(i * 2)
            },
            |i, v| {
                assert_eq!(v, i * 2);
                seen.push(i);
                Ok(false)
            },
        )?;
        assert!(!stopped);
        assert_eq!(seen, (0..1000).collect::<Vec<_>>());

        let done = AtomicUsize::new(0);
        let stopped = ordered(
            100_000,
            6,
            || (),
            |i, _| {
                done.fetch_add(1, Ordering::Relaxed);
                Ok(i)
            },
            |i, _| Ok(i == 40),
        )?;
        assert!(stopped);
        // Workers never run further ahead than the window.
        assert!(done.load(Ordering::Relaxed) <= 41 + 6 * 3 + 6);
        Ok(())
    }

    #[test]
    fn the_first_error_in_order_is_reported() {
        let error = ordered(
            500,
            4,
            || (),
            |i, _| {
                if i == 123 || i == 300 {
                    Err(io::Error::other(format!("item {i}")))
                } else {
                    Ok(i)
                }
            },
            |_, _| Ok(false),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "item 123");
    }
}
