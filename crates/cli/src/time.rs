//! Moments as logs write them, and the span of a time-ordered log between two
//! moments, found by bisecting its timestamps instead of reading it.
//!
//! A log's format is taken from its first timestamped line and then only that
//! format is read, so a date inside a message does not count. A line without a
//! timestamp, such as part of a stack trace, belongs to the line before it.
use linework_storage::Document;
use regex::bytes::Regex;
use std::{cmp::Ordering, io, ops::Range, sync::OnceLock};

/// A moment as a log line writes it. When either side has no year, as in
/// syslog, two moments compare from the month.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Moment {
    year: u32,
    month: u32,
    day: u32,
    /// Seconds since midnight, and the fraction of the second.
    seconds: u32,
    nanos: u32,
}

impl Moment {
    fn key(&self, years: bool) -> (u32, u32, u32, u32, u32) {
        (if years { self.year } else { 0 }, self.month, self.day, self.seconds, self.nanos)
    }
    pub fn compare(&self, other: &Moment) -> Ordering {
        let years = self.year != 0 && other.year != 0;
        self.key(years).cmp(&other.key(years))
    }
}

/// The timestamp formats read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// `2026-11-21 21:40:15.123`, also with `T`, a comma, or no seconds.
    Iso,
    /// `21/Nov/2026:21:40:15`, as web servers write it.
    Common,
    /// `Nov 21 21:40:15` at the start of the line, without a year.
    Syslog,
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

fn patterns() -> &'static [(Format, Regex)] {
    static PATTERNS: OnceLock<Vec<(Format, Regex)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let months = MONTHS.join("|");
        vec![
            (Format::Iso, Regex::new(r"(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2})(?::(\d{2})(?:[.,](\d{1,9}))?)?").unwrap()),
            (Format::Common, Regex::new(&format!(r"(\d{{2}})/({months})/(\d{{4}}):(\d{{2}}):(\d{{2}}):(\d{{2}})")).unwrap()),
            (Format::Syslog, Regex::new(&format!(r"^({months}) +(\d{{1,2}}) (\d{{2}}):(\d{{2}}):(\d{{2}})")).unwrap()),
        ]
    })
}

fn number(bytes: Option<regex::bytes::Match>) -> u32 {
    bytes.and_then(|m| std::str::from_utf8(m.as_bytes()).ok()?.parse().ok()).unwrap_or(0)
}

fn month(bytes: Option<regex::bytes::Match>) -> u32 {
    bytes.and_then(|m| MONTHS.iter().position(|name| name.as_bytes() == m.as_bytes())).map_or(0, |i| i as u32 + 1)
}

/// A fraction of a second written with 1 to 9 digits, in nanoseconds.
fn fraction(bytes: Option<regex::bytes::Match>) -> u32 {
    let Some(m) = bytes else { return 0 };
    let digits = m.as_bytes();
    number(Some(m)) * 10u32.pow(9 - digits.len() as u32)
}

/// The moment of a line by `format`; only the first 200 bytes are read.
pub fn of_line(format: Format, line: &[u8]) -> Option<Moment> {
    let head = &line[..line.len().min(200)];
    let (_, pattern) = patterns().iter().find(|(f, _)| *f == format)?;
    let c = pattern.captures(head)?;
    let clock = |h: usize, m: usize, s: usize| number(c.get(h)) * 3600 + number(c.get(m)) * 60 + number(c.get(s));
    Some(match format {
        Format::Iso => Moment { year: number(c.get(1)), month: number(c.get(2)), day: number(c.get(3)), seconds: clock(4, 5, 6), nanos: fraction(c.get(7)) },
        Format::Common => Moment { year: number(c.get(3)), month: month(c.get(2)), day: number(c.get(1)), seconds: clock(4, 5, 6), nanos: 0 },
        Format::Syslog => Moment { year: 0, month: month(c.get(1)), day: number(c.get(2)), seconds: clock(3, 4, 5), nanos: 0 },
    })
}

/// The format of a line, trying each in turn.
fn detect(line: &[u8]) -> Option<(Format, Moment)> {
    patterns().iter().find_map(|(format, _)| of_line(*format, line).map(|m| (*format, m)))
}

