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
//! document keeps a private clone of the file made when the two were known
//! to be equal. Every block of the file is then shared with the clone and
//! none can be written where it lies: any change, by any route, puts the
//! changed part in new blocks. After the file's unwritten changes are pushed
//! to the volume, the parts of the file whose blocks are still where the
//! clone's are have not changed, and only the parts that moved need
//! reading. Those are what this document itself wrote into the file since
//! (`mac_inplace.rs`), which is little, and whatever another program wrote.
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
    placement_of(file, 0, size)
}

/// The same for the part of the file from `from` up to `to`.
fn placement_of(file: &File, from: u64, to: u64) -> Option<Vec<(u64, u64, u64)>> {
    // <sys/fcntl.h> declares this structure with four-byte packing.
    #[repr(C, packed(4))]
    struct Run {
        flags: u32,
        bytes: i64,
        place: i64,
    }
    let mut runs: Vec<(u64, u64, u64)> = Vec::new();
    // Asked from where a block begins: from a place inside one the system
    // was seen to refuse.
    let mut at = from & !4095;
    while at < to {
        let mut run = Run { flags: 0, bytes: (to - at) as i64, place: at as i64 };
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

/// A read-only view of a whole file, never read through. It is there to
/// have the system write out, for a part of the file, what other programs
/// changed through views of their own: such a change is seen by every read
/// at once but is in no block until it is written out. `fsync` is meant to
/// do that for the whole file and was found to leave such a page behind
/// while another program was writing elsewhere in the file; asked for the
/// part itself, through a view, the system wrote it.
pub(crate) struct View {
    at: *mut libc::c_void,
    length: usize,
}
unsafe impl Send for View {}
unsafe impl Sync for View {}

impl View {
    pub(crate) fn of(file: &File) -> Option<Self> {
        let length = usize::try_from(file.metadata().ok()?.len()).ok().filter(|length| *length != 0)?;
        let at = unsafe { libc::mmap(std::ptr::null_mut(), length, libc::PROT_READ, libc::MAP_SHARED, file.as_raw_fd(), 0) };
        (at != libc::MAP_FAILED).then_some(Self { at, length })
    }

    fn write_out(&self, from: u64, length: u64) -> bool {
        // Whole pages, of the largest size the system uses.
        const PAGE: u64 = 16 * 1024;
        let start = from & !(PAGE - 1);
        let end = (from + length).next_multiple_of(PAGE).min(self.length as u64);
        start < end && unsafe { libc::msync(self.at.add(start as usize), (end - start) as usize, libc::MS_SYNC) } == 0
    }
}

impl Drop for View {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.at, self.length) };
    }
}

/// Whether `length` bytes of `file` from `at` on are in the blocks its clone
/// has them in, and so hold what the clone holds there. What the system has
/// of a file in memory is kept for that file alone: a clone of a file that
/// is in memory is read from the drive. So text is read from the file it was
/// cloned from, which is in memory, and this is asked of what was read.
pub(crate) fn same_blocks(file: &File, view: &View, clone: &File, at: u64, length: u64) -> Option<bool> {
    if !view.write_out(at, length) || unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
        return None;
    }
    Some(placement_of(file, at, at + length)? == placement_of(clone, at, at + length)?)
}

/// A private clone of a file, made when the file was known to hold what the
/// document had saved. It has no name and is never written to, and it is
/// its being there that keeps the file's blocks from being written where
/// they lie.
pub(crate) struct Reference {
    clone: File,
    size: u64,
    volume: u64,
}

impl Reference {
    pub(crate) fn new(file: File) -> io::Result<Self> {
        let status = file.metadata()?;
        Ok(Self { clone: file, size: status.len(), volume: status.dev() })
    }

