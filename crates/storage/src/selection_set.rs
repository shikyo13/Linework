use crate::{invalid, read_at, Document};
use std::{
    fs::File,
    io::{self, BufWriter, Write},
    ops::Range,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

/// Immutable, ordered, non-overlapping file offsets. Dense sets spill to a
/// temporary file and are visited in blocks; projection needs only a binary lookup.
#[derive(Clone, Debug)]
pub struct SelectionSet {
    backing: Arc<Backing>,
    count: u64,
}
#[derive(Debug)]
enum Backing {
    Memory(Vec<Range<u64>>),
    Disk(File),
}
#[derive(Default)]
pub struct SelectionBuilder {
    small: Vec<Range<u64>>,
    disk: Option<BufWriter<File>>,
    count: u64,
    last: Option<Range<u64>>,
}
impl SelectionBuilder {
    pub fn push(&mut self, r: Range<u64>) -> io::Result<()> {
        if self.last.as_ref() == Some(&r) {
            return Ok(());
        }
        if r.end < r.start || self.last.as_ref().is_some_and(|p| p.end > r.start) {
            return Err(invalid("Overlapping selection ranges"));
        }
        if self.small.len() == 4096 {
            let mut out = BufWriter::with_capacity(131072, tempfile::tempfile()?);
            for p in self.small.drain(..) {
                write_record(&mut out, &p)?;
            }
            self.disk = Some(out);
        }
        if let Some(out) = &mut self.disk {
            write_record(out, &r)?;
        } else {
            self.small.push(r.clone());
        }
        self.last = Some(r);
        self.count += 1;
        Ok(())
    }
    pub fn finish(self) -> io::Result<SelectionSet> {
        let backing = match self.disk {
            Some(out) => Backing::Disk(out.into_inner().map_err(|e| e.into_error())?),
            None => Backing::Memory(self.small),
        };
        Ok(SelectionSet {
            backing: Arc::new(backing),
            count: self.count,
        })
    }
}
fn write_record(out: &mut impl Write, r: &Range<u64>) -> io::Result<()> {
    out.write_all(&r.start.to_le_bytes())?;
    out.write_all(&r.end.to_le_bytes())
}
fn decode(b: &[u8]) -> Range<u64> {
    u64::from_le_bytes(b[..8].try_into().unwrap())..u64::from_le_bytes(b[8..16].try_into().unwrap())
}
impl SelectionSet {
    pub fn from_ranges(mut rs: Vec<Range<u64>>) -> io::Result<Self> {
        rs.sort_unstable_by_key(|r| (r.start, r.end));
        let mut out = SelectionBuilder::default();
        for r in rs {
            out.push(r)?;
        }
        out.finish()
    }
    pub fn len(&self) -> u64 {
        self.count
    }
    pub fn at(&self, i: u64) -> io::Result<Range<u64>> {
        if i >= self.count {
            return Err(invalid("Unknown selection"));
        }
        match &*self.backing {
            Backing::Memory(v) => Ok(v[i as usize].clone()),
            Backing::Disk(f) => {
                let mut b = [0; 16];
                read_at(f, i * 16, &mut b)?;
                Ok(decode(&b))
            }
        }
    }
    pub fn lower_bound(&self, at: u64) -> io::Result<u64> {
        let (mut lo, mut hi) = (0, self.count);
        while lo < hi {
            let m = lo + (hi - lo) / 2;
            if self.at(m)?.end < at {
                lo = m + 1
            } else {
                hi = m
            }
        }
        Ok(lo)
    }
    pub fn visit(
        &self,
        mut i: u64,
        mut f: impl FnMut(Range<u64>) -> io::Result<bool>,
    ) -> io::Result<()> {
        match &*self.backing {
            Backing::Memory(v) => {
                for r in &v[(i.min(self.count) as usize)..] {
                    if !f(r.clone())? {
                        break;
                    }
                }
            }
            Backing::Disk(file) => {
                let mut block = vec![0u8; 65536];
                while i < self.count {
                    let n = ((self.count - i).min((block.len() / 16) as u64) as usize) * 16;
                    read_at(file, i * 16, &mut block[..n])?;
                    for record in block[..n].chunks_exact(16) {
                        if !f(decode(record))? {
                            return Ok(());
                        }
                    }
                    i += (n / 16) as u64;
                }
            }
        }
        Ok(())
    }
    pub fn with(&self, add: Range<u64>, remove: Option<Range<u64>>) -> io::Result<Self> {
        let mut out = SelectionBuilder::default();
        let mut added = false;
        self.visit(0, |r| {
            if !added && add.start <= r.start {
                out.push(add.clone())?;
                added = true;
            }
            if Some(&r) != remove.as_ref() {
                out.push(r)?;
            }
            Ok(true)
        })?;
        if !added {
            out.push(add)?;
        }
        out.finish()
    }
    pub fn split_lines(doc: &Document, span: Range<u64>, cancel: &AtomicBool) -> io::Result<Self> {
        let mut out = SelectionBuilder::default();
        let mut at = span.start;
        let mut line = at;
        let mut cr = false;
        while at < span.end {
            if cancel.load(Ordering::Relaxed) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let bytes = doc.read(at, (span.end - at).min(1024 * 1024) as usize)?;
            for i in memchr::memchr2_iter(b'\r', b'\n', &bytes) {
                let pos = at + i as u64;
                if !(bytes[i] == b'\n' && cr && line == pos) {
                    out.push(line..pos)?;
                }
                line = pos + 1;
                cr = bytes[i] == b'\r';
            }
            at += bytes.len() as u64;
        }
        if line < span.end || span.is_empty() {
            out.push(line..span.end)?;
        }
        out.finish()
    }
}
