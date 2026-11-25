use super::*;
use std::cell::{Cell, RefCell};
use std::sync::Mutex;

#[derive(Clone)]
struct Request {
    query: String,
    replacement: Option<String>,
    options: SearchOptions,
    selected: bool,
    bounds: Option<Range<u64>>,
}
impl Request {
    fn key(&self) -> String {
        format!(
            "{}{}{}:{}",
            self.options.regex as u8,
            self.options.case_sensitive as u8,
            self.options.whole_word as u8,
            self.query
        )
    }
    fn step(&self) -> Step {
        Step::Find {
            query: self.query.clone(),
            replace: self.replacement.clone(),
            regex: self.options.regex,
            case: self.options.case_sensitive,
            whole: self.options.whole_word,
            back: self.options.backwards,
            wrap: self.options.wrap,
            selection: self.selected,
        }
    }
    fn run(
        &self,
        doc: &Document,
        start: u64,
        cancel: &AtomicBool,
    ) -> Result<(Option<Range<u64>>, Option<String>), String> {
        let bounds = self.bounds.clone().unwrap_or(0..doc.len());
        let engine = self.engine()?;
        let found = doc
            .find_in(&engine.text, start, engine.options, bounds.clone(), cancel)
            .map_err(|e| e.to_string())?;
        let mut expanded = None;
        if let (Some(hit), Some(replacement)) = (&found, &self.replacement) {
            let bytes = doc
                .replacement_in(
                    &engine.text,
                    hit.clone(),
                    engine.options,
                    &engine.replacement(replacement),
                    bounds,
                    cancel,
                )
                .map_err(|e| e.to_string())?;
            expanded = Some(
                String::from_utf8(bytes).map_err(|_| "Replacement contains incomplete UTF-8")?,
            );
        }
        Ok((found, expanded))
    }
}
/// A find bar search as the file engine takes it, compiled by lw's pattern
/// code: the same text, case and whole-word choices find the same matches in
/// the find bar, the search panel and lw. Patterns may span lines here, since
/// a document is searched as one text.
pub(crate) struct EngineQuery {
    text: String,
    options: SearchOptions,
    /// The replacement is taken as written, although the engine gets a pattern
    /// (whole words without regular expressions): `$` in it stays a dollar sign.
    literal: bool,
}
impl EngineQuery {
    fn replacement(&self, written: &str) -> String {
        if self.literal && self.options.regex {
            written.replace('$', "$$")
        } else {
            written.to_owned()
        }
    }
}
impl Request {
    fn engine(&self) -> Result<EngineQuery, String> {
        use linework_search::matcher::{Case, Query, Spec, Syntax};
        let query = Query::build(&Spec {
            patterns: vec![self.query.clone()],
            syntax: if self.options.regex { Syntax::Regex } else { Syntax::Fixed },
            case: if self.options.case_sensitive { Case::Sensitive } else { Case::Insensitive },
            word: self.options.whole_word,
            multiline: true,
            ..Spec::default()
        })?;
        let (text, regex, case_sensitive) = query.engine_form().ok_or("This pattern needs PCRE2, which the find bar does not use")?;
        Ok(EngineQuery {
            text,
            options: SearchOptions { regex, case_sensitive, whole_word: false, ..self.options },
            literal: !self.options.regex,
        })
    }
}
struct Pending {
    job: u64,
    request: Request,
    from_macro: bool,
    expanded: Arc<Mutex<Option<String>>>,
    /// Where the search began, to tell a wrapped match from the next one.
    start: u64,
}
thread_local! {
    static WRAP:Cell<bool>=const{Cell::new(true)};
    static PENDING:RefCell<Option<Pending>>=const{RefCell::new(None)};
    static READY:RefCell<Option<(String,String)>>=const{RefCell::new(None)};
    static SELECTION:RefCell<Option<SelectionScope>>=const{RefCell::new(None)};
    static REPLACE_SCOPE:RefCell<Option<(u64,Range<u64>)>>=const{RefCell::new(None)};
}
#[derive(Clone)]
struct SelectionScope {
    id: u64,
    rev: u64,
    version: u64,
    bounds: Range<u64>,
}

