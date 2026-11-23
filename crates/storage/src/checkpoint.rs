use crate::{
    codec::Codec, conflict, revision::guard_destination, source::Source, tree, Document, Stamp,
};
use std::{
    fs::{File, OpenOptions},
    io,
    mem::{offset_of, size_of, zeroed},
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::{Path, PathBuf},
    sync::Arc,
};
use windows_sys::Win32::{
    Storage::FileSystem::*,
    System::WindowsProgramming::{
        FILE_RENAME_FLAG_POSIX_SEMANTICS, FILE_RENAME_FLAG_REPLACE_IF_EXISTS,
    },
};

pub(crate) struct DiskCheckpoint {
    root: tree::Root,
    codec: Codec,
    bom: bool,
    stamp: Stamp,
    digest: Option<[u8; 32]>,
    path: PathBuf,
    destination: PathBuf,
    source: Arc<Source>,
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

pub(crate) fn atomic_replace(dest: &Path, staged: &Path) -> io::Result<()> {
    replace_version(dest, staged, None)
}

fn replace_version(dest: &Path, staged: &Path, backup: Option<&Path>) -> io::Result<()> {
    let backup = backup.map(wide);
    if unsafe {
        ReplaceFileW(
            wide(dest).as_ptr(),
            wide(staged).as_ptr(),
            backup.as_ref().map_or(std::ptr::null(), |path| path.as_ptr()),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Rename the checked checkpoint handle itself. A different file subsequently
/// placed at the private backup name can never become the publication source.
fn rename_checkpoint(path: &Path, dest: &Path, expected: &Stamp, replace: bool) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options
        .access_mode(DELETE | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE);
    let file = options.open(path)?;
    if Stamp::from(&file)? != *expected || (replace && link_count(&file)? != 1) {
        return Err(conflict());
    }
    // SetFileInformationByHandle takes a full Win32 path for a cross-directory
    // rename. A directory handle plus a relative name returns ERROR_INVALID_PARAMETER.
    // Keep a trailing NUL for Win32 path conversion; FileNameLength counts
    // only the actual UTF-16 name, as required by FILE_RENAME_INFO.
    let name = wide(dest);
    let offset = offset_of!(FILE_RENAME_INFO, FileName);
    let size = (offset + name.len() * 2).max(size_of::<FILE_RENAME_INFO>());
    let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*info).Anonymous.Flags = FILE_RENAME_FLAG_POSIX_SEMANTICS
            | if replace { FILE_RENAME_FLAG_REPLACE_IF_EXISTS } else { 0 };
        (*info).RootDirectory = std::ptr::null_mut();
        (*info).FileNameLength = ((name.len() - 1) * 2) as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
        if SetFileInformationByHandle(
            file.as_raw_handle(),
            FileRenameInfoEx,
            info.cast(),
            size as u32,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Seek, SeekFrom, Write};

    #[test]
    fn undo_save_republishes_original_file_after_worker_handoff() -> io::Result<()> {
        let folder = tempfile::tempdir()?;
        let path = folder.path().join("résumé.txt");
        let mut file = File::create(&path)?;
        let block = vec![b'a'; 1024 * 1024];
        for _ in 0..25 {
            file.write_all(&block)?;
        }
        drop(file);
        let original = Stamp::get(&path)?.unwrap();
        let mut doc = Document::open(&path)?;
        // A length-changing edit at the front deliberately exercises staging:
        // the shifted suffix exceeds the transaction's compact-write limit.
        doc.replace(0, 4, b"changed")?;
        let mut worker = doc.clone();
        worker.save(&path)?;
        doc.adopt_save(worker);
        assert_ne!(Stamp::get(&path)?.unwrap().id, original.id);
        let backup = &doc.previous_disk.as_ref().unwrap().path;
        assert_eq!(Stamp::get(backup)?, Some(original.clone()));
        assert!(OpenOptions::new().write(true).open(backup).is_err());
        assert!(doc.undo());
        let mut worker = doc.clone();
        worker.save(&path)?;
        doc.adopt_save(worker);
        // Byte equality alone also passes when the fallback rewrites every byte.
        assert_eq!(Stamp::get(&path)?.unwrap().id, original.id);
        assert_eq!(fs::metadata(&path)?.len(), original.size);
        let mut restored = File::open(&path)?;
        restored.seek(SeekFrom::End(-4))?;
        let mut tail = [0; 4];
        restored.read_exact(&mut tail)?;
        assert_eq!(&tail, b"aaaa");
        Ok(())
    }

    #[test]
    fn interrupted_replace_restores_only_a_vacant_public_name() -> io::Result<()> {
        let folder = tempfile::tempdir()?;
        let backup = folder.path().join("retained");
        let dest = folder.path().join("document.txt");
        fs::write(&backup, b"original")?;
        let stamp = Stamp::get(&backup)?.unwrap();
        fs::write(&dest, b"new external revision")?;
        assert!(rename_checkpoint(&backup, &dest, &stamp, false).is_err());
        assert_eq!(fs::read(&dest)?, b"new external revision");
        assert_eq!(fs::read(&backup)?, b"original");
        fs::remove_file(&dest)?;
        rename_checkpoint(&backup, &dest, &stamp, false)?;
        assert_eq!(Stamp::get(&dest)?, Some(stamp));
        assert_eq!(fs::read(&dest)?, b"original");
        Ok(())
    }
}

pub(super) fn link_count(file: &File) -> io::Result<u32> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.nNumberOfLinks)
}

