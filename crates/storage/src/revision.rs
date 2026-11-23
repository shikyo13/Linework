use crate::{Document, Stamp, conflict};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::Path,
};

/// Compare the *saved* bytes with the destination immediately before replacing
/// it. Size, file ID and a restored/coarse modification time can all agree even
/// after an external writer has rewritten the content.
impl Document {
    pub(crate) fn verify_disk_revision(&self, path: &Path, expected: &Stamp) -> io::Result<File> {
        let mut file = guard_destination(path, expected)?;
        #[cfg(windows)]
        if let Some(root) = &self.saved {
            let mut protected = false;
            root.pieces(&mut |s, _, _| {
                protected |= s.holds_unchanged_file(expected);
                Ok(())
            })?;
            if protected {
                return Ok(file);
            }
        }
        let mut buffer = vec![0; 1024 * 1024];
        if let Some(expected) = self.saved_disk_digest {
            let mut digest = Sha256::new();
            loop {
                let got = file.read(&mut buffer)?;
                if got == 0 {
                    break;
                }
                digest.update(&buffer[..got]);
            }
            if <[u8; 32]>::from(digest.finalize()) != expected {
                return Err(conflict());
            }
        } else {
            let mut saved = self.clone();
            saved.root = self.saved.clone();
            let mut compare = Compare {
                file: &mut file,
                buffer: &mut buffer,
            };
            self.saved_codec
                .write(&saved, &mut compare, self.saved_bom)?;
            if file.read(&mut buffer[..1])? != 0 {
                return Err(conflict());
            }
        }
        Ok(file)
    }
}

pub(crate) fn guard_destination(path: &Path, expected: &Stamp) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_SEQUENTIAL_SCAN, FILE_SHARE_DELETE, FILE_SHARE_READ,
        };
        // ReplaceFileW opens the old destination for read/delete. Keep
        // writers out during verification and through that atomic replace.
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN);
    }
    let file = options.open(path).map_err(|e| {
        if e.raw_os_error() == Some(32) {
            conflict()
        } else {
            e
        }
    })?;
    if &Stamp::from(&file)? != expected {
        return Err(conflict());
    }
    Ok(file)
}

/// Hash each successfully written encoded byte while staging the atomic save.
/// A later Save can verify this checkpoint with one sequential destination read.
pub(crate) struct CheckpointWriter<W> {
    inner: W,
    digest: Sha256,
}
impl<W: Write> CheckpointWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self {
            inner,
            digest: Sha256::new(),
        }
    }
    pub(crate) fn finish(self) -> [u8; 32] {
        self.digest.finalize().into()
    }
}
impl<W: Write> Write for CheckpointWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.digest.update(&bytes[..written]);
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_hashes_only_bytes_accepted_by_partial_writes() {
        struct PartialWriter;
        impl Write for PartialWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                Ok(buf.len().min(3))
            }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
        }
        let bytes = b"\xef\xbb\xbfline\r\n";
        let mut writer = CheckpointWriter::new(PartialWriter);
        writer.write_all(bytes).unwrap();
        assert_eq!(writer.finish(), <[u8;32]>::from(Sha256::digest(bytes)));
    }
}

struct Compare<'a> {
    file: &'a mut File,
    buffer: &'a mut [u8],
}
impl Write for Compare<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for chunk in bytes.chunks(self.buffer.len()) {
            let actual = &mut self.buffer[..chunk.len()];
            if let Err(e) = self.file.read_exact(actual) {
                return Err(if e.kind() == io::ErrorKind::UnexpectedEof {
                    conflict()
                } else {
                    e
                });
            }
            if actual != chunk {
                return Err(conflict());
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
