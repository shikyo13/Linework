//! Random-access recovery storage. The index has one entry per logical MiB.
//! Equal blocks share a payload, and incompressible payloads are stored raw.
use crate::{invalid, source::read_at, tree};
use flate2::{bufread::DeflateDecoder, write::DeflateEncoder, Compression};
use crate::hash;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

pub(super) const BLOCK: usize = 1024 * 1024;
const MAGIC: &[u8; 4] = b"LWB3";
const HEADER: u64 = 12;
const ENTRY: u64 = 12;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Entry {
    offset: u64,
    stored: u32,
}
fn data_start(len: u64) -> u64 {
    HEADER + ENTRY * len.div_ceil(BLOCK as u64)
}

pub(super) fn write(
    file: &mut File,
    root: &tree::Root,
    cancel: &AtomicBool,
) -> io::Result<[u8; 32]> {
    let len = root.as_ref().map_or(0, |r| r.len);
    file.seek(SeekFrom::Start(data_start(len)))?;
    let mut unique = HashMap::new();
    let mut index = Vec::new();
    let mut digest = hash::Hasher::new();
    let mut buf = vec![0; BLOCK];
    let mut previous = vec![0; BLOCK];
    let mut last: Option<(usize, Entry)> = None;
    let mut compressed = Vec::new();
    let mut offset = data_start(len);
    if let Some(root) = root {
        for pos in (0..len).step_by(BLOCK) {
            if cancel.load(Ordering::Relaxed) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let block = &mut buf[..(len - pos).min(BLOCK as u64) as usize];
            root.read(pos, block)?;
            digest.update(&*block);
            // Repeated log blocks often occur in long consecutive runs. They only
            // need to pass through the whole-file hash once per occurrence.
            let entry = if let Some((_, entry)) =
                last.filter(|(n, _)| *n == block.len() && previous[..*n] == *block)
            {
                entry
            } else {
                let key = hash::of(block);
                match unique.entry((key, block.len())) {
                    std::collections::hash_map::Entry::Occupied(existing) => *existing.get(),
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        compressed.clear();
                        let mut encoder = DeflateEncoder::new(&mut compressed, Compression::fast());
                        encoder.write_all(block)?;
                        encoder.finish()?;
                        let payload: &[u8] = if compressed.len() < block.len() {
                            &compressed
                        } else {
                            block
                        };
                        file.write_all(payload)?;
                        let entry = Entry {
                            offset,
                            stored: payload.len() as u32,
                        };
                        offset += payload.len() as u64;
                        *slot.insert(entry)
                    }
                }
            };
            last = Some((block.len(), entry));
            std::mem::swap(&mut previous, &mut buf);
            index.extend_from_slice(&entry.offset.to_le_bytes());
            index.extend_from_slice(&entry.stored.to_le_bytes());
        }
    }
    file.seek(SeekFrom::Start(0))?;
    file.write_all(MAGIC)?;
    file.write_all(&len.to_le_bytes())?;
    file.write_all(&index)?;
    Ok(hash::finish(&digest))
}