impl App {
    pub(crate) fn selection_search_active(&self) -> bool {
        self.current().is_some_and(|i| {
            SELECTION.with_borrow(|s| {
                s.as_ref()
                    .is_some_and(|scope| scope.id == self.buffers[i].id)
            })
        })
    }
    fn selected_bounds(&self) -> Range<u64> {
        self.wide_span().unwrap_or_else(|| {
            let (a, b) = self.edit().selection();
            events::point(self.active,a.min(b) as u64)..events::point(self.active,a.max(b) as u64)
        })
    }
    fn capture_scope(&mut self) -> Result<(), String> {
        self.drain_edits();
        let i = self
            .current()
            .ok_or("Open a document and select text first")?;
        let bounds = self.selected_bounds();
        if bounds.is_empty() {
            return Err("Select a range of text first".into());
        }
        let b = &self.buffers[i];
        SELECTION.with_borrow_mut(|s| {
            *s = Some(SelectionScope {
                id: b.id,
                rev: b.rev,
                version: b.doc.version(),
                bounds,
            })
        });
        self.last_find = None;
        self.last_job_hit = None;
        Ok(())
    }
    fn scope_for(&mut self, i: usize, requested: bool) -> Result<Option<Range<u64>>, String> {
        if !requested {
            return Ok(None);
        }
        let b = &self.buffers[i];
        let saved = SELECTION.with_borrow(|s| s.clone());
        if let Some(s) = saved
            && s.id == b.id
            && s.rev == b.rev
            && (!b.large || s.version == b.doc.version())
        {
            return Ok(Some(s.bounds));
        }
        Err("The captured selection changed. Select text and choose Find in selection again, or clear the selection option.".into())
    }
    fn shift_scope(&self, i: usize, old_rev: u64, removed: u64, inserted: u64) {
        let b = &self.buffers[i];
        SELECTION.with_borrow_mut(|s| {
            if let Some(s) = s
                && s.id == b.id
                && s.rev == old_rev
            {
                s.rev = b.rev;
                s.version = b.doc.version();
                s.bounds.end = s.bounds.end - removed + inserted;
            }
        });
    }
    pub(super) fn show_selection_find(&mut self) {
        if let Err(e) = self.capture_scope() {
            self.notice(e);
            return;
        }
        self.open_findbar(false, true);
        self.notice("Searching the captured selection");
    }
    pub(super) fn show_find(&mut self, replace: bool) {
        let within_selection = replace && self.findbar && SELECTION.with_borrow(|s| s.is_some());
        self.open_findbar(replace, within_selection);
    }
    fn open_findbar(&mut self, replace: bool, keep_selection: bool) {
        if !keep_selection {
            SELECTION.with_borrow_mut(|s| *s = None);
        }
        self.findbar = true;
        self.repbar = replace;
        self.folder_scope = false;
        if self.current().is_some() && !SELECTION.with_borrow(|s| s.is_some()) {
            let (a, b) = self.edit().selection();
            if a != b && a.abs_diff(b) <= 4096 {
                let s = self.edit().text_range(a.min(b), a.max(b));
                if !s.contains(['\r', '\n', '\0']) {
                    set_text(self.searchbox, &s);
                }
            }
        }
        self.layout();
        focus(self.searchbox);
        msg(self.searchbox, 0xb1, 0, -1);
    }
    pub(super) fn hide_find(&mut self) {
        if PENDING.with_borrow(|p| {
            p.as_ref()
                .is_some_and(|p| self.current_job("find", p.job) && !p.from_macro)
        }) {
            self.cancel_job("find");
            PENDING.with_borrow_mut(|p| *p = None);
        }
        self.findbar = false;
        self.repbar = false;
        self.folder_scope = false;
        self.layout();
        if self.current().is_some() {
            focus(self.edit().hwnd());
        }
    }
    pub(super) fn find_options(&mut self) {
        if let Some((_, v)) = forms::form(
            self.h,
            "Find options",
            &[],
            &[
                ("Match case", self.case_),
                ("Whole words", self.whole),
                ("Regular expression", self.regex),
                ("Wrap around", WRAP.get()),
                (
                    "Search within captured selection",
                    SELECTION.with_borrow(|s| s.is_some()),
                ),
            ],
            "Apply",
        ) {
            (self.case_, self.whole, self.regex) = (v[0], v[1], v[2]);
            WRAP.set(v[3]);
            if v[4] {
                let valid = self
                    .current()
                    .is_some_and(|i| self.scope_for(i, true).is_ok());
                if !valid && let Err(e) = self.capture_scope() {
                    self.notice(e);
                    return;
                }
            } else {
                SELECTION.with_borrow_mut(|s| *s = None);
            }
            self.refresh();
        }
    }
    fn request(&self, backwards: bool, replace: bool) -> Request {
        Request {
            query: text(self.searchbox),
            replacement: replace.then(|| text(self.replacebox)),
            selected: SELECTION.with_borrow(|s| s.is_some()),
            bounds: None,
            options: SearchOptions {
                regex: self.regex,
                case_sensitive: self.case_,
                whole_word: self.whole,
                backwards,
                wrap: WRAP.get(),
            },
        }
    }
    pub(crate) fn find_marked_term(&mut self, term: String) {
        let req = Request {
            query: term,
            replacement: None,
            selected: false,
            bounds: None,
            options: SearchOptions {
                case_sensitive: true,
                wrap: true,
                ..Default::default()
            },
        };
        if let Err(error) = self.start_find(req, false) {
            self.notice(error);
        }
    }
    pub(super) fn find(&mut self, backwards: bool, replace: bool) {
        if self.folder_scope {
            self.run_folder_search(replace);
            return;
        }
        let req = self.request(backwards, replace);
        if req.query.is_empty() {
            self.open_findbar(replace, req.selected);
            return;
        }
        if let Err(e) = self.start_find(req, false) {
            self.notice(e);
        }
    }
    fn find_start(&self, i: usize, req: &Request) -> u64 {
        let b = &self.buffers[i];
        let bounds = self.selected_bounds();
        let (from, to) = (bounds.start, bounds.end);
        if req
            .bounds
            .as_ref()
            .is_some_and(|range| range.start == from && range.end == to)
            && self.last_find.is_none()
        {
            return if req.options.backwards { to + 1 } else { from };
        }
        let key = req.key();
        let after_replace = format!("@{key}");
        let previous = self.last_find.as_ref().filter(|(id, rev, pos, last)| {
            *id == b.id && *rev == b.rev && *pos == from && (*last == key || *last == after_replace)
        });
        if let Some((_, _, _, last_key)) = previous
            && let Some(hit) = &self.last_job_hit
        {
            if hit.end == to {
                if req.replacement.is_some() && *last_key == key {
                    return hit.start;
                }
                if req.options.backwards {
                    return hit.start;
                }
                if !hit.is_empty() {
                    return hit.end;
                }
                let bytes = b.doc.read(to, 1).unwrap_or_default();
                return to
                    + match bytes.first().copied() {
                        Some(0xf0..=0xf4) => 4,
                        Some(0xe0..=0xef) => 3,
                        Some(0xc2..=0xdf) => 2,
                        _ => 1,
                    };
            }
        }
        if req.options.backwards || req.replacement.is_some() {
            from
        } else {
            to
        }
    }
    fn start_find(&mut self, mut req: Request, from_macro: bool) -> Result<Option<bool>, String> {
        self.drain_edits();
        let i = self.current().ok_or("Open a document to search")?;
        if req.replacement.is_some() && (self.buffers[i].saving || self.buffers[i].follow) {
            return Err("The document is currently read-only".into());
        }
        self.sync_buffer(i)?;
        req.bounds = self.scope_for(i, req.selected)?;
        let start = self.find_start(i, &req);
        let (id, rev, doc) = {
            let b = &self.buffers[i];
            (b.id, b.rev, b.doc.clone())
        };
        if self.recording && !from_macro && self.macro_steps.len() < 100_000 {
            self.macro_steps.push(req.step());
        }
        let (job, cancel) = self.new_job("find");
        PENDING.with_borrow_mut(|p| *p = None);
        if doc.len() <= 1024 * 1024 {
            self.jobs.remove("find");
            let (found, expanded) = req.run(&doc, start, &cancel)?;
            return self
                .accept_find(i, &req, found, expanded, start, from_macro)
                .map(Some);
        }
        let tx = self.tx.clone();
        let host = self.h;
        let expanded = Arc::new(Mutex::new(None));
        PENDING.with_borrow_mut(|p| {
            *p = Some(Pending {
                job,
                from_macro,
                request: req.clone(),
                expanded: expanded.clone(),
                start,
            })
        });
        self.notice("Searching…");
        std::thread::spawn(move || {
            let r = req.run(&doc, start, &cancel).map(|(hit, text)| {
                *expanded.lock().unwrap() = text;
                hit
            });
            let _ = tx.send(Worker::Found(job, id, rev, r, req.replacement.is_some()));
            post(host, WM_TIMER, 1, 0);
        });
        Ok(None)
    }
    fn accept_find(
        &mut self,
        i: usize,
        req: &Request,
        found: Option<Range<u64>>,
        expanded: Option<String>,
        start: u64,
        from_macro: bool,
    ) -> Result<bool, String> {
        let Some(hit) = found else {
            if !from_macro {
                self.notice(if req.selected {
                    "No matches in the captured selection"
                } else {
                    "No matches"
                });
            }
            return Ok(false);
        };
        if let Some(text) = expanded {
            let inserted = text.len() as u64;
            READY.with_borrow_mut(|ready| *ready = Some((req.key(), text)));
            let removed = hit.end - hit.start;
            let old_rev = self.buffers[i].rev;
            self.replace_hit(hit.clone())?;
            if req.selected {
                self.shift_scope(i, old_rev, removed, inserted);
            }
            if !from_macro {
                self.notice("Replaced one match");
            }
        } else {
            if self.buffers[i].large && hit.end - hit.start > PAGE as u64 / 2 {
                self.select_span(hit.clone(), false);
            } else {
                self.go(hit.start, (hit.end - hit.start) as usize);
            }
            self.last_find = Some((
                self.buffers[i].id,
                self.buffers[i].rev,
                hit.start,
                req.key(),
            ));
            // A forward search looks from `start` on, a backward one before
            // it. A match on the other side means the search went around.
            let wrapped = if req.options.backwards {
                hit.start >= start
            } else {
                hit.start < start
            };
            let again = wrapped && self.last_job_hit.as_ref() == Some(&hit);
            self.last_job_hit = Some(hit);
            if !from_macro {
                self.notice(match (again, wrapped, req.options.backwards, req.selected) {
                    (true, _, _, true) => "The only match in the selection",
                    (true, _, _, false) => "The only match",
                    (_, true, false, true) => "Wrapped to the first match in the selection",
                    (_, true, false, false) => "Wrapped to the first match",
                    (_, true, true, true) => "Wrapped to the last match in the selection",
                    (_, true, true, false) => "Wrapped to the last match",
                    (_, false, _, true) => "Match in selection",
                    (_, false, _, false) => "Match found",
                });
            }
        }
        Ok(true)
    }
    pub(super) fn finish_find(
        &mut self,
        job: u64,
        id: u64,
        rev: u64,
        result: Result<Option<Range<u64>>, String>,
        _replace: bool,
    ) {
        let pending = PENDING.with_borrow_mut(|p| {
            if p.as_ref().is_some_and(|r| r.job == job) {
                p.take()
            } else {
                None
            }
        });
        let Some(p) = pending else { return };
        if p.from_macro && !self.playing {
            return;
        }
        self.drain_edits();
        let r = (|| {
            let i = self
                .current()
                .filter(|&i| self.buffers[i].id == id && self.buffers[i].rev == rev)
                .ok_or("The document changed during the search")?;
            if self
                .jobs
                .get("find")
                .is_some_and(|(_, c)| c.load(Ordering::Relaxed))
            {
                return Err("Search cancelled".into());
            }
            if p.request.selected && p.request.bounds != self.scope_for(i, true)? {
                return Err("The search selection changed".into());
            }
            let expanded = p.expanded.lock().unwrap().take();
            self.accept_find(i, &p.request, result?, expanded, p.start, p.from_macro)
        })();
        if p.from_macro {
            self.complete_macro_find(r);
        } else if let Err(e) = r {
            self.notice(e);
        }
    }
    pub(super) fn replace_hit(&mut self, hit: Range<u64>) -> Result<(), String> {
        let i = self.current().ok_or("Open a document to replace text")?;
        let (key, replacement) = if let Some(ready) = READY.with(|v| v.take()) {
            ready
        } else {
            self.sync_buffer(i)?;
            let request = self.request(false, true);
            let engine = request.engine()?;
            let bytes = self.buffers[i]
                .doc
                .replacement_for(
                    &engine.text,
                    hit.clone(),
                    engine.options,
                    &engine.replacement(request.replacement.as_deref().unwrap_or("")),
                    &AtomicBool::new(false),
                )
                .map_err(|e| e.to_string())?;
            (
                request.key(),
                String::from_utf8(bytes).map_err(|_| "Replacement contains incomplete UTF-8")?,
            )
        };
        let end = hit.start + replacement.len() as u64;
        self.change_open(i, vec![(hit.start as usize, hit.end as usize, replacement)])?;
        self.go(end, 0);
        if hit.is_empty() {
            self.last_find = Some((
                self.buffers[i].id,
                self.buffers[i].rev,
                end,
                format!("@{key}"),
            ));
            self.last_job_hit = Some(end..end);
        } else {
            self.last_find = None;
            self.last_job_hit = None;
        }
        Ok(())
    }
    pub(crate) fn macro_find(&mut self, step: &Step, repeat_to_end: bool) -> Result<bool, String> {
        let Step::Find {
            query,
            replace,
            case,
            regex,
            whole,
            back,
            wrap,
            selection,
        } = step
        else {
            return Err("Expected a recorded search".into());
        };
        let req = Request {
            query: query.clone(),
            replacement: replace.clone(),
            selected: *selection,
            bounds: None,
            options: SearchOptions {
                regex: *regex,
                case_sensitive: *case,
                whole_word: *whole,
                backwards: *back,
                // An EOF run must never cycle back and edit an earlier match in its final pass.
                wrap: *wrap && !repeat_to_end,
            },
        };
        if *selection
            && self
                .current()
                .is_some_and(|i| self.scope_for(i, true).is_err())
        {
            self.capture_scope()?;
        }
        match self.start_find(req, true)? {
            Some(found) => Ok(found),
            None => {
                crate::macros::wait_for_find();
                Ok(false)
            }
        }
    }
    pub(super) fn replace_all(&mut self) {
        if self.folder_scope {
            self.run_folder_search(true);
            return;
        }
        let mut req = self.request(false, true);
        if req.query.is_empty() {
            self.open_findbar(true, req.selected);
            return;
        }
        self.drain_edits();
        let Some(i) = self.current() else { return };
        if self.buffers[i].saving || self.buffers[i].follow {
            self.notice("The document is currently read-only");
            return;
        }
        if let Err(e) = self.sync_buffer(i) {
            self.notice(e);
            return;
        }
        req.bounds = match self.scope_for(i, req.selected) {
            Ok(s) => s,
            Err(e) => {
                self.notice(e);
                return;
            }
        };
        let engine = match req.engine() {
            Ok(engine) => engine,
            Err(e) => {
                self.notice(e);
                return;
            }
        };
        let (id, rev, mut doc) = {
            let b = &self.buffers[i];
            (b.id, b.rev, b.doc.clone())
        };
        let mut layout = BulkView::capture(self, i);
        let (job, cancel) = self.new_job("replace");
        REPLACE_SCOPE.with_borrow_mut(|s| *s = req.bounds.clone().map(|range| (job, range)));
        let tx = self.tx.clone();
        let host = self.h;
        self.notice("Replacing all matches…");
        std::thread::spawn(move || {
            let r = doc
                .replace_observed(
                    &engine.text,
                    &engine.replacement(req.replacement.as_deref().unwrap_or("")),
                    engine.options,
                    req.bounds.unwrap_or(0..doc.len()),
                    &cancel,
                    |r, add| layout.edit(r, add),
                )
                .map(|n| (doc, n, layout))
                .map_err(|e| e.to_string());
            let _ = tx.send(Worker::Replaced(job, id, rev, r));
            post(host, WM_TIMER, 1, 0);
        });
    }
    pub(super) fn finish_replace(
        &mut self,
        id: u64,
        rev: u64,
        result: Result<(Document, usize, BulkView), String>,
    ) {
        self.drain_edits();
        let Some(i) = self.idx(id) else { return };
        let scope = REPLACE_SCOPE
            .with(|s| s.take())
            .filter(|(job, _)| self.current_job("replace", *job))
            .map(|(_, range)| range);
        let r = (|| {
            let b = &self.buffers[i];
            if b.rev != rev || b.saving || b.follow {
                return Err("The document changed before replacement completed".to_owned());
            }
            if self
                .jobs
                .get("replace")
                .is_some_and(|(_, c)| c.load(Ordering::Relaxed))
            {
                return Err("Replacement cancelled".into());
            }
            let (doc, n, layout) = result?;
            if scope.is_some() && scope != self.scope_for(i, true)? {
                return Err("The search selection changed".into());
            }
            let b = &self.buffers[i];
            if n != 0 {
                let before = b.doc.len();
                let after = doc.len();
                if b.large || after > LARGE {
                    let promote = !b.large;
                    self.buffers[i]
                        .doc
                        .apply_snapshot(doc)
                        .map_err(|e| e.to_string())?;
                    self.buffers[i].rev += 1;
                    self.buffers[i].dirty = true;
                    if promote {
                        self.promote(i)
                    } else {
                        self.reload_views(i)
                    }
                    self.track_edit();
                } else {
                    let old_len = b.doc.len() as usize;
                    let len = doc.len() as usize;
                    let raw = doc.read(0, len).map_err(|e| e.to_string())?;
                    let value =
                        String::from_utf8(raw).map_err(|_| "Replacement contains invalid UTF-8")?;
                    self.change_open(i, vec![(0, old_len, value)])?;
                }
                layout.restore(self, i);
                if scope.is_some() {
                    self.shift_scope(
                        i,
                        rev,
                        before.saturating_sub(after),
                        after.saturating_sub(before),
                    );
                }
            }
            Ok(n)
        })();
        match r {
            Ok(n) => self.notice(format!(
                "Replaced {} {}",
                state::pretty(n as u64),
                if n == 1 { "match" } else { "matches" }
            )),
            Err(e) => self.notice(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(text: &str, query: &str, case: bool, regex: bool, whole: bool, replacement: Option<&str>) -> (Vec<String>, Option<String>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        fs::write(&path, text).unwrap();
        let doc = Document::open(&path).unwrap();
        let req = Request {
            query: query.into(),
            replacement: replacement.map(str::to_owned),
            selected: false,
            bounds: None,
            options: SearchOptions { case_sensitive: case, regex, whole_word: whole, backwards: false, wrap: false },
        };
        let cancel = AtomicBool::new(false);
        let (mut at, mut all, mut expanded) = (0, Vec::new(), None);
        while let (Some(hit), e) = req.run(&doc, at, &cancel).unwrap() {
            all.push(text[hit.start as usize..hit.end as usize].to_owned());
            expanded = expanded.or(e);
            at = hit.end.max(hit.start + 1);
        }
        (all, expanded)
    }

    #[test]
    fn the_find_bar_matches_as_lw_does() {
        // Whole words as lw and ripgrep take them: no word character just before or after the match.
        assert_eq!(found("a -x b-x -xy", "-x", true, false, true, None).0, ["-x"]);
        assert_eq!(found("Cat cat CAT", "cat", false, false, false, None).0, ["Cat", "cat", "CAT"]);
        assert_eq!(found("Cat cat", "cat", true, false, false, None).0, ["cat"]);
        // A pattern may span lines in a document.
        assert_eq!(found("one\ntwo\n", "e\nt", true, true, false, None).0, ["e\nt"]);
        // Groups in a replacement with regular expressions; a dollar sign kept as written without them.
        assert_eq!(found("red1", r"red(\d)", true, true, false, Some("blue$1")).1.as_deref(), Some("blue1"));
        assert_eq!(found("price", "price", true, false, true, Some("$5")).1.as_deref(), Some("$5"));
    }
}
