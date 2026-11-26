//! Replace All for a literal search. The edited document is assembled from
//! pieces of the existing one around a single shared copy of the replacement,
//! so nothing is copied and the cost follows the number of matches, not the
//! size of the file. The threads that find the matches also count the line
//! breaks between them, which makes every new piece complete when it is made.
use super::{
    parallel,
    replacing::{Change, SPARSE_BYTES, SPARSE_MATCHES},
    Groups, Search, CHUNK, PARALLEL_MIN,
};
use crate::{
    patched::{Patched, Stretch},
    source::{count, Source},
    tree::{self, Body, Node},
    Document,
};
use std::{io, ops::Range, sync::Arc};

/// Each replacement adds about 300 bytes of piece tree that undo keeps alive.
/// Past this many, the result is streamed to a temporary file instead.
const MAX_PIECES: usize = 1_000_000;

pub(super) enum Event {
    /// Unchanged bytes inside one existing piece, with their line breaks. A CR
    /// directly before an LF is not counted: the LF is, wherever it falls.
    Gap { len: u32, breaks: u32 },
    /// A match, with the byte before it, its first byte and the byte after it.
    Hit { len: u32, prev: u8, head: u8, next: u8 },
}

/// Everything in `from..right` in order. `cuts` holds the offsets at which
/// the document's existing pieces start, so no gap spans two of them.
fn census(
    search: &Search<'_>,
    cuts: &[u64],
    from: u64,
    (left, right): (u64, u64),
    bytes: &mut Vec<u8>,
    read: impl FnMut(u64, &mut [u8]) -> io::Result<()>,
) -> io::Result<Vec<Event>> {
    let (hits, origin) = hits_in(search, from, (left, right), bytes, read)?;
    let at = |pos: u64| byte_at(search, bytes, origin, pos);
    let mut events = Vec::with_capacity(hits.len() * 2 + 2);
    let mut cut = cuts.partition_point(|&c| c <= from);
    let mut pos = from;
    for hit in hits.into_iter().map(Some).chain([None]) {
        let stop = hit.as_ref().map_or(right, |h| h.start);
        while pos < stop {
            let end = cuts.get(cut).map_or(stop, |&c| c.min(stop));
            let text = &bytes[(pos - origin) as usize..(end - origin) as usize];
            events.push(Event::Gap {
                len: text.len() as u32,
                breaks: count(text, at(end)) as u32,
            });
            pos = end;
            cut += usize::from(cuts.get(cut) == Some(&pos));
        }
        let Some(hit) = hit else { break };
        events.push(Event::Hit {
            len: (hit.end - hit.start) as u32,
            prev: hit.start.checked_sub(1).and_then(at).unwrap_or(0),
            head: at(hit.start).unwrap_or(0),
            next: at(hit.end).unwrap_or(0),
        });
        pos = hit.end;
        cut += cuts[cut..].partition_point(|&c| c <= pos);
    }
    Ok(events)
}

/// The byte at `pos`, from `bytes` (read from `origin`) or, past them, from the document.
fn byte_at(search: &Search<'_>, bytes: &[u8], origin: u64, pos: u64) -> Option<u8> {
    bytes.get((pos - origin) as usize).copied().or_else(|| {
        let mut one = [0u8];
        (pos < search.d.len() && search.d.read_into(pos, &mut one).is_ok()).then_some(one[0])
    })
}

/// The matches `hits` (all starting in `from..right`) as a stretch from `from`,
/// which runs on to the end of a match that ends past `right`.
fn stretch_of(from: u64, right: u64, hits: &[Range<u64>]) -> Stretch {
    let len = hits.first().map_or(0, |h| (h.end - h.start) as u32);
    let uniform = hits.iter().all(|h| h.end - h.start == u64::from(len));
    Stretch {
        old: from,
        old_len: hits.last().map_or(right, |h| h.end.max(right)) - from,
        hits: hits.iter().map(|h| (h.start - from) as u32).collect(),
        lens: (!uniform).then(|| hits.iter().map(|h| (h.end - h.start) as u32).collect()),
        len,
    }
}

