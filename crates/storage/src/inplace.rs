//! A bounded change written straight into the file, with no transaction. The
//! caller keeps every other writer away from the rest of the file; a flushed
//! record of the replaced bytes covers a crash in the middle of the write.
//! Unlike a committed NTFS transaction, this leaves the file in Windows' cache.
use crate::{checkpoint::link_count, conflict, save_journal, transactional, Stamp};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    time::SystemTime,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_SHARE_READ,
};

/// Returns None only before anything has been changed. A mismatch with the
/// known disk revision is a conflict, never a fallback signal. `alone` runs
/// once the writer is open and no other writer can be. `seal` runs after the
/// change is on disk and before the writer closes, so the caller can lock the
/// changed range while still no other writer can be open.
pub(super) fn rewrite(
    path: &Path, expected: &Stamp, offset: u64, before: &[u8], after: &[u8], new_len: u64, journal: &Path,
    verify: impl FnOnce(&mut File) -> io::Result<()>, alone: &dyn Fn(), seal: &dyn Fn(),
) -> io::Result<Option<Stamp>> {
    if !transactional::bounded(path, expected, offset, before, after, new_len) {
        return Ok(None);
    }
    // Sharing only reads keeps every other writer out for as long as this is open.
    let mut file = match OpenOptions::new().read(true).write(true).share_mode(FILE_SHARE_READ).open(path) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    alone();
    let outcome = change(&mut file, path, expected, offset, before, after, new_len, journal, verify);
    // On every way out, the caller's protection is back before this writer,
    // the only thing keeping other writers away meanwhile, is closed.
    seal();
    drop(file);
    outcome
}

/// The file system is NTFS on this computer. Range locks and an open section
/// are only known to hold other programs off there; a file server applies its
/// own rules to other machines, and other file systems have not been tested.
fn local_ntfs(file: &File) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileRemoteProtocolInfo, GetFileInformationByHandleEx, GetVolumeInformationByHandleW,
        FILE_REMOTE_PROTOCOL_INFO,
    };
    let mut name = [0u16; 32];
    let named = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(), std::ptr::null_mut(), 0, std::ptr::null_mut(), std::ptr::null_mut(),
            std::ptr::null_mut(), name.as_mut_ptr(), name.len() as u32,
        )
    };
    // This query succeeds only for a file reached over a network protocol.
    let mut remote: FILE_REMOTE_PROTOCOL_INFO = unsafe { std::mem::zeroed() };
    let is_remote = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(), FileRemoteProtocolInfo, (&mut remote as *mut FILE_REMOTE_PROTOCOL_INFO).cast(),
            std::mem::size_of::<FILE_REMOTE_PROTOCOL_INFO>() as u32,
        )
    } != 0;
    named != 0 && !is_remote && name.starts_with(&[78, 84, 70, 83, 0])
}

