//! The text after a Replace All with too many matches to hold as pieces: the
//! old text, where each match starts, and one copy of the replacement. Reads
//! put the new text together from those, so Replace All costs the search and
//! a few bytes a match, not a copy of the document.
use crate::tree::Node;
use std::{io, sync::Arc};

/// A stretch of the old text and the matches that start in it. Stretches
/// follow one another with no gap, and a match ends inside its own stretch.
pub(crate) struct Stretch {
    /// Where it starts in the old text, and its length there.
    pub old: u64,
    pub old_len: u64,
    /// Where each match starts, from `old`.
    pub hits: Vec<u32>,
    /// Each match's length, or None when every match here is `len` long.
    pub lens: Option<Vec<u32>>,
    pub len: u32,
}

struct Placed {
    old: u64,
    /// Where it starts and ends in the new text.
    new: u64,
    new_end: u64,
    /// The matches' lengths added up.
    lens: u64,
    hits: Box<[u32]>,
    /// The matches' lengths added up before each match; empty when every
    /// match is `fixed` long.
    before: Box<[u32]>,
}

pub(crate) struct Patched {
    base: Arc<Node>,
    replacement: Box<[u8]>,
    fixed: Option<u32>,
    stretches: Vec<Placed>,
    len: u64,
}

impl Patched {
    /// `base` is the old text the stretches are measured in.
    pub fn new(base: Arc<Node>, replacement: &[u8], stretches: Vec<Stretch>) -> Self {
        // One length for every match lets reads work it out instead of looking it up.
        let mut lengths = stretches.iter().filter(|s| !s.hits.is_empty());
        let first = lengths.clone().next().map(|s| s.len);
        let fixed = match first {
            None => Some(0),
            Some(l) => lengths.all(|s| s.lens.is_none() && s.len == l).then_some(l),
        };
        let r = replacement.len() as u64;
        let mut new = 0u64;
        let placed = stretches
            .into_iter()
            .map(|s| {
                let (lens, before) = match (&s.lens, fixed) {
                    (None, _) => (s.hits.len() as u64 * u64::from(s.len), match fixed {
                        Some(_) => Box::default(),
                        None => (0..s.hits.len() as u32).map(|j| j * s.len).collect(),
                    }),
                    (Some(each), _) => {
                        let mut sum = 0u32;
                        let before = each.iter().map(|&l| { let b = sum; sum += l; b }).collect();
                        (u64::from(sum), before)
                    }
                };
                let new_end = new + s.old_len - lens + s.hits.len() as u64 * r;
                let p = Placed { old: s.old, new, new_end, lens, hits: s.hits.into(), before };
                new = new_end;
                p
            })
            .collect();
        Self { base, replacement: replacement.into(), fixed, stretches: placed, len: new }
    }
    pub fn len(&self) -> u64 {
        self.len
    }
    /// Length of match `j` of a stretch.
    fn hit_len(&self, s: &Placed, j: usize) -> u64 {
        match self.fixed {
            Some(l) => u64::from(l),
            None => s.before.get(j + 1).map_or(s.lens, |&b| u64::from(b)) - u64::from(s.before[j]),
        }
    }
    /// Where match `j` of a stretch starts in the new text, from the stretch's start.
    fn new_start(&self, s: &Placed, j: usize) -> u64 {
        let before = match self.fixed {
            Some(l) => j as u64 * u64::from(l),
            None => u64::from(s.before[j]),
        };
        u64::from(s.hits[j]) + j as u64 * self.replacement.len() as u64 - before
    }
    /// The old position of the text at new position `rel` of a stretch, with
    /// the replacement's text counted as the match it replaces: a place inside
    /// a replacement maps to where that match ends.
    fn old_of(&self, s: &Placed, rel: u64) -> u64 {
        let r = self.replacement.len() as u64;
        let j = self.count_before(s, rel);
        if j == 0 {
            return rel;
        }
        let at = self.new_start(s, j - 1);
        let end = u64::from(s.hits[j - 1]) + self.hit_len(s, j - 1);
        if rel < at + r {
            end
        } else {
            end + (rel - at - r)
        }
    }
    /// How many matches of a stretch start at or before new position `rel`.
    fn count_before(&self, s: &Placed, rel: u64) -> usize {
        let (mut lo, mut hi) = (0, s.hits.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.new_start(s, mid) <= rel {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
    pub fn read(&self, mut pos: u64, mut dst: &mut [u8]) -> io::Result<()> {
        if pos + dst.len() as u64 > self.len {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Read outside replaced text"));
        }
        let r = self.replacement.len() as u64;
        let mut i = self.stretches.partition_point(|s| s.new <= pos).saturating_sub(1);
        let mut old = Vec::new();
        while !dst.is_empty() {
            let s = &self.stretches[i];
            let stretch_end = s.new_end;
            let rel = pos - s.new;
            let want = (dst.len() as u64).min(stretch_end - s.new - rel);
            // The old bytes this part of the stretch is made from, read at once.
            let from = self.old_of(s, rel);
            let to = self.old_of(s, rel + want).max(from);
            old.resize((to - from) as usize, 0);
            self.base.read(s.old + from, &mut old)?;
            let mut j = self.count_before(s, rel);
            let mut at = rel;
            let mut out = 0usize;
            while (out as u64) < want {
                let need = want - out as u64;
                // Inside the replacement of match j - 1?
                if j > 0 {
                    let start = self.new_start(s, j - 1);
                    if at < start + r {
                        let k = (start + r - at).min(need) as usize;
                        let off = (at - start) as usize;
                        dst[out..out + k].copy_from_slice(&self.replacement[off..off + k]);
                        out += k;
                        at += k as u64;
                        continue;
                    }
                }
                // Unchanged text up to the next match.
                let next = if j < s.hits.len() { self.new_start(s, j) } else { stretch_end - s.new };
                let k = (next - at).min(need) as usize;
                if k > 0 {
                    // The gap after match j - 1, known without a search.
                    let old_at = if j == 0 {
                        at
                    } else {
                        let start = self.new_start(s, j - 1);
                        u64::from(s.hits[j - 1]) + self.hit_len(s, j - 1) + (at - start - r)
                    };
                    let o = (old_at - from) as usize;
                    dst[out..out + k].copy_from_slice(&old[o..o + k]);
                    out += k;
                    at += k as u64;
                }
                if at == next && j < s.hits.len() {
                    j += 1;
                }
            }
            dst = &mut dst[want as usize..];
            pos += want;
            i += 1;
        }
        Ok(())
    }
}