/// The matches that start in `from..right` and end in the search's scope, in
/// order, with the chunk read into `bytes` from the returned origin.
fn hits_in(
    search: &Search<'_>,
    from: u64,
    (left, right): (u64, u64),
    bytes: &mut Vec<u8>,
    read: impl FnMut(u64, &mut [u8]) -> io::Result<()>,
) -> io::Result<(Vec<Range<u64>>, u64)> {
    let mut hits = Vec::new();
    let mut keep = |hit: Range<u64>, _: Option<&Groups<'_>>| {
        if hit.end <= search.scope.end {
            hits.push(hit);
        }
        Ok(false)
    };
    // A pattern reads on to the end of the piece's last line; when that line
    // is too long, the streaming engine finds the piece's matches.
    let origin = if search.lined {
        let (origin, whole) = search.load_lines(left, right, bytes, read)?;
        if whole {
            search.matches(bytes, origin, &(from..right), false, &mut keep)?;
        } else {
            search.streamed_part(from..right, &mut keep)?;
        }
        origin
    } else {
        let origin = search.load(left, right, bytes, read)?;
        search.matches(bytes, origin, &(from..right), false, &mut keep)?;
        origin
    };
    Ok((hits, origin))
}

struct Splice<'a> {
    old: &'a [Arc<Node>],
    /// Where each existing piece starts, then the document's length.
    starts: &'a [u64],
    leaf: usize,
    pos: u64,
    /// Start of the unchanged run that ends at `pos`, its first byte when it
    /// does not start a piece, and its line breaks so far.
    from: u64,
    first: u8,
    breaks: u64,
    insert: Option<Arc<Node>>,
    out: Vec<Arc<Node>>,
}
impl Splice<'_> {
    /// The run `from..pos` as a piece of the existing leaf it lies in.
    fn part(&self, last: u8, seam: bool) -> Arc<Node> {
        let node = &self.old[self.leaf];
        let Body::Leaf(source, at) = &node.body else {
            unreachable!("leaf list")
        };
        let start = self.starts[self.leaf];
        let first = if self.from == start { node.first } else { self.first };
        tree::piece(
            source.clone(),
            at + (self.from - start),
            self.pos - self.from,
            first,
            last,
            self.breaks + u64::from(seam),
        )
    }
    fn gap(&mut self, len: u64, breaks: u64) {
        self.breaks += breaks;
        self.pos += len;
        if self.pos < self.starts[self.leaf + 1] {
            return;
        }
        let node = &self.old[self.leaf];
        if self.from == self.starts[self.leaf] {
            // Untouched: keep the piece itself, with everything it has cached.
            self.out.push(node.clone());
        } else {
            let seam = node.last == b'\r'
                && self.old.get(self.leaf + 1).is_some_and(|n| n.first == b'\n');
            self.out.push(self.part(node.last, seam));
        }
        self.leaf += 1;
        self.from = self.pos;
        self.breaks = 0;
    }
    fn hit(&mut self, len: u64, prev: u8, head: u8, next: u8) {
        if self.from < self.pos {
            self.out.push(self.part(prev, prev == b'\r' && head == b'\n'));
        }
        self.out.extend(self.insert.clone());
        self.pos += len;
        while self.leaf < self.old.len() && self.starts[self.leaf + 1] <= self.pos {
            self.leaf += 1;
        }
        self.from = self.pos;
        self.first = next;
        self.breaks = 0;
    }
    /// Apply one chunk. Returns true at the match that exceeds the piece limit,
    /// before it is applied.
    fn apply(
        &mut self,
        events: Vec<Event>,
        replacement: &[u8],
        edits: &mut Edits,
        observe: &mut dyn FnMut(Range<u64>, u64),
    ) -> bool {
        for event in events {
            match event {
                Event::Gap { len, breaks } => self.gap(len.into(), breaks.into()),
                Event::Hit { len, prev, head, next } => {
                    if edits.count == MAX_PIECES {
                        return true;
                    }
                    let len = u64::from(len);
                    observe(self.pos..self.pos + len, replacement.len() as u64);
                    edits.count += 1;
                    // A few edits are applied to the existing tree, which
                    // then shares all its other pieces with the undo history.
                    if edits.count <= SPARSE_MATCHES
                        && (edits.count * replacement.len()) as u64 <= SPARSE_BYTES
                    {
                        edits.few.push((self.pos, len, replacement.to_vec()));
                    } else {
                        edits.few = Vec::new();
                    }
                    self.hit(len, prev, head, next);
                }
            }
        }
        false
    }
}
struct Edits {
    count: usize,
    few: Vec<(u64, u64, Vec<u8>)>,
}

