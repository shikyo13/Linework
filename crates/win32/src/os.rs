//! What the front end takes from `std::os::windows`, with the meaning macOS can
//! give each.
pub mod ffi {
    use std::ffi::{OsStr, OsString};

    pub trait OsStrExt {
        fn encode_wide(&self) -> std::vec::IntoIter<u16>;
    }
    impl OsStrExt for OsStr {
        fn encode_wide(&self) -> std::vec::IntoIter<u16> {
            self.to_string_lossy().encode_utf16().collect::<Vec<_>>().into_iter()
        }
    }
    pub trait OsStringExt {
        fn from_wide(wide: &[u16]) -> Self;
    }
    impl OsStringExt for OsString {
        fn from_wide(wide: &[u16]) -> Self {
            String::from_utf16_lossy(wide).into()
        }
    }
}

pub mod fs {
    use std::fs::{File, Metadata, OpenOptions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::path::Path;

    unsafe extern "C" {
        fn flock(descriptor: i32, operation: i32) -> i32;
    }

    pub trait OpenOptionsExt {
        /// The front end passes 0: nobody else may have the file open. Here the
        /// file is opened and locked whole without waiting, and the lock lasts
        /// while the file stays open, so a second opener fails as on Windows.
        fn share_mode(&mut self, mode: u32) -> Exclusive<'_>;
    }
    impl OpenOptionsExt for OpenOptions {
        fn share_mode(&mut self, mode: u32) -> Exclusive<'_> {
            Exclusive { options: self, exclusive: mode == 0 }
        }
    }
    pub struct Exclusive<'a> {
        options: &'a mut OpenOptions,
        exclusive: bool,
    }
    impl Exclusive<'_> {
        pub fn open(&mut self, path: impl AsRef<Path>) -> io::Result<File> {
            let file = self.options.open(path)?;
            // 2 | 4: an exclusive lock, without waiting.
            if self.exclusive && unsafe { flock(file.as_raw_fd(), 2 | 4) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(file)
        }
    }

    pub trait MetadataExt {
        fn file_attributes(&self) -> u32;
    }
    impl MetadataExt for Metadata {
        /// Only the reparse-point bit is asked for, and a link is its nearest kin.
        fn file_attributes(&self) -> u32 {
            if self.file_type().is_symlink() { 0x400 } else { 0 }
        }
    }
}

pub mod process {
    pub trait CommandExt {
        fn creation_flags(&mut self, flags: u32) -> &mut Self;
    }
    impl CommandExt for std::process::Command {
        /// The flag asks Windows for no console window; there is none to hide here.
        fn creation_flags(&mut self, _: u32) -> &mut Self {
            self
        }
    }
}
