use crate::{hash, Document, Stamp, conflict};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::Path,
    sync::mpsc::{channel, sync_channel, Receiver, SyncSender},
    thread,
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
            let mut digest = hash::Hasher::new();
            loop {
                let got = file.read(&mut buffer)?;
                if got == 0 {
                    break;
                }
                digest.update(&buffer[..got]);
            }
            if hash::finish(&digest) != expected {
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

const BLOCK: usize = 1024 * 1024;
/// Blocks waiting at each stage. Past this the stage before it waits.
const DEPTH: usize = 4;

/// Files from this size on are staged past the Windows file cache, which on a
/// fast disk is about twice as quick as writing through it and flushing.
#[cfg(not(test))]
const DIRECT_FROM: u64 = 64 * 1024 * 1024;
#[cfg(test)]
const DIRECT_FROM: u64 = 1024 * 1024;
const SECTOR: usize = 4096;
const RUN: usize = 4 * 1024 * 1024;

/// Memory aligned to a disk sector, as an unbuffered write requires.
struct Aligned(std::ptr::NonNull<u8>);
unsafe impl Send for Aligned {}
impl Aligned {
    fn layout() -> std::alloc::Layout {
        std::alloc::Layout::from_size_align(RUN, SECTOR).expect("constant layout")
    }
    fn new() -> Self {
        let memory = unsafe { std::alloc::alloc_zeroed(Self::layout()) };
        Self(std::ptr::NonNull::new(memory).unwrap_or_else(|| std::alloc::handle_alloc_error(Self::layout())))
    }
    fn bytes(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.0.as_ptr(), RUN) }
    }
}
impl Drop for Aligned {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.0.as_ptr(), Self::layout()) }
    }
}

/// The file a save is staged in. A large one is written unbuffered, in whole
/// sectors, through a second handle; its exact length is set at the end. A
/// volume that refuses that is written through the cache like a small file.
pub(crate) struct Staged<'a> {
    file: &'a File,
    direct: Option<(File, Aligned)>,
    filled: usize,
    written: u64,
}
impl<'a> Staged<'a> {
    /// `expected` is the encoded size when it is known before writing.
    pub(crate) fn new(file: &'a File, path: &Path, expected: Option<u64>) -> Self {
        let direct = expected
            .filter(|size| *size >= DIRECT_FROM)
            .and_then(|size| Self::unbuffered(file, path, size));
        Self {
            file,
            direct,
            filled: 0,
            written: 0,
        }
    }
    #[cfg(windows)]
    fn unbuffered(file: &File, path: &Path, size: u64) -> Option<(File, Aligned)> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_NO_BUFFERING;
        let handle = OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_NO_BUFFERING)
            .open(path)
            .ok()?;
        // Growing the file once, not with every write.
        file.set_len(size.next_multiple_of(SECTOR as u64)).ok()?;
        Some((handle, Aligned::new()))
    }
    #[cfg(not(windows))]
    fn unbuffered(_: &File, _: &Path, _: u64) -> Option<(File, Aligned)> {
        None
    }
    /// Write the collected run, padded to a whole sector.
    fn send(&mut self) -> io::Result<()> {
        let Some((handle, memory)) = &mut self.direct else {
            return Ok(());
        };
        let padded = self.filled.next_multiple_of(SECTOR);
        memory.bytes()[self.filled..padded].fill(0);
        match handle.write_all(&memory.bytes()[..padded]) {
            Ok(()) => {
                self.written += self.filled as u64;
                self.filled = 0;
                Ok(())
            }
            // A volume that cannot do this says so on the first write, when
            // nothing is in the file yet. Carry on through the cache.
            Err(_) if self.written == 0 => {
                let (_, mut memory) = self.direct.take().expect("checked above");
                self.file.set_len(0)?;
                let mut file = self.file;
                file.write_all(&memory.bytes()[..self.filled])
            }
            Err(e) => Err(e),
        }
    }
}
impl Write for Staged<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some((_, memory)) = &mut self.direct else {
            let mut file = self.file;
            return file.write(bytes);
        };
        let count = bytes.len().min(RUN - self.filled);
        memory.bytes()[self.filled..self.filled + count].copy_from_slice(&bytes[..count]);
        self.filled += count;
        if self.filled == RUN {
            self.send()?;
        }
        Ok(count)
    }
    /// Ends an unbuffered file: the last sector, then the exact length.
    fn flush(&mut self) -> io::Result<()> {
        self.send()?;
        if self.direct.take().is_some() {
            self.file.set_len(self.written)?;
        }
        Ok(())
    }
}

