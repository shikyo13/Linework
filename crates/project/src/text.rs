use encoding_rs::{CoderResult, Decoder, UTF_16BE, UTF_16LE, WINDOWS_1252};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Windows1252,
}

impl Encoding {
    pub fn bytes(self, s: &str) -> u64 {
        match self {
            Self::Utf8 => s.len() as u64,
            Self::Utf16Le | Self::Utf16Be => 2 * s.encode_utf16().count() as u64,
            Self::Windows1252 => s.chars().count() as u64,
        }
    }

    pub fn encode(self, text: &str) -> Result<Vec<u8>, String> {
        match self {
            Self::Utf8 => Ok(text.as_bytes().to_vec()),
            Self::Utf16Le => Ok(text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()),
            Self::Utf16Be => Ok(text.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()),
            Self::Windows1252 => {
                let (bytes, _, bad) = WINDOWS_1252.encode(text);
                if bad {
                    Err("Replacement contains characters outside Windows-1252".into())
                } else {
                    Ok(bytes.into_owned())
                }
            }
        }
    }
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

pub(crate) fn lines(
    mut reader: impl BufRead,
    cancel: &AtomicBool,
    mut visit: impl FnMut(&str, usize) -> Result<bool, String>,
) -> Result<(), String> {
    const MAX_LINE: usize = 8 * 1024 * 1024;
    let mut held = Vec::with_capacity(256);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let block = reader.fill_buf().map_err(|e| e.to_string())?;
        if block.is_empty() {
            let text = std::str::from_utf8(&held).map_err(|_| "invalid UTF-8 text")?;
            visit(text, 0)?;
            return Ok(());
        }
        let end = memchr::memchr2(b'\n', b'\r', block).unwrap_or(block.len());
        if held.len() + end > MAX_LINE {
            return Err("workspace scan stopped at a line longer than 8 MiB; use file search for this document".into());
        }
        let terminator = block.get(end).copied();
        // LF and most CRLF records fit one buffer. Match directly in that buffer.
        if held.is_empty() && matches!(terminator, Some(b'\n')) {
            let text = std::str::from_utf8(&block[..end]).map_err(|_| "invalid UTF-8 text")?;
            if !visit(text, 1)? {
                return Ok(());
            }
            reader.consume(end + 1);
            continue;
        }
        if held.is_empty() && terminator == Some(b'\r') && end + 1 < block.len() {
            let eol = 1 + (block[end + 1] == b'\n') as usize;
            let text = std::str::from_utf8(&block[..end]).map_err(|_| "invalid UTF-8 text")?;
            if !visit(text, eol)? {
                return Ok(());
            }
            reader.consume(end + eol);
            continue;
        }
        held.extend_from_slice(&block[..end]);
        reader.consume(end + usize::from(terminator.is_some()));
        if let Some(term) = terminator {
            let mut eol = 1;
            if term == b'\r'
                && reader.fill_buf().map_err(|e| e.to_string())?.first() == Some(&b'\n')
            {
                reader.consume(1);
                eol = 2;
            }
            let text = std::str::from_utf8(&held).map_err(|_| "invalid UTF-8 text")?;
            if !visit(text, eol)? {
                return Ok(());
            }
            held.clear();
        }
    }
}
