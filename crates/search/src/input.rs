//! What a search reads from a file: its bytes as they are, or decompressed
//! (`-z`), or what a preprocessor prints for it (`--pre`), and decoded to
//! UTF-8 when it starts with a byte-order mark or `-E` names its encoding.
//! Everything here is a stream, so a large compressed file is searched as
//! it is decompressed, never held whole.
use crate::ignore::{Rules, Verdict};
use std::{
    io::{self, Read},
    path::Path,
    process::{Child, Command, Stdio},
};

/// How file bytes become text.
#[derive(Clone, Copy, Debug, Default)]
pub enum Encoding {
    /// UTF-16 with a byte-order mark is decoded and a UTF-8 mark dropped;
    /// anything else is searched as the bytes it is.
    #[default]
    Sniff,
    /// The bytes as they are, mark and all (`-E none`).
    Raw,
    /// Decoded from this encoding, unless a byte-order mark says otherwise.
    Named(&'static encoding_rs::Encoding),
}

impl Encoding {
    /// `-E`'s value: an encoding label, `auto`, or `none`.
    pub fn from_label(label: &str) -> Result<Self, String> {
        match label.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Sniff),
            "none" => Ok(Self::Raw),
            other => encoding_rs::Encoding::for_label(other.as_bytes())
                .map(Self::Named)
                .ok_or_else(|| format!("{label} is not an encoding lw knows; see https://encoding.spec.whatwg.org/#concept-encoding-get")),
        }
    }
}

/// A compression format, known by the file's extension, as ripgrep knows it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Packed {
    Gzip,
    Bzip2,
    Xz,
    Lzma,
    Lz4,
    Zstd,
    Brotli,
    /// Unix `compress`, `.Z`.
    Compress,
}

impl Packed {
    pub fn of(path: &str) -> Option<Self> {
        let ext = path.rsplit_once('.').map(|(_, e)| e)?;
        Some(match ext {
            "Z" => Self::Compress,
            _ => match ext.to_ascii_lowercase().as_str() {
                "gz" | "tgz" => Self::Gzip,
                "bz2" | "tbz2" | "tbz" => Self::Bzip2,
                "xz" | "txz" => Self::Xz,
                "lzma" => Self::Lzma,
                "lz4" => Self::Lz4,
                "zst" | "zstd" => Self::Zstd,
                "br" => Self::Brotli,
                _ => return None,
            },
        })
    }

    /// The format a file's first bytes announce, for formats that have a mark
    /// (lzma and brotli have none and are known by extension only).
    pub fn sniff(head: &[u8]) -> Option<Self> {
        Some(match head {
            [0x1F, 0x8B, ..] => Self::Gzip,
            [b'B', b'Z', b'h', ..] => Self::Bzip2,
            [0xFD, b'7', b'z', b'X', b'Z', 0x00, ..] => Self::Xz,
            [0x28, 0xB5, 0x2F, 0xFD, ..] => Self::Zstd,
            [0x04, 0x22, 0x4D, 0x18, ..] => Self::Lz4,
            [0x1F, 0x9D, ..] => Self::Compress,
            _ => return None,
        })
    }

    /// A stream of the decompressed bytes.
    pub fn open<'a>(self, packed: Box<dyn Read + Send + 'a>) -> io::Result<Box<dyn Read + Send + 'a>> {
        Ok(match self {
            Self::Gzip => Box::new(flate2::read::MultiGzDecoder::new(packed)),
            Self::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(packed)),
            Self::Xz => Box::new(xz2::read::XzDecoder::new_multi_decoder(packed)),
            Self::Lzma => Box::new(xz2::read::XzDecoder::new_stream(packed, xz2::stream::Stream::new_lzma_decoder(u64::MAX).map_err(io::Error::other)?)),
            Self::Lz4 => Box::new(lz4_flex::frame::FrameDecoder::new(packed)),
            Self::Zstd => Box::new(zstd::stream::read::Decoder::new(packed)?),
            Self::Brotli => Box::new(brotli_decompressor::Decompressor::new(packed, 64 * 1024)),
            Self::Compress => Box::new(Unlzw::new(packed)),
        })
    }
}

/// What turns files into text, chosen on the command line.
#[derive(Default)]
pub struct Config {
    pub encoding: Encoding,
    /// Search inside compressed files (`-z`).
    pub unpack: bool,
    /// A program whose output is searched instead of each file (`--pre`).
    pub pre: Option<String>,
    /// The files `--pre` is used for (`--pre-glob`), and whether some glob
    /// picks rather than leaves out; all files when there is none.
    pub pre_globs: Option<(Rules, bool)>,
}

