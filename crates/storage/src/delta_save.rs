//! Identify a small physical change from immutable piece-tree ranges. UTF-16,
//! Save As, and edits that shift a large suffix use the ordinary staged save.
use crate::{conflict, inplace, save_journal, source::read_at, transactional, Document, Source, Stamp};
use sha2::{Digest, Sha256};
use std::{fs::File, io, path::Path, sync::Arc};

use crate::tree::{shared_bytes, Leaves};

/// A snapshot pinned before dropping the previous deny-write lease supplies
/// the old revision. Check the writer's whole view, including unchanged bytes,
/// and compute the digest of the new revision without staging its contents.
/// With no snapshot, the recorded digest alone identifies the old revision.
fn verify_and_hash(
    pinned: Option<&File>, writer: &File, stamp: &Stamp, prior_digest: Option<[u8; 32]>,
    offset: u64, removed: u64, replacement: &[u8],
) -> io::Result<[u8; 32]> {
    let mut prior = prior_digest.map(|_| Sha256::new());
    let mut next = Sha256::new();
    let mut disk = vec![0; 1024 * 1024];
    let mut original = vec![0; disk.len()];
    let mut replaced = false;
    let mut at = 0;
    while at < stamp.size {
        let n = (stamp.size - at).min(disk.len() as u64) as usize;
        let bytes = &mut disk[..n];
        read_at(writer, at, bytes)?;
        if let Some(pinned) = pinned {
            read_at(pinned, at, &mut original[..n])?;
            if *bytes != original[..n] { return Err(conflict()); }
        }
        if let Some(hash) = &mut prior { hash.update(&*bytes); }
        let before = offset.saturating_sub(at).min(n as u64) as usize;
        next.update(&bytes[..before]);
        if at + n as u64 >= offset && !replaced {
            next.update(replacement);
            replaced = true;
        }
        let after = (offset + removed).saturating_sub(at).min(n as u64) as usize;
        next.update(&bytes[after..]);
        at += n as u64;
    }
    if let (Some(hash), Some(expected)) = (prior, prior_digest) {
        if <[u8; 32]>::from(hash.finalize()) != expected { return Err(conflict()); }
    }
    Ok(next.finalize().into())
}

impl Document {
    pub(super) fn save_small_delta(&mut self, dest: &Path) -> io::Result<bool> {
        self.save_small_delta_via(dest, save_journal::folder().as_deref())
    }

    /// With a journal folder the change is written straight into the file,
    /// which stays in Windows' cache. Without one it goes through an NTFS
    /// transaction, after which Windows drops the file from its cache.
    fn save_small_delta_via(&mut self, dest: &Path, journal: Option<&Path>) -> io::Result<bool> {
        let Some(stamp) = self.stamp.as_ref() else { return Ok(false) };
        if self.path.as_deref() != Some(dest) || stamp.size <= 24 * 1024 * 1024
            || !self.codec.native() || self.codec != self.saved_codec || self.emit_bom != self.saved_bom
            || !self.accepted_appends.is_empty() || self.previous_disk.is_some() {
            return Ok(false);
        }
        let header = if self.emit_bom { self.codec.bom().len() as u64 } else { 0 };
        let old_len = self.saved.as_ref().map_or(0, |n| n.len);
        if header + old_len != stamp.size { return Ok(false); }
        let new_len = self.len();
        let start = shared_bytes(&self.saved, &self.root, false);
        let suffix = if old_len == new_len {
            shared_bytes(&self.saved, &self.root, true).min(old_len - start)
        } else { 0 };
        let removed = old_len - start - suffix;
        let added = new_len - start - suffix;
        if removed.max(added) > transactional::MAX_PATCH_BYTES as u64 { return Ok(false); }

        let mut origin: Option<&Arc<Source>> = None;
        for root in [&self.saved, &self.root] {
            let mut pieces = Leaves::new(root, false);
            while let Some((src, _, _)) = pieces.next() {
                if src.file_id() == Some((stamp.volume, stamp.id)) {
                    if origin.is_some_and(|seen| !Arc::ptr_eq(seen, src)) { return Ok(false); }
                    origin = Some(src);
                }
            }
        }
        let Some(src) = origin else { return Ok(false) };
        let mut before = vec![0; removed as usize];
        if let Some(saved) = &self.saved { saved.read(start, &mut before)?; }
        let after = self.read(start, added as usize)?;
        let offset = header + start;
        let mut digest = None;
        let committed = if let Some(journal) = journal {
            src.patch_range(dest, stamp, offset, removed, &after, |alone, seal| {
                inplace::rewrite(dest, stamp, offset, &before, &after, header + new_len, journal, |writer| {
                    // Range locks kept every other byte still since the lease was
                    // held. Only a revision known by its digest needs reading.
                    if self.saved_disk_digest.is_some() {
                        digest = Some(verify_and_hash(None, writer, stamp, self.saved_disk_digest, offset, removed, &after)?);
                    }
                    Ok(())
                }, alone, seal)
            })?
        } else { src.transact_range(dest, stamp, offset, removed, &after, |pinned| {
            let mut expected_version = None;
            let commit = transactional::rewrite(dest, stamp, offset, &before, &after, header + new_len, |writer| {
                // The writer now excludes every competing writer. Our continuously
                // pinned reader also blocked ordinary writes throughout the handoff.
                // NTFS records every intervening transactional commit on this handle.
                let versions = transactional::versions(pinned);
                if self.saved_disk_digest.is_some() || !versions.is_some_and(|(base, latest)| base == latest) {
                    digest = Some(verify_and_hash(Some(pinned), writer, stamp, self.saved_disk_digest, offset, removed, &after)?);
                }
                expected_version = versions.and_then(|(base, latest)| {
                    latest.checked_add(u32::from(before != after)).map(|next| (base, next))
                });
                Ok(())
            })?;
            Ok(commit.map(|stamp| (stamp, expected_version)))
        })? };
        let Some(stamp) = committed else { return Ok(false) };
        self.stamp = Some(stamp);
        self.saved_disk_digest = digest;
        self.saved = self.root.clone();
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::{Seek, SeekFrom, Write}};