pub(crate) struct Packed {
    file: File,
    len: u64,
    end: u64,
    cache: Mutex<Option<(Entry, Arc<[u8]>)>>,
}
impl Packed {
    pub(super) fn open(file: File, len: u64) -> io::Result<Self> {
        let mut head = [0; HEADER as usize];
        read_at(&file, 0, &mut head)?;
        let end = file.metadata()?.len();
        if &head[..4] != MAGIC
            || u64::from_le_bytes(head[4..].try_into().unwrap()) != len
            || data_start(len) > end
        {
            return Err(invalid("Invalid recovery block index"));
        }
        Ok(Self {
            file,
            len,
            end,
            cache: Mutex::default(),
        })
    }
    fn entry(&self, block: u64, length: usize) -> io::Result<Entry> {
        let mut record = [0; ENTRY as usize];
        read_at(&self.file, HEADER + block * ENTRY, &mut record)?;
        let offset = u64::from_le_bytes(record[..8].try_into().unwrap());
        let stored = u32::from_le_bytes(record[8..].try_into().unwrap());
        if stored == 0
            || stored as usize > length
            || offset < data_start(self.len)
            || offset > self.end
            || u64::from(stored) > self.end - offset
        {
            return Err(invalid("Invalid recovery block range"));
        }
        Ok(Entry { offset, stored })
    }
    fn decoded(&self, entry: Entry, length: usize) -> io::Result<Arc<[u8]>> {
        if let Some((_, data)) = self
            .cache
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(key, data)| *key == entry && data.len() == length)
        {
            return Ok(data.clone());
        }
        let mut payload = vec![0; entry.stored as usize];
        read_at(&self.file, entry.offset, &mut payload)?;
        let mut decoder = DeflateDecoder::new(payload.as_slice());
        let mut data = vec![0; length];
        decoder.read_exact(&mut data)?;
        if decoder.read(&mut [0])? != 0 || decoder.total_in() != u64::from(entry.stored) {
            return Err(invalid("Invalid compressed recovery block"));
        }
        let data: Arc<[u8]> = data.into();
        *self.cache.lock().unwrap() = Some((entry, data.clone()));
        Ok(data)
    }
    pub(super) fn read(&self, mut pos: u64, mut out: &mut [u8]) -> io::Result<()> {
        if pos > self.len || out.len() as u64 > self.len - pos {
            return Err(invalid("Read outside recovery base"));
        }
        while !out.is_empty() {
            let number = pos / BLOCK as u64;
            let start = number * BLOCK as u64;
            let length = (self.len - start).min(BLOCK as u64) as usize;
            let from = (pos - start) as usize;
            let count = (length - from).min(out.len());
            let entry = self.entry(number, length)?;
            let (dst, rest) = out.split_at_mut(count);
            if entry.stored as usize == length {
                read_at(&self.file, entry.offset + from as u64, dst)?;
            } else {
                dst.copy_from_slice(&self.decoded(entry, length)?[from..from + count]);
            }
            out = rest;
            pos += count as u64;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Source;

    #[test]
    fn random_access_handles_raw_compressed_and_repeated_blocks_and_checks_corruption(
    ) -> io::Result<()> {
        let mut seed = 0x454e393du32;
        let mut random = vec![0; BLOCK];
        for b in &mut random {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *b = seed as u8;
        }
        let repeated = vec![b'w'; BLOCK];
        let original = [&random[..], &repeated, &random, &repeated, b"tail"].concat();
        let root = tree::leaf(Source::memory(&original), 0, original.len() as u64)?;
        let mut f = tempfile::tempfile()?;
        let hash = write(&mut f, &root, &AtomicBool::new(false))?;
        assert_eq!(hash, hash::of(&original));
        assert!(f.metadata()?.len() < (BLOCK + BLOCK / 16) as u64);
        let packed = Packed::open(f.try_clone()?, original.len() as u64)?;
        for (at, n) in [
            (0, 16),
            (BLOCK - 3, 256),
            (BLOCK + 15, 4096),
            (2 * BLOCK - 2, 8),
            (4 * BLOCK - 4, 8),
        ] {
            let mut bytes = vec![0; n];
            packed.read(at as u64, &mut bytes)?;
            assert_eq!(bytes, original[at..at + n]);
        }
        let raw = packed.entry(0, BLOCK)?;
        assert_eq!(raw.stored as usize, BLOCK);
        assert_eq!(raw.offset, packed.entry(2, BLOCK)?.offset);
        assert!(packed.entry(1, BLOCK)?.stored < 8192);
        drop(packed);
        // Invalid table offsets cannot make the reader interpret the header as data.
        f.seek(SeekFrom::Start(HEADER))?;
        f.write_all(&0u64.to_le_bytes())?;
        let bad = Packed::open(f.try_clone()?, original.len() as u64)?;
        assert!(bad.read(0, &mut [0; 1]).is_err());
        assert_eq!(
            write(&mut tempfile::tempfile()?, &root, &AtomicBool::new(true))
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        Ok(())
    }
}