impl Config {
    /// Whether a file's bytes can be searched as read: no preprocessor, no
    /// decompression by its name, no named encoding. A byte-order mark, and
    /// with -z a compression mark in the bytes, are checked apart.
    pub fn plain_for(&self, path: &str) -> bool {
        self.pre_for(path).is_none() && !(self.unpack && Packed::of(path).is_some()) && !matches!(self.encoding, Encoding::Named(_))
    }

    /// With -z, the compression a file's bytes start with.
    pub fn packed_bytes(&self, bytes: &[u8]) -> Option<Packed> {
        self.unpack.then(|| Packed::sniff(bytes)).flatten()
    }

    fn pre_for(&self, path: &str) -> Option<&str> {
        let pre = self.pre.as_deref()?;
        match &self.pre_globs {
            None => Some(pre),
            // A --pre-glob picks where an ignore line would ignore, as -g does:
            // with only ! globs, every file they do not name goes to --pre.
            Some((globs, picking)) => match globs.verdict(path.replace('\\', "/").as_bytes(), false) {
                Some(Verdict::Ignored) => Some(pre),
                Some(Verdict::Kept) => None,
                None => (!picking).then_some(pre),
            },
        }
    }

    /// The text of one file as a stream: through `--pre` or a decompressor
    /// when they apply, then decoded. `shown` is the path as the walk names
    /// it, for choosing by extension and glob.
    pub fn open<'a>(&self, path: &Path, shown: &str) -> io::Result<Source<'a>> {
        let mut child = None;
        let raw: Box<dyn Read + Send + 'a> = match self.pre_for(shown) {
            Some(command) => {
                let mut spawned = Command::new(command)
                    .arg(path)
                    .stdin(std::fs::File::open(path)?)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|e| io::Error::new(e.kind(), format!("preprocessor {command}: {e}")))?;
                let out = spawned.stdout.take().expect("piped");
                child = Some((spawned, command.to_owned()));
                Box::new(out)
            }
            None => {
                let mut file = std::fs::File::open(path)?;
                if !self.unpack {
                    Box::new(file)
                } else {
                    // Known by extension, or else by the mark its first bytes carry.
                    let mut head = [0u8; 6];
                    let mut got = 0;
                    while got < head.len() {
                        let n = file.read(&mut head[got..])?;
                        if n == 0 {
                            break;
                        }
                        got += n;
                    }
                    let stream: Box<dyn Read + Send + 'a> = Box::new(io::Cursor::new(head[..got].to_vec()).chain(file));
                    match Packed::of(shown).or_else(|| Packed::sniff(&head[..got])) {
                        Some(packed) => packed.open(stream)?,
                        None => stream,
                    }
                }
            }
        };
        Ok(Source { reader: decode(raw, self.encoding), child })
    }

    /// Bytes already read, compressed as `packed`, as a stream of their text.
    pub fn from_bytes<'a>(&self, bytes: Vec<u8>, packed: Packed) -> io::Result<Source<'a>> {
        let stream = packed.open(Box::new(io::Cursor::new(bytes)))?;
        Ok(Source { reader: decode(stream, self.encoding), child: None })
    }

    /// Standard input as a stream of text.
    pub fn stdin<'a>(&self) -> Source<'a> {
        Source { reader: decode(Box::new(io::stdin()), self.encoding), child: None }
    }
}

/// The program a command name runs: a path as given, or the first match in
/// PATH, trying each extension in PATHEXT, as Windows itself looks.
pub fn find_program(name: &str) -> Result<std::path::PathBuf, String> {
    let given = Path::new(name);
    let extensions: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|e| !e.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let candidates = |base: &Path| -> Vec<std::path::PathBuf> {
        let mut all = vec![base.to_path_buf()];
        if base.extension().is_none() {
            all.extend(extensions.iter().map(|e| base.with_extension(e.trim_start_matches('.'))));
        }
        all
    };
    if given.components().count() > 1 || given.is_absolute() {
        return candidates(given).into_iter().find(|p| p.is_file()).ok_or_else(|| format!("{name}: could not find executable"));
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .flat_map(|folder| candidates(&folder.join(name)))
        .find(|p| p.is_file())
        .ok_or_else(|| format!("{name}: could not find executable in PATH"))
}

/// A stream of a file's text, and the preprocessor making it, if any.
pub struct Source<'a> {
    pub reader: Box<dyn Read + Send + 'a>,
    child: Option<(Child, String)>,
}

