use super::{invalid, Document, Groups, Search};
use crate::{
    source::Source,
    tree::{self, Root},
};
use regex_cursor::regex_automata::{util::interpolate, PatternID};
use std::{
    borrow::Cow,
    fs::File,
    io::{self, BufWriter, Write},
    ops::Range,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

const BLOCK: usize = 256 * 1024;
const SPARSE_MATCHES: usize = 1024;
const SPARSE_BYTES: u64 = 2 * 1024 * 1024;

enum Part<'a> {
    Text(Cow<'a, [u8]>),
    Group(usize),
}
pub(super) struct Template<'a> {
    parts: Vec<Part<'a>>,
    captures: bool,
}
impl<'a> Template<'a> {
    pub fn new(search: &Search<'_>, value: &'a str) -> Self {
        if !search.opt.regex || !value.as_bytes().contains(&b'$') {
            return Self {
                parts: vec![Part::Text(Cow::Borrowed(value.as_bytes()))],
                captures: false,
            };
        }
        // Compile literal fragments and capture references using Rust regex's
        // replacement syntax once for the entire operation.
        let mut parts = Vec::new();
        let mut literal = Vec::new();
        let mut captures = false;
        interpolate::bytes(
            value.as_bytes(),
            |index, out| {
                if !out.is_empty() {
                    parts.push(Part::Text(Cow::Owned(std::mem::take(out))));
                }
                captures = true;
                parts.push(Part::Group(index));
            },
            |name| {
                if let Some(re) = &search.streamed {
                    return re.group_info().to_index(PatternID::ZERO, name);
                }
                search
                    .pattern
                    .as_ref()?
                    .capture_names()
                    .position(|n| n == Some(name))
            },
            &mut literal,
        );
        if !literal.is_empty() {
            parts.push(Part::Text(Cow::Owned(literal)));
        }
        Self { parts, captures }
    }
    pub fn bytes(value: &'a [u8]) -> Self {
        Self {
            parts: vec![Part::Text(Cow::Borrowed(value))],
            captures: false,
        }
    }
    pub fn captures(&self) -> bool {
        self.captures
    }
    pub fn length(&self, groups: Option<&Groups<'_>>) -> io::Result<u64> {
        self.parts.iter().try_fold(0u64, |n, p| {
            let len = match p {
                Part::Text(s) => s.len() as u64,
                Part::Group(i) => groups.and_then(|f| f(*i)).map_or(0, |r| r.end - r.start),
            };
            n.checked_add(len)
                .ok_or_else(|| invalid("Replacement exceeds addressable file size"))
        })
    }
    pub fn write(
        &self,
        groups: Option<&Groups<'_>>,
        reader: &mut Reader<'_>,
        out: &mut impl Write,
    ) -> io::Result<()> {
        for p in &self.parts {
            match p {
                Part::Text(bytes) => {
                    for block in bytes.chunks(BLOCK) {
                        reader.check()?;
                        out.write_all(block)?;
                    }
                }
                Part::Group(i) => {
                    if let Some(span) = groups.and_then(|f| f(*i)) {
                        reader.copy(span, out)?;
                    }
                }
            }
        }
        Ok(())
    }
}
/// Adjacent matches usually reuse the same source block for their unchanged
/// gaps and captures, including captures referenced out of order.
pub(super) struct Reader<'a> {
    doc: &'a Document,
    cancel: &'a AtomicBool,
    at: u64,
    cache: Vec<u8>,
}
impl<'a> Reader<'a> {
    pub fn new(doc: &'a Document, cancel: &'a AtomicBool) -> Self {
        Self {
            doc,
            cancel,
            at: 0,
            cache: Vec::new(),
        }
    }
    fn check(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Replacement cancelled",
            ));
        }
        Ok(())
    }
    fn copy(&mut self, span: Range<u64>, out: &mut impl Write) -> io::Result<()> {
        let mut pos = span.start;
        while pos < span.end {
            self.check()?;
            if pos < self.at || pos - self.at >= self.cache.len() as u64 {
                self.at = pos / BLOCK as u64 * BLOCK as u64;
                self.cache
                    .resize((self.doc.len() - self.at).min(BLOCK as u64) as usize, 0);
                self.doc.read_into(self.at, &mut self.cache)?;
            }
            let off = (pos - self.at) as usize;
            let n = (span.end - pos).min((self.cache.len() - off) as u64) as usize;
            out.write_all(&self.cache[off..off + n])?;
            pos += n as u64;
        }
        Ok(())
    }
}
type Edit = (u64, u64, Vec<u8>);
pub(super) enum Change {
    Sparse(Vec<Edit>),
    File(Root),
}
pub(super) struct Batch<'a> {
    reader: Reader<'a>,
    small: Vec<Edit>,
    buffered: u64,
    writer: Option<BufWriter<File>>,
    end: u64,
    count: usize,
}
impl<'a> Batch<'a> {
    pub fn new(doc: &'a Document, cancel: &'a AtomicBool) -> Self {
        Self {
            reader: Reader::new(doc, cancel),
            small: Vec::new(),
            buffered: 0,
            writer: None,
            end: 0,
            count: 0,
        }
    }
    pub fn end(&self) -> u64 {
        self.end
    }
    fn spill(&mut self) -> io::Result<()> {
        let mut out = BufWriter::with_capacity(BLOCK, tempfile::tempfile()?);
        let mut at = 0;
        for (start, delete, bytes) in self.small.drain(..) {
            self.reader.copy(at..start, &mut out)?;
            out.write_all(&bytes)?;
            at = start + delete;
        }
        self.writer = Some(out);
        Ok(())
    }
    pub fn push(
        &mut self,
        hit: Range<u64>,
        plan: &Template<'_>,
        groups: Option<&Groups<'_>>,
    ) -> io::Result<()> {
        self.reader.check()?;
        if self.writer.is_none() {
            let size = plan.length(groups)?;
            if self.small.len() == SPARSE_MATCHES || size > SPARSE_BYTES - self.buffered {
                self.spill()?;
            } else {
                let mut text = Vec::with_capacity(size as usize);
                plan.write(groups, &mut self.reader, &mut text)?;
                self.buffered += size;
                self.small.push((hit.start, hit.end - hit.start, text));
            }
        }
        if let Some(out) = self.writer.as_mut() {
            self.reader.copy(self.end..hit.start, out)?;
            plan.write(groups, &mut self.reader, out)?;
        }
        self.end = hit.end;
        self.count += 1;
        Ok(())
    }
    pub fn finish(mut self) -> io::Result<(usize, Change)> {
        let Some(mut out) = self.writer else {
            return Ok((self.count, Change::Sparse(self.small)));
        };
        self.reader
            .copy(self.end..self.reader.doc.len(), &mut out)?;
        let file = out.into_inner().map_err(|e| e.into_error())?;
        self.reader.check()?;
        let n = file.metadata()?.len();
        let root = tree::leaf(Source::file(Arc::new(file), 0, n)?, 0, n)?;
        Ok((self.count, Change::File(root)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancelling_a_spilled_batch_discards_the_entire_candidate() -> io::Result<()> {
        let mut d = Document::empty();
        d.replace(0, 0, &vec![b'a'; 2 * SPARSE_MATCHES])?;
        let version = d.version();
        let cancel = AtomicBool::new(false);
        let mut batch = Batch::new(&d, &cancel);
        let template = Template {
            parts: vec![Part::Text(Cow::Borrowed(b"b"))],
            captures: false,
        };
        for n in 0..=SPARSE_MATCHES as u64 {
            batch.push(n..n + 1, &template, None)?;
        }
        assert!(batch.writer.is_some());
        cancel.store(true, Ordering::Relaxed);
        assert!(matches!(batch.finish(),Err(e) if e.kind()==io::ErrorKind::Interrupted));
        assert_eq!(d.version(), version);
        assert_eq!(d.read(0, 1)?, b"a");
        Ok(())
    }
}
