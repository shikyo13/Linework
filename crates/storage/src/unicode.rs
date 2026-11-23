//! Sparse UTF-8 scalar offsets for Python views. Only requested source files
//! incur a Unicode scan. Persistent piece-tree snapshots share the result.
use crate::{
    invalid,
    source::Source,
    tree::{Body, Node},
    Document,
};
use std::io;

const BLOCK: usize = 256 * 1024;

#[derive(Clone, Copy)]
struct Checkpoint {
    byte: u64,
    chars: u64,
}
pub(crate) struct UnicodeIndex {
    points: Vec<Checkpoint>,
}
impl UnicodeIndex {
    pub(crate) fn build(
        source: &Source,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> io::Result<Self> {
        let mut buf = vec![0u8; source.len.min(BLOCK as u64) as usize];
        let mut at = 0;
        let mut chars = 0;
        let mut points = vec![Checkpoint { byte: 0, chars: 0 }];
        while at < source.len {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let n = (source.len - at).min(BLOCK as u64) as usize;
            source.read(at, &mut buf[..n])?;
            let block = match std::str::from_utf8(&buf[..n]) {
                Ok(s) => s,
                Err(err) if err.error_len().is_none() && at + (n as u64) < source.len => {
                    std::str::from_utf8(&buf[..err.valid_up_to()]).expect("validated prefix")
                }
                Err(err) => {
                    return Err(invalid(format!(
                        "Invalid UTF-8 at source byte {}. Reopen with the file's encoding.",
                        at + err.valid_up_to() as u64
                    )))
                }
            };
            at += block.len() as u64;
            chars += block.chars().count() as u64;
            points.push(Checkpoint { byte: at, chars });
        }
        Ok(Self { points })
    }
    fn adjacent(
        &self,
        value: u64,
        by: impl Fn(Checkpoint) -> u64,
    ) -> (Checkpoint, Option<Checkpoint>) {
        let i = self.points.partition_point(|p| by(*p) <= value) - 1;
        (self.points[i], self.points.get(i + 1).copied())
    }
    pub(crate) fn rank(&self, source: &Source, byte: u64) -> io::Result<u64> {
        let (p, next) = self.adjacent(byte, |p| p.byte);
        if byte == p.byte {
            return Ok(p.chars);
        }
        let end = next.ok_or_else(|| invalid("Byte position outside source"))?;
        if end.byte - p.byte == end.chars - p.chars {
            return Ok(p.chars + byte - p.byte);
        }
        let mut buf = vec![0; (byte - p.byte) as usize];
        source.read(p.byte, &mut buf)?;
        let s = std::str::from_utf8(&buf)
            .map_err(|_| invalid("Byte offset is inside a UTF-8 character"))?;
        Ok(p.chars + s.chars().count() as u64)
    }
    pub(crate) fn select(&self, source: &Source, character: u64) -> io::Result<u64> {
        let (p, next) = self.adjacent(character, |p| p.chars);
        if character == p.chars {
            return Ok(p.byte);
        }
        let end = next.ok_or_else(|| invalid("Character position outside source"))?;
        let skip = character - p.chars;
        if end.byte - p.byte == end.chars - p.chars {
            return Ok(p.byte + skip);
        }
        let mut buf = vec![0; (end.byte - p.byte) as usize];
        source.read(p.byte, &mut buf)?;
        let s = std::str::from_utf8(&buf)
            .map_err(|_| invalid("Source text changed since Unicode indexing"))?;
        let pos = s
            .char_indices()
            .nth(skip as usize)
            .ok_or_else(|| invalid("Character outside indexed block"))?
            .0;
        Ok(p.byte + pos as u64)
    }
}
impl Node {
    fn char_rank(&self, bytes: u64, cancel: &std::sync::atomic::AtomicBool) -> io::Result<u64> {
        if bytes == 0 {
            return Ok(0);
        }
        if bytes == self.len {
            if let Some(&count) = self.char_count.get() {
                return Ok(count);
            }
        }
        let n = match &self.body {
            Body::Leaf(s, at) => {
                let idx = s.unicode_cancel(cancel)?;
                idx.rank(s, at + bytes)?
                    .checked_sub(idx.rank(s, *at)?)
                    .ok_or_else(|| invalid("Source character index changed"))?
            }
            Body::Branch(left, _) if bytes <= left.len => left.char_rank(bytes, cancel)?,
            Body::Branch(left, right) => {
                left.char_rank(left.len, cancel)? + right.char_rank(bytes - left.len, cancel)?
            }
        };
        if bytes == self.len {
            let _ = self.char_count.set(n);
        }
        Ok(n)
    }
    fn char_select(&self, ch: u64) -> io::Result<u64> {
        if ch == 0 {
            return Ok(0);
        }
        match &self.body {
            Body::Leaf(s, at) => {
                let idx = s.unicode()?;
                let target = idx
                    .rank(s, *at)?
                    .checked_add(ch)
                    .ok_or_else(|| invalid("Character position outside document"))?;
                let pos = idx.select(s, target)? - *at;
                if pos > self.len {
                    return Err(invalid("Character position outside piece"));
                }
                Ok(pos)
            }
            Body::Branch(left, right) => {
                let count = left.char_rank(left.len, &std::sync::atomic::AtomicBool::new(false))?;
                if ch <= count {
                    left.char_select(ch)
                } else {
                    Ok(left.len + right.char_select(ch - count)?)
                }
            }
        }
    }
}
impl Document {
    /// Counts Unicode scalar values. The first call touching a disk source
    /// builds a shared sparse index and should run on a background thread.
    pub fn char_len(&self) -> io::Result<u64> {
        self.byte_to_char(self.len())
    }
    pub fn char_len_cancel(&self, cancel: &std::sync::atomic::AtomicBool) -> io::Result<u64> {
        self.root
            .as_ref()
            .map_or(Ok(0), |n| n.char_rank(n.len, cancel))
    }
    pub fn byte_to_char(&self, byte: u64) -> io::Result<u64> {
        if byte > self.len() {
            return Err(invalid("Byte position outside document"));
        }
        self.root.as_ref().map_or(Ok(0), |n| {
            n.char_rank(byte, &std::sync::atomic::AtomicBool::new(false))
        })
    }
    pub fn char_to_byte(&self, character: u64) -> io::Result<u64> {
        match &self.root {
            Some(n) => n.char_select(character),
            None if character == 0 => Ok(0),
            None => Err(invalid("Character position outside document")),
        }
    }
}