impl Source<'_> {
    /// Waits for the preprocessor and turns its failure into an error with what it printed.
    pub fn finish(self) -> io::Result<()> {
        drop(self.reader);
        let Some((child, command)) = self.child else { return Ok(()) };
        let output = child.wait_with_output()?;
        if output.status.success() {
            return Ok(());
        }
        let said = String::from_utf8_lossy(&output.stderr);
        Err(io::Error::other(format!("preprocessor {command} failed ({}): {}", output.status, said.trim())))
    }
}

/// Bytes already read, decoded as `encoding` says: borrowed when they need
/// nothing, else the UTF-8 text.
pub fn decode_bytes(bytes: &[u8], encoding: Encoding) -> std::borrow::Cow<'_, [u8]> {
    use std::borrow::Cow;
    let (sniffed, mark) = match encoding_rs::Encoding::for_bom(bytes) {
        Some((found, len)) => (Some(found), len),
        None => (None, 0),
    };
    let chosen = match (encoding, sniffed) {
        (Encoding::Raw, _) => return Cow::Borrowed(bytes),
        (_, Some(found)) => found,
        (Encoding::Named(named), None) => named,
        (Encoding::Sniff, None) => return Cow::Borrowed(bytes),
    };
    if chosen == encoding_rs::UTF_8 && sniffed.is_some() && matches!(encoding, Encoding::Sniff) {
        return Cow::Borrowed(&bytes[mark..]);
    }
    let (text, _) = chosen.decode_without_bom_handling(&bytes[mark..]);
    Cow::Owned(text.into_owned().into_bytes())
}

/// A stream decoded as `encoding` says.
fn decode<'a>(inner: Box<dyn Read + Send + 'a>, encoding: Encoding) -> Box<dyn Read + Send + 'a> {
    match encoding {
        Encoding::Raw => inner,
        _ => Box::new(Decoding { inner, encoding, decoder: None, started: false, input: Vec::new(), at: 0, ended: false, passthrough: false, spill: Vec::new(), spill_at: 0 }),
    }
}

/// Decodes a stream to UTF-8, choosing by its byte-order mark first.
struct Decoding<'a> {
    inner: Box<dyn Read + Send + 'a>,
    encoding: Encoding,
    decoder: Option<encoding_rs::Decoder>,
    started: bool,
    /// Bytes read but not yet decoded, from `at`.
    input: Vec<u8>,
    at: usize,
    ended: bool,
    /// No decoding at all: the bytes go out as they came.
    passthrough: bool,
    /// Decoded bytes that did not fit the caller's buffer, from `spill_at`.
    spill: Vec<u8>,
    spill_at: usize,
}

impl Decoding<'_> {
    /// Reads until there are at least `want` bytes waiting, or the stream ends.
    fn fill(&mut self, want: usize) -> io::Result<()> {
        if self.at > 0 && self.at == self.input.len() {
            self.input.clear();
            self.at = 0;
        }
        while self.input.len() - self.at < want && !self.ended {
            let mut block = [0u8; 64 * 1024];
            let n = self.inner.read(&mut block)?;
            if n == 0 {
                self.ended = true;
            }
            self.input.extend_from_slice(&block[..n]);
        }
        Ok(())
    }

    fn start(&mut self) -> io::Result<()> {
        self.started = true;
        self.fill(3)?;
        let head = &self.input[self.at..];
        let sniffed = encoding_rs::Encoding::for_bom(head);
        let chosen = match (self.encoding, sniffed) {
            (_, Some((found, len))) => {
                self.at += len;
                if found == encoding_rs::UTF_8 && matches!(self.encoding, Encoding::Sniff) {
                    self.passthrough = true;
                    return Ok(());
                }
                found
            }
            (Encoding::Named(named), None) => named,
            _ => {
                self.passthrough = true;
                return Ok(());
            }
        };
        self.decoder = Some(chosen.new_decoder_without_bom_handling());
        Ok(())
    }
}

