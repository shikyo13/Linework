//! What a save needs from macOS that the portable path does not give.
//!
//! A saved file is a new file renamed over the old one. Unless it is told
//! otherwise it has the temporary file's owner-only permissions and none of
//! what the old file carried, so a saved script stops being executable and a
//! tagged file loses its tags.
//!
//! A large file is protected and saved through APFS clones. A clone is a
//! second file that shares the first one's blocks until either is written
//! to, so it costs neither time nor space in proportion to the file. The
//! copy that keeps the original text for undo is a clone, and a save is a
//! clone of that copy with only the changed text written into it. The save
//! is still a new file renamed over the old one: a crash leaves the old file
//! or the new one, never a mixture, and no record of the old bytes is needed.
//!
//! macOS cannot keep other programs from writing to a file, so before a save
//! replaces the file it must be shown to be what this document last saved,
//! or another program's change would be lost. Its dates do not show that: a
//! program writing through a memory map leaves them alone. Reading all of it
//! does, at the cost of reading all of it. A clone shows it at no cost. The
//! document keeps a private clone of the file as saved, so every block of
//! the file is shared with the clone and no block can be written where it
//! lies: any change, by any route, puts the changed part in new blocks.
//! After the file's unwritten changes are pushed to the volume, a file whose
//! blocks are all still where the clone's are has not changed.
use crate::{conflict, invalid, source::Source, tree, Document, Stamp};
use std::{
    ffi::CString,
    fs::{File, OpenOptions, Permissions},
    io,
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{FileExt, MetadataExt, PermissionsExt},
        },
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::SystemTime,
};
use tempfile::{NamedTempFile, TempPath};

/// Files up to this size are held in memory and saved whole.
const SMALL: u64 = 24 * 1024 * 1024;
const BLOCK: u64 = 1024 * 1024;

/// Where a file's bytes lie on its volume: runs of (place in the file,
/// length, place on the volume), adjoining runs joined. `None` when the
/// volume does not say for every byte, as for a file with holes.
fn placement(file: &File, size: u64) -> Option<Vec<(u64, u64, u64)>> {
    // <sys/fcntl.h> declares this structure with four-byte packing.
    #[repr(C, packed(4))]
    struct Run {
        flags: u32,
        bytes: i64,
        place: i64,
    }
    let mut runs: Vec<(u64, u64, u64)> = Vec::new();
    let mut at = 0;
    while at < size {
        let mut run = Run { flags: 0, bytes: (size - at) as i64, place: at as i64 };
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_LOG2PHYS_EXT, &mut run) } != 0 {
            return None;
        }
        let (bytes, place) = (run.bytes, run.place);
        if bytes <= 0 || place < 0 {
            return None;
        }
        let (bytes, place) = (bytes as u64, place as u64);
        match runs.last_mut() {
            Some(last) if last.0 + last.1 == at && last.2 + last.1 == place => last.1 += bytes,
            _ => runs.push((at, bytes, place)),
        }
        at += bytes;
    }
    Some(runs)
}

/// Whether `file` still has every block where `clone`, made from it, has it.
fn shares_every_block(file: &File, clone: &File, size: u64) -> bool {
    let sized = |f: &File| f.metadata().is_ok_and(|m| m.len() == size);
    // Changes another program has made through a memory map are in memory
    // only, and in no block yet, until they are pushed to the volume.
    sized(file)
        && sized(clone)
        && unsafe { libc::fsync(file.as_raw_fd()) } == 0
        && placement(file, size).is_some_and(|runs| Some(runs) == placement(clone, size))
}

/// A private clone of a large file just written, kept to show at the next
/// save that the file has not been changed since.
pub(crate) fn reference(file: &File, path: &Path) -> Option<Arc<File>> {
    if file.metadata().ok()?.len() <= SMALL {
        return None;
    }
    private_clone(file, path).map(Arc::new)
}

/// Windows refuses to replace a file marked read-only. A rename here would
/// replace one without asking, so the same question is put to the file first.
fn writable(dest: &Path) -> io::Result<()> {
    OpenOptions::new().write(true).open(dest).map(drop)
}

