use crate::invalid;
use std::io;

#[derive(Clone, Copy)]
struct Piece {
    source: Option<u64>,
    len: u64,
}
impl Piece {
    fn tail(self, skip: u64) -> Self {
        Self {
            source: self.source.map(|p| p + skip),
            len: self.len - skip,
        }
    }
}

/// Translates edits made against successive document versions into disjoint
/// original ranges. The gap follows the last edit, so adjacent edits in either
/// direction only visit affected pieces. No document bytes are stored here.
pub struct ChangeMap {
    original: u64,
    len: u64,
    position: u64,
    before: Vec<Piece>,
    after: Vec<Piece>,
}
impl ChangeMap {
    pub fn new(original_len: u64) -> Self {
        let after = if original_len == 0 {
            Vec::new()
        } else {
            vec![Piece {
                source: Some(0),
                len: original_len,
            }]
        };
        Self {
            original: original_len,
            len: original_len,
            position: 0,
            before: Vec::new(),
            after,
        }
    }
    fn push_before(&mut self, p: Piece) {
        if let Some(last) = self.before.last_mut() {
            if last.source.map(|at| at + last.len) == p.source {
                last.len += p.len;
                return;
            }
        }
        self.before.push(p);
    }
    fn seek(&mut self, at: u64) {
        while self.position < at {
            let p = self.after.pop().expect("gap within document");
            let used = p.len.min(at - self.position);
            self.push_before(Piece { len: used, ..p });
            if used < p.len {
                self.after.push(p.tail(used));
            }
            self.position += used;
        }
        while self.position > at {
            let p = self.before.pop().expect("gap within document");
            let used = p.len.min(self.position - at);
            self.after.push(p.tail(p.len - used));
            if used < p.len {
                self.before.push(Piece {
                    len: p.len - used,
                    ..p
                });
            }
            self.position -= used;
        }
    }
    pub fn replace(&mut self, at: u64, deleted: u64, inserted: u64) -> io::Result<()> {
        if at > self.len || deleted > self.len - at {
            return Err(invalid("Edit outside document"));
        }
        let len = (self.len - deleted)
            .checked_add(inserted)
            .ok_or_else(|| invalid("Edit exceeds address space"))?;
        if deleted == 0 && inserted == 0 {
            return Ok(());
        }
        self.seek(at);
        let mut left = deleted;
        while left > 0 {
            let p = self.after.pop().expect("validated edit interval");
            if p.len > left {
                self.after.push(p.tail(left));
            }
            left = left.saturating_sub(p.len);
        }
        if inserted > 0 {
            self.push_before(Piece {
                source: None,
                len: inserted,
            });
        }
        self.position = at + inserted;
        self.len = len;
        Ok(())
    }
    pub fn changes(&self) -> Vec<(u64, u64, u64)> {
        let mut rows = Vec::new();
        let (mut old, mut added) = (0, 0);
        for p in self.before.iter().chain(self.after.iter().rev()) {
            if let Some(start) = p.source {
                if start > old || added > 0 {
                    rows.push((old, start - old, added));
                }
                old = start + p.len;
                added = 0;
            } else {
                added += p.len;
            }
        }
        if old < self.original || added > 0 {
            rows.push((old, self.original - old, added));
        }
        rows
    }
}