impl Read for Decoding<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if !self.started {
            self.start()?;
        }
        if self.passthrough {
            if self.at < self.input.len() {
                let n = (self.input.len() - self.at).min(out.len());
                out[..n].copy_from_slice(&self.input[self.at..self.at + n]);
                self.at += n;
                return Ok(n);
            }
            return self.inner.read(out);
        }
        // Bytes of a character that did not fit last time go first.
        if self.spill_at < self.spill.len() {
            let n = (self.spill.len() - self.spill_at).min(out.len());
            out[..n].copy_from_slice(&self.spill[self.spill_at..self.spill_at + n]);
            self.spill_at += n;
            return Ok(n);
        }
        // The decoder needs room for a whole character; a smaller buffer gets
        // what fits, and the rest is kept for the next read.
        if out.len() < 4 {
            let mut small = [0u8; 4];
            let n = self.read(&mut small)?;
            let fit = n.min(out.len());
            out[..fit].copy_from_slice(&small[..fit]);
            self.spill.clear();
            self.spill.extend_from_slice(&small[fit..n]);
            self.spill_at = 0;
            return Ok(fit);
        }
        loop {
            self.fill(1)?;
            let decoder = self.decoder.as_mut().expect("started");
            let last = self.ended;
            let (_, read, written, _) = decoder.decode_to_utf8(&self.input[self.at..], out, last);
            self.at += read;
            if written > 0 || (last && self.at == self.input.len()) {
                return Ok(written);
            }
        }
    }
}

/// Unix `compress` (`.Z`) decompression: LZW with codes of 9 to 16 bits,
/// written in groups of eight codes, and a code (256) that clears the table
/// in block mode.
struct Unlzw<'a> {
    inner: Box<dyn Read + Send + 'a>,
    header: bool,
    max_bits: u32,
    block_mode: bool,
    bits: u32,
    /// Bytes of the current group of eight codes.
    group: Vec<u8>,
    bit: usize,
    prefix: Vec<u32>,
    suffix: Vec<u8>,
    previous: Option<u32>,
    first_byte: u8,
    pending: Vec<u8>,
    at: usize,
    ended: bool,
    stack: Vec<u8>,
}

impl<'a> Unlzw<'a> {
    fn new(inner: Box<dyn Read + Send + 'a>) -> Self {
        Self {
            inner,
            header: false,
            max_bits: 16,
            block_mode: true,
            bits: 9,
            group: Vec::new(),
            bit: 0,
            prefix: Vec::new(),
            suffix: Vec::new(),
            previous: None,
            first_byte: 0,
            pending: Vec::new(),
            at: 0,
            ended: false,
            stack: Vec::new(),
        }
    }

    fn reset(&mut self) {
        self.prefix = (0..256).map(|_| 0).collect();
        self.suffix = (0..=255).collect();
        if self.block_mode {
            // Code 256 is the clear code and holds no string.
            self.prefix.push(0);
            self.suffix.push(0);
        }
        self.bits = 9;
        self.previous = None;
    }

    /// Reads the next group of `bits`-bit codes: compress writes codes eight at a time.
    fn next_group(&mut self) -> io::Result<bool> {
        let want = self.bits as usize;
        self.group.resize(want, 0);
        let mut got = 0;
        while got < want {
            let n = self.inner.read(&mut self.group[got..])?;
            if n == 0 {
                break;
            }
            got += n;
        }
        self.group.truncate(got);
        self.bit = 0;
        Ok(got > 0)
    }

    fn next_code(&mut self) -> io::Result<Option<u32>> {
        loop {
            let bits = self.bits as usize;
            if self.bit + bits <= self.group.len() * 8 {
                let mut code = 0u32;
                for i in 0..bits {
                    let at = self.bit + i;
                    if self.group[at / 8] & (1 << (at % 8)) != 0 {
                        code |= 1 << i;
                    }
                }
                self.bit += bits;
                return Ok(Some(code));
            }
            if !self.next_group()? {
                return Ok(None);
            }
        }
    }

    fn produce(&mut self) -> io::Result<bool> {
        if !self.header {
            let mut head = [0u8; 3];
            self.inner.read_exact(&mut head).map_err(|_| io::Error::other("a .Z file shorter than its header"))?;
            if head[0] != 0x1F || head[1] != 0x9D {
                return Err(io::Error::other("not a .Z (compress) file"));
            }
            self.max_bits = u32::from(head[2] & 0x1F);
            self.block_mode = head[2] & 0x80 != 0;
            if !(9..=16).contains(&self.max_bits) {
                return Err(io::Error::other("a .Z file with an unsupported code size"));
            }
            self.header = true;
            self.reset();
        }
        let Some(code) = self.next_code()? else { return Ok(false) };
        if self.block_mode && code == 256 {
            // Clear: the rest of this group is padding; start again at 9 bits.
            self.reset();
            self.group.clear();
            self.bit = 0;
            return Ok(true);
        }
        let next = self.prefix.len() as u32;
        let mut current = code;
        self.stack.clear();
        if code > next || (code == next && self.previous.is_none()) {
            return Err(io::Error::other("a damaged .Z file"));
        }
        if code == next {
            self.stack.push(self.first_byte);
            current = self.previous.expect("checked");
        }
        while current >= 256 {
            self.stack.push(self.suffix[current as usize]);
            current = self.prefix[current as usize];
        }
        self.stack.push(current as u8);
        self.first_byte = current as u8;
        self.pending.extend(self.stack.iter().rev());
        if let Some(previous) = self.previous {
            if (self.prefix.len() as u32) < (1 << self.max_bits) {
                self.prefix.push(previous);
                self.suffix.push(self.first_byte);
            }
        }
        self.previous = Some(code);
        // The code size grows when the table fills it; a new size starts a new group.
        if self.prefix.len() as u32 >= (1 << self.bits) && self.bits < self.max_bits {
            self.bits += 1;
            self.group.clear();
            self.bit = 0;
        }
        Ok(true)
    }
}