/// Collects encoded bytes into blocks for the hashing thread.
struct Blocks {
    block: Vec<u8>,
    filled: SyncSender<Vec<u8>>,
    spare: Receiver<Vec<u8>>,
}
impl Blocks {
    fn send(&mut self) -> io::Result<()> {
        let mut next = self
            .spare
            .try_recv()
            .unwrap_or_else(|_| Vec::with_capacity(BLOCK));
        next.clear();
        // The receiver only goes away early when the write failed. That
        // thread's own error is the one reported.
        self.filled
            .send(std::mem::replace(&mut self.block, next))
            .map_err(|_| io::ErrorKind::BrokenPipe.into())
    }
}
impl Write for Blocks {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = bytes.len().min(BLOCK - self.block.len());
        self.block.extend_from_slice(&bytes[..count]);
        if self.block.len() == BLOCK {
            self.send()?;
        }
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Stage an atomic save: `produce` writes the encoded document, which is
/// hashed and written to `out` by two further threads, so reading, hashing
/// and writing overlap. Returns the BLAKE3 digest of exactly the bytes written. A
/// later Save can verify this checkpoint with one sequential destination read.
pub(crate) fn stage(
    out: impl Write + Send,
    produce: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<[u8; 32]> {
    thread::scope(|scope| {
        let (filled, to_hash) = sync_channel::<Vec<u8>>(DEPTH);
        let (hashed, to_write) = sync_channel::<Vec<u8>>(DEPTH);
        let (recycle, spare) = channel();
        let hasher = scope.spawn(move || {
            let mut digest = hash::Hasher::new();
            for block in to_hash {
                digest.update(&block);
                if hashed.send(block).is_err() {
                    return None;
                }
            }
            Some(hash::finish(&digest))
        });
        let writer = scope.spawn(move || -> io::Result<()> {
            let mut out = out;
            for block in to_write {
                out.write_all(&block)?;
                let _ = recycle.send(block);
            }
            out.flush()
        });
        let mut blocks = Blocks {
            block: Vec::with_capacity(BLOCK),
            filled,
            spare,
        };
        let produced = produce(&mut blocks).and_then(|()| blocks.send());
        // Closing the first channel ends the hashing thread, which ends the writer.
        drop(blocks);
        let digest = hasher.join().expect("hashing thread");
        writer.join().expect("writing thread")?;
        produced?;
        digest.ok_or_else(|| io::ErrorKind::BrokenPipe.into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_writes_every_byte_in_order_and_hashes_the_same_bytes() -> io::Result<()> {
        let bytes: Vec<u8> = (0..5 * BLOCK + 12_345).map(|i| (i * 31 % 251) as u8).collect();
        let mut written = Vec::new();
        let digest = stage(&mut written, |out| {
            // Uneven pieces, including ones that end exactly on a block.
            for piece in bytes.chunks(BLOCK / 3 + 7) {
                out.write_all(piece)?;
            }
            Ok(())
        })?;
        assert!(written == bytes);
        assert_eq!(digest, hash::of(&bytes));
        assert_eq!(stage(&mut Vec::new(), |_| Ok(()))?, hash::of(b""));
        Ok(())
    }

    #[test]
    fn a_large_file_staged_past_the_cache_has_the_exact_bytes_and_length() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        // Not a whole number of sectors or runs, and one that is.
        for size in [2 * RUN + SECTOR + 123, RUN, DIRECT_FROM as usize - 1] {
            let bytes: Vec<u8> = (0..size).map(|i| (i * 7 % 253) as u8).collect();
            let staged = tempfile::NamedTempFile::new_in(dir.path())?;
            let out = Staged::new(staged.as_file(), staged.path(), Some(size as u64));
            assert_eq!(out.direct.is_some(), cfg!(windows) && size as u64 >= DIRECT_FROM);
            let digest = stage(out, |out| out.write_all(&bytes))?;
            assert_eq!(digest, hash::of(&bytes));
            assert_eq!(staged.as_file().metadata()?.len(), size as u64);
            assert!(std::fs::read(staged.path())? == bytes);
        }
        // A size that turned out wrong is corrected, shorter or longer.
        let bytes = vec![9u8; RUN + 77];
        for expected in [3 * RUN as u64, DIRECT_FROM] {
            let staged = tempfile::NamedTempFile::new_in(dir.path())?;
            stage(Staged::new(staged.as_file(), staged.path(), Some(expected)), |out| out.write_all(&bytes))?;
            assert!(std::fs::read(staged.path())? == bytes);
        }
        Ok(())
    }

    #[test]
    fn a_failed_write_or_a_failed_source_is_the_error_reported() {
        struct Full(usize);
        impl Write for Full {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.0 < bytes.len() {
                    return Err(io::Error::new(io::ErrorKind::StorageFull, "disk full"));
                }
                self.0 -= bytes.len();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let block = vec![7u8; BLOCK];
        let error = stage(Full(3 * BLOCK), |out| {
            for _ in 0..64 {
                out.write_all(&block)?;
            }
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        let error = stage(Full(usize::MAX), |out| {
            out.write_all(&block)?;
            Err(io::Error::new(io::ErrorKind::InvalidData, "cannot encode"))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
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
