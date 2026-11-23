//! Bounded writes on volumes with native NTFS transaction support.
//! The unchanged prefix is never staged. TxF owns isolation and crash rollback.
use crate::{checkpoint::link_count, conflict, Stamp};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr::{null, null_mut},
    time::SystemTime,
};
use windows_sys::Win32::{Foundation::*, Storage::FileSystem::*};

pub(super) const MAX_PATCH_BYTES: usize = 1024 * 1024;
// WinNT.h, GetVolumeInformationByHandleW file-system flags.
const SUPPORTS_TRANSACTIONS: u32 = 0x0020_0000;

struct Transaction {
    handle: OwnedHandle,
    committed: bool,
}

impl Transaction {
    fn begin() -> Option<Self> {
        let raw = unsafe { CreateTransaction(null_mut(), null_mut(), 0, 0, 0, 0, null()) };
        if raw == INVALID_HANDLE_VALUE {
            return None;
        }
        Some(Self { handle: unsafe { OwnedHandle::from_raw_handle(raw) }, committed: false })
    }

    fn open(&self, path: &Path, access: u32, share: u32) -> io::Result<File> {
        let wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileTransactedW(
                wide.as_ptr(), access, share, null(), OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL,
                null_mut(), self.handle.as_raw_handle(), null(), null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_handle(handle) })
    }

    fn commit(mut self) -> io::Result<()> {
        if unsafe { CommitTransaction(self.handle.as_raw_handle()) } == 0 {
            let err = io::Error::last_os_error();
            // Never retry a write as a conventional save on an ambiguous commit.
            let mut outcome = 0;
            let queried = unsafe {
                GetTransactionInformation(
                    self.handle.as_raw_handle(), &mut outcome, null_mut(), null_mut(), null_mut(),
                    0, null_mut(),
                )
            };
            if queried == 0 || outcome != TransactionOutcomeCommitted as u32 {
                return Err(err);
            }
        }
        self.committed = true;
        Ok(())
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if !self.committed {
            unsafe { RollbackTransaction(self.handle.as_raw_handle()); }
        }
    }
}

/// The read handle belongs to a DIFFERENT transaction from the writer. It keeps
/// the old bytes readable throughout commit and the source-protection handoff.
/// Fields drop in declaration order, closing the file before ending its transaction.
pub(super) struct ReadSnapshot {
    file: File,
    _transaction: Transaction,
}

impl ReadSnapshot {
    pub(super) fn open(path: &Path, expected: &Stamp) -> io::Result<Option<Self>> {
        let Some(transaction) = Transaction::begin() else { return Ok(None) };
        let file = match transaction.open(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE) {
            Ok(file) => file,
            Err(_) => return Ok(None),
        };
        if Stamp::from(&file)? != *expected {
            return Err(conflict());
        }
        Ok(Some(Self { file, _transaction: transaction }))
    }

    pub(super) fn file(&self) -> &File { &self.file }
}

/// The base identifies this reader's original view. The latest version advances
/// on committed changes while this same handle stays open. Values taken on a
/// reopened handle cannot certify continuity with an earlier observation.
pub(super) fn versions(reader: &File) -> Option<(u32, u32)> {
    use windows_sys::Win32::System::{
        Ioctl::{
            FSCTL_TXFS_GET_TRANSACTED_VERSION, TXFS_GET_TRANSACTED_VERSION,
            TXFS_TRANSACTED_VERSION_NONTRANSACTED,
        },
        IO::DeviceIoControl,
    };

    let mut version = TXFS_GET_TRANSACTED_VERSION {
        ThisBaseVersion: 0,
        LatestVersion: 0,
        ThisMiniVersion: 0,
        FirstMiniVersion: 0,
        LatestMiniVersion: 0,
    };
    let size = std::mem::size_of_val(&version) as u32;
    let mut returned = 0;
    let ok = unsafe {
        DeviceIoControl(
            reader.as_raw_handle(), FSCTL_TXFS_GET_TRANSACTED_VERSION, null(), 0,
            (&mut version as *mut TXFS_GET_TRANSACTED_VERSION).cast(), size, &mut returned,
            null_mut(),
        )
    };
    // NONTRANSACTED and the next value, UNCOMMITTED, are reserved sentinels.
    if ok == 0 || returned != size || version.ThisBaseVersion >= TXFS_TRANSACTED_VERSION_NONTRANSACTED
        || version.LatestVersion >= TXFS_TRANSACTED_VERSION_NONTRANSACTED
        || version.LatestVersion < version.ThisBaseVersion || version.ThisMiniVersion != 0
        || version.FirstMiniVersion != 0 || version.LatestMiniVersion != 0 {
        return None;
    }
    Some((version.ThisBaseVersion, version.LatestVersion))
}