    #[test]
    fn tiny_edits_preserve_snapshots_disk_revision_and_undo() -> io::Result<()> {
        tiny_edits(false)
    }

    #[test]
    fn tiny_edits_written_in_place_preserve_snapshots_disk_revision_and_undo() -> io::Result<()> {
        tiny_edits(true)
    }

    fn tiny_edits(in_place: bool) -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let journal = dir.path().join("journal");
        save_journal::FOR_THIS_THREAD.set(in_place.then(|| journal.clone()));
        let file = dir.path().join("tiny-edits.txt");
        let mut input = b"abcdefgh".repeat(25 * 1024 * 1024 / 8);
        fs::write(&file, &input)?;
        let mut doc = Document::open(&file)?;
        let original = doc.clone();
        let stamp = Stamp::get(&file)?.unwrap();
        for (at, count, text) in [(input.len()-16, 3, b"length changed".as_slice()), (13, 3, b"XYZ".as_slice())] {
            // Each case starts with an independent file-open and history.
            doc = Document::open(&file)?;
            let old_view = doc.clone();
            doc.replace(at as u64, count as u64, text)?;
            let mut worker = doc.clone();
            assert!(worker.save_small_delta(&file.canonicalize()?)?, "transaction declined");
            doc.adopt_save(worker);
            let edited_view = doc.clone();
            let mut expected = input.clone();
            expected.splice(at..at+count, text.iter().copied());
            assert_eq!(Stamp::get(&file)?.unwrap().id, stamp.id);
            assert!(fs::read(&file)? == expected);
            assert_eq!(old_view.read(at as u64, count)?, input[at..at + count]);
            // A version-certified save can authenticate the immutable saved tree
            // directly. A byte-verified fallback also records its SHA-256.
            if let Some(digest) = doc.saved_disk_digest {
                assert_eq!(digest, <[u8; 32]>::from(Sha256::digest(&expected)));
            }
            doc.verify_disk_revision(&file, doc.stamp.as_ref().unwrap())?;
            assert!(doc.undo());
            assert_eq!(doc.read(at as u64, count)?, input[at..at + count]);
            doc.save(&file)?;
            assert!(fs::read(&file)? == input);
            assert_eq!(edited_view.read(at as u64, text.len())?, text);
            assert!(doc.redo());
            doc.save(&file)?;
            assert!(fs::read(&file)? == expected);
            assert!(doc.undo());
            doc.save(&file)?;
        }
        assert_eq!(original.read(0, 8)?, &input[..8]);
        // A real outside writer can obtain the file after a successful commit.
        // Its equal-size, backdated change must survive the local Save attempt.
        let old_time = fs::metadata(&file)?.modified()?;
        doc.replace(3, 3, b"new")?;
        let mut external = fs::OpenOptions::new().write(true).open(&file)?;
        external.seek(SeekFrom::Start(1000))?;
        external.write_all(b"EXT")?;
        external.set_modified(old_time)?;
        drop(external);
        assert_eq!(doc.save(&file).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        input[1000..1003].copy_from_slice(b"EXT");
        assert!(fs::read(&file)? == input);
        let copy = dir.path().join("local-copy.txt");
        doc.save(&copy)?;
        input[1000..1003].copy_from_slice(b"abc");
        input[3..6].copy_from_slice(b"new");
        assert!(fs::read(copy)? == input);
        // The in-place path made a record for every save and removed each one.
        assert_eq!(in_place, journal.exists());
        assert!(!in_place || fs::read_dir(&journal)?.count() == 0);
        save_journal::FOR_THIS_THREAD.set(None);
        Ok(())
    }
}
