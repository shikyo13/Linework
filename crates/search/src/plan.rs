//! A search as its settings describe it, compiled once: the patterns, the
//! file choice, the conditions and the line selection. lw and the editor both
//! search from a `Plan`, so the same words mean the same search in both.
use crate::matcher::{self, Query};
use crate::pick::Pick;
use crate::searcher::{self, Searcher};
use crate::settings::{Near, Settings};

pub struct Plan {
    /// Every pattern, compiled together.
    pub query: Query,
    /// The globs and types that choose files, if any were given.
    pub pick: Option<Pick>,
    /// `--and`: a file is searched only if each of these matches somewhere in it.
    pub and: Vec<Query>,
    /// `--not`: a file is searched only if none of these matches anywhere in it.
    pub not: Vec<Query>,
    /// `--near`: a line is kept only if each of these matches within its reach, on its side.
    pub near: Vec<(Query, u64, Near)>,
    /// Context, inversion and the limits on selected lines.
    pub config: searcher::Config,
}

/// The lines each `--near` pattern matches in one file, ready to test a line against.
pub struct NearLines(Vec<(Vec<u64>, u64, Near)>);

impl NearLines {
    /// From each `--near` pattern's ascending matching line numbers, with its reach and side.
    pub fn new(lines: Vec<(Vec<u64>, u64, Near)>) -> Self {
        Self(lines)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether line `number` has every `--near` pattern within reach.
    pub fn keeps(&self, number: u64) -> bool {
        self.0.iter().all(|(numbers, reach, side)| close(numbers, number, *reach, *side))
    }
}

impl Plan {
    /// The plan for `patterns` under the settings `s`.
    pub fn new(s: &Settings, patterns: Vec<String>) -> Result<Self, String> {
        let mut spec = s.matcher.clone();
        spec.patterns = patterns;
        let query = Query::build(&spec).map_err(|e| format!("regex parse error: {e}"))?;
        let pick = Pick::new(&s.choices, &s.types, s.glob_fold)?;
        // A condition's pattern takes the main pattern's case and word rules, and is a regular expression where it looks like one.
        let other = |p: &String| {
            let mut one = s.matcher.clone();
            one.patterns = vec![p.clone()];
            one.syntax = matcher::Syntax::Auto;
            Query::build(&one).map_err(|e| format!("regex parse error in {p:?}: {e}"))
        };
        let and = s.and.iter().map(other).collect::<Result<Vec<_>, _>>()?;
        let not = s.not.iter().map(other).collect::<Result<Vec<_>, _>>()?;
        let near = s.near.iter().map(|(p, n, side)| Ok((other(p)?, *n, *side))).collect::<Result<Vec<_>, String>>()?;
        let (before, after) = s.context_lines();
        let config = searcher::Config {
            invert: s.invert,
            before,
            after,
            passthru: s.passthru,
            max_count: s.max_count,
            stop_on_nonmatch: s.stop_on_nonmatch,
            numbered: true,
        };
        Ok(Self { query, pick, and, not, near, config })
    }

    /// Whether a file must be read whole before it is searched: a pattern
    /// that may span lines, a condition over the whole file, or `--near`.
    pub fn needs_whole_text(&self) -> bool {
        self.query.multiline() || !self.and.is_empty() || !self.not.is_empty() || !self.near.is_empty()
    }

    /// `--and` and `--not` over a file's whole text.
    pub fn file_passes(&self, text: &[u8]) -> bool {
        self.and.iter().all(|q| q.within(text)) && !self.not.iter().any(|q| q.within(text))
    }

    /// The `--near` lines of a file's whole text.
    pub fn near_lines(&self, text: &[u8]) -> NearLines {
        NearLines(
            self.near
                .iter()
                .map(|(q, reach, side)| {
                    let mut numbers = Vec::new();
                    let mut searcher = Searcher::new(q, searcher::Config { numbered: true, ..searcher::Config::default() });
                    searcher.search(text, &mut Numbers(&mut numbers));
                    (numbers, *reach, *side)
                })
                .collect(),
        )
    }

    /// A searcher for one file, keeping only the lines `keep` allows when given.
    pub fn searcher<'q>(&'q self, keep: Option<searcher::Keep<'q>>) -> Searcher<'q> {
        let s = Searcher::new(&self.query, self.config);
        match keep {
            Some(keep) => s.keeping(keep),
            None => s,
        }
    }
}

#[cfg(windows)]
impl Plan {
    /// How a folder is walked under the settings `s`: the ignore rules, the
    /// file choice, depth, size, links. The caller adds its own reporting.
    pub fn walk_options<'a>(&'a self, s: &'a Settings) -> crate::walk::Options<'a> {
        let mut options = crate::walk::Options {
            hidden: s.hidden,
            ignore: s.ignore,
            ignore_dot: s.ignore_dot,
            ignore_vcs: s.ignore_vcs,
            ignore_exclude: s.ignore_exclude,
            ignore_global: s.ignore_global,
            ignore_parent: s.ignore_parent,
            extra: &s.ignore_files,
            ignore_fold: s.ignore_fold,
            batch: 8,
            gitignore_anywhere: !s.require_git,
            pick: self.pick.as_ref(),
            max_depth: s.max_depth,
            max_filesize: s.max_filesize,
            follow: s.follow,
            one_file_system: s.one_file_system,
            ..crate::walk::Options::default()
        };
        if let Some(threads) = s.threads {
            options.threads = threads;
        }
        options
    }
}

/// Collects the numbers of matching lines.
struct Numbers<'a>(&'a mut Vec<u64>);

impl searcher::Sink for Numbers<'_> {
    fn matched(&mut self, number: u64, _offset: u64, _text: &[u8], _ended: bool) -> bool {
        self.0.push(number);
        true
    }
    fn context(&mut self, _number: u64, _offset: u64, _text: &[u8], _after: bool, _ended: bool) -> bool {
        true
    }
    fn context_break(&mut self) -> bool {
        true
    }
}

/// Whether any of the ascending `numbers` lies within `reach` lines of `line`, on the side asked.
fn close(numbers: &[u64], line: u64, reach: u64, side: Near) -> bool {
    let (low, high) = match side {
        Near::Either => (line.saturating_sub(reach), line + reach),
        Near::Before => (line.saturating_sub(reach), line),
        Near::After => (line, line + reach),
    };
    let from = numbers.partition_point(|&n| n < low);
    numbers.get(from).is_some_and(|&n| n <= high)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn near_looks_on_the_side_asked() {
        let numbers = [10, 20];
        assert!(close(&numbers, 12, 2, Near::Either));
        assert!(!close(&numbers, 13, 2, Near::Either));
        assert!(close(&numbers, 12, 2, Near::Before));
        assert!(!close(&numbers, 8, 2, Near::Before));
        assert!(close(&numbers, 8, 2, Near::After));
        assert!(!close(&numbers, 12, 2, Near::After));
    }

    #[test]
    fn conditions_and_near_come_from_the_same_words_as_lw() {
        let s = Settings::from_words(["--and", "beta", "--near", "gamma", "1", "--not", "delta"].map(String::from)).unwrap();
        let plan = Plan::new(&s, vec!["alpha".into()]).unwrap();
        assert!(plan.needs_whole_text());
        assert!(plan.file_passes(b"alpha\nbeta\n"));
        assert!(!plan.file_passes(b"alpha\nbeta\ndelta\n"));
        assert!(!plan.file_passes(b"alpha\n"));
        let near = plan.near_lines(b"alpha\ngamma\n\n\nalpha\n");
        assert!(near.keeps(1) && !near.keeps(5));
    }
}