/// A clone of an open file under a new name in `dir`.
fn clone_into(file: &File, dir: &Path) -> io::Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let moment = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    let name = dir.join(format!(
        ".linework-{}-{}-{moment:x}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let path = CString::new(name.as_os_str().as_bytes()).map_err(|_| invalid("Unusable folder name"))?;
    // What another program has changed through a memory map and not yet
    // pushed to the volume is in the file as read, so it belongs in the clone.
    unsafe { libc::fsync(file.as_raw_fd()) };
    if unsafe { libc::fclonefileat(file.as_raw_fd(), libc::AT_FDCWD, path.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(name)
}

/// A clone of `file` that has no name, so nothing else can reach it. The
/// temporary folder is tried first; for a file on another volume the clone
/// has to be made beside it. `None` where the volume cannot clone.
pub(crate) fn private_clone(file: &File, path: &Path) -> Option<File> {
    [Some(std::env::temp_dir()), path.parent().map(Path::to_owned)]
        .into_iter()
        .flatten()
        .find_map(|dir| {
            let name = clone_into(file, &dir).ok()?;
            let opened = File::open(&name);
            let _ = std::fs::remove_file(&name);
            opened.ok()
        })
}

/// The file a save is written to before it takes the destination's name. One
/// that replaces a file starts with that file's permissions, group, access
/// list, extended attributes, creation date and Finder flags; one that makes
/// a new file has the permissions any new file has here.
pub(crate) fn staging(dir: &Path, dest: &Path, replaces: bool) -> io::Result<NamedTempFile> {
    if !replaces {
        return tempfile::Builder::new()
            .permissions(Permissions::from_mode(0o666))
            .tempfile_in(dir);
    }
    writable(dest)?;
    let staged = NamedTempFile::new_in(dir)?;
    inherit(&File::open(dest)?, staged.as_file())?;
    Ok(staged)
}

fn inherit(old: &File, new: &File) -> io::Result<()> {
    let had = old.metadata()?;
    let fd = new.as_raw_fd();
    // A write clears set-user-ID and set-group-ID bits, so they are not carried.
    new.set_permissions(Permissions::from_mode(had.mode() & 0o777))?;
    // The rest is carried where the volume and our rights allow it. A volume
    // without access lists or attributes, or a group we are not in, must not
    // stop the text from being saved.
    unsafe {
        libc::fchown(fd, u32::MAX, had.gid());
        libc::fcopyfile(
            old.as_raw_fd(),
            fd,
            std::ptr::null_mut(),
            libc::COPYFILE_ACL | libc::COPYFILE_XATTR,
        );
        let mut status: libc::stat = std::mem::zeroed();
        if libc::fstat(old.as_raw_fd(), &mut status) == 0 {
            libc::fchflags(fd, status.st_flags & (libc::UF_HIDDEN | libc::UF_NODUMP));
            let mut list: libc::attrlist = std::mem::zeroed();
            list.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
            list.commonattr = libc::ATTR_CMN_CRTIME;
            let mut created = libc::timespec {
                tv_sec: status.st_birthtime,
                tv_nsec: status.st_birthtime_nsec,
            };
            libc::fsetattrlist(
                fd,
                (&mut list as *mut libc::attrlist).cast(),
                (&mut created as *mut libc::timespec).cast(),
                std::mem::size_of::<libc::timespec>(),
                0,
            );
        }
    }
    // Setting the permissions last would be undone by nothing above, but an
    // access list copied onto the file may have changed the mode bits.
    new.set_permissions(Permissions::from_mode(had.mode() & 0o777))
}

impl Document {
    /// Save over the file this document was opened from by writing only the
    /// text that is not already in its place. Declines (`false`) when no
    /// text is, or when the volume cannot clone.
    pub(crate) fn save_by_clone(&mut self, dest: &Path, before: &Stamp) -> io::Result<bool> {
        if before.size <= SMALL
            || !self.codec.native()
            || self.codec != self.saved_codec
            || self.emit_bom != self.saved_bom
        {
            return Ok(false);
        }
        let Some(root) = self.root.clone() else {
            return Ok(false);
        };
        let head = if self.emit_bom { self.codec.bom().len() as u64 } else { 0 };
        // For each file a piece of the text is in: how much of the new text
        // is where it would already be in a clone of that file.
        let mut placed: Vec<(Arc<Source>, u64)> = Vec::new();
        let mut at = head;
        root.pieces(&mut |source, offset, len| {
            if source.settled().is_some_and(|(_, origin)| origin + offset == at) {
                match placed.iter_mut().find(|(known, _)| Arc::ptr_eq(known, source)) {
                    Some((_, bytes)) => *bytes += len,
                    None => placed.push((source.clone(), len)),
                }
            }
            at += len;
            Ok(())
        })?;
        let total = at;
        // Text moved along by an insertion before it is written again, but
        // to new blocks, which measured no slower than a file written whole
        // however little of the text stays in place.
        let Some((base, _)) = placed.into_iter().max_by_key(|(_, bytes)| *bytes) else {
            return Ok(false);
        };
        writable(dest)?;
        let (file, origin) = base.settled().expect("counted above");
        let dir = dest.parent().ok_or_else(|| invalid("Missing destination folder"))?;
        let Ok(name) = clone_into(file, dir) else {
            return Ok(false);
        };
        let staged = TempPath::try_from_path(name)?;
        // The clone has the permissions the original had when it was opened,
        // which may not let even its owner write.
        std::fs::set_permissions(&staged, Permissions::from_mode(0o600))?;
        // Read as well as write: the clone kept of the finished file is made
        // from this handle.
        let out = OpenOptions::new().read(true).write(true).open(&staged)?;
        if head != 0 {
            out.write_all_at(self.codec.bom(), 0)?;
        }
        // Past the last text that stays where it is, nothing of the clone
        // is wanted. Cut it off there, so what follows is written to new
        // blocks: writing over blocks shared with the clone's origin
        // measured half as fast as writing the whole file anew.
        let mut kept = head;
        let mut at = head;
        root.pieces(&mut |source, offset, len| {
            if Arc::ptr_eq(source, &base) && origin + offset == at {
                kept = at + len;
            }
            at += len;
            Ok(())
        })?;
        out.set_len(kept)?;
        let mut buffer = Vec::new();
        let mut at = head;
        root.pieces(&mut |source, offset, len| {
            if !(Arc::ptr_eq(source, &base) && origin + offset == at) {
                let mut done = 0;
                while done < len {
                    buffer.resize((len - done).min(BLOCK) as usize, 0);
                    source.read(offset + done, &mut buffer)?;
                    out.write_all_at(&buffer, at + done)?;
                    done += buffer.len() as u64;
                }
            }
            at += len;
            Ok(())
        })?;
        out.set_len(total)?;
        out.set_modified(SystemTime::now())?;
        inherit(&File::open(dest)?, &out)?;
        out.sync_all()?;
        let stamp = Stamp::from(&out)?;
        let saved_clone = reference(&out, dest);
        self.check_original()?;
        let guard = self.verify_disk_revision(dest, before)?;
        if Stamp::get(dest)?.as_ref() != Some(before) {
            return Err(conflict());
        }
        staged.persist(dest).map_err(|e| e.error)?;
        drop(guard);
        self.stamp = Some(stamp);
        self.path = Some(dest.to_owned());
        self.saved_disk_digest = None;
        self.saved_clone = saved_clone;
        self.saved = self.root.clone();
        self.saved_codec = self.codec;
        self.saved_bom = self.emit_bom;
        Ok(true)
    }

    /// Whether the volume shows `file`, the destination of a save, to be
    /// exactly what this document last saved or, not having saved yet, what
    /// it opened. `false` says only that it could not be shown this way.
    pub(crate) fn untouched_since_saved(&self, file: &File, expected: &Stamp) -> bool {
        if let Some(clone) = &self.saved_clone {
            return shares_every_block(file, clone, expected.size);
        }
        // Not saved yet: the clone that protects the opened text is one of
        // the whole file, when that text is the whole file.
        let Some(root) = &self.saved else {
            return false;
        };
        let tree::Body::Leaf(source, 0) = &root.body else {
            return false;
        };
        let head = if self.saved_bom { self.saved_codec.bom().len() as u64 } else { 0 };
        self.saved_codec.native()
            && root.len == source.len
            && head + root.len == expected.size
            && source
                .settled()
                .is_some_and(|(clone, origin)| origin == head && shares_every_block(file, clone, expected.size))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// What a person relies on when saving a large file, each checked against
    /// bytes worked out apart from the engine: the file is exactly the edited
    /// text, earlier views and undo still read what they did, the file keeps
    /// its permissions, and a change another program made is never lost.
    #[test]
    fn a_large_file_is_saved_by_clone_and_outside_changes_are_refused() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("large.log").canonicalize().unwrap_or(dir.path().join("large.log"));
        let mut input = b"abcdefgh".repeat(25 * 1024 * 1024 / 8);
        fs::write(&file, &input)?;
        fs::set_permissions(&file, Permissions::from_mode(0o640))?;
        let file = file.canonicalize()?;
        let middle = input.len() / 2;
        for (at, count, text) in [
            (input.len() - 16, 3, b"length changed".as_slice()),
            (13, 3, b"XYZ".as_slice()),
            (middle, 0, b"put in the middle, moving half the file".as_slice()),
        ] {
            let mut doc = Document::open(&file)?;
            let old_view = doc.clone();
            doc.replace(at as u64, count as u64, text)?;
            let stamp = Stamp::get(&file)?.unwrap();
            let mut worker = doc.clone();
            assert!(worker.save_by_clone(&file, &stamp)?, "save by clone declined");
            doc.adopt_save(worker);
            let edited_view = doc.clone();
            let mut expected = input.clone();
            expected.splice(at..at + count, text.iter().copied());
            assert!(fs::read(&file)? == expected);
            assert_eq!(fs::metadata(&file)?.permissions().mode() & 0o7777, 0o640);
            assert_eq!(old_view.read(at as u64, count)?, input[at..at + count]);
            // The next saves find the file untouched without reading it, and
            // go by clone again: only a save that writes the whole file
            // leaves a checksum.
            assert!(doc.undo());
            doc.save(&file)?;
            assert!(doc.saved_disk_digest.is_none());
            assert!(fs::read(&file)? == input);
            assert_eq!(edited_view.read(at as u64, text.len())?, text);
            assert!(doc.redo());
            doc.save(&file)?;
            assert!(fs::read(&file)? == expected);
            assert!(doc.undo());
            doc.save(&file)?;
            assert!(fs::read(&file)? == input);
        }

        // Text put at the very start moves all of the file: written anew.
        let mut doc = Document::open(&file)?;
        doc.replace(0, 0, b"front ")?;
        let stamp = Stamp::get(&file)?.unwrap();
        assert!(!doc.clone().save_by_clone(&file, &stamp)?);
        doc.save(&file)?;
        assert!(fs::read(&file)?.strip_prefix(b"front ".as_slice()) == Some(&input[..]));
        assert!(doc.undo());
        doc.save(&file)?;
        assert!(fs::read(&file)? == input);

        // Another program changes the file and puts its date back. Its
        // change must survive, and the text can still be saved elsewhere.
        let old_time = fs::metadata(&file)?.modified()?;
        doc.replace(3, 3, b"new")?;
        let outside = OpenOptions::new().write(true).open(&file)?;
        outside.write_all_at(b"EXT", 1000)?;
        outside.set_modified(old_time)?;
        drop(outside);
        assert_eq!(doc.save(&file).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        input[1000..1003].copy_from_slice(b"EXT");
        assert!(fs::read(&file)? == input);
        let copy = dir.path().join("copy.log");
        doc.save(&copy)?;
        let mut ours = input.clone();
        ours[1000..1003].copy_from_slice(b"abc");
        ours[3..6].copy_from_slice(b"new");
        assert!(fs::read(&copy)? == ours);

        // Another program changes the file through a memory map, which leaves
        // the file's dates as they were and puts nothing on the volume yet.
        let mut doc = Document::open(&file)?;
        doc.replace(3, 3, b"new")?;
        let mapped = OpenOptions::new().read(true).write(true).open(&file)?;
        let view = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                input.len(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                mapped.as_raw_fd(),
                0,
            )
        };
        assert_ne!(view, libc::MAP_FAILED);
        unsafe { *view.cast::<u8>().add(middle) = b'M' };
        let refused = doc.save(&file);
        unsafe { libc::munmap(view, input.len()) };
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        input[middle] = b'M';
        assert!(fs::read(&file)? == input);

        // Another program writes the bytes that were already there. The file
        // has new blocks but the same text, so reading it settles the matter
        // and the save goes ahead.
        let mut doc = Document::open(&file)?;
        doc.replace(3, 3, b"new")?;
        let old_time = fs::metadata(&file)?.modified()?;
        let outside = OpenOptions::new().write(true).open(&file)?;
        outside.write_all_at(&input[2000..2003], 2000)?;
        outside.set_modified(old_time)?;
        drop(outside);
        doc.save(&file)?;
        input[3..6].copy_from_slice(b"new");
        assert!(fs::read(&file)? == input);
        Ok(())
    }
}
