//! What a save needs from macOS that the portable path does not give.
//!
//! A saved file is a new file renamed over the old one. Unless it is told
//! otherwise it has the temporary file's owner-only permissions and none of
//! what the old file carried, so a saved script stops being executable and a
//! tagged file loses its tags.
use std::{
    fs::{File, OpenOptions, Permissions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::Path,
};
use tempfile::NamedTempFile;

/// Windows refuses to replace a file marked read-only. A rename here would
/// replace one without asking, so the same question is put to the file first.
fn writable(dest: &Path) -> io::Result<()> {
    OpenOptions::new().write(true).open(dest).map(drop)
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