/// A time as a person writes it: a date and time (`2026-11-21 21:40`,
/// `2026-11-21T21:40:15.5`), a date alone, or a time alone, which takes the
/// date of `first`, the log's first timestamp. Returns the start of the time
/// written and the end of it at the precision written: the end of that day,
/// minute, second or fraction, so that `--until 21:45` includes 21:45:30.
pub fn parse(text: &str, first: &Moment) -> Result<(Moment, Moment), String> {
    let text = text.trim();
    let iso = Regex::new(r"^(\d{4})-(\d{2})-(\d{2})(?:[T ](\d{2}):(\d{2})(?::(\d{2})(?:[.,](\d{1,9}))?)?)?$").unwrap();
    let clock = Regex::new(r"^(\d{1,2}):(\d{2})(?::(\d{2})(?:[.,](\d{1,9}))?)?$").unwrap();
    // The date, and the capture group where the hour is.
    let (date, c, hour) = if let Some(c) = iso.captures(text.as_bytes()) {
        (Moment { year: number(c.get(1)), month: number(c.get(2)), day: number(c.get(3)), seconds: 0, nanos: 0 }, c, 4)
    } else if let Some(c) = clock.captures(text.as_bytes()) {
        (Moment { seconds: 0, nanos: 0, ..*first }, c, 1)
    } else {
        return Err(format!("{text} is not a time; write 2026-11-21 21:40, 2026-11-21 or 21:40:15"));
    };
    let (h, m, s, f) = (c.get(hour), c.get(hour + 1), c.get(hour + 2), c.get(hour + 3));
    if h.is_none() {
        // A date alone: the whole day.
        return Ok((date, Moment { day: date.day + 1, ..date }));
    }
    let start = Moment { seconds: number(h) * 3600 + number(m) * 60 + number(s), nanos: fraction(f), ..date };
    // How long the written precision lasts, in nanoseconds.
    let unit: u64 = match (s, f) {
        (None, _) => 60_000_000_000,
        (Some(_), None) => 1_000_000_000,
        (Some(_), Some(digits)) => 10u64.pow(9 - digits.as_bytes().len() as u32),
    };
    let total = u64::from(start.nanos) + unit;
    // Seconds past midnight may pass 86,399 here: still after every time that day and before the next.
    let end = Moment { seconds: start.seconds + (total / 1_000_000_000) as u32, nanos: (total % 1_000_000_000) as u32, ..start };
    Ok((start, end))
}

/// The start of the first line at or after `at`. Lines end with LF.
fn line_start(doc: &Document, at: u64) -> io::Result<u64> {
    if at == 0 {
        return Ok(0);
    }
    let mut pos = at - 1;
    while pos < doc.len() {
        let block = doc.read(pos, 64 * 1024)?;
        if let Some(i) = memchr::memchr(b'\n', &block) {
            return Ok(pos + i as u64 + 1);
        }
        pos += block.len() as u64;
    }
    Ok(doc.len())
}

/// The bytes read past a place before deciding no timestamp follows it.
const LOOK: u64 = 16 * 1024 * 1024;

/// The first line starting at or after `from` and before `limit` that has a
/// timestamp: where it starts and its moment.
fn next_stamped(doc: &Document, format: Format, from: u64, limit: u64) -> io::Result<Option<(u64, Moment)>> {
    let mut pos = line_start(doc, from)?;
    let give_up = pos.saturating_add(LOOK);
    'blocks: while pos < limit && pos < give_up {
        let block = doc.read(pos, 256 * 1024)?;
        let last = pos + block.len() as u64 >= doc.len();
        let mut at = 0usize;
        while at < block.len() {
            let start = pos + at as u64;
            if start >= limit {
                return Ok(None);
            }
            let end = memchr::memchr(b'\n', &block[at..]).map(|i| at + i);
            if end.is_none() && !last && at > 0 {
                // A line cut by the end of the block is read again from its start.
                pos = start;
                continue 'blocks;
            }
            // A timestamp is in a line's first bytes, so a line longer than the
            // block is judged by its start.
            if let Some(moment) = of_line(format, &block[at..end.unwrap_or(block.len())]) {
                return Ok(Some((start, moment)));
            }
            match end {
                Some(e) => at = e + 1,
                None if last => return Ok(None),
                None => {
                    pos = line_start(doc, pos + block.len() as u64)?;
                    continue 'blocks;
                }
            }
        }
        pos += at as u64;
    }
    Ok(None)
}

/// A time-ordered log's format and first moment.
pub fn first(doc: &Document) -> io::Result<Option<(Format, Moment)>> {
    let block = doc.read(0, LOOK as usize)?;
    Ok(block.split(|&b| b == b'\n').find_map(detect))
}

/// The start of the first timestamped line whose moment is at or after
/// `target` (after it, with `after`), or the end of the text.
pub fn locate(doc: &Document, format: Format, target: &Moment, after: bool) -> io::Result<u64> {
    let early = |m: &Moment| match m.compare(target) {
        Ordering::Less => true,
        Ordering::Equal => after,
        Ordering::Greater => false,
    };
    let (mut low, mut high, mut found) = (0u64, doc.len(), doc.len());
    while high - low > 64 * 1024 {
        let middle = low + (high - low) / 2;
        match next_stamped(doc, format, middle, high)? {
            None => high = middle,
            Some((start, moment)) if early(&moment) => low = start + 1,
            Some((start, _)) => {
                found = start;
                high = start;
            }
        }
    }
    let mut pos = low;
    while let Some((start, moment)) = next_stamped(doc, format, pos, high)? {
        if !early(&moment) {
            return Ok(start);
        }
        pos = start + 1;
    }
    Ok(found)
}

/// The part of a time-ordered log from `since` up to and including `until`.
pub fn span(doc: &Document, since: Option<&str>, until: Option<&str>) -> Result<Range<u64>, String> {
    let io = |e: io::Error| e.to_string();
    let (format, first) = first(doc).map_err(io)?.ok_or("no timestamp was found in the first 16 MiB of the file")?;
    // From the start of the time written to the end of the one written, at its precision.
    let start = match since {
        Some(text) => locate(doc, format, &parse(text, &first)?.0, false).map_err(io)?,
        None => 0,
    };
    let end = match until {
        Some(text) => locate(doc, format, &parse(text, &first)?.1, false).map_err(io)?,
        None => doc.len(),
    };
    Ok(start..end.max(start))
}
