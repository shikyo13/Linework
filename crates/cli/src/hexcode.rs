//! `--decode-hex`: printed lines with hashcat's `$HEX[...]` decoded.
use linework_search::hexcode::decode;
use std::io::{self, Write};

/// Output with `$HEX[...]` decoded, a whole line at a time so that one
/// split across two writes is still found. What is left of a last line
/// without an ending is written when the decoder is dropped.
pub struct Decoder<'a> {
    inner: &'a mut dyn Write,
    line: Vec<u8>,
}

impl<'a> Decoder<'a> {
    pub fn new(inner: &'a mut dyn Write) -> Self {
        Self { inner, line: Vec::new() }
    }
}

impl Write for Decoder<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.line.extend_from_slice(bytes);
        if let Some(last) = memchr::memrchr(b'\n', &self.line) {
            self.inner.write_all(&decode(&self.line[..=last]))?;
            self.line.drain(..=last);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Drop for Decoder<'_> {
    fn drop(&mut self) {
        if !self.line.is_empty() {
            let _ = self.inner.write_all(&decode(&self.line));
            let _ = self.inner.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_split_write_is_decoded_whole() {
        let mut out = Vec::new();
        {
            let mut d = Decoder::new(&mut out);
            d.write_all(b"h:$HEX[70").unwrap();
            d.write_all(b"61]\nlast $HEX[7a]").unwrap();
        }
        assert_eq!(out, b"h:pa\nlast z");
    }
}
