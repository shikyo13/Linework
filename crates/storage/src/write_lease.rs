//! Keep an edited file's original bytes available without copying it on every
//! first keystroke. Denying write sharing is essential: a read-cache oplock
//! alone only reports writes after they can already have changed the file.
use crate::{source::Source, Stamp};
use std::{
    fs::OpenOptions,
    io,
    mem::{size_of, zeroed},
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr,
    sync::{mpsc, Arc},
    thread::{self, JoinHandle},
};
use windows_sys::Win32::{
    Foundation::{ERROR_IO_PENDING, HANDLE, WAIT_OBJECT_0},
    Storage::FileSystem::{
        LockFileEx, UnlockFileEx, FILE_FLAG_OVERLAPPED, FILE_SHARE_DELETE, FILE_SHARE_READ,
        LOCKFILE_FAIL_IMMEDIATELY,
    },
    System::{
        Ioctl::*,
        Threading::{
            CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject, INFINITE,
        },
        WindowsProgramming::FILE_FLAG_OPEN_REQUIRING_OPLOCK,
        IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED},
    },
};

pub(super) struct Lease {
    cancel: Arc<OwnedHandle>,
    worker: Option<JoinHandle<()>>,
}

pub(super) struct ReadRange<'a> {
    file: &'a std::fs::File,
    length: u64,
    at: OVERLAPPED,
}
impl<'a> ReadRange<'a> {
    pub(super) fn lock(file: &'a std::fs::File, offset: u64, length: u64) -> io::Result<Self> {
        let mut at: OVERLAPPED = unsafe { zeroed() };
        at.Anonymous.Anonymous.Offset = offset as u32;
        at.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
        let ok = unsafe {
            LockFileEx(
                file.as_raw_handle(),
                LOCKFILE_FAIL_IMMEDIATELY,
                0,
                length as u32,
                (length >> 32) as u32,
                &mut at,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { file, length, at })
    }
}
impl Drop for ReadRange<'_> {
    fn drop(&mut self) {
        unsafe {
            UnlockFileEx(
                self.file.as_raw_handle(),
                0,
                self.length as u32,
                (self.length >> 32) as u32,
                &mut self.at,
            )
        };
    }
}

fn event() -> io::Result<OwnedHandle> {
    let h = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if h.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

fn request(
    handle: HANDLE,
    flags: u32,
    level: u32,
    completed: &OwnedHandle,
    output: &mut REQUEST_OPLOCK_OUTPUT_BUFFER,
    operation: &mut OVERLAPPED,
    input: &mut REQUEST_OPLOCK_INPUT_BUFFER,
) -> io::Result<bool> {
    *input = REQUEST_OPLOCK_INPUT_BUFFER {
        StructureVersion: REQUEST_OPLOCK_CURRENT_VERSION as u16,
        StructureLength: size_of::<REQUEST_OPLOCK_INPUT_BUFFER>() as u16,
        RequestedOplockLevel: level,
        Flags: flags,
    };
    operation.hEvent = completed.as_raw_handle();
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_REQUEST_OPLOCK,
            input as *const _ as _,
            size_of::<REQUEST_OPLOCK_INPUT_BUFFER>() as _,
            output as *mut _ as _,
            size_of::<REQUEST_OPLOCK_OUTPUT_BUFFER>() as _,
            ptr::null_mut(),
            operation,
        )
    };
    if ok != 0 {
        return Ok(false);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
        Ok(true)
    } else {
        Err(error)
    }
}

impl Lease {
    pub(super) fn acquire(path: &Path, expected: &Stamp, source: &Arc<Source>) -> Option<Self> {
        let cancel = Arc::new(event().ok()?);
        let stop = cancel.clone();
        let path = path.to_owned();
        let expected = expected.clone();
        let source = Arc::downgrade(source);
        let (ready, result) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("Linework external change".into())
            .spawn(move || {
                let Ok(completed) = event() else { return };
                let Ok(file) = OpenOptions::new()
                    .read(true)
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
                    .custom_flags(FILE_FLAG_OVERLAPPED | FILE_FLAG_OPEN_REQUIRING_OPLOCK)
                    .open(path)
                else {
                    return;
                };
                let h = file.as_raw_handle();
                let mut operation: OVERLAPPED = unsafe { zeroed() };
                let mut input = unsafe { zeroed() };
                let mut output = unsafe { zeroed() };
                if !matches!(
                    request(
                        h,
                        REQUEST_OPLOCK_INPUT_FLAG_REQUEST,
                        OPLOCK_LEVEL_CACHE_READ | OPLOCK_LEVEL_CACHE_HANDLE,
                        &completed,
                        &mut output,
                        &mut operation,
                        &mut input
                    ),
                    Ok(true)
                ) {
                    return;
                }
                // Do not touch the stream between the atomic open and the oplock request.
                let valid = Stamp::from(&file).is_ok_and(|actual| actual == expected);
                let accepted = valid && ready.send(()).is_ok();
                drop(ready);
                let events = [stop.as_raw_handle(), completed.as_raw_handle()];
                let canceled = !accepted
                    || unsafe { WaitForMultipleObjects(2, events.as_ptr(), 0, INFINITE) }
                        != WAIT_OBJECT_0 + 1;
                if canceled {
                    unsafe { CancelIoEx(h, &operation) };
                }
                let mut transferred = 0;
                // All IOCTL input/output buffers stay alive until kernel completion.
                let ok = unsafe { GetOverlappedResult(h, &operation, &mut transferred, 1) };
                if canceled {
                    return;
                }
                let Some(source) = source.upgrade() else {
                    return;
                };
                if source.materialize(&expected, false).is_ok() {
                    return;
                } // Closing the handle acknowledges the break.
                drop(source);
                // If the private volume is full, protect the only remaining copy.
                // Acknowledge, then retain the deny-write handle: the competing open
                // receives a sharing violation and the user can still Save As elsewhere.
                if ok != 0 && output.Flags & REQUEST_OPLOCK_OUTPUT_FLAG_ACK_REQUIRED != 0 {
                    let mut ack: OVERLAPPED = unsafe { zeroed() };
                    let mut ack_input = unsafe { zeroed() };
                    let mut ack_output = unsafe { zeroed() };
                    if matches!(
                        request(
                            h,
                            REQUEST_OPLOCK_INPUT_FLAG_ACK,
                            0,
                            &completed,
                            &mut ack_output,
                            &mut ack,
                            &mut ack_input
                        ),
                        Ok(true)
                    ) {
                        unsafe { GetOverlappedResult(h, &ack, &mut transferred, 1) };
                    }
                }
                unsafe { WaitForSingleObject(stop.as_raw_handle(), INFINITE) };
            })
            .ok()?;
        let lease = Self {
            cancel,
            worker: Some(worker),
        };
        result.recv().ok().map(|()| lease)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        unsafe { SetEvent(self.cancel.as_raw_handle()) };
        if let Some(worker) = self.worker.take() {
            if worker.thread().id() != thread::current().id() {
                let _ = worker.join();
            }
        }
    }
}
