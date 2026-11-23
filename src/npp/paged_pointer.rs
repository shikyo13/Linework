use super::{Document, err};
use std::{
    cell::RefCell,
    fs::File,
    io::{self, Write},
    ops::Range,
    os::windows::io::AsRawHandle,
    ptr::null,
};
use windows_sys::Win32::{
    Foundation::CloseHandle,
    System::{
        Memory::*,
        SystemInformation::{GetSystemInfo, SYSTEM_INFO},
    },
};

struct Mapping {
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    offset: usize,
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(self.view);
        }
    }
}
impl Mapping {
    fn new(file: &File, start: u64, len: u64) -> io::Result<Self> {
        unsafe {
            let mut info: SYSTEM_INFO = std::mem::zeroed();
            GetSystemInfo(&mut info);
            let gran = info.dwAllocationGranularity as u64;
            let base = start / gran * gran;
            let offset = (start - base) as usize;
            let size = usize::try_from(len)
                .ok()
                .and_then(|n| n.checked_add(offset))
                .ok_or_else(|| err("Mapped range exceeds address space"))?;
            let map = CreateFileMappingW(file.as_raw_handle(), null(), PAGE_READONLY, 0, 0, null());
            if map.is_null() {
                return Err(io::Error::last_os_error());
            }
            let view = MapViewOfFile(map, FILE_MAP_READ, (base >> 32) as u32, base as u32, size);
            let error = if view.Value.is_null() {
                Some(io::Error::last_os_error())
            } else {
                None
            };
            CloseHandle(map);
            if let Some(e) = error {
                return Err(e);
            }
            Ok(Self { view, offset })
        }
    }
    fn address(&self) -> isize {
        unsafe { self.view.Value.cast::<u8>().add(self.offset) as isize }
    }
}
enum Backing {
    Small(Vec<u8>),
    Map(Mapping),
}
impl Backing {
    fn address(&self) -> isize {
        match self {
            Self::Small(s) => s.as_ptr() as isize,
            Self::Map(m) => m.address(),
        }
    }
}
struct Cache {
    id: u64,
    rev: u64,
    version: u64,
    range: Range<u64>,
    nul: bool,
    data: Backing,
}
thread_local! {static CACHE:RefCell<[Option<Cache>;2]>=const{RefCell::new([None,None])};}
pub(super) fn invalidate(id: u64) {
    CACHE.with_borrow_mut(|v| {
        for x in v {
            if x.as_ref().is_some_and(|e| e.id == id) {
                *x = None;
            }
        }
    });
}
pub(super) fn address(
    p: usize,
    id: u64,
    rev: u64,
    doc: &Document,
    r: Range<u64>,
    nul: bool,
) -> io::Result<isize> {
    CACHE.with_borrow_mut(|v| {
        let key = (id, rev, doc.version());
        if let Some(c) = &v[p] {
            if (c.id, c.rev, c.version) == key && r == c.range && (!nul || c.nul) {
                return Ok(c.data.address());
            }
        }
        let len = r.end - r.start;
        let data = if len <= 65536 {
            let mut bytes = doc.read(r.start, len as usize)?;
            bytes.push(0);
            Backing::Small(bytes)
        } else if let Some((file, offset)) = if nul {
            None
        } else {
            doc.file_range(r.clone())?
        } {
            Backing::Map(Mapping::new(&file, offset, len)?)
        } else {
            // Windows retains the file object for the mapped view. The unnamed
            // temporary file disappears as soon as the view is replaced.
            let mut stream = tempfile::tempfile()?;
            let mut buf = vec![0; 256 * 1024];
            let mut at = r.start;
            while at < r.end {
                let take = (r.end - at).min(buf.len() as u64) as usize;
                doc.read_into(at, &mut buf[..take])?;
                stream.write_all(&buf[..take])?;
                at += take as u64;
            }
            stream.write_all(&[0])?;
            Backing::Map(Mapping::new(&stream, 0, len + 1)?)
        };
        let ptr = data.address();
        v[p] = Some(Cache {
            id,
            rev,
            version: key.2,
            range: r,
            nul,
            data,
        });
        Ok(ptr)
    })
}
