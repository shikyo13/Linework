use super::{invalid, Search, Visitor};
use crate::Document;
pub(super) use regex_cursor::engines::meta::Regex;
use regex_cursor::{regex_automata::util::syntax, Cursor, Input};
use std::{
    io,
    ops::Range,
    panic::{catch_unwind, resume_unwind, AssertUnwindSafe},
    sync::atomic::{AtomicBool, Ordering},
};

const WINDOW: usize = 256 * 1024;

pub(super) fn compile(pattern: &str, case: bool) -> io::Result<Regex> {
    Regex::builder()
        .syntax(
            syntax::Config::new()
                .case_insensitive(!case)
                .multi_line(true)
                .crlf(true),
        )
        .configure(Regex::config().nfa_size_limit(Some(16 * 1024 * 1024)))
        .build(pattern)
        .map_err(|e| invalid(e.to_string()))
}
struct PageCursor<'a> {
    doc: &'a Document,
    cancel: &'a AtomicBool,
    page: usize,
    offset: usize,
    first: usize,
    bytes: Vec<u8>,
}
// Cursor's boolean return value is reserved for the real end of the haystack.
// Propagate cancellations and failed file reads across that infallible API without
// returning a match for truncated input. Only our own payload is handled in scan.
struct ReadFailure(io::Error);
impl<'a> PageCursor<'a> {
    fn new(d: &'a Document, cancel: &'a AtomicBool, at: usize) -> io::Result<Self> {
        let mut c = Self {
            doc: d,
            cancel,
            page: 0,
            offset: 0,
            first: 0,
            bytes: vec![],
        };
        let page = at.min((d.len() as usize).saturating_sub(1)) / WINDOW;
        c.load(page)?;
        if c.chunk().is_empty() && page > 0 {
            c.load(page - 1)?;
        }
        Ok(c)
    }
    fn load(&mut self, page: usize) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Search cancelled",
            ));
        }
        let start = page * WINDOW;
        let size = self.doc.len() as usize;
        let end = (start + WINDOW).min(size);
        // Assign the tail of a split UTF-8 scalar to the preceding window.
        // Previous/next movement computes exactly the same boundary either way.
        self.bytes.resize((size - start).min(WINDOW + 3), 0);
        self.doc.read_into(start as u64, &mut self.bytes)?;
        let tail = |at: usize| {
            self.bytes[at..]
                .iter()
                .take(3)
                .take_while(|b| **b & 0xc0 == 0x80)
                .count()
        };
        self.first = if start == 0 { 0 } else { tail(0) };
        let len = end - start;
        let extra = tail(len);
        self.bytes.truncate(len + extra);
        self.offset = start + self.first;
        self.page = page;
        Ok(())
    }
    fn switch(&mut self, n: usize) -> bool {
        if let Err(e) = self.load(n) {
            resume_unwind(Box::new(ReadFailure(e)));
        }
        true
    }
}
impl Cursor for PageCursor<'_> {
    fn chunk(&self) -> &[u8] {
        &self.bytes[self.first..]
    }
    fn offset(&self) -> usize {
        self.offset
    }
    fn total_bytes(&self) -> Option<usize> {
        Some(self.doc.len() as usize)
    }
    fn advance(&mut self) -> bool {
        self.offset + self.chunk().len() < self.doc.len() as usize && self.switch(self.page + 1)
    }
    fn backtrack(&mut self) -> bool {
        self.page > 0 && self.switch(self.page - 1)
    }
}
pub(super) fn scan(
    search: &Search<'_>,
    re: &Regex,
    range: Range<u64>,
    captures: bool,
    visit: &mut Visitor<'_>,
) -> io::Result<bool> {
    if range.start > search.d.len() {
        return Ok(false);
    }
    let c = PageCursor::new(search.d, search.cancel, range.start as usize)?;
    let end = if search.opt.backwards {
        range.end.min(search.scope.end)
    } else {
        search.scope.end
    };
    let mut input = Input::new(c);
    let run = catch_unwind(AssertUnwindSafe(|| -> io::Result<bool> {
        input.set_span(range.start as usize..end as usize);
        if captures {
            for captures in re.captures_iter(input) {
                search.check()?;
                let m = captures.get_match().unwrap();
                if m.start() as u64 >= range.end {
                    break;
                }
                let group = |index| {
                    captures
                        .get_group(index)
                        .map(|g| g.start as u64..g.end as u64)
                };
                if visit(m.start() as u64..m.end() as u64, Some(&group))? {
                    return Ok(true);
                }
            }
        } else {
            let mut last = None;
            for m in re.find_iter(input) {
                search.check()?;
                if m.start() as u64 >= range.end {
                    break;
                }
                let span = m.start() as u64..m.end() as u64;
                if search.opt.backwards {
                    last = Some(span);
                } else if visit(span, None)? {
                    return Ok(true);
                }
            }
            if let Some(span) = last {
                return visit(span, None);
            }
        }
        search.check()?;
        Ok(false)
    }));
    match run {
        Ok(r) => r,
        Err(payload) => match payload.downcast::<ReadFailure>() {
            Ok(e) => Err(e.0),
            Err(other) => resume_unwind(other),
        },
    }
}
