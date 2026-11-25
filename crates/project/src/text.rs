use encoding_rs::{CoderResult, Decoder, UTF_16BE, UTF_16LE, WINDOWS_1252};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Windows1252,
}

/// Supply a bounded UTF-8 reader while retaining the on-disk coordinate system.
pub(crate) fn open_text(path: &Path) -> io::Result<(Box<dyn BufRead>, Encoding, usize)> {
    let mut file = File::open(path)?;
    let mut sample = [0; 8192];
    let n = file.metadata()?.len().min(sample.len() as u64) as usize;
    file.read_exact(&mut sample[..n])?;
    let bytes = &sample[..n];
    let (encoding, bom) =
        if bytes.starts_with(&[0xff, 0xfe, 0, 0]) || bytes.starts_with(&[0, 0, 0xfe, 0xff]) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "UTF-32 file"));
        } else if bytes.starts_with(&[0xff, 0xfe]) {
            (Encoding::Utf16Le, 2)
        } else if bytes.starts_with(&[0xfe, 0xff]) {
            (Encoding::Utf16Be, 2)
        } else if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
            (Encoding::Utf8, 3)
        } else if bytes.len() >= 4 && bytes.contains(&0) {
            let pairs = bytes.len() / 2;
            let zeros = |col: usize| bytes.chunks_exact(2).filter(|v| v[col] == 0).count();
            match (zeros(0), zeros(1)) {
                (a, b) if b > pairs / 2 && a < pairs / 10 + 1 => (Encoding::Utf16Le, 0),
                (a, b) if a > pairs / 2 && b < pairs / 10 + 1 => (Encoding::Utf16Be, 0),
                _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "binary file")),
            }
        } else if bytes.contains(&0) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "binary file"));
        } else if std::str::from_utf8(bytes).is_err_and(|err| err.error_len().is_some()) {
            (Encoding::Windows1252, 0)
        } else {
            (Encoding::Utf8, 0)
        };
    reader_as(file, encoding, bom)
}

pub(crate) fn reader_as(
    mut file: File,
    encoding: Encoding,
    bom: usize,
) -> io::Result<(Box<dyn BufRead>, Encoding, usize)> {
    file.seek(SeekFrom::Start(bom as u64))?;
    let reader = BufReader::with_capacity(64 * 1024, file);
    if encoding == Encoding::Utf8 {
        return Ok((Box::new(reader), encoding, bom));
    }
    let spec = match encoding {
        Encoding::Utf16Le => UTF_16LE,
        Encoding::Utf16Be => UTF_16BE,
        Encoding::Windows1252 => WINDOWS_1252,
        Encoding::Utf8 => unreachable!(),
    };
    let transcode = Transcode {
        inner: reader,
        decoder: spec.new_decoder_without_bom_handling(),
        done: false,
    };
    Ok((
        Box::new(BufReader::with_capacity(64 * 1024, transcode)),
        encoding,
        bom,
    ))
}

struct Transcode<R> {
    inner: R,
    decoder: Decoder,
    done: bool,
}
impl<R: BufRead> Read for Transcode<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.done || output.is_empty() {
            return Ok(0);
        }
        loop {
            let bytes = self.inner.fill_buf()?;
            let eof = bytes.is_empty();
            let (result, read, written, bad) = self.decoder.decode_to_utf8(bytes, output, eof);
            self.inner.consume(read);
            if bad {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid encoded text",
                ));
            }
            self.done = eof && result == CoderResult::InputEmpty;
            if written > 0 || self.done {
                return Ok(written);
            }
            if result == CoderResult::OutputFull {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "decoder output buffer is too short",
                ));
            }
        }
    }
}
