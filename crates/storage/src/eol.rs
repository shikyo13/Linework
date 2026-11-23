use super::{invalid, tree, Document, Source};
use std::{
    io::{self, BufWriter, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

impl Document {
    pub fn set_newline(&mut self, style: &'static str) {
        if matches!(style, "\r\n" | "\r" | "\n") {
            self.newline = style;
        }
    }
    /// A backing file keeps conversion memory bounded even for dense CRLF input.
    pub fn convert_eol(&mut self, style: &'static str, cancel: &AtomicBool) -> io::Result<()> {
        self.convert_eol_observed(style, cancel, |_, _| {})
    }
    pub fn convert_eol_observed(
        &mut self,
        style: &'static str,
        cancel: &AtomicBool,
        mut observe: impl FnMut(std::ops::Range<u64>, u64),
    ) -> io::Result<()> {
        if !matches!(style, "\r\n" | "\r" | "\n") {
            return Err(invalid("Unknown line ending"));
        }
        let mut temp = tempfile::tempfile()?;
        let n = self.len();
        {
            let mut out = BufWriter::with_capacity(256 * 1024, &mut temp);
            let mut pos = 0;
            let mut previous_cr = false;
            while pos < n {
                if cancel.load(Ordering::Relaxed) {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "Conversion canceled",
                    ));
                }
                let bytes = self.read(pos, (n - pos).min(512 * 1024) as usize)?;
                let mut start = 0;
                for index in memchr::memchr2_iter(b'\n', b'\r', &bytes) {
                    if index > start {
                        out.write_all(&bytes[start..index])?;
                        previous_cr = false;
                    }
                    if bytes[index] != b'\n' || !previous_cr {
                        let at = pos + index as u64;
                        observe(at..at + 1, style.len() as u64);
                        out.write_all(style.as_bytes())?;
                    }
                    if bytes[index] == b'\n' && previous_cr {
                        let at = pos + index as u64;
                        observe(at..at + 1, 0);
                    }
                    previous_cr = bytes[index] == b'\r';
                    start = index + 1;
                }
                if start < bytes.len() {
                    out.write_all(&bytes[start..])?;
                    previous_cr = false;
                }
                pos += bytes.len() as u64;
            }
            out.flush()?;
        }
        let len = temp.metadata()?.len();
        let next = tree::leaf(Source::file(Arc::new(temp), 0, len)?, 0, len)?;
        self.commit(next)?;
        self.set_newline(style);
        Ok(())
    }
}