    /// The parts of `file`, now `size` long, that are not in the blocks this
    /// clone has them in: all that can differ from the clone. `None` when
    /// the volume does not say.
    fn moved(&self, file: &File, size: u64) -> Option<Vec<(u64, u64)>> {
        // Places are places on one volume: on two they say nothing.
        let status = file.metadata().ok()?;
        if status.dev() != self.volume || status.len() != size {
            return None;
        }
        // Changes another program has made through a memory map are in
        // memory only, and in no block yet, until they are pushed out.
        if unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
            return None;
        }
        // The clone's blocks are asked for each time. They are not known to
        // stay where they are for as long as the clone lives, and a place
        // remembered wrongly would hide a change.
        let there = placement(&self.clone, self.size)?;
        let here = placement(file, size)?;
        let shared = size.min(self.size);
        let mut moved: Vec<(u64, u64)> = Vec::new();
        let mut add = |from: u64, to: u64| match moved.last_mut() {
            Some(last) if last.0 + last.1 == from => last.1 += to - from,
            _ => moved.push((from, to - from)),
        };
        let (mut a, mut b, mut at) = (0, 0, 0);
        while at < shared {
            let (one, other) = (here.get(a)?, there.get(b)?);
            let to = shared.min(one.0 + one.1).min(other.0 + other.1);
            if one.2 + (at - one.0) != other.2 + (at - other.0) {
                add(at, to);
            }
            at = to;
            a += usize::from(at == one.0 + one.1);
            b += usize::from(at == other.0 + other.1);
        }
        if size > shared {
            add(shared, size);
        }
        Some(moved)
    }
}

/// What the volume shows of a file a save is about to replace or write to.
pub(crate) enum Examined {
    /// It holds what the document last saved. With how much of it had to be
    /// read to know: what has moved since the clone was made.
    Untouched(u64),
    /// It does not.
    Changed,
    /// The volume does not say, or too much has moved: read all of it.
    Unknown,
}

/// The most that is read part by part before reading the whole file is the
/// plainer way to the same answer.
const READ_APART: u64 = 64 * 1024 * 1024;

/// Whether a file about to be written, `expected` bytes long, is one whose
/// clone can stand in for its checksum: a large one on a volume that clones.
pub(crate) fn clone_will_tell(file: &File, path: &Path, expected: Option<u64>) -> bool {
    expected.is_some_and(|size| size > SMALL) && private_clone(file, path).is_some()
}

