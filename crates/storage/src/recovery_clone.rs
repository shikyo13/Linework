//! Capture a read-only recovery base using filesystem copy-on-write where available.
use crate::{source::read_at, Document, Stamp};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Seek, SeekFrom, Write},
    mem::size_of_val,
    os::windows::io::AsRawHandle,
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};
use windows_sys::Win32::{
    Storage::FileSystem::GetVolumeInformationByHandleW,
    System::{
        Ioctl::{DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE, FSCTL_SET_SPARSE},
        SystemServices::FILE_SUPPORTS_BLOCK_REFCOUNTING,
        IO::DeviceIoControl,
    },
};

fn clone_volume(file: &File) -> Option<u32> {
    let mut flags = 0;
    let mut serial = 0;
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            ptr::null_mut(),
            0,
            &mut serial,
            ptr::null_mut(),
            &mut flags,
            ptr::null_mut(),
            0,
        )
    };
    (ok != 0 && flags & FILE_SUPPORTS_BLOCK_REFCOUNTING != 0).then_some(serial)
}

/// Only the private destination is opened for writing. A clone is accepted after
/// comparison with the frozen piece tree, even if a source rename or write raced it.
pub(super) fn capture(
    doc: &Document,
    out: &mut File,
    cancel: &AtomicBool,
) -> io::Result<Option<(u64, [u8; 32])>> {
    let Some(volume) = clone_volume(out) else {
        return Ok(None);
    };
    let (Some(path), Some(stamp), Some(root)) = (&doc.path, &doc.stamp, &doc.saved) else {
        return Ok(None);
    };
    if !doc.saved_codec.native() || stamp.volume != volume {
        return Ok(None);
    }
    let prefix = if doc.saved_bom {
        doc.saved_codec.bom()
    } else {
        &[]
    };
    let head = prefix.len() as u64;
    if root.len.checked_add(head) != Some(stamp.size) {
        return Ok(None);
    }
    let source = File::open(path)?;
    if Stamp::from(&source)? != *stamp {
        return Ok(None);
    }
    let mut returned = 0;
    // A sparse destination reserves no file-sized allocation during pre-sizing
    // or when the first clone request is rejected by a driver.
    if unsafe {
        DeviceIoControl(
            out.as_raw_handle(),
            FSCTL_SET_SPARSE,
            ptr::null(),
            0,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
        )
    } == 0
    {
        return Ok(None);
    }
    out.set_len(stamp.size)?;
    // ReFS uses 4 KiB or 64 KiB clusters. Both divide 64 KiB.
    const ALIGN: u64 = 64 * 1024;
    let aligned = stamp.size / ALIGN * ALIGN;
    let mut pos = 0;
    while pos < aligned {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let size = (aligned - pos).min(64 * 1024 * 1024);
        let data = DUPLICATE_EXTENTS_DATA {
            FileHandle: source.as_raw_handle(),
            SourceFileOffset: pos as i64,
            TargetFileOffset: pos as i64,
            ByteCount: size as i64,
        };
        if unsafe {
            DeviceIoControl(
                out.as_raw_handle(),
                FSCTL_DUPLICATE_EXTENTS_TO_FILE,
                (&data as *const DUPLICATE_EXTENTS_DATA).cast(),
                size_of_val(&data) as u32,
                ptr::null_mut(),
                0,
                &mut returned,
                ptr::null_mut(),
            )
        } == 0
        {
            return Ok(None);
        }
        pos += size;
    }
    let mut tail = vec![0; (stamp.size - aligned) as usize];
    read_at(&source, aligned, &mut tail)?;
    out.seek(SeekFrom::Start(aligned))?;
    out.write_all(&tail)?;
    let mut bom = vec![0; prefix.len()];
    read_at(out, 0, &mut bom)?;
    if bom != prefix {
        return Ok(None);
    }
    let mut disk = vec![0; super::recovery_blocks::BLOCK];
    let mut saved = vec![0; disk.len()];
    let mut hash = Sha256::new();
    for offset in (0..root.len).step_by(disk.len()) {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let n = (root.len - offset).min(disk.len() as u64) as usize;
        read_at(out, head + offset, &mut disk[..n])?;
        root.read(offset, &mut saved[..n])?;
        if disk[..n] != saved[..n] {
            return Ok(None);
        }
        hash.update(&disk[..n]);
    }
    Ok(Some((head, hash.finalize().into())))
}