fn eligible(file: &File) -> io::Result<bool> {
    if link_count(file)? != 1 {
        return Ok(false);
    }
    let attrs = file.metadata()?.file_attributes();
    if attrs & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_ENCRYPTED | FILE_ATTRIBUTE_REPARSE_POINT
        | FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_OFFLINE) != 0 {
        return Ok(false);
    }
    let mut fs_name = [0u16; 32];
    let mut flags = 0;
    let ok = unsafe {
        GetVolumeInformationByHandleW(file.as_raw_handle(), null_mut(), 0, null_mut(), null_mut(),
            &mut flags, fs_name.as_mut_ptr(), fs_name.len() as u32)
    };
    Ok(ok != 0 && flags & SUPPORTS_TRANSACTIONS != 0 && fs_name.starts_with(&[78, 84, 70, 83, 0]))
}

/// Returns None only before any transaction data has been changed. A mismatch
/// with the user's known disk revision is a conflict, never a fallback signal.
/// For length changes the caller must supply BOTH entire tails. For equal-length
/// overwrites a bounded interior range is sufficient.
pub(super) fn rewrite(
    path: &Path, expected: &Stamp, offset: u64, before: &[u8], after: &[u8], new_len: u64,
    verify: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<Option<Stamp>> {
    let Some(name) = path.file_name() else { return Ok(None) };
    if before.len().max(after.len()) > MAX_PATCH_BYTES || name.encode_wide().any(|c| c == b':' as u16) {
        return Ok(None);
    }
    let Some(old_end) = offset.checked_add(before.len() as u64) else { return Ok(None) };
    let Some(new_end) = offset.checked_add(after.len() as u64) else { return Ok(None) };
    let is_tail = old_end == expected.size && new_end == new_len;
    let is_overwrite = before.len() == after.len() && new_len == expected.size && old_end <= expected.size;
    if !is_tail && !is_overwrite {
        return Ok(None);
    }
    // A normal reader pins the public name and identity across writer close
    // and synchronous commit. It allows the transaction's write access, but
    // omits FILE_SHARE_DELETE so another process cannot swap in a different file.
    let pin = match OpenOptions::new().read(true).share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE).open(path) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    if Stamp::from(&pin)? != *expected { return Err(conflict()); }
    if !eligible(&pin)? { return Ok(None); }
    let Some(transaction) = Transaction::begin() else { return Ok(None) };
    let mut file = match transaction.open(path, GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    if Stamp::from(&file)? != *expected { return Err(conflict()); }
    if link_count(&file)? != 1 { return Ok(None); }
    verify(&mut file)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut previous = vec![0; before.len()];
    file.read_exact(&mut previous)?;
    if previous != before { return Err(conflict()); }
    if before == after && new_len == expected.size { return Ok(Some(expected.clone())); }

    file.seek(SeekFrom::Start(offset))?;
    file.write_all(after)?;
    if new_len != expected.size {
        file.seek(SeekFrom::Start(new_len))?;
        if unsafe { SetEndOfFile(file.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    file.set_modified(SystemTime::now())?;
    file.sync_all()?;
    let next = Stamp::from(&file)?;
    if next.volume != expected.volume || next.id != expected.id || next.size != new_len {
        return Err(io::Error::other("Transaction returned an unexpected file identity or length"));
    }
    drop(file);
    #[cfg(test)]
    tests::interruptible_commit_boundary(path)?;
    transaction.commit()?;
    // The committed change must never be reported as an aborted transaction due
    // to a subsequent fallible metadata read. The stamp was captured in its view.
    Ok(Some(next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, fs, process::{Command, Stdio}, thread, time::{Duration, Instant}};

    // Only the sacrificial process gets these variables; the release binary has
    // no test switch. Kernel transaction rollback is exercised by terminating
    // the process in the production path after its verified write and flush.
    const KILL_TARGET: &str = "LINEWORK_TXF_TEST_KILL_TARGET";
    const KILL_READY: &str = "LINEWORK_TXF_TEST_KILL_READY";

    pub(super) fn interruptible_commit_boundary(path: &Path) -> io::Result<()> {
        if env::var_os(KILL_TARGET).as_deref() != Some(path.as_os_str()) {
            return Ok(());
        }
        fs::write(env::var_os(KILL_READY).expect("missing crash-test signal path"), b"flushed")?;
        thread::sleep(Duration::from_secs(30));
        Err(io::Error::new(io::ErrorKind::TimedOut, "Crash test did not terminate its child"))
    }

    #[test]
    fn flushed_writer_child() -> io::Result<()> {
        let Some(target) = env::var_os(KILL_TARGET) else { return Ok(()) };
        let path = Path::new(&target);
        let old = Stamp::get(path)?.unwrap();
        let result = rewrite(path, &old, 7, b"old-tail", b"NEW", 10, |_| Ok(()))?;
        Err(io::Error::other(format!("Child passed the precommit barrier: {result:?}")))
    }

    #[test]
    fn process_death_after_flushing_rolls_back_bytes_length_and_identity() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("document.txt");
        let ready = dir.path().join("writer-ready");
        let original = b"prefix:old-tail";
        fs::write(&path, original)?;
        if !eligible(&File::open(&path)?)? {
            return Err(io::Error::other("This test requires local NTFS transaction support. It did not exercise a transaction."));
        }
        let stamp = Stamp::get(&path)?;
        let mut child = Command::new(env::current_exe()?)
            .args(["--exact", "transactional::tests::flushed_writer_child", "--test-threads=1"])
            .env(KILL_TARGET, &path)
            .env(KILL_READY, &ready)
            .stdin(Stdio::null())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while !ready.exists() && child.try_wait()?.is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let flushed = ready.exists();
        let before_kill = fs::read(&path);
        let _ = child.kill();
        let status = child.wait()?;
        assert!(flushed && !status.success(), "The private write was never interrupted");
        assert_eq!(before_kill?, original, "A live observer saw the uncommitted change");
        assert_eq!(fs::read(&path)?, original, "The terminated writer's change escaped rollback");
        assert_eq!(Stamp::get(&path)?, stamp);
        Ok(())
    }

    #[test]
    fn committed_tail_preserves_identity_other_stream_and_pinned_old_bytes() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("tail.txt");
        fs::write(&path, b"prefix:old-tail")?;
        if !eligible(&File::open(&path)?)? {
            return Err(io::Error::other("This test requires local NTFS transaction support. It did not exercise a transaction."));
        }
        let stream = dir.path().join("tail.txt:metadata");
        fs::write(&stream, b"keep-stream")?;
        let old = Stamp::get(&path)?.unwrap();
        let snapshot = ReadSnapshot::open(&path, &old)?.expect("missing read snapshot");
        let new = rewrite(&path, &old, 7, b"old-tail", b"NEW", 10, |_| Ok(()))?
            .expect("transaction unavailable after capability probe");
        assert_eq!(Stamp::get(&path)?, Some(new.clone()), "The returned stamp must match disk");
        assert_eq!(new.id, old.id);
        assert_eq!(fs::read(&path)?, b"prefix:NEW");
        assert_eq!(fs::read(&stream)?, b"keep-stream");
        let mut old_view = Vec::new();
        snapshot.file().read_to_end(&mut old_view)?;
        assert_eq!(old_view, b"prefix:old-tail");
        drop(snapshot);
        let same = rewrite(&path, &new, 7, b"NEW", b"NEW", 10, |_| Ok(()))?;
        assert_eq!(same, Some(new.clone()));
        assert_eq!(Stamp::get(&path)?, same, "A no-op save must preserve the timestamp");
        let restored = rewrite(&path, &new, 7, b"NEW", b"old-tail", 15, |_| Ok(()))?;
        assert_eq!(Stamp::get(&path)?, restored);
        assert_eq!(fs::read(&path)?, b"prefix:old-tail");
        Ok(())
    }

    #[test]
    fn hard_links_and_unbounded_length_changes_decline_without_mutation() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("document.txt");
        let alias = dir.path().join("other-name.txt");
        fs::write(&path, b"prefix:old-tail")?;
        let stamp = Stamp::get(&path)?.unwrap();
        let never = |_: &mut File| -> io::Result<()> { panic!("Unexpected eligible candidate") };
        assert!(rewrite(&path, &stamp, 2, b"e", b"edited", 20, never)?.is_none());
        fs::hard_link(&path, &alias)?;
        assert!(rewrite(&path, &stamp, 7, b"old-tail", b"NEW", 10, never)?.is_none());
        assert_eq!(fs::read(&alias)?, b"prefix:old-tail");
        assert_eq!(Stamp::get(&path)?, Some(stamp));
        Ok(())
    }
}
