use super::{invalid, Document};
use encoding_rs::{CoderResult, Encoding, UTF_16BE, UTF_16LE, WINDOWS_1252};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use sha2::{Digest,Sha256};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Codec {
    Utf8,
    Bom,
    Utf16Le,
    Utf16Be,
    Other(&'static Encoding),
}
impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Bom => "UTF-8 BOM",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
            Self::Other(e) => e.name(),
        }
    }
    pub fn named(n: &str) -> io::Result<Self> {
        let word = n.trim().to_ascii_lowercase().replace([' ', '_', '-'], "");
        match word.as_str() {
            "utf8" => Ok(Self::Utf8),
            "utf8bom" => Ok(Self::Bom),
            "utf16le" | "utf16" => Ok(Self::Utf16Le),
            "utf16be" => Ok(Self::Utf16Be),
            "ansi" | "cp1252" | "windows1252" => Ok(Self::Other(WINDOWS_1252)),
            _ => {
                let enc = Encoding::for_label(n.trim().as_bytes())
                    .ok_or_else(|| invalid("Unknown text encoding"))?;
                if enc == encoding_rs::REPLACEMENT {
                    return Err(invalid("Unsupported encoding"));
                }
                Ok(Self::Other(enc))
            }
        }
    }
    pub fn bom(self) -> &'static [u8] {
        match self {
            Self::Bom => b"\xef\xbb\xbf",
            Self::Utf16Le => b"\xff\xfe",
            Self::Utf16Be => b"\xfe\xff",
            _ => b"",
        }
    }
    pub fn detect(sample: &[u8], final_chunk: bool) -> io::Result<Self> {
        if sample.starts_with(&[0xff, 0xfe, 0, 0]) || sample.starts_with(&[0, 0, 0xfe, 0xff]) {
            return Err(invalid("UTF-32 input is unsupported"));
        }
        if sample.starts_with(Self::Bom.bom()) {
            return Ok(Self::Bom);
        }
        if sample.starts_with(Self::Utf16Le.bom()) {
            return Ok(Self::Utf16Le);
        }
        if sample.starts_with(Self::Utf16Be.bom()) {
            return Ok(Self::Utf16Be);
        }
        if sample.len() >= 4 && sample.contains(&0) {
            let pairs = sample.len() / 2;
            let zeros = |col| sample.chunks_exact(2).filter(|a| a[col] == 0).count();
            if zeros(1) > pairs / 2 && zeros(0) < pairs / 10 + 1 {
                return Ok(Self::Utf16Le);
            }
            if zeros(0) > pairs / 2 && zeros(1) < pairs / 10 + 1 {
                return Ok(Self::Utf16Be);
            }
            return Err(invalid("This file appears to contain binary data"));
        }
        match std::str::from_utf8(sample) {
            Ok(_) => Ok(Self::Utf8),
            Err(e) if !final_chunk && e.error_len().is_none() => Ok(Self::Utf8),
            Err(_) => Ok(Self::Other(WINDOWS_1252)),
        }
    }
    pub fn native(self) -> bool {
        matches!(self, Self::Utf8 | Self::Bom)
    }
    pub fn decode(self, mut file: File, prefix: &[u8]) -> io::Result<(File,[u8;32])> {
        let encoding = match self {
            Self::Utf16Le => UTF_16LE,
            Self::Utf16Be => UTF_16BE,
            Self::Other(e) => e,
            _ => unreachable!(),
        };
        let mut decoder = encoding.new_decoder_without_bom_handling();
        file.seek(SeekFrom::Start(prefix.len() as u64))?;
        let mut digest = Sha256::new();
        digest.update(prefix);
        let mut dest = tempfile::tempfile()?;
        let mut bytes = vec![0; 128 * 1024];
        let mut output = vec![0; 512 * 1024];
        loop {
            let size = file.read(&mut bytes)?;
            digest.update(&bytes[..size]);
            let mut cursor = 0;
            loop {
                let (res, read, written, bad) =
                    decoder.decode_to_utf8(&bytes[cursor..size], &mut output, size == 0);
                if bad {
                    return Err(invalid(
                        "The source contains invalid bytes for its selected encoding",
                    ));
                }
                dest.write_all(&output[..written])?;
                cursor += read;
                if res == CoderResult::InputEmpty {
                    break;
                }
            }
            if size == 0 {
                break;
            }
        }
        Ok((dest,digest.finalize().into()))
    }
    pub fn write(self, d: &Document, w: &mut impl Write, bom: bool) -> io::Result<()> {
        if bom {
            w.write_all(self.bom())?;
        }
        let mut pos = 0;
        let mut buffer = vec![0; 1024 * 1024 + 4];
        let mut carry = 0;
        let mut encoder = if let Self::Other(e) = self {
            Some(e.new_encoder())
        } else {
            None
        };
        let mut out = Vec::with_capacity(2 * 1024 * 1024 + 16);
        loop {
            let n = (d.len() - pos).min(1024 * 1024) as usize;
            d.read_into(pos, &mut buffer[carry..carry + n])?;
            pos += n as u64;
            if self.native() {
                w.write_all(&buffer[..n])?;
                if pos == d.len() {
                    break;
                }
                continue;
            }
            let total = carry + n;
            let valid = match std::str::from_utf8(&buffer[..total]) {
                Ok(_) => total,
                Err(e) if e.error_len().is_none() && pos < d.len() => e.valid_up_to(),
                Err(_) => {
                    return Err(invalid(
                        "The document contains invalid UTF-8 and cannot be converted",
                    ))
                }
            };
            let s = std::str::from_utf8(&buffer[..valid]).map_err(|e| invalid(e.to_string()))?;
            out.clear();
            match self {
                Self::Utf16Le | Self::Utf16Be => {
                    for v in s.encode_utf16() {
                        out.extend(if self == Self::Utf16Le {
                            v.to_le_bytes()
                        } else {
                            v.to_be_bytes()
                        });
                    }
                    w.write_all(&out)?;
                }
                Self::Other(_) => {
                    let mut at = 0;
                    let encoder = encoder.as_mut().unwrap();
                    loop {
                        out.clear();
                        let (res, read, err) =
                            encoder.encode_from_utf8_to_vec(&s[at..], &mut out, pos == d.len());
                        if err {
                            return Err(invalid(format!(
                                "Some characters cannot be represented in {}",
                                self.name()
                            )));
                        }
                        w.write_all(&out)?;
                        at += read;
                        if res == CoderResult::InputEmpty {
                            break;
                        }
                    }
                }
                _ => unreachable!(),
            }
            carry = total - valid;
            buffer.copy_within(valid..total, 0);
            if pos == d.len() {
                break;
            }
        }
        Ok(())
    }
}
