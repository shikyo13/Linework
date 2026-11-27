//! Keep the line index of a very large file between sessions, so opening it
//! again does not read it all to count its lines. An entry is used only for
//! the same file, size and modification time, when its own checksum holds and
//! a sample of its pages still counts the same. It holds counts, never text.
use super::{count, Backing, Lane, Source, PAGE};
use crate::Stamp;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{atomic::Ordering, RwLock},
    time::SystemTime,
};

static FOLDER: RwLock<Option<PathBuf>> = RwLock::new(None);
const MAGIC: &[u8; 8] = b"LWLINES1";
const SUFFIX: &str = "lwlines";
/// A smaller file is counted faster than anyone can notice.
#[cfg(not(test))]
const SMALLEST: u64 = 1024 * 1024 * 1024;
#[cfg(test)]
const SMALLEST: u64 = 4 * 1024 * 1024;
/// Entries kept. The least recently used go first. One is 4 bytes per 64 KiB of file.
const KEPT: usize = 64;
/// Pages recounted before an entry is trusted.
const SAMPLES: u64 = 16;

/// Where line indexes of large files are kept, or `None` to keep none.
pub fn set_index_cache(folder: Option<&Path>) {
    *FOLDER.write().unwrap() = folder.map(Path::to_owned);
}

impl Source {
    /// The entry's file and the header that identifies this exact revision.
    fn cache_entry(&self) -> Option<(PathBuf, Vec<u8>)> {
        let folder = FOLDER.read().unwrap().clone()?;
        let external = self.external.as_ref()?;
        let Backing::Disk(_, origin) = &self.backing else {
            return None;
        };
        let stamp = self.recorded.as_ref()?;
        if self.len < SMALLEST || self.private_file().is_some() {
            return None;
        }
        #[cfg(windows)]
        if external.delta.read().unwrap().is_virtual() {
            return None;
        }
        let name = Sha256::digest(external.path.as_os_str().as_encoded_bytes());
        let name: String = name[..16].iter().map(|b| format!("{b:02x}")).collect();
        let mut header = MAGIC.to_vec();
        header.extend((PAGE as u64).to_le_bytes());
        for value in [self.len, *origin, stamp.size, stamp.id, u64::from(stamp.volume)] {
            header.extend(value.to_le_bytes());
        }
        header.extend(stamp.modified.to_le_bytes());
        Some((folder.join(name).with_extension(SUFFIX), header))
    }
    /// Take the line index from an earlier session. Returns false when there
    /// is none that provably belongs to this file as it is now.
    pub(super) fn load_index(&self) -> bool {
        let Some((file, header)) = self.cache_entry() else {
            return false;
        };
        let Ok(bytes) = fs::read(&file) else {
            return false;
        };
        let pages = self.len.div_ceil(PAGE as u64);
        let body = header.len() + pages as usize * 4;
        if bytes.len() != body + 32
            || bytes[..header.len()] != header[..]
            || Sha256::digest(&bytes[..body])[..] != bytes[body..]
        {
            return false;
        }
        let saved = |page: u64| {
            let at = header.len() + page as usize * 4;
            u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
        };
        // Size and time can survive a rewrite. Recount pages spread over the
        // file, its first and last among them.
        let mut lane = Lane::default();
        let mut buffer = vec![0; PAGE + 1];
        for sample in 0..SAMPLES.min(pages) {
            let page = sample * (pages - 1) / (SAMPLES - 1);
            let pos = page * PAGE as u64;
            let length = (self.len - pos).min(PAGE as u64) as usize;
            let read = length + usize::from(pos + (length as u64) < self.len);
            if self.read_lane(&mut lane, pos, &mut buffer[..read]).is_err()
                || count(&buffer[..length], buffer[..read].get(length).copied()) != u64::from(saved(page))
            {
                return false;
            }
        }
        let mut total = 0;
        let mut counts = Vec::with_capacity(pages as usize + 1);
        counts.push(0);
        counts.extend((0..pages).map(|page| {
            total += u64::from(saved(page));
            total
        }));
        *self.counts.write().unwrap() = counts;
        self.prefix.store(self.len, Ordering::Release);
        // Mark the entry as used, for the order in which old ones are removed.
        if let Ok(entry) = fs::OpenOptions::new().write(true).open(&file) {
            let _ = entry.set_modified(SystemTime::now());
        }
        true
    }
    /// Keep the finished line index. Every failure only means the next open counts again.
    pub(super) fn store_index(&self) {
        let Some((file, mut bytes)) = self.cache_entry() else {
            return;
        };
        let Backing::Disk(handle, _) = &self.backing else {
            return;
        };
        // The counts describe the file as it was read. Keep them only if that
        // is still the file that was opened.
        if Stamp::from(handle).ok().as_ref() != self.recorded.as_ref() {
            return;
        }
        {
            let counts = self.counts.read().unwrap();
            if counts.len() as u64 != self.len.div_ceil(PAGE as u64) + 1 {
                return;
            }
            bytes.reserve(counts.len() * 4 + 32);
            for pair in counts.windows(2) {
                bytes.extend(((pair[1] - pair[0]) as u32).to_le_bytes());
            }
        }
        let digest = Sha256::digest(&bytes);
        bytes.extend(digest);
        let Some(folder) = file.parent() else {
            return;
        };
        let written = (|| -> io::Result<()> {
            fs::create_dir_all(folder)?;
            let mut staged = tempfile::NamedTempFile::new_in(folder)?;
            staged.write_all(&bytes)?;
            staged.persist(&file).map_err(|e| e.error)?;
            Ok(())
        })();
        if written.is_ok() {
            prune(folder);
        }
    }
}