#[allow(clippy::too_many_arguments)]
fn change(
    file: &mut File, path: &Path, expected: &Stamp, offset: u64, before: &[u8], after: &[u8], new_len: u64,
    journal: &Path, verify: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<Option<Stamp>> {
    if Stamp::from(file)? != *expected { return Err(conflict()); }
    let unsuitable = FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_READONLY
        | FILE_ATTRIBUTE_OFFLINE;
    if link_count(file)? != 1 || file.metadata()?.file_attributes() & unsuitable != 0 || !local_ntfs(file) {
        return Ok(None);
    }
    verify(file)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut previous = vec![0; before.len()];
    file.read_exact(&mut previous)?;
    if previous != before { return Err(conflict()); }
    if before == after && new_len == expected.size { return Ok(Some(expected.clone())); }

    // Without a record there is no crash protection, and nothing has been
    // changed yet: let the ordinary save take over.
    let Ok(pending) = save_journal::Pending::record(journal, path, expected, offset, before, after, new_len) else {
        return Ok(None);
    };
    let written = (|| {
        #[cfg(test)]
        tests::interruptible_write(path, file, offset, after)?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(after)?;
        if new_len != expected.size { file.set_len(new_len)?; }
        file.set_modified(SystemTime::now())?;
        file.sync_all()?;
        let next = Stamp::from(file)?;
        if next.volume != expected.volume || next.id != expected.id || next.size != new_len {
            return Err(io::Error::other("The file changed identity or length while it was being written"));
        }
        Ok(next)
    })();
    match written {
        Ok(next) => {
            // The file is whole and is what the document holds. Any older
            // record for it describes a state that no longer applies, and
            // replaying one later could put superseded bytes back.
            pending.finish_and_retire(journal, &next);
            Ok(Some(next))
        }
        Err(error) => {
            // Put the old bytes back now if the disk allows it. If it does
            // not, the record stays and the next start tries again.
            if save_journal::restore(file, expected, offset, before).is_ok() {
                pending.finish();
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, fs, process::{Command, Stdio}, thread, time::{Duration, Instant}};

    // Only the sacrificial process gets these variables. It stops in the real
    // write path with half of the new bytes flushed to disk, and is then killed.
    const KILL_TARGET: &str = "LINEWORK_INPLACE_TEST_KILL_TARGET";
    const KILL_READY: &str = "LINEWORK_INPLACE_TEST_KILL_READY";
    const KILL_JOURNAL: &str = "LINEWORK_INPLACE_TEST_KILL_JOURNAL";

    pub(super) fn interruptible_write(path: &Path, file: &mut File, offset: u64, after: &[u8]) -> io::Result<()> {
        if env::var_os(KILL_TARGET).as_deref() != Some(path.as_os_str()) {
            return Ok(());
        }
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(&after[..after.len() / 2])?;
        file.sync_all()?;
        fs::write(env::var_os(KILL_READY).expect("missing crash-test signal path"), b"half written")?;
        thread::sleep(Duration::from_secs(30));
        Err(io::Error::new(io::ErrorKind::TimedOut, "Crash test did not terminate its child"))
    }

    #[test]
    fn half_writer_child() -> io::Result<()> {
        let Some(target) = env::var_os(KILL_TARGET) else { return Ok(()) };
        let path = Path::new(&target);
        let journal = env::var_os(KILL_JOURNAL).expect("missing crash-test journal folder");
        let old = Stamp::get(path)?.unwrap();
        let result = rewrite(path, &old, 7, b"old-tail", b"REPLACEMENT-TAIL", 23, Path::new(&journal), |_| Ok(()), &|| (), &|| ())?;
        Err(io::Error::other(format!("Child passed the write barrier: {result:?}")))
    }

    #[test]
    fn process_death_in_the_middle_of_the_write_is_undone_at_the_next_start() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("document.txt");
        let ready = dir.path().join("writer-ready");
        let journal = dir.path().join("journal");
        let original = b"prefix:old-tail";
        fs::write(&path, original)?;
        let stamp = Stamp::get(&path)?;
        let mut child = Command::new(env::current_exe()?)
            .args(["--exact", "inplace::tests::half_writer_child", "--test-threads=1"])
            .env(KILL_TARGET, &path)
            .env(KILL_READY, &ready)
            .env(KILL_JOURNAL, &journal)
            .stdin(Stdio::null())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while !ready.exists() && child.try_wait()?.is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let half = ready.exists();
        let _ = child.kill();
        let status = child.wait()?;
        assert!(half && !status.success(), "The write was never interrupted");
        assert_eq!(fs::read(&path)?, b"prefix:REPLACEM", "The test did not leave a half-written file");
        save_journal::replay(&journal);
        assert_eq!(fs::read(&path)?, original, "The half-written change was not undone");
        assert_eq!(Stamp::get(&path)?, stamp);
        assert_eq!(fs::read_dir(&journal)?.count(), 0);
        Ok(())
    }

    #[test]
    fn a_change_keeps_identity_and_other_streams_and_declines_what_it_cannot_bound() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("tail.txt");
        let journal = dir.path().join("journal");
        fs::write(&path, b"prefix:old-tail")?;
        let stream = dir.path().join("tail.txt:metadata");
        fs::write(&stream, b"keep-stream")?;
        let old = Stamp::get(&path)?.unwrap();
        let sealed = std::cell::Cell::new(0);
        let seal = || sealed.set(sealed.get() + 1);
        let new = rewrite(&path, &old, 7, b"old-tail", b"NEW", 10, &journal, |_| Ok(()), &|| (), &seal)?.expect("declined");
        assert_eq!(Stamp::get(&path)?, Some(new.clone()), "The returned stamp must match disk");
        assert_eq!(new.id, old.id);
        assert_eq!(fs::read(&path)?, b"prefix:NEW");
        assert_eq!(fs::read(&stream)?, b"keep-stream");
        assert_eq!(sealed.get(), 1);
        let same = rewrite(&path, &new, 7, b"NEW", b"NEW", 10, &journal, |_| Ok(()), &|| (), &seal)?;
        assert_eq!(same, Some(new.clone()));
        assert_eq!(Stamp::get(&path)?, same, "A no-op save must preserve the timestamp");
        // The disk no longer holds what the caller believes: a conflict, untouched.
        assert_eq!(
            rewrite(&path, &new, 7, b"OLD", b"XYZ", 10, &journal, |_| Ok(()), &|| (), &seal).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        // Every way out that opened the writer hands protection back before closing it.
        assert_eq!(sealed.get(), 3);
        let never = |_: &mut File| -> io::Result<()> { panic!("Unexpected eligible candidate") };
        assert!(rewrite(&path, &new, 2, b"e", b"edited", 15, &journal, never, &|| (), &seal)?.is_none());
        assert_eq!(sealed.get(), 3, "A change that cannot be bounded never opens the file");
        // With nowhere to put a record there is no crash protection: decline, untouched.
        let blocked = dir.path().join("not-a-folder");
        fs::write(&blocked, b"")?;
        assert!(rewrite(&path, &new, 7, b"NEW", b"XYZ", 10, &blocked, |_| Ok(()), &|| (), &seal)?.is_none());
        assert_eq!(fs::read(&path)?, b"prefix:NEW");
        assert_eq!(sealed.get(), 4);
        let alias = dir.path().join("other-name.txt");
        fs::hard_link(&path, &alias)?;
        assert!(rewrite(&path, &new, 7, b"NEW", b"old-tail", 15, &journal, never, &|| (), &seal)?.is_none());
        assert_eq!(sealed.get(), 5);
        assert_eq!(fs::read(&alias)?, b"prefix:NEW");
        assert_eq!(fs::read_dir(&journal)?.count(), 0);
        Ok(())
    }

    #[test]
    fn a_later_save_retires_an_older_record_so_it_cannot_undo_that_save() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("word.txt");
        let journal = dir.path().join("journal");
        // "cat" was saved as "dog", and the record of that save was never removed.
        fs::write(&path, b"prefix:cat")?;
        let first = Stamp::get(&path)?.unwrap();
        drop(save_journal::Pending::record(&journal, &path, &first, 7, b"cat", b"dog", 10)?);
        fs::write(&path, b"prefix:dog")?;
        assert_eq!(fs::read_dir(&journal)?.count(), 1);
        // A later save makes it "dot": two letters of the new text, one of the old.
        let second = Stamp::get(&path)?.unwrap();
        let saved = rewrite(&path, &second, 7, b"dog", b"dot", 10, &journal, |_| Ok(()), &|| (), &|| ())?;
        assert!(saved.is_some());
        assert_eq!(fs::read_dir(&journal)?.count(), 0, "The older record outlived a later save");
        save_journal::replay(&journal);
        assert_eq!(fs::read(&path)?, b"prefix:dot");
        Ok(())
    }
}