pub(super) fn replace(
    doc: &Document,
    search: &Search<'_>,
    replacement: &[u8],
    observe: &mut dyn FnMut(Range<u64>, u64),
) -> io::Result<(usize, Change)> {
    let range = search.scope.start..search.scope.end.min(doc.len());
    if range.is_empty() {
        return Ok((0, Change::Sparse(Vec::new())));
    }
    let old = tree::leaves(&doc.root);
    let mut starts = Vec::with_capacity(old.len() + 1);
    let mut total = 0;
    for node in &old {
        starts.push(total);
        total += node.len;
    }
    starts.push(total);
    let leaf = starts.partition_point(|&s| s <= range.start) - 1;
    let mut state = Splice {
        old: &old,
        starts: &starts,
        leaf,
        pos: range.start,
        from: range.start,
        first: if range.start == starts[leaf] {
            0
        } else {
            doc.read(range.start, 1)?[0]
        },
        breaks: 0,
        insert: tree::leaf(Source::memory(replacement), 0, replacement.len() as u64)?,
        out: Vec::new(),
    };
    let mut edits = Edits {
        count: 0,
        few: Vec::new(),
    };
    let chunks = (range.end - range.start).div_ceil(CHUNK);
    let workers = parallel::threads().min(chunks as usize);
    let mut bytes = Vec::new();
    let dense = if workers < 2 || chunks < PARALLEL_MIN {
        let mut dense = false;
        for k in 0..chunks {
            search.check()?;
            let (left, right) = search.bounds(&range, k);
            if state.pos < right {
                let events = census(search, &starts, state.pos, (left, right), &mut bytes, |at, out| {
                    doc.read_into(at, out)
                })?;
                if state.apply(events, replacement, &mut edits, observe) {
                    dense = true;
                    break;
                }
            }
        }
        dense
    } else {
        let lead = doc.lead(range.start);
        parallel::ordered(
            chunks as usize,
            workers,
            || (parallel::Reader::led(doc, lead.as_ref()), Vec::new(), search.fork()),
            |k, (reader, bytes, search)| {
                search.check()?;
                let (left, right) = search.bounds(&range, k as u64);
                reader.ahead(left)?;
                census(search, &starts, left, (left, right), bytes, |at, out| {
                    reader.read_into(at, out)
                })
            },
            |k, mut events| {
                let (left, right) = search.bounds(&range, k as u64);
                if state.pos >= right {
                    return Ok(false);
                }
                if state.pos > left {
                    // The previous match ends inside this chunk, which its
                    // thread examined from the first byte. Do it again from there.
                    events = census(search, &starts, state.pos, (left, right), &mut bytes, |at, out| {
                        doc.read_into(at, out)
                    })?;
                }
                // Past the first few chunks, a count heading over the piece limit
                // switches to the patched text at once instead of at the limit.
                let dense = k >= 3 && edits.count as u64 * chunks / (k as u64 + 1) > MAX_PIECES as u64;
                Ok(dense || state.apply(events, replacement, &mut edits, observe))
            },
        )?
    };
    if dense {
        return stream(doc, search, replacement, state.pos, observe);
    }
    if state.from < state.pos {
        // A selection that ends inside a piece.
        let edge = doc.read(state.pos - 1, 2)?;
        state.out.push(state.part(edge[0], edge[0] == b'\r' && edge.get(1) == Some(&b'\n')));
    }
    if edits.count == edits.few.len() {
        return Ok((edits.count, Change::Sparse(edits.few)));
    }
    let mut root = tree::build(&state.out);
    if range.start != 0 || range.end != doc.len() {
        let (before, _) = tree::split(doc.root.clone(), range.start)?;
        let (_, after) = tree::split(doc.root.clone(), range.end)?;
        root = tree::join(tree::join(before, root), after);
    }
    Ok((edits.count, Change::Whole(root)))
}