impl Read for Unlzw<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.at == self.pending.len() {
            self.pending.clear();
            self.at = 0;
            if self.ended || !self.produce()? {
                self.ended = true;
                return Ok(0);
            }
        }
        let n = (self.pending.len() - self.at).min(out.len());
        out[..n].copy_from_slice(&self.pending[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn all(mut r: impl Read) -> Vec<u8> {
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn compressed_streams_come_out_as_written() {
        let text: Vec<u8> = (0..20_000).flat_map(|i| format!("line {i} of the test\n").into_bytes()).collect();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&text).unwrap();
        let gz = gz.finish().unwrap();
        assert_eq!(all(Packed::Gzip.open(Box::new(std::io::Cursor::new(gz))).unwrap()), text);
        let zst = zstd::stream::encode_all(&text[..], 3).unwrap();
        assert_eq!(all(Packed::Zstd.open(Box::new(std::io::Cursor::new(zst))).unwrap()), text);
        let mut lz = lz4_flex::frame::FrameEncoder::new(Vec::new());
        lz.write_all(&text).unwrap();
        let lz = lz.finish().unwrap();
        assert_eq!(all(Packed::Lz4.open(Box::new(std::io::Cursor::new(lz))).unwrap()), text);
        assert_eq!(Packed::of("a/b.tar.gz"), Some(Packed::Gzip));
        assert_eq!(Packed::of("x.Z"), Some(Packed::Compress));
        assert_eq!(Packed::of("x.z"), None);
        assert_eq!(Packed::sniff(&[0x1F, 0x8B, 8, 0]), Some(Packed::Gzip));
        assert_eq!(Packed::sniff(b"BZh9"), Some(Packed::Bzip2));
        assert_eq!(Packed::sniff(b"plain"), None);
        assert!(find_program("cmd").is_ok());
        assert!(find_program("no-such-program-anywhere").unwrap_err().contains("could not find executable in PATH"));
    }

    #[test]
    fn marks_and_encodings_decode() {
        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain("héllo\n".encode_utf16().flat_map(u16::to_le_bytes)).collect();
        assert_eq!(all(decode(Box::new(std::io::Cursor::new(utf16.clone())), Encoding::Sniff)), "héllo\n".as_bytes());
        assert_eq!(decode_bytes(&utf16, Encoding::Sniff).as_ref(), "héllo\n".as_bytes());
        assert_eq!(all(decode(Box::new(std::io::Cursor::new(b"\xEF\xBB\xBFabc".to_vec())), Encoding::Sniff)), b"abc");
        assert_eq!(all(decode(Box::new(std::io::Cursor::new(b"\xEF\xBB\xBFabc".to_vec())), Encoding::Raw)), b"\xEF\xBB\xBFabc");
        let latin = Encoding::from_label("latin1").unwrap();
        assert_eq!(all(decode(Box::new(std::io::Cursor::new(b"caf\xE9".to_vec())), latin)), "café".as_bytes());
        assert_eq!(all(decode(Box::new(std::io::Cursor::new(b"plain".to_vec())), Encoding::Sniff)), b"plain");
        assert!(Encoding::from_label("klingon").is_err());
        // Read one byte at a time, a character split across reads arrives whole.
        let mut one = decode(Box::new(std::io::Cursor::new(b"caf\xE9!".to_vec())), latin);
        let mut got = Vec::new();
        let mut byte = [0u8; 1];
        while one.read(&mut byte).unwrap() == 1 {
            got.push(byte[0]);
        }
        assert_eq!(got, "café!".as_bytes());
    }
}
