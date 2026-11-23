//! Line-oriented comparison over immutable document views. Only small changed regions
//! are tokenized in memory. The ordered hunk index lives in a temporary file.
use imara_diff::{Algorithm, Diff, InternedInput};
use linework_storage::Document;
use std::{
    fs::File,
    io::{self, BufWriter, Read, Seek, SeekFrom, Write},
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
const BLOCK: usize = 128 * 1024;
const LEAF_BYTES: u64 = 2 * 1024 * 1024;
const LEAF_LINES: u64 = 24_000;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub sides: [Range<u64>; 2],
}
impl Hunk {
    pub fn kind(&self) -> &'static str {
        if self.sides[0].is_empty() {
            "Added"
        } else if self.sides[1].is_empty() {
            "Removed"
        } else {
            "Changed"
        }
    }
}
pub struct Comparison {
    pub docs: [Document; 2],
    pub names: [String; 2],
    pub count: u64,
    index: File,
}
fn check(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Comparison canceled",
        ))
    } else {
        Ok(())
    }
}
struct Index {
    out: BufWriter<File>,
    pending: Option<Hunk>,
    count: u64,
}
impl Index {
    fn flush_hunk(&mut self) -> io::Result<()> {
        if let Some(h) = self.pending.take() {
            for r in h.sides {
                self.out.write_all(&r.start.to_le_bytes())?;
                self.out.write_all(&r.end.to_le_bytes())?;
            }
            self.count += 1;
        }
        Ok(())
    }
    fn push(&mut self, left: Range<u64>, right: Range<u64>) -> io::Result<()> {
        if left.is_empty() && right.is_empty() {
            return Ok(());
        }
        if let Some(previous) = self.pending.as_mut()
            && previous.sides[0].end == left.start
            && previous.sides[1].end == right.start
        {
            previous.sides[0].end = left.end;
            previous.sides[1].end = right.end;
            return Ok(());
        }
        self.flush_hunk()?;
        self.pending = Some(Hunk {
            sides: [left, right],
        });
        Ok(())
    }
}
fn line_end(doc: &Document) -> io::Result<u64> {
    if doc.len() == 0 {
        return Ok(0);
    }
    let last = doc.read(doc.len() - 1, 1)?[0];
    Ok(doc.line_count() - u64::from(matches!(last, b'\r' | b'\n')))
}
pub fn run(docs: [Document; 2], names: [String; 2], cancel: &AtomicBool) -> io::Result<Comparison> {
    for d in &docs {
        while d.indexed_bytes() < d.len() {
            check(cancel)?;
            if let Some(error) = d.index_error() {
                return Err(io::Error::other(error));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Some(error) = d.index_error() {
            return Err(io::Error::other(error));
        }
    }
    let mut index = Index {
        out: BufWriter::new(tempfile::tempfile()?),
        pending: None,
        count: 0,
    };
    let mut regions = vec![[0..line_end(&docs[0])?, 0..line_end(&docs[1])?]];
    while let Some(mut lines) = regions.pop() {
        check(cancel)?;
        trim(&docs, &mut lines, false, cancel)?;
        trim(&docs, &mut lines, true, cancel)?;
        let bytes = [
            byte_range(&docs[0], &lines[0]),
            byte_range(&docs[1], &lines[1]),
        ];
        if lines.iter().any(Range::is_empty) {
            index.push(lines[0].clone(), lines[1].clone())?;
        } else if bytes.iter().all(|r| r.end - r.start <= LEAF_BYTES)
            && lines.iter().all(|r| r.end - r.start <= LEAF_LINES)
        {
            let a = docs[0].read(bytes[0].start, (bytes[0].end - bytes[0].start) as usize)?;
            let b = docs[1].read(bytes[1].start, (bytes[1].end - bytes[1].start) as usize)?;
            let mut input = InternedInput::new(&b""[..], &b""[..]);
            input.update_before(records(&a));
            input.update_after(records(&b));
            for h in Diff::compute(Algorithm::Histogram, &input).hunks() {
                index.push(
                    lines[0].start + u64::from(h.before.start)
                        ..lines[0].start + u64::from(h.before.end),
                    lines[1].start + u64::from(h.after.start)
                        ..lines[1].start + u64::from(h.after.end),
                )?;
            }
        } else if let Some([a, b, width]) = anchor(&docs, &lines, &bytes, cancel)? {
            regions.push([a + width..lines[0].end, b + width..lines[1].end]);
            regions.push([lines[0].start..a, lines[1].start..b]);
        } else if lines.iter().all(|r| r.end - r.start > 1) {
            // Subdivide highly dissimilar regions without building a whole-file token table.
            // Coalescing joins adjacent edits on both sides of this arbitrary split.
            let a = lines[0].start + (lines[0].end - lines[0].start) / 2;
            let b = lines[1].start + (lines[1].end - lines[1].start) / 2;
            regions.push([a..lines[0].end, b..lines[1].end]);
            regions.push([lines[0].start..a, lines[1].start..b]);
        } else {
            index.push(lines[0].clone(), lines[1].clone())?;
        }
    }
    index.flush_hunk()?;
    Ok(Comparison {
        docs,
        names,
        count: index.count,
        index: index.out.into_inner()?,
    })
}
fn records(mut data: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        if data.is_empty() {
            return None;
        }
        let end = memchr::memchr2(b'\r', b'\n', data)
            .map(|i| i + 1 + usize::from(data[i] == b'\r' && data.get(i + 1) == Some(&b'\n')))
            .unwrap_or(data.len());
        let (head, tail) = data.split_at(end);
        data = tail;
        Some(head)
    })
}
pub fn byte_range(doc: &Document, lines: &Range<u64>) -> Range<u64> {
    doc.line_start(lines.start)..doc.line_start(lines.end)
}
// Preserve complete records when skipping equal bytes, including a shared partial line.
fn trim(
    docs: &[Document; 2],
    lines: &mut [Range<u64>; 2],
    back: bool,
    cancel: &AtomicBool,
) -> io::Result<()> {
    if lines.iter().any(Range::is_empty) {
        return Ok(());
    }
    let rr = [
        byte_range(&docs[0], &lines[0]),
        byte_range(&docs[1], &lines[1]),
    ];
    let limit = rr.iter().map(|r| r.end - r.start).min().unwrap();
    let mut equal = 0;
    let mut a = vec![0; BLOCK];
    let mut b = vec![0; BLOCK];
    while equal < limit {
        check(cancel)?;
        let n = (limit - equal).min(BLOCK as u64) as usize;
        let pos = |r: &Range<u64>| {
            if back {
                r.end - equal - n as u64
            } else {
                r.start + equal
            }
        };
        docs[0].read_into(pos(&rr[0]), &mut a[..n])?;
        docs[1].read_into(pos(&rr[1]), &mut b[..n])?;
        if a[..n] != b[..n] {
            let same = if back {
                a[..n]
                    .iter()
                    .rev()
                    .zip(b[..n].iter().rev())
                    .take_while(|(x, y)| x == y)
                    .count()
            } else {
                a[..n]
                    .iter()
                    .zip(&b[..n])
                    .take_while(|(x, y)| x == y)
                    .count()
            };
            equal += same as u64;
            break;
        }
        equal += n as u64;
    }
    let complete = |i: usize| {
        let doc = &docs[i];
        let r = &rr[i];
        let l = &lines[i];
        if equal == r.end - r.start {
            return l.end - l.start;
        }
        if back {
            let at = r.end - equal;
            let n = doc.line_of(at);
            let next = n + u64::from(doc.line_start(n) < at);
            l.end.saturating_sub(next)
        } else {
            doc.line_of(r.start + equal).saturating_sub(l.start)
        }
    };
    let n = complete(0).min(complete(1));
    for l in lines {
        if back {
            l.end -= n;
        } else {
            l.start += n;
        }
    }
    Ok(())
}
fn anchor(
    docs: &[Document; 2],
    lines: &[Range<u64>; 2],
    bytes: &[Range<u64>; 2],
    cancel: &AtomicBool,
) -> io::Result<Option<[u64; 3]>> {
    let i = usize::from(lines[1].end - lines[1].start < lines[0].end - lines[0].start);
    let j = 1 - i;
    let span = lines[i].end - lines[i].start;
    if span < 3 {
        return Ok(None);
    }
    for numerator in [2, 1, 3] {
        let n = lines[i].start + span * numerator / 4;
        let width = 3.min(lines[i].end - n);
        let r = byte_range(&docs[i], &(n..n + width));
        if !(4..=8192).contains(&(r.end - r.start)) {
            continue;
        }
        let key = docs[i].read(r.start, (r.end - r.start) as usize)?;
        let finder = memchr::memmem::Finder::new(&key);
        let mut at = bytes[j].start;
        let mut best = None;
        // Repeated context is common in logs. Prefer the matching line nearest
        // the proportional position; this also keeps recursive splits balanced.
        let target = lines[j].start + (lines[j].end - lines[j].start) * numerator / 4;
        // Sparse edits in long logs usually shift this anchor by only a few
        // records. Check the nearest lines before scanning repeated text.
        for distance in 0..=16 {
            for m in [target.saturating_sub(distance),target.saturating_add(distance)] {
                if m < lines[j].start || m+width>lines[j].end {continue;}
                let other=byte_range(&docs[j],&(m..m+width));
                if other.end-other.start==key.len() as u64 && docs[j].read(other.start,key.len())?==key {
                    return Ok(Some(if i==0 {[n,m,width]} else {[m,n,width]}));
                }
            }
        }
        while at < bytes[j].end {
            check(cancel)?;
            let stride = (bytes[j].end - at).min(BLOCK as u64);
            let end = (at + stride + key.len() as u64 - 1).min(bytes[j].end);
            let block = docs[j].read(at, (end - at) as usize)?;
            for off in finder.find_iter(&block) {
                if off as u64 >= stride {
                    break;
                }
                let pos = at + off as u64;
                let line = docs[j].line_of(pos);
                if docs[j].line_start(line) == pos
                    && line + width <= lines[j].end
                    && docs[j].line_start(line + width) == pos + key.len() as u64
                    && best.is_none_or(|prev: u64| line.abs_diff(target) < prev.abs_diff(target))
                {
                    best = Some(line);
                    if line >= target {
                        break;
                    }
                }
            }
            if best.is_some_and(|x| x >= target) {
                break;
            }
            at += stride;
        }
        if let Some(m) = best {
            return Ok(Some(if i == 0 { [n, m, width] } else { [m, n, width] }));
        }
    }
    Ok(None)
}
impl Comparison {
    pub fn hunk(&mut self, index: u64) -> io::Result<Hunk> {
        if index >= self.count {
            return Err(io::Error::other("Invalid change number"));
        }
        self.index.seek(SeekFrom::Start(index * 32))?;
        let mut raw = [0; 32];
        self.index.read_exact(&mut raw)?;
        let value = |i: usize| u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().unwrap());
        Ok(Hunk {
            sides: [value(0)..value(1), value(2)..value(3)],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn doc(text: &[u8]) -> Document {
        let mut d = Document::empty();
        d.replace(0, 0, text).unwrap();
        d
    }
    fn compare(a: &[u8], b: &[u8]) -> Comparison {
        run(
            [doc(a), doc(b)],
            ["a".into(), "b".into()],
            &AtomicBool::new(false),
        )
        .unwrap()
    }
    fn verify(a: &[u8], b: &[u8], expected: Option<&[[Range<u64>; 2]]>) {
        let mut result = compare(a, b);
        let mut final_bytes = Vec::new();
        let mut previous = 0;
        if let Some(e) = expected {
            assert_eq!(result.count, e.len() as u64);
        }
        for i in 0..result.count {
            let h = result.hunk(i).unwrap();
            if let Some(e) = expected {
                assert_eq!(&h.sides, &e[i as usize]);
            }
            let left = byte_range(&result.docs[0], &h.sides[0]);
            let right = byte_range(&result.docs[1], &h.sides[1]);
            final_bytes.extend_from_slice(&a[previous..left.start as usize]);
            final_bytes.extend_from_slice(&b[right.start as usize..right.end as usize]);
            previous = left.end as usize;
        }
        final_bytes.extend_from_slice(&a[previous..]);
        assert_eq!(final_bytes, b);
    }
    #[test]
    fn separates_insert_remove_replace_and_preserves_undo_target() {
        verify(
            b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven",
            b"one\r\ninserted\r\ntwo\r\nthree\r\nfive\r\nSIX\r\nseven",
            Some(&[[1..1, 1..2], [3..4, 4..4], [5..6, 5..6]]),
        );
        verify(b"one\rtwo\rthree", b"one\rthree", Some(&[[1..2, 1..1]]));
        verify(b"one\n", b"one", Some(&[[0..1, 0..1]]));
        verify(b"", b"\n", Some(&[[0..0, 0..1]]));
        verify(b"same", b"same", Some(&[]));
    }
    #[test]
    fn patch_reconstructs_mixed_repeated_lines_and_line_endings() {
        let mut rng = 2389371_u32;
        let mut next = || {
            rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
            rng as usize
        };
        for _ in 0..200 {
            let mut make = || {
                let mut bytes = Vec::new();
                for _ in 0..next() % 33 {
                    bytes.extend_from_slice(
                        [
                            &b"beta"[..],
                            b"",
                            b"ab",
                            '海'.to_string().as_bytes(),
                            b"abc",
                        ][next() % 5],
                    );
                    bytes.extend_from_slice([&b"\n"[..], b"\r\n", b"\r", b""][next() % 4]);
                }
                bytes
            };
            let a = make();
            let b = make();
            verify(&a, &b, None);
        }
    }
    #[test]
    fn large_file_alignment_skips_shared_bytes_and_finds_distant_changes() {
        let mut a = Vec::new();
        let mut b = Vec::new();
        for i in 0..90_000 {
            let row = format!("dock {i:05} | weekly counts and notes | north station\n");
            a.extend_from_slice(row.as_bytes());
            if i == 23 {
                b.extend_from_slice(b"new berth\n");
            }
            if i != 42_100 {
                b.extend_from_slice(row.as_bytes());
            }
            if i == 89_972 {
                b.extend_from_slice(b"last berth\n");
            }
        }
        verify(
            &a,
            &b,
            Some(&[
                [23..23, 23..24],
                [42_100..42_101, 42_101..42_101],
                [89_973..89_973, 89_973..89_974],
            ]),
        );
    }
    #[test]
    fn cancelled_comparison_does_not_publish_partial_hunks() {
        let result = run(
            [doc(b"one"), doc(b"two")],
            ["a".into(), "b".into()],
            &AtomicBool::new(true),
        );
        assert!(matches!(result,Err(e) if e.kind()==io::ErrorKind::Interrupted));
    }
}