/// Too many matches to hold as pieces. The pieces made so far are dropped and
/// the scope becomes one piece of a `Patched` source: the old text, where each
/// match starts, and the replacement. Threads find each chunk's matches; they
/// are taken in order. Matches before `reported` were passed to `observe` already.
fn stream(
    doc: &Document,
    search: &Search<'_>,
    replacement: &[u8],
    reported: u64,
    observe: &mut dyn FnMut(Range<u64>, u64),
) -> io::Result<(usize, Change)> {
    let range = search.scope.start..search.scope.end;
    let base = doc.root.clone().ok_or_else(|| io::Error::other("empty document"))?;
    let (mut stretches, mut count, mut cursor) = (Vec::new(), 0, range.start);
    let chunks = (range.end - range.start).div_ceil(CHUNK) as usize;
    let lead = doc.lead(range.start);
    let mut bytes = Vec::new();
    parallel::ordered(
        chunks,
        parallel::threads().min(chunks),
        || (parallel::Reader::led(doc, lead.as_ref()), Vec::new(), search.fork()),
        |k, (reader, bytes, search)| {
            search.check()?;
            let (left, right) = search.bounds(&range, k as u64);
            reader.ahead(left)?;
            let hits = hits_in(search, left, (left, right), bytes, |at, buf| reader.read_into(at, buf))?.0;
            Ok(stretch_of(left, right, &hits))
        },
        |k, mut stretch: Stretch| {
            let (left, right) = search.bounds(&range, k as u64);
            if cursor >= right {
                // A match that started earlier covers this whole chunk.
                return Ok(false);
            }
            if cursor > left {
                if stretch.hits.first().is_some_and(|&h| left + u64::from(h) < cursor) {
                    // Its thread began inside the previous match: search it again from that match's end.
                    let hits = hits_in(search, cursor, (left, right), &mut bytes, |at, buf| doc.read_into(at, buf))?.0;
                    stretch = stretch_of(cursor, right, &hits);
                } else {
                    // The stretch starts where the previous match ended.
                    let shift = (cursor - left) as u32;
                    stretch.hits.iter_mut().for_each(|h| *h -= shift);
                    stretch.old = cursor;
                    stretch.old_len -= u64::from(shift);
                }
            }
            for (j, &h) in stretch.hits.iter().enumerate() {
                let start = stretch.old + u64::from(h);
                if start >= reported {
                    let len = stretch.lens.as_ref().map_or(stretch.len, |l| l[j]);
                    observe(start..start + u64::from(len), replacement.len() as u64);
                }
            }
            count += stretch.hits.len();
            cursor = stretch.old + stretch.old_len;
            stretches.push(stretch);
            Ok(false)
        },
    )?;
    search.check()?;
    let text = Patched::new(base, replacement, stretches);
    let len = text.len();
    let mut root = tree::leaf(Source::patched(text)?, 0, len)?;
    if range.start != 0 || range.end != doc.len() {
        let (before, _) = tree::split(doc.root.clone(), range.start)?;
        let (_, after) = tree::split(doc.root.clone(), range.end)?;
        root = tree::join(tree::join(before, root), after);
    }
    Ok((count, Change::Whole(root)))
}

#[cfg(test)]
mod tests {
    use crate::{Document, SearchOptions};
    use std::io;

    /// Past `MAX_PIECES` matches the rest is written by threads chunk by chunk;
    /// the text is what one replace over the whole of it gives, and undo restores it.
    #[test]
    fn many_matches_are_written_in_order() -> io::Result<()> {
        let mut text = Vec::new();
        for n in 0..6_000_000u32 {
            text.extend_from_slice(if n % 1000 == 999 { b"abb\r\n" } else { b"ab\n" });
        }
        let mut d = Document::empty();
        d.replace(0, 0, &text)?;
        let cases = [
            ("b", "XYZ", SearchOptions { case_sensitive: true, wrap: false, ..Default::default() }),
            ("bb?", "", SearchOptions { regex: true, case_sensitive: true, wrap: false, ..Default::default() }),
        ];
        for (query, with, options) in cases {
            let re = regex::bytes::Regex::new(&if options.regex { query.to_string() } else { regex::escape(query) }).unwrap();
            let want = re.replace_all(&text, with.as_bytes()).into_owned();
            let mut seen = Vec::new();
            let all = 0..d.len();
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let count = d.replace_observed(query, with, options, all, &cancel, |hit, _| seen.push(hit))?;
            assert_eq!(count, re.find_iter(&text).count(), "{query}");
            // Each match is reported once, in order, including across the switch to the file.
            assert!(seen.iter().map(|h| h.start..h.end).eq(re.find_iter(&text).map(|m| m.start() as u64..m.end() as u64)), "{query}: reported");
            assert!(d.read(0, d.len() as usize)? == want, "{query}: text differs");
            assert_eq!(d.line_count(), 6_000_001, "{query}: lines");
            d.undo();
            assert!(d.read(0, d.len() as usize)? == text, "{query}: undo differs");
        }
        Ok(())
    }
}