fn prune(folder: &Path) {
    let Ok(entries) = fs::read_dir(folder) else {
        return;
    };
    let mut entries: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == SUFFIX))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    if entries.len() > KEPT {
        entries.sort();
        for (_, path) in &entries[..entries.len() - KEPT] {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Document;
    use std::{fs::File, io::Seek};

    fn text(lines: usize) -> Vec<u8> {
        (0..lines)
            .flat_map(|i| format!("line {i} of a file that is opened twice\r\n").into_bytes())
            .collect()
    }
    fn opened(path: &Path) -> io::Result<Document> {
        let doc = Document::open(path)?;
        while !doc.indexing_complete() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(doc.index_error(), None);
        Ok(doc)
    }
    fn entries(folder: &Path) -> Vec<PathBuf> {
        fs::read_dir(folder).map_or(Vec::new(), |d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == SUFFIX))
                .collect()
        })
    }
    /// The entry is written by the indexing thread; other entries are pruned after it.
    fn eventually(mut done: impl FnMut() -> bool) -> bool {
        for _ in 0..2000 {
            if done() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        false
    }

    #[test]
    fn a_saved_index_is_used_only_for_the_file_it_was_counted_from() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = dir.path().join("line-index");
        let path = dir.path().join("big.log");
        // Larger than the files Linework simply holds in memory.
        let lines = 700_000;
        fs::write(&path, text(lines))?;
        set_index_cache(Some(&cache));

        let first = opened(&path)?;
        assert_eq!(first.line_count(), lines as u64 + 1);
        assert!(eventually(|| entries(&cache).len() == 1), "the finished index is kept");
        let entry = entries(&cache);
        let kept = fs::read(&entry[0])?;
        drop(first);

        // A second open takes the entry: make that visible by planting a
        // different (but self-consistent) count on a page that is not sampled.
        let pages = fs::metadata(&path)?.len().div_ceil(PAGE as u64) as usize;
        let header = kept.len() - 32 - pages * 4;
        let mut planted = kept[..kept.len() - 32].to_vec();
        let odd = header + 3 * 4;
        let value = u32::from_le_bytes(planted[odd..odd + 4].try_into().unwrap()) + 5;
        planted[odd..odd + 4].copy_from_slice(&value.to_le_bytes());
        let digest = Sha256::digest(&planted);
        planted.extend(digest);
        fs::write(&entry[0], &planted)?;
        assert_eq!(opened(&path)?.line_count(), lines as u64 + 6, "the entry was not used");

        // A damaged entry is ignored and replaced.
        let mut damaged = kept.clone();
        damaged[odd] ^= 1;
        fs::write(&entry[0], &damaged)?;
        assert_eq!(opened(&path)?.line_count(), lines as u64 + 1);
        assert!(eventually(|| fs::read(&entry[0]).is_ok_and(|now| now == kept)));

        // The same size and time with different text: a sampled page disagrees.
        let time = fs::metadata(&path)?.modified()?;
        let mut file = File::options().write(true).open(&path)?;
        file.rewind()?;
        file.write_all(b"\n\n\n\n")?;
        file.set_modified(time)?;
        drop(file);
        assert_eq!(opened(&path)?.line_count(), lines as u64 + 1 + 4);
        assert!(eventually(|| fs::read(&entry[0]).is_ok_and(|now| now != kept)));

        // A file that grew is a different revision.
        let mut file = File::options().append(true).open(&path)?;
        file.write_all(b"one more line\r\n")?;
        drop(file);
        assert_eq!(opened(&path)?.line_count(), lines as u64 + 1 + 4 + 1);
        assert_eq!(entries(&cache).len(), 1, "one entry per file");
        set_index_cache(None);
        Ok(())
    }
}