impl Document {
    /// Preserve the old file by name on its own volume while replacing the
    /// public name. The guard spans cancellation and reacquisition of the RH
    /// oplock, so our own rename never resembles an outside content write.
    pub(super) fn publish_with_checkpoint(
        &mut self,
        dest: &Path,
        staged: &Path,
        stamp: &Stamp,
    ) -> io::Result<bool> {
        let Some(root) = &self.saved else {
            return Ok(false);
        };
        let tree::Body::Leaf(source, 0) = &root.body else {
            return Ok(false);
        };
        if !self.saved_codec.native()
            || root.len != source.len
            || !source.holds_unchanged_file(stamp)
        {
            return Ok(false);
        }
        let Ok(folder) = tempfile::Builder::new()
            .prefix(".linework-undo-")
            .tempdir_in(dest.parent().unwrap())
        else {
            return Ok(false);
        };
        let folder = Arc::new(folder);
        let alias = folder.path().join("revision");
        let mut keep_backup = false;
        let result = source.publish_guarded(dest, &alias, stamp, Some(folder.clone()), || {
            if Stamp::get(dest)?.as_ref() != Some(stamp) {
                return Err(conflict());
            }
            // Have Windows move the displaced file into the backup slot during
            // publication. Pre-linking the live original exposes a full-sized
            // backup alongside the full staging file, even before the commit.
            match replace_version(dest, staged, Some(&alias)) {
                Ok(()) if Stamp::get(&alias).is_ok_and(|s| s.as_ref() == Some(stamp)) => Ok(()),
                Ok(()) => {
                    keep_backup = true;
                    Err(io::Error::other(format!(
                        "The displaced file changed during Save. Its backup is retained at {}",
                        alias.display()
                    )))
                }
                Err(error) => {
                    // Windows can report ERROR_UNABLE_TO_MOVE_REPLACEMENT_2
                    // after moving the original to the backup name. Restore
                    // its checked handle without clobbering a concurrent file.
                    if Stamp::get(dest).is_ok_and(|s| s.as_ref() == Some(stamp)) {
                        return Err(error);
                    }
                    if alias.try_exists().is_ok_and(|exists| !exists) {
                        return Err(error);
                    }
                    if let Err(restore) = rename_checkpoint(&alias, dest, stamp, false) {
                        keep_backup = true;
                        return Err(io::Error::other(format!(
                            "Save failed ({error}). The backup at {} was retained: {restore}",
                            alias.display()
                        )));
                    }
                    Err(error)
                }
            }
        });
        if keep_backup {
            // On failure publish_guarded never takes ownership of the folder.
            let folder = Arc::into_inner(folder).expect("uncommitted backup directory");
            let _ = folder.keep();
        }
        match result {
            Ok(Some(())) => {
                self.previous_disk = Some(Arc::new(DiskCheckpoint {
                    root: self.saved.clone(),
                    codec: self.saved_codec,
                    bom: self.saved_bom,
                    stamp: stamp.clone(),
                    digest: self.saved_disk_digest,
                    path: alias,
                    destination: dest.to_owned(),
                    source: source.clone(),
                }));
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub(super) fn restore_checkpoint(&mut self, dest: &Path) -> io::Result<bool> {
        let Some(previous) = self.previous_disk.clone() else {
            return Ok(false);
        };
        if previous.destination != dest || !tree::same(&previous.root, &self.root)
            || self.codec != previous.codec
            || self.emit_bom != previous.bom
            || !previous.source.holds_unchanged_file(&previous.stamp)
        {
            return Ok(false);
        }
        let Ok(guard) = guard_destination(&previous.path, &previous.stamp) else {
            return Ok(false);
        };
        if link_count(&guard)? != 1 {
            return Ok(false);
        }
        let current = self.stamp.as_ref().ok_or_else(conflict)?;
        let _target = self.verify_disk_revision(dest, current)?;
        let result =
            previous
                .source
                .publish_guarded(&previous.path, dest, &previous.stamp, None, || {
                    if Stamp::get(dest)?.as_ref() != Some(current) {
                        return Err(conflict());
                    }
                    rename_checkpoint(&previous.path, dest, &previous.stamp, true)
                });
        match result {
            Ok(Some(())) => {
                self.stamp = Some(previous.stamp.clone());
                self.path = Some(dest.to_owned());
                self.saved_disk_digest = previous.digest;
                self.saved = self.root.clone();
                self.saved_codec = self.codec;
                self.saved_bom = self.emit_bom;
                self.previous_disk = None;
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(error),
            // Older filesystems may not implement the extended rename class.
            // The regular path independently rechecks the destination again.
            _ => Ok(false),
        }
    }
}