/// A private clone of a large file just written, kept to show at the next
/// save that the file has not been changed since.
pub(crate) fn reference(file: &File, path: &Path) -> Option<Arc<Reference>> {
    if file.metadata().ok()?.len() <= SMALL {
        return None;
    }
    Reference::new(private_clone(file, path)?).ok().map(Arc::new)
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
    // A clone carries its origin's flags. One of a locked file would be
    // locked too: it could not be written, renamed or removed again.
    unsafe { libc::chflags(path.as_ptr(), 0) };
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
    // All of this is carried where the volume and our rights allow it. A
    // volume without attributes or permissions must not stop the text from
    // being saved; the file then stays the owner's alone.
    unsafe {
        libc::fchown(fd, u32::MAX, had.gid());
        libc::fcopyfile(old.as_raw_fd(), fd, std::ptr::null_mut(), libc::COPYFILE_XATTR);
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
    // A write clears set-user-ID and set-group-ID bits, so they are not
    // carried. Where the old file's group could not be kept, what its group
    // was allowed must not pass to another group: that group gets what
    // everyone had.
    let mut mode = had.mode() & 0o777;
    if new.metadata()?.gid() != had.gid() {
        mode = (mode & 0o707) | ((mode & 0o007) << 3);
    }
    let _ = new.set_permissions(Permissions::from_mode(mode));
    Ok(())
}

/// The old file's access list, put on the file that has just taken its
/// name. Not before: an entry that forbids deleting the old file would stop
/// the rename and then forbid removing the file that was to replace it.
pub(crate) fn keep_access_list(old: &File, new: &File) {
    unsafe {
        libc::fcopyfile(old.as_raw_fd(), new.as_raw_fd(), std::ptr::null_mut(), libc::COPYFILE_ACL);
    }
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
        keep_access_list(&guard, &out);
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

    /// The clone that protects the opened text, as a reference for a file
    /// this document has not saved yet: it is a clone of the whole file when
    /// that text is the whole file.
    pub(crate) fn opened_reference(&self, expected: &Stamp) -> Option<Reference> {
        let root = self.saved.as_ref()?;
        let tree::Body::Leaf(source, 0) = &root.body else {
            return None;
        };
        let head = if self.saved_bom { self.saved_codec.bom().len() as u64 } else { 0 };
        let (clone, origin) = source.settled()?;
        if !self.saved_codec.native() || root.len != source.len || head + root.len != expected.size || origin != head {
            return None;
        }
        Reference::new(clone.try_clone().ok()?).ok().filter(|reference| reference.size == expected.size)
    }

    /// Whether `file`, the destination of a save, is exactly what this
    /// document last saved or, not having saved yet, what it opened.
    pub(crate) fn examine(&self, file: &File, expected: &Stamp) -> Examined {
        match &self.saved_clone {
            Some(reference) => self.examine_against(file, expected.size, reference),
            None => match self.opened_reference(expected) {
                Some(reference) => self.examine_against(file, expected.size, &reference),
                None => Examined::Unknown,
            },
        }
    }

    /// A new clone of `file` to take the place of the one kept, once it is
    /// shown to hold the saved text itself. A clone cannot change, so what is
    /// shown of it stays true, which it would not of the file it was made
    /// from: that may be written to in the moment after it was examined.
    pub(crate) fn renewed_reference(&self, file: &File, dest: &Path, expected: &Stamp) -> Option<Arc<Reference>> {
        let kept = self.saved_clone.as_ref()?;
        let renewed = reference(file, dest)?;
        matches!(self.examine_against(&renewed.clone, expected.size, kept), Examined::Untouched(_)).then_some(renewed)
    }

    fn examine_against(&self, file: &File, size: u64, reference: &Reference) -> Examined {
        let Some(moved) = reference.moved(file, size) else {
            return Examined::Unknown;
        };
        let bytes: u64 = moved.iter().map(|part| part.1).sum();
        if bytes > READ_APART {
            return Examined::Unknown;
        }
        // What has not moved is what the clone holds, and nothing this
        // document saved since was written there. What has moved is read.
        let mut saved = self.clone();
        saved.root = self.saved.clone();
        let head = if self.saved_bom { self.saved_codec.bom() } else { &[] };
        if !self.saved_codec.native() || head.len() as u64 + saved.len() != size {
            return Examined::Unknown;
        }
        let mut found = vec![0; BLOCK as usize];
        for (start, length) in moved {
            let mut at = start;
            while at < start + length {
                let count = (start + length - at).min(BLOCK) as usize;
                if file.read_exact_at(&mut found[..count], at).is_err() {
                    return Examined::Unknown;
                }
                // The first bytes of the file may be its byte-order mark.
                let marked = (head.len() as u64).saturating_sub(at).min(count as u64) as usize;
                if marked != 0 && found[..marked] != head[at as usize..at as usize + marked] {
                    return Examined::Changed;
                }
                let text = at + marked as u64 - head.len() as u64;
                match saved.read(text, count - marked) {
                    Ok(wanted) if wanted == found[marked..count] => {}
                    Ok(_) => return Examined::Changed,
                    Err(_) => return Examined::Unknown,
                }
                at += count as u64;
            }
        }
        Examined::Untouched(bytes)
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
        // The document still reads its own text there, not the other
        // program's: text is read from the file itself only where the
        // file's blocks show it to be what the document opened.
        assert_eq!(doc.read(middle as u64, 1)?, [input[middle]]);
        let mut whole = Vec::new();
        let mut at = 0;
        while at < doc.len() {
            let part = doc.read(at, 4 << 20)?;
            at += part.len() as u64;
            whole.extend_from_slice(&part);
        }
        let mut ours = input.clone();
        ours[3..6].copy_from_slice(b"new");
        assert!(whole == ours);
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

        // A locked file is not saved over. Once it is unlocked it is, though
        // the clones made while it was locked were clones of a locked file.
        let lock = |flags| {
            let name = CString::new(file.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::chflags(name.as_ptr(), flags) }, 0);
        };
        lock(libc::UF_IMMUTABLE);
        let mut doc = Document::open(&file)?;
        doc.replace(3, 3, b"abc")?;
        assert_eq!(doc.save(&file).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        lock(0);
        doc.save(&file)?;
        input[3..6].copy_from_slice(b"abc");
        assert!(fs::read(&file)? == input);

        // Nothing of all this is left beside the person's files.
        let mut left: Vec<_> = fs::read_dir(dir.path())?.map(|entry| entry.unwrap().file_name()).collect();
        left.sort();
        assert_eq!(left, ["copy.log", "large.log"]);
        Ok(())
    }
}
