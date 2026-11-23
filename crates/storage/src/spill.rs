use crate::{invalid, source::Source, tree, Document};
use std::{
    io::{self, Write},
    sync::Arc,
};
impl Document {
    /// A physically contiguous range can be mapped directly by a native
    /// extension. The duplicate handle pins that version across atomic saves.
    pub fn file_range(
        &self,
        range: std::ops::Range<u64>,
    ) -> io::Result<Option<(std::fs::File, u64)>> {
        if range.end > self.len() || range.start > range.end {
            return Err(invalid("Mapping outside document"));
        }
        if range.is_empty() {
            return Ok(None);
        }
        let mut node = self.root.as_deref().expect("nonempty range");
        let (mut lo, mut hi) = (range.start, range.end);
        loop {
            match &node.body {
                tree::Body::Leaf(s, at) => {
                    let accepted = s.file_id().and_then(|id| self.accepted_appends.get(&id));
                    if !s.prepare_mapping(accepted)? {
                        return Ok(None);
                    }
                    s.validate(accepted)?;
                    return s.disk_at(at + lo);
                }
                tree::Body::Branch(left, _) if hi <= left.len => node = left,
                tree::Body::Branch(left, right) if lo >= left.len => {
                    lo -= left.len;
                    hi -= left.len;
                    node = right;
                }
                _ => return Ok(None),
            }
        }
    }
    /// Spill insertion pieces of a grouped edit to a private file. Keep
    /// original file ranges shared and preserve the group's original undo root.
    pub fn spill_inserted(&mut self) -> io::Result<()> {
        let Some((_, base)) = &self.group else {
            return Err(invalid("Spilling insertions requires an edit group"));
        };
        if tree::same(base, &self.root) {
            return Ok(());
        }
        let Some(root) = &self.root else {
            return Ok(());
        };
        let mut pieces = Vec::new();
        root.pieces(&mut |s, at, len| {
            pieces.push((s.clone(), at, len));
            Ok(())
        })?;
        if !pieces.iter().any(|(s, _, _)| s.memory_backed()) {
            return Ok(());
        }
        let mut block = vec![0u8; 128 * 1024];
        let mut file = std::io::BufWriter::new(tempfile::tempfile()?);
        let mut length = 0;
        for (source, start, len) in &pieces {
            if !source.memory_backed() {
                continue;
            }
            let mut i = 0;
            while i < *len {
                let n = (len - i).min(block.len() as u64) as usize;
                source.read(start + i, &mut block[..n])?;
                file.write_all(&block[..n])?;
                i += n as u64;
            }
            length += len;
        }
        let disk = Source::file(
            Arc::new(file.into_inner().map_err(|e| e.into_error())?),
            0,
            length,
        )?;
        let mut next = None;
        let mut offset = 0;
        let mut pending = 0;
        for (source, start, len) in pieces {
            if source.memory_backed() {
                pending += len;
                continue;
            }
            if pending > 0 {
                next = tree::join(next, tree::leaf(disk.clone(), offset, pending)?);
                offset += pending;
                pending = 0;
            }
            next = tree::join(next, tree::leaf(source, start, len)?);
        }
        if pending > 0 {
            next = tree::join(next, tree::leaf(disk, offset, pending)?);
        }
        self.root = next;
        Ok(())
    }
}
