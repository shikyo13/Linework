use super::{invalid, Document};
use encoding_rs::{CoderResult, Encoding, UTF_16BE, UTF_16LE, WINDOWS_1252};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{OnceLock, RwLock};

/// Documents from this size on are saved from several reading threads.
#[cfg(not(test))]
const PARALLEL_SAVE_FROM: u64 = 64 * 1024 * 1024;
#[cfg(test)]
const PARALLEL_SAVE_FROM: u64 = 2 * 1024 * 1024;
/// Reading threads for a large save.
const SAVE_READERS: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Codec {
    Utf8,
    Bom,
    Utf16Le,
    Utf16Be,
    Other(&'static Encoding),
    /// A single-byte Windows code page that encoding_rs does not carry: the
    /// DOS code pages, true ISO-8859-1 and -9, and the Mac code pages.
    Table(&'static Table),
    /// Bytes shown as hexadecimal text, sixteen to a line. Saving parses the
    /// hex digits back into bytes, so a binary file can be edited safely.
    Hex,
}

/// One single-byte code page, read from Windows once and kept.
pub(crate) struct Table {
    name: &'static str,
    chars: [Option<char>; 256],
    back: Vec<(char, u8)>,
}
impl PartialEq for Table {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}
impl Eq for Table {}

/// The code pages read through Windows: (code page, name, other labels).
const TABLES: &[(u32, &str, &[&str])] = &[
    (437, "IBM437", &["cp437", "437", "dos437", "oem437", "oemus"]),
    (737, "IBM737", &["cp737", "737"]),
    (775, "IBM775", &["cp775", "775"]),
    (850, "IBM850", &["cp850", "850", "dos850"]),
    (852, "IBM852", &["cp852", "852", "dos852"]),
    (855, "IBM855", &["cp855", "855"]),
    (857, "IBM857", &["cp857", "857"]),
    (858, "IBM858", &["cp858", "858"]),
    (860, "IBM860", &["cp860", "860"]),
    (861, "IBM861", &["cp861", "861"]),
    (862, "IBM862", &["cp862", "862"]),
    (863, "IBM863", &["cp863", "863"]),
    (865, "IBM865", &["cp865", "865"]),
    (869, "IBM869", &["cp869", "869"]),
    (28591, "ISO-8859-1", &["iso88591", "latin1", "l1", "cp28591"]),
    (28599, "ISO-8859-9", &["iso88599", "latin5", "l5", "cp28599"]),
    (10006, "x-mac-greek", &["xmacgreek", "macgreek"]),
    (10029, "x-mac-ce", &["xmacce", "maccentraleurope", "maccentraleuropean"]),
    (10079, "x-mac-icelandic", &["xmacicelandic", "macicelandic"]),
    (10081, "x-mac-turkish", &["xmacturkish", "macturkish"]),
];

/// Every encoding Linework can open or save, as (menu text, label). The label
/// is what `Codec::named` takes and what the status bar shows.
pub const ENCODINGS: &[(&str, &str)] = &[
    ("Unicode (UTF-8)", "UTF-8"),
    ("Unicode (UTF-8 with BOM)", "UTF-8 BOM"),
    ("Unicode (UTF-16 LE with BOM)", "UTF-16 LE"),
    ("Unicode (UTF-16 BE with BOM)", "UTF-16 BE"),
    ("Western (Windows 1252)", "windows-1252"),
    ("Western (ISO 8859-1)", "ISO-8859-1"),
    ("Western (ISO 8859-15)", "ISO-8859-15"),
    ("Western (Mac Roman)", "macintosh"),
    ("Western (DOS 437)", "IBM437"),
    ("Western European (DOS 850)", "IBM850"),
    ("Western European (DOS 858)", "IBM858"),
    ("Portuguese (DOS 860)", "IBM860"),
    ("Icelandic (DOS 861)", "IBM861"),
    ("Icelandic (Mac)", "x-mac-icelandic"),
    ("French Canadian (DOS 863)", "IBM863"),
    ("Nordic (DOS 865)", "IBM865"),
    ("Nordic (ISO 8859-10)", "ISO-8859-10"),
    ("Central European (Windows 1250)", "windows-1250"),
    ("Central European (ISO 8859-2)", "ISO-8859-2"),
    ("Central European (DOS 852)", "IBM852"),
    ("Central European (Mac)", "x-mac-ce"),
    ("Cyrillic (Windows 1251)", "windows-1251"),
    ("Cyrillic (KOI8-R)", "KOI8-R"),
    ("Cyrillic (KOI8-U)", "KOI8-U"),
    ("Cyrillic (ISO 8859-5)", "ISO-8859-5"),
    ("Cyrillic (DOS 866)", "IBM866"),
    ("Cyrillic (DOS 855)", "IBM855"),
    ("Cyrillic (Mac)", "x-mac-cyrillic"),
    ("Greek (Windows 1253)", "windows-1253"),
    ("Greek (ISO 8859-7)", "ISO-8859-7"),
    ("Greek (DOS 737)", "IBM737"),
    ("Greek (DOS 869)", "IBM869"),
    ("Greek (Mac)", "x-mac-greek"),
    ("Turkish (Windows 1254)", "windows-1254"),
    ("Turkish (ISO 8859-9)", "ISO-8859-9"),
    ("Turkish (DOS 857)", "IBM857"),
    ("Turkish (Mac)", "x-mac-turkish"),
    ("South European (ISO 8859-3)", "ISO-8859-3"),
    ("Baltic (Windows 1257)", "windows-1257"),
    ("Baltic (ISO 8859-4)", "ISO-8859-4"),
    ("Baltic (ISO 8859-13)", "ISO-8859-13"),
    ("Baltic (DOS 775)", "IBM775"),
    ("Celtic (ISO 8859-14)", "ISO-8859-14"),
    ("Romanian (ISO 8859-16)", "ISO-8859-16"),
    ("Hebrew (Windows 1255)", "windows-1255"),
    ("Hebrew (ISO 8859-8)", "ISO-8859-8"),
    ("Hebrew (DOS 862)", "IBM862"),
    ("Arabic (Windows 1256)", "windows-1256"),
    ("Arabic (ISO 8859-6)", "ISO-8859-6"),
    ("Vietnamese (Windows 1258)", "windows-1258"),
    ("Thai (Windows 874)", "windows-874"),
    ("Japanese (Shift JIS)", "Shift_JIS"),
    ("Japanese (EUC-JP)", "EUC-JP"),
    ("Japanese (ISO-2022-JP)", "ISO-2022-JP"),
    ("Chinese Simplified (GB18030)", "gb18030"),
    ("Chinese Simplified (GBK)", "GBK"),
    ("Chinese Traditional (Big5)", "Big5"),
    ("Korean (EUC-KR)", "EUC-KR"),
    ("Hexadecimal (raw bytes)", "Hexadecimal"),
];

/// How files without a byte order mark and without valid UTF-8 are read.
#[derive(Clone, Copy)]
struct Detection {
    auto: bool,
    fallback: Codec,
}
static DETECTION: RwLock<Detection> = RwLock::new(Detection {
    auto: true,
    fallback: Codec::Other(WINDOWS_1252),
});

/// Set how text that is not UTF-8 is read: guessed from its bytes, or always
/// `fallback`. With guessing on, `fallback` is unused.
pub fn set_detection(auto: bool, fallback: &str) -> io::Result<()> {
    let fallback = match Codec::named(fallback)? {
        c @ (Codec::Other(_) | Codec::Table(_)) => c,
        _ => return Err(invalid("The fallback encoding must be a single-language encoding")),
    };
    *DETECTION.write().unwrap_or_else(|e| e.into_inner()) = Detection { auto, fallback };
    Ok(())
}

fn tables() -> &'static Vec<&'static Table> {
    static ALL: OnceLock<Vec<&'static Table>> = OnceLock::new();
    ALL.get_or_init(|| {
        TABLES
            .iter()
            .filter_map(|&(cp, name, _)| build(cp, name).map(|t| &*Box::leak(Box::new(t))))
            .collect()
    })
}

#[cfg(windows)]
fn build(cp: u32, name: &'static str) -> Option<Table> {
    use windows_sys::Win32::Globalization::{MultiByteToWideChar, MB_ERR_INVALID_CHARS};
    let mut chars = [None; 256];
    for (b, slot) in chars.iter_mut().enumerate() {
        let mut out = [0u16; 2];
        let n = unsafe { MultiByteToWideChar(cp, MB_ERR_INVALID_CHARS, &(b as u8), 1, out.as_mut_ptr(), 2) };
        if n == 1 {
            *slot = char::from_u32(out[0] as u32);
        }
    }
    if chars[b'A' as usize] != Some('A') {
        return None;
    }
    let mut back: Vec<(char, u8)> = chars
        .iter()
        .enumerate()
        .filter_map(|(b, c)| c.map(|c| (c, b as u8)))
        .collect();
    // When two bytes show the same character, saving writes the lower one.
    back.sort();
    back.dedup_by_key(|x| x.0);
    Some(Table { name, chars, back })
}
#[cfg(not(windows))]
fn build(_: u32, _: &'static str) -> Option<Table> {
    None
}

/// The legacy encoding to read `bytes` with, which are not valid UTF-8.
pub(crate) fn guess(bytes: &[u8], last: bool) -> Codec {
    let d = *DETECTION.read().unwrap_or_else(|e| e.into_inner());
    if !d.auto {
        return d.fallback;
    }
    let mut detector = chardetng::EncodingDetector::new();
    // A few MiB decide as well as the whole file, and keep large opens quick.
    let cap = bytes.len().min(4 * 1024 * 1024);
    detector.feed(&bytes[..cap], last && cap == bytes.len());
    match detector.guess(None, false) {
        e if e == UTF_16LE || e == UTF_16BE || e == encoding_rs::UTF_8 => Codec::Other(WINDOWS_1252),
        e => Codec::Other(e),
    }
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Bom => "UTF-8 BOM",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
            Self::Other(e) => e.name(),
            Self::Table(t) => t.name,
            Self::Hex => "Hexadecimal",
        }
    }
    pub fn named(n: &str) -> io::Result<Self> {
        let word = n.trim().to_ascii_lowercase().replace([' ', '_', '-'], "");
        match word.as_str() {
            "utf8" => return Ok(Self::Utf8),
            "utf8bom" | "utf8withbom" => return Ok(Self::Bom),
            "utf16le" | "utf16" | "utf16lebom" | "utf16lewithbom" => return Ok(Self::Utf16Le),
            "utf16be" | "utf16bebom" | "utf16bewithbom" => return Ok(Self::Utf16Be),
            "ansi" | "cp1252" | "windows1252" => return Ok(Self::Other(WINDOWS_1252)),
            "hex" | "hexadecimal" => return Ok(Self::Hex),
            _ => {}
        }
        for &(cp, name, labels) in TABLES {
            let own = name.to_ascii_lowercase().replace([' ', '_', '-'], "");
            if own == word || labels.contains(&word.as_str()) {
                return tables()
                    .iter()
                    .find(|t| t.name == name)
                    .map(|t| Self::Table(t))
                    .ok_or_else(|| invalid(format!("Windows does not provide code page {cp}")));
            }
        }
        let enc = Encoding::for_label(n.trim().as_bytes()).ok_or_else(|| invalid("Unknown text encoding"))?;
        if enc == encoding_rs::REPLACEMENT {
            return Err(invalid("Unsupported encoding"));
        }
        Ok(match enc {
            e if e == encoding_rs::UTF_8 => Self::Utf8,
            e if e == UTF_16LE => Self::Utf16Le,
            e if e == UTF_16BE => Self::Utf16Be,
            e => Self::Other(e),
        })
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
            return Err(invalid("UTF-32 text is not supported. This file appears to contain binary data"));
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
        Ok(Self::detect_text(sample, final_chunk))
    }
    /// The encoding of a sample known to be text: UTF-8 when it is, else a guess.
    pub fn detect_text(sample: &[u8], final_chunk: bool) -> Self {
        match std::str::from_utf8(sample) {
            Ok(_) => Self::Utf8,
            Err(e) if !final_chunk && e.error_len().is_none() => Self::Utf8,
            Err(_) => guess(sample, final_chunk),
        }
    }
    /// Whether a file whose start holds zero bytes is text inside: every sample
    /// taken from within it has no zero byte and almost no other control
    /// characters. A file sorted by its bytes, such as a password list, puts its
    /// lines of control characters first; a binary file is binary throughout.
    pub fn text_inside(samples: &[Vec<u8>]) -> bool {
        !samples.is_empty()
            && samples.iter().all(|s| {
                let controls = s.iter().filter(|&&c| c < 32 && !matches!(c, 9..=13 | 27)).count();
                !s.is_empty() && !s.contains(&0) && controls * 100 <= s.len()
            })
    }
    pub fn native(self) -> bool {
        matches!(self, Self::Utf8 | Self::Bom)
    }
    pub fn decode(self, mut file: File, prefix: &[u8]) -> io::Result<(File, [u8; 32])> {
        file.seek(SeekFrom::Start(prefix.len() as u64))?;
        let mut digest = crate::hash::Hasher::new();
        digest.update(prefix);
        let mut dest = io::BufWriter::with_capacity(1024 * 1024, tempfile::tempfile()?);
        let mut bytes = vec![0; 128 * 1024];
        let mut output = vec![0; 512 * 1024];
        let mut text = String::with_capacity(512 * 1024);
        let mut decoder = match self {
            Self::Utf16Le => Some(UTF_16LE.new_decoder_without_bom_handling()),
            Self::Utf16Be => Some(UTF_16BE.new_decoder_without_bom_handling()),
            Self::Other(e) => Some(e.new_decoder_without_bom_handling()),
            Self::Table(_) | Self::Hex => None,
            _ => unreachable!(),
        };
        let mut offset = 0u64;
        loop {
            // Fill the whole buffer, so only the last read is short and hex
            // rows stay sixteen bytes from the start of the file.
            let mut size = 0;
            while size < bytes.len() {
                match file.read(&mut bytes[size..])? {
                    0 => break,
                    k => size += k,
                }
            }
            digest.update(&bytes[..size]);
            match self {
                Self::Table(t) => {
                    text.clear();
                    for (k, &b) in bytes[..size].iter().enumerate() {
                        text.push(t.chars[b as usize].ok_or_else(|| {
                            invalid(format!(
                                "Byte {b:#04x} at offset {} has no character in {}",
                                offset + k as u64,
                                t.name
                            ))
                        })?);
                    }
                    dest.write_all(text.as_bytes())?;
                }
                Self::Hex => {
                    text.clear();
                    for row in bytes[..size].chunks(16) {
                        for (k, b) in row.iter().enumerate() {
                            if k > 0 && k % 2 == 0 {
                                text.push(' ');
                            }
                            text.push(char::from(b"0123456789abcdef"[(b >> 4) as usize]));
                            text.push(char::from(b"0123456789abcdef"[(b & 15) as usize]));
                        }
                        text.push('\n');
                    }
                    dest.write_all(text.as_bytes())?;
                }
                _ => {
                    let decoder = decoder.as_mut().unwrap();
                    let mut cursor = 0;
                    loop {
                        let (res, read, written, bad) =
                            decoder.decode_to_utf8(&bytes[cursor..size], &mut output, size == 0);
                        if bad {
                            return Err(invalid(format!(
                                "The source contains bytes that are not valid {} near offset {}",
                                self.name(),
                                offset + cursor as u64
                            )));
                        }
                        dest.write_all(&output[..written])?;
                        cursor += read;
                        if res == CoderResult::InputEmpty {
                            break;
                        }
                    }
                }
            }
            offset += size as u64;
            if size == 0 {
                break;
            }
        }
        let dest = dest.into_inner().map_err(|e| e.into_error())?;
        Ok((dest, crate::hash::finish(&digest)))
    }
    pub fn write(self, d: &Document, w: &mut impl Write, bom: bool) -> io::Result<()> {
        if bom {
            w.write_all(self.bom())?;
        }
        if self.native() && d.len() >= PARALLEL_SAVE_FROM {
            return crate::parallel::copy_out(d, SAVE_READERS, w);
        }
        let mut pos = 0;
        let mut buffer = vec![0; 1024 * 1024 + 4];
        let mut carry = 0;
        let mut encoder = if let Self::Other(e) = self { Some(e.new_encoder()) } else { None };
        let mut nibble: Option<u8> = None;
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
                Err(_) => return Err(invalid("The document contains invalid UTF-8 and cannot be converted")),
            };
            let s = std::str::from_utf8(&buffer[..valid]).map_err(|e| invalid(e.to_string()))?;
            out.clear();
            match self {
                Self::Utf16Le | Self::Utf16Be => {
                    for v in s.encode_utf16() {
                        out.extend(if self == Self::Utf16Le { v.to_le_bytes() } else { v.to_be_bytes() });
                    }
                    w.write_all(&out)?;
                }
                Self::Table(t) => {
                    for c in s.chars() {
                        match t.back.binary_search_by_key(&c, |x| x.0) {
                            Ok(k) => out.push(t.back[k].1),
                            Err(_) => {
                                return Err(invalid(format!(
                                    "The character {c:?} (U+{:04X}) cannot be represented in {}",
                                    c as u32,
                                    t.name
                                )))
                            }
                        }
                    }
                    w.write_all(&out)?;
                }
                Self::Hex => {
                    for &b in s.as_bytes() {
                        let v = match b {
                            b'0'..=b'9' => b - b'0',
                            b'a'..=b'f' => b - b'a' + 10,
                            b'A'..=b'F' => b - b'A' + 10,
                            b' ' | b'\t' | b'\r' | b'\n' => continue,
                            _ => {
                                return Err(invalid(
                                    "Hexadecimal text may only contain the digits 0-9 and a-f, and spaces",
                                ))
                            }
                        };
                        match nibble.take() {
                            Some(high) => out.push(high << 4 | v),
                            None => nibble = Some(v),
                        }
                    }
                    w.write_all(&out)?;
                }
                Self::Other(_) => {
                    let mut at = 0;
                    let encoder = encoder.as_mut().unwrap();
                    loop {
                        out.clear();
                        let (res, read, err) = encoder.encode_from_utf8_to_vec(&s[at..], &mut out, pos == d.len());
                        if err {
                            return Err(invalid(format!("Some characters cannot be represented in {}", self.name())));
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
        if nibble.is_some() {
            return Err(invalid("Hexadecimal text has an odd number of digits, so the last byte is incomplete"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for &(_, label) in ENCODINGS {
            let c = Codec::named(label).unwrap_or_else(|e| panic!("{label}: {e}"));
            assert!(Codec::named(c.name()).unwrap() == c, "{label} -> {}", c.name());
        }
    }

    #[test]
    fn guesses_legacy_text() {
        let sjis = encoding_rs::SHIFT_JIS.encode("これは日本語の文章です。テキストエディタで開きます。").0;
        assert_eq!(guess(&sjis, true).name(), "Shift_JIS");
        let ru = encoding_rs::WINDOWS_1251.encode("Это обычный русский текст для проверки кодировки файла.").0;
        assert_eq!(guess(&ru, true).name(), "windows-1251");
        let fr = WINDOWS_1252.encode("Le café était fermé, déjà à côté de l'hôtel.").0;
        assert_eq!(guess(&fr, true).name(), "windows-1252");
    }

    #[cfg(windows)]
    #[test]
    fn dos_table_maps_box_drawing() {
        let Codec::Table(t) = Codec::named("cp437").unwrap() else { panic!() };
        assert_eq!(t.chars[0xc9], Some('╔'));
        assert_eq!(t.back.binary_search_by_key(&'╔', |x| x.0).map(|k| t.back[k].1), Ok(0xc9));
    }
}
