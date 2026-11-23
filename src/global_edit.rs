#[path = "extension_document.rs"]
mod extension_document;
use super::*;
use linework_storage::{Fold, FoldMap, SelectionSet};
use std::{cell::RefCell, collections::HashMap};
#[derive(Clone)]
struct Cursors {
    id: u64,
    rev: u64,
    set: SelectionSet,
    main: Range<u64>,
    // Preserve the direction and order requested by extension APIs until a
    // native selection operation installs a new set.
    extension_order: Option<Arc<[(u64,u64)]>>,
}
#[derive(Default)]
struct Pane {
    cursors: Option<Cursors>,
    history: Vec<Option<Cursors>>,
    proxy: Option<(usize, usize)>,
    backward: bool,
}
struct Closed {
    rev: u64,
    folds: Vec<Fold>,
    map: Arc<FoldMap>,
    view: Document,
}
#[derive(Default)]
pub(super) struct ViewState {
    panes: RefCell<[Pane; 2]>,
    folds: HashMap<u64, Closed>,
    deferred: HashMap<u64, (usize, u64, Vec<Input>)>,
}
impl App {
    pub(super) fn projection(&self, id: u64, rev: u64) -> Option<(Arc<FoldMap>, Document)> {
        self.views
            .folds
            .get(&id)
            .filter(|s| s.rev == rev)
            .map(|s| (s.map.clone(), s.view.clone()))
    }
    pub(super) fn closed(&self, id: u64, rev: u64) -> Vec<Fold> {
        self.views
            .folds
            .get(&id)
            .filter(|s| s.rev == rev)
            .map(|s| s.folds.clone())
            .unwrap_or_default()
    }
}
pub(super) fn pending(id: u64) -> bool {
    PENDING.with_borrow(|jobs| jobs.contains_key(&id))
}
pub(super) fn busy() -> bool {
    PENDING.with_borrow(|all| !all.is_empty())
}
impl App {
    pub(super) fn forget_view(&mut self, id: u64) {
        self.views.deferred.remove(&id);
        if let Some(p) = PENDING.with_borrow_mut(|all| all.remove(&id)) {
            p.cancel.store(true, Ordering::Relaxed);
        }
        self.views.folds.remove(&id);
        for p in self.views.panes.borrow_mut().iter_mut() {
            if p.cursors.as_ref().is_some_and(|s| s.id == id) {
                p.cursors = None;
                p.proxy = None;
            }
            p.history
                .retain(|s| !s.as_ref().is_some_and(|s| s.id == id));
        }
    }
}
enum Output {
    Cursors(SelectionSet, Range<u64>),
    Folds(Closed, u64),
}
pub struct Reply {
    job: u64,
    id: u64,
    rev: u64,
    pane: usize,
    result: io::Result<Output>,
}
impl Reply {
    pub(super) fn job_id(&self) -> u64 {
        self.job
    }
}
fn invalid(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, s)
}
#[derive(Clone)]
enum Input {
    Motion {
        key: u16,
        shift: bool,
        ctrl: bool,
        backward: bool,
        page: u64,
        folds: Arc<FoldMap>,
    },
    Text(Vec<u8>),
    Back,
    Forward,
    Upper,
    Lower,
    Number(i64, i64),
}
fn neighbor(doc: &Document, at: u64, back: bool) -> io::Result<Range<u64>> {
    let lo = if back { at.saturating_sub(4) } else { at };
    let end = if back { at } else { (at + 4).min(doc.len()) };
    let bytes = doc.read(lo, (end - lo) as usize)?;
    if bytes.is_empty() {
        return Ok(at..at);
    }
    if back {
        if bytes.ends_with(b"\r\n") {
            return Ok(at - 2..at);
        }
        let mut n = bytes.len() - 1;
        while n > 0 && bytes[n] & 0xc0 == 0x80 {
            n -= 1;
        }
        Ok(lo + n as u64..at)
    } else {
        if bytes.starts_with(b"\r\n") {
            return Ok(at..at + 2);
        }
        let len = 1 + bytes[1..].iter().take_while(|c| **c & 0xc0 == 0x80).count();
        Ok(at..at + len as u64)
    }
}
fn line_bounds(d: &Document, line: u64) -> io::Result<Range<u64>> {
    let start = d.line_start(line);
    let stop = if line + 1 < d.line_count() {
        d.line_start(line + 1)
    } else {
        d.len()
    };
    let n = (stop - start).min(2);
    let suffix = d.read(stop - n, n as usize)?;
    let trim = suffix
        .iter()
        .rev()
        .take_while(|&&c| matches!(c, 10 | 13))
        .count() as u64;
    Ok(start..stop - trim)
}
/// Scintilla's logical column counts Unicode scalars and expands tab stops.
fn column(d: &Document, r: Range<u64>, target: Option<u64>) -> io::Result<u64> {
    let mut col = 0;
    let mut at = r.start;
    while at < r.end {
        let b = d.read(at, (r.end - at).min(65536) as usize)?;
        for &v in &b {
            if v & 0xc0 != 0x80 {
                if target.is_some_and(|n| col >= n) {
                    return Ok(at);
                }
                col = if v == 9 { (col / 4 + 1) * 4 } else { col + 1 };
            }
            at += 1;
        }
    }
    Ok(if target.is_some() { r.end } else { col })
}
fn word_step(d: &Document, pos: u64, back: bool) -> io::Result<u64> {
    let mut p = pos;
    let classify = |s: &[u8]| {
        if s.first().is_some_and(|c| c.is_ascii_whitespace()) {
            0
        } else if s
            .first()
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c >= 128)
        {
            1
        } else {
            2
        }
    };
    let mut kind = None;
    loop {
        let r = neighbor(d, p, back)?;
        if r.is_empty() {
            return Ok(p);
        }
        let t = classify(&d.read(r.start, (r.end - r.start) as usize)?);
        if let Some(k) = kind {
            if t != k && !(back && k == 0 || !back && t == 0) {
                return Ok(p);
            }
        }
        p = if back { r.start } else { r.end };
        kind = Some(t);
    }
}
fn cursor_move(
    d: &Document,
    r: &Range<u64>,
    key: u16,
    shift: bool,
    ctrl: bool,
    backward: bool,
    page: u64,
    folds: &FoldMap,
) -> io::Result<Range<u64>> {
    let head = if backward { r.start } else { r.end };
    let anchor = if backward { r.end } else { r.start };
    let left = matches!(key, VK_LEFT | VK_UP | VK_HOME | VK_PRIOR);
    let moved = if !shift && !r.is_empty() && matches!(key, VK_LEFT | VK_RIGHT) {
        if left { r.start } else { r.end }
    } else if matches!(key, VK_LEFT | VK_RIGHT) {
        let pos = if ctrl {
            word_step(d, head, left)?
        } else {
            let c = neighbor(d, head, left)?;
            if left { c.start } else { c.end }
        };
        if left {
            folds.hidden(pos).map_or(pos, |f| f.start.saturating_sub(1))
        } else {
            folds.physical(folds.visible(pos))
        }
    } else if ctrl && matches!(key, VK_HOME | VK_END) {
        if left { 0 } else { d.len() }
    } else {
        if head > d.indexed_bytes() {
            return Err(invalid("Line indexing is still in progress here"));
        }
        let row = d.line_of(head);
        let current = line_bounds(d, row)?;
        match key {
            VK_HOME => current.start,
            VK_END => current.end,
            _ => {
                let count = if matches!(key, VK_PRIOR | VK_NEXT) {
                    page
                } else {
                    1
                };
                let mut target = row;
                for _ in 0..count {
                    let next = if left {
                        target.saturating_sub(1)
                    } else {
                        (target + 1).min(d.line_count().saturating_sub(1))
                    };
                    if next == target {
                        break;
                    }
                    let offset = d.line_start(next);
                    target = if let Some(f) = folds.hidden(offset) {
                        d.line_of(if left { f.header } else { f.end })
                    } else {
                        next
                    };
                }
                let col = column(d, current.start..head.min(current.end), None)?;
                column(d, line_bounds(d, target)?, Some(col))?
            }
        }
    };
    Ok(if shift {
        anchor.min(moved)..anchor.max(moved)
    } else {
        moved..moved
    })
}
fn compute_edit(
    doc: &mut Document,
    s: &Cursors,
    input: Input,
    cancel: &AtomicBool,
    layout: &mut BulkView,
) -> io::Result<(SelectionSet, Range<u64>)> {
    if let Input::Motion {
        key,
        shift,
        ctrl,
        backward,
        page,
        folds,
    } = &input
    {
        let mut out = linework_storage::SelectionBuilder::default();
        let mut previous: Option<Range<u64>> = None;
        let mut main = None;
        s.set.visit(0, |r| {
            if cancel.load(Ordering::Relaxed) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let moved = cursor_move(doc, &r, *key, *shift, *ctrl, *backward, *page, folds)?;
            if r == s.main {
                main = Some(moved.clone());
            }
            match &mut previous {
                Some(prev) if moved.start < prev.end || *prev == moved => {
                    prev.start = prev.start.min(moved.start);
                    prev.end = prev.end.max(moved.end);
                }
                _ => {
                    if let Some(prior) = previous.replace(moved) {
                        out.push(prior)?;
                    }
                }
            }
            Ok(true)
        })?;
        if let Some(prior) = previous {
            out.push(prior)?;
        }
        let set = out.finish()?;
        let m = main.unwrap_or(set.at(0)?);
        let mut j = set.lower_bound(m.start)?;
        if j + 1 < set.len() && set.at(j)?.end == m.start && !m.is_empty() {
            j += 1;
        }
        let active = set.at(j.min(set.len() - 1))?;
        return Ok((set, active));
    }
    let mut main = s.set.lower_bound(s.main.start)?;
    while main + 1 < s.set.len() && s.set.at(main)? != s.main {
        main += 1;
    }
    doc.edit_carets(&s.set, main, cancel, |n, r, d| {
        let mut bytes = vec![];
        let mut selection = r;
        match &input {
            Input::Text(text) => bytes.clone_from(text),
            Input::Back | Input::Forward if selection.is_empty() => {
                selection = neighbor(d, selection.start, matches!(input, Input::Back))?
            }
            Input::Upper | Input::Lower => {
                if selection.end - selection.start > 64 * 1024 * 1024 {
                    return Err(invalid(
                        "A selection exceeds the 64 MiB case conversion limit",
                    ));
                }
                let text = String::from_utf8(
                    d.read(selection.start, (selection.end - selection.start) as usize)?,
                )
                .map_err(|_| invalid("Selection contains invalid UTF-8"))?;
                bytes = if matches!(input, Input::Upper) {
                    text.to_uppercase()
                } else {
                    text.to_lowercase()
                }
                .into_bytes();
            }
            Input::Number(start, step) => {
                bytes = start
                    .saturating_add((n as i64).saturating_mul(*step))
                    .to_string()
                    .into_bytes()
            }
            _ => {}
        }
        layout.edit(selection.clone(), bytes.len() as u64);
        Ok((selection, bytes))
    })
    .map(|(set, at)| (set, at..at))
}
struct Pending {
    cancel: Arc<AtomicBool>,
    queue: std::collections::VecDeque<Queued>,
}
enum Queued {
    Edit(Input),
    Command(String),
}
thread_local! {
    static PENDING:RefCell<HashMap<u64,Pending>>=RefCell::new(HashMap::new());
    static SURROGATE:std::cell::Cell<Option<u16>>=const{std::cell::Cell::new(None)};
}
fn queue_input(id: u64, input: Input) -> bool {
    PENDING.with_borrow_mut(|all| {
        let Some(p) = all.get_mut(&id) else {
            return false;
        };
        if let Input::Text(next) = &input {
            if let Some(Queued::Edit(Input::Text(text))) = p.queue.back_mut() {
                text.extend_from_slice(next);
                return true;
            }
        }
        p.queue.push_back(Queued::Edit(input));
        true
    })
}
pub struct EditReply {
    id: u64,
    rev: u64,
    pane: usize,
    result: io::Result<(Document, SelectionSet, Range<u64>, BulkView)>,
}
impl App {
    pub(super) fn global_count_for(&self, p: usize) -> bool {
        self.cursors(p).is_some()
    }
    fn cursors(&self, p: usize) -> Option<Cursors> {
        let id = self.assigned[p]?;
        let rev = self.buffers[self.idx(id)?].rev;
        self.views.panes.borrow()[p]
            .cursors
            .clone()
            .filter(|s| s.id == id && s.rev == rev)
    }
    pub(super) fn global_count(&self) -> Option<u64> {
        self.cursors(self.active).map(|s| s.set.len())
    }
    fn clear_cursors(&self, p: usize) {
        {
            let pane = &mut self.views.panes.borrow_mut()[p];
            pane.cursors = None;
            pane.proxy = None;
        }
        self.paint_global(p);
    }
    fn install_cursors(
        &mut self,
        p: usize,
        i: usize,
        set: SelectionSet,
        main: Range<u64>,
        remember: bool,
    ) {
        let cursors = Cursors {
            id: self.buffers[i].id,
            rev: self.buffers[i].rev,
            set,
            main: main.clone(),
            extension_order: None,
        };
        {
            let state = &mut self.views.panes.borrow_mut()[p];
            if remember {
                if state.history.len() >= 32 {
                    state.history.remove(0);
                }
                state.history.push(state.cursors.clone());
            }
            state.cursors = Some(cursors);
        }
        self.active = p;
        self.go(main.start, 0);
        self.wide_selection[p] = None;
        self.paint_global(p);
        self.refresh();
    }
    pub(super) fn paint_fold_headers(&self, i: usize, p: usize) {
        let b = &self.buffers[i];
        if !b.large {
            return;
        }
        let e = &self.editors[p];
        quiet(|| {
            e.send(EOLANNOTATIONCLEARALL, 0, 0);
            if let Some((map, _)) = self.projection(b.id, b.rev) {
                e.send(EOLANNOTATIONSETVISIBLE, 1, 0);
                for fold in map.headers(events::point(p, 0), events::point(p, e.len() as u64)) {
                    let row = e
                        .send(LINEFROMPOSITION, events::offset(p, fold.header) as usize, 0)
                        .max(0) as usize;
                    let label = if fold.end <= b.doc.indexed_bytes() {
                        format!(
                            "   ⋯  {} lines folded  \0",
                            state::pretty(b.doc.line_of(fold.end) - b.doc.line_of(fold.start))
                        )
                    } else {
                        "   ⋯  Folded  \0".into()
                    };
                    msg(e.hwnd(), EOLANNOTATIONSETTEXT, row, label.as_ptr() as isize);
                    e.send(EOLANNOTATIONSETSTYLE, row, 33);
                    e.send(SETFOLDEXPANDED, row, 0);
                }
            }
        });
    }
    pub(super) fn paint_global(&self, p: usize) {
        self.render_cursors(p, true);
    }
    pub(super) fn render_cursors(&self, p: usize, select: bool) {
        let e = &self.editors[p];
        let len = e.len();
        quiet(|| {
            if !events::binding(p).large {
                return;
            }
            for (indicator, style) in [(26, 7), (27, 11)] {
                e.send(SETINDICATORCURRENT, indicator, 0);
                e.send(INDICATORCLEARRANGE, 0, len as isize);
                e.send(INDICSETSTYLE, indicator, style);
                e.send(INDICSETFORE, indicator, 0xcfac63);
                e.send(INDICSETALPHA, indicator, 75);
            }
            let Some(s) = self.cursors(p) else {
                return;
            };
            let page_lo = events::point(p, 0);
            let page_hi = events::point(p, len as u64);
            let first_line = e
                .send(
                    DOCLINEFROMVISIBLE,
                    e.send(GETFIRSTVISIBLELINE, 0, 0).max(0) as usize,
                    0,
                )
                .max(0) as usize;
            let bottom = first_line + e.send(LINESONSCREEN, 0, 0).max(1) as usize + 2;
            let visible_start = e.send(POSITIONFROMLINE, first_line, 0).max(0) as u64;
            let end_byte = e.send(POSITIONFROMLINE, bottom, 0);
            let visible_end = if end_byte < 0 {
                len as u64
            } else {
                end_byte as u64
            };
            let lo = events::point(p, visible_start);
            let hi = events::point(p, visible_end);

            let Ok(first) = s.set.lower_bound(lo) else {
                return;
            };
            let _ = s.set.visit(first, |r| {
                if r.start > hi {
                    return Ok(false);
                }
                let a = events::offset(p, r.start).min(len as u64) as usize;
                let b = events::offset(p, r.end).min(len as u64) as usize;
                if r != s.main && (b > a || r.is_empty() && a < len) {
                    e.send(SETINDICATORCURRENT, if a == b { 27 } else { 26 }, 0);
                    e.send(INDICATORFILLRANGE, a, (b - a).max(1) as isize);
                }
                Ok(true)
            });
            if !select {
                return;
            }
            if s.main.start >= page_lo && s.main.start <= page_hi {
                e.set_selection(
                    events::offset(p, s.main.start) as usize,
                    (events::offset(p, s.main.end) as usize).min(len),
                );
            }
            let (a, b) = e.selection();
            self.views.panes.borrow_mut()[p].proxy = Some((a.min(b), a.max(b)));
        });
    }
    pub(super) fn check_global_selection(&mut self, p: usize) {
        let Some(i) = self.assigned[p].and_then(|id| self.idx(id)) else {
            return;
        };
        if !self.buffers[i].large {
            return;
        }
        let e = &self.editors[p];
        let (a, b) = e.selection();
        let live = (a.min(b), a.max(b));
        if e.send(GETSELECTIONS, 0, 0) > 1 {
            let ranges: Vec<_> = e
                .selections()
                .into_iter()
                .map(|(a, b)| events::point(p, a.min(b) as u64)..events::point(p, a.max(b) as u64))
                .collect();
            let main = events::range(p, live.0 as u64, live.1 as u64);
            let result = (|| -> io::Result<_> {
                if let Some(old) = self.cursors(p) {
                    let mut set = old.set;
                    for r in ranges {
                        set = set.with(r, None)?;
                    }
                    Ok(set)
                } else {
                    SelectionSet::from_ranges(ranges)
                }
            })();
            match result {
                Ok(set) => self.install_cursors(p, i, set, main, true),
                Err(e) => self.notice(e.to_string()),
            }
        } else if self.cursors(p).is_some() && self.views.panes.borrow()[p].proxy != Some(live) {
            self.clear_cursors(p);
        }
    }
    fn start_global(
        &mut self,
        i: usize,
        op: impl FnOnce(Document, Arc<AtomicBool>) -> io::Result<Output> + Send + 'static,
        notice: &str,
    ) {
        let (job, cancel) = self.new_job("global selection");
        let b = &self.buffers[i];
        let (id, rev, pane) = (b.id, b.rev, self.active);
        let doc = b.doc.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = op(doc, cancel);
            let _ = tx.send(Worker::Global(Reply {
                job,
                id,
                rev,
                pane,
                result,
            }));
        });
        self.notice(notice);
    }
    pub(super) fn global_result(&mut self, r: Reply) {
        if !self.current_job("global selection", r.job) || self.assigned[r.pane] != Some(r.id) {
            return;
        }
        let Some(i) = self.idx(r.id) else {
            return;
        };
        if self.buffers[i].rev != r.rev {
            self.notice("File changed during the operation");
            return;
        }
        match r.result {
            Ok(Output::Cursors(s, m)) => {
                let n = s.len();
                self.install_cursors(r.pane, i, s, m, true);
                self.notice(format!("{} selections across the file", state::pretty(n)));
            }
            Ok(Output::Folds(folds, at)) => {
                let n = folds.folds.len();
                {
                    let states = &mut self.views.folds;
                    states.insert(r.id, folds)
                };
                self.bind_folds(i, at);
                self.notice(format!("{} folded regions", state::pretty(n as u64)));
            }
            Err(e) => self.notice(e.to_string()),
        }
    }
    pub(super) fn shifted_folds(
        &self,
        i: usize,
        pos: u64,
        del: u64,
        add: u64,
    ) -> Option<Vec<Fold>> {
        let b = &self.buffers[i];
        let mut folds = self.closed(b.id, b.rev);
        if folds.is_empty() {
            return None;
        }
        folds.retain_mut(|f| {
            if pos >= f.end {
                return true;
            }
            if pos + del <= f.header {
                f.header = f.header - del + add;
                f.start = f.start - del + add;
                f.end = f.end - del + add;
                return true;
            }
            if pos + del < f.start && pos >= f.header {
                f.start = f.start - del + add;
                f.end = f.end - del + add;
                return true;
            }
            false
        });
        Some(folds)
    }
    /// A visible native edit is already present in its Scintilla page. Advance
    /// the persistent projected tree with the same edit and retain the viewport.
    pub(super) fn advance_folds(
        &mut self,
        i: usize,
        p: usize,
        folds: Vec<Fold>,
        pos: u64,
        del: u64,
        bytes: &[u8],
    ) {
        let b = &self.buffers[i];
        let old = self
            .views
            .folds
            .get(&b.id)
            .map(|f| (f.map.clone(), f.view.clone(), f.folds.len()));
        if let Some((previous, mut view, count)) = old {
            let at = previous.visible(pos);
            if !folds.is_empty()
                && folds.len() == count
                && previous.visible(pos + del) - at == del
                && view.replace(at, del, bytes).is_ok()
            {
                let binding = events::binding(p);
                let base = previous.visible(binding.base);
                let map = Arc::new(FoldMap::new(folds.clone()));
                events::install(
                    p,
                    Binding {
                        base: map.physical(base),
                        ..binding
                    },
                    map.clone(),
                );
                self.views.folds.insert(
                    b.id,
                    Closed {
                        rev: b.rev,
                        folds,
                        map,
                        view,
                    },
                );
                self.margin_cache[p].set(None);
                self.margins(p);
                self.paint_fold_headers(i, p);
                return;
            }
        }
        self.apply_folds(i, folds, pos + bytes.len() as u64);
    }
    pub(super) fn apply_folds(&mut self, i: usize, folds: Vec<Fold>, at: u64) {
        let b = &self.buffers[i];
        let id = b.id;
        if folds.is_empty() {
            self.views.folds.remove(&id);
        } else {
            let map = Arc::new(FoldMap::new(folds.clone()));
            let view = match map.project(&b.doc) {
                Ok(v) => v,
                Err(e) => {
                    self.notice(e.to_string());
                    return;
                }
            };
            self.views.folds.insert(
                id,
                Closed {
                    rev: b.rev,
                    folds,
                    map,
                    view,
                },
            );
        }
        self.bind_folds(i, at);
    }
    fn bind_folds(&mut self, i: usize, at: u64) {
        let id = self.buffers[i].id;
        for p in 0..2 {
            if self.assigned[p] == Some(id) {
                let pos = if p == self.active {
                    at
                } else {
                    events::point(p, self.editors[p].caret() as u64)
                };
                let pos = self
                    .projection(id, self.buffers[i].rev)
                    .and_then(|(m, _)| m.hidden(pos))
                    .map_or(pos, |f| f.header);
                self.bind(p, i, pos);
            }
        }
        self.refresh();
    }
    pub(super) fn reveal_global(&mut self, p: usize, i: usize, at: u64) {
        let b = &self.buffers[i];
        if self
            .projection(b.id, b.rev)
            .is_some_and(|(m, _)| m.hidden(at).is_some())
        {
            let mut folds = self.closed(b.id, b.rev);
            folds.retain(|f| !(f.start <= at && at < f.end));
            self.active = p;
            self.apply_folds(i, folds, at);
        }
    }
    pub(super) fn toggle_file_fold(&mut self, at: u64, all: bool) {
        let Some(i) = self.current() else {
            return;
        };
        let b = &self.buffers[i];
        let mut collapsed = self.closed(b.id, b.rev);
        if !all {
            if let Some(k) = collapsed
                .iter()
                .position(|f| f.header <= at && at < f.start)
            {
                collapsed.remove(k);
                self.apply_folds(i, collapsed, at);
                return;
            }
        }
        let lang = b.lang.clone();
        let rev = b.rev;
        self.start_global(
            i,
            move |doc, cancel| {
                let found =
                    doc.structural_folds(&lang, if all { None } else { Some(at) }, &cancel)?;
                if found.is_empty() {
                    return Err(invalid("No foldable region here"));
                }
                if all {
                    collapsed = found;
                } else {
                    collapsed.extend(found);
                }
                let map = Arc::new(FoldMap::new(collapsed.clone()));
                let view = map.project(&doc)?;
                Ok(Output::Folds(
                    Closed {
                        rev,
                        folds: collapsed,
                        map,
                        view,
                    },
                    at,
                ))
            },
            "Finding fold regions across the file…",
        );
    }
    fn occurrence(&mut self, i: usize, id: &str) -> io::Result<()> {
        let p = self.active;
        let e = self.edit();
        let old = self.cursors(p);
        let mut main = old
            .as_ref()
            .map(|s| s.main.clone())
            .or_else(|| self.wide_span())
            .unwrap_or_else(|| {
                let (a, b) = e.selection();
                events::point(p, a.min(b) as u64)..events::point(p, a.max(b) as u64)
            });
        if main.is_empty() {
            let at = e.caret();
            let a = e.send(sn::SCI_WORDSTARTPOSITION, at, 1).max(0) as usize;
            let b = e.send(sn::SCI_WORDENDPOSITION, at, 1).max(0) as usize;
            if a == b {
                return Err(invalid("Place the caret on a word or select some text"));
            }
            main = events::point(p, a as u64)..events::point(p, b as u64);
            if id == "selnext" {
                self.install_cursors(
                    p,
                    i,
                    SelectionSet::from_ranges(vec![main.clone()])?,
                    main,
                    true,
                );
                return Ok(());
            }
        }
        if main.end - main.start > 1024 * 1024 {
            return Err(invalid("Select up to 1 MiB of text to match"));
        }
        let query = String::from_utf8(
            self.buffers[i]
                .doc
                .read(main.start, (main.end - main.start) as usize)?,
        )
        .map_err(|_| invalid("Selection contains invalid UTF-8"))?;
        let all = id == "selmatches";
        let skip = id == "selskipped";
        self.start_global(
            i,
            move |doc, cancel| {
                if all {
                    let set = doc.occurrences(&query, &cancel)?;
                    if set.len() == 0 {
                        return Err(invalid("No matching occurrences"));
                    }
                    let index = set.lower_bound(main.start)?;
                    let active = if index < set.len() {
                        set.at(index)?
                    } else {
                        set.at(0)?
                    };
                    return Ok(Output::Cursors(set, active));
                }
                let set = if let Some(s) = old {
                    s.set
                } else {
                    SelectionSet::from_ranges(vec![main.clone()])?
                };
                let opt = SearchOptions {
                    case_sensitive: true,
                    wrap: false,
                    ..Default::default()
                };
                for range in [main.end..doc.len(), 0..main.start] {
                    let mut from = range.start;
                    while let Some(hit) = doc.find_in(&query, from, opt, range.clone(), &cancel)? {
                        let j = set.lower_bound(hit.start)?;
                        let mut k = j;
                        while k < set.len() && set.at(k)?.end <= hit.start {
                            k += 1;
                        }
                        if k == set.len() || set.at(k)?.start >= hit.end {
                            let next = set
                                .with(hit.clone(), if skip { Some(main.clone()) } else { None })?;
                            return Ok(Output::Cursors(next, hit));
                        }
                        from = set.at(k)?.end.max(hit.end);
                        if from >= range.end {
                            break;
                        }
                    }
                }
                Err(invalid("All matching occurrences are selected"))
            },
            "Selecting across the file…",
        );
        Ok(())
    }
    fn change_global(&mut self, p: usize, input: Input) {
        let Some(i) = self.assigned[p].and_then(|id| self.idx(id)) else {
            return;
        };
        let Some(s) = self.cursors(p) else {
            return;
        };
        if !matches!(input, Input::Motion { .. }) && !self.writable(i) {
            if self.buffers[i].preparing.is_some() {
                let b = &self.buffers[i];
                self.views
                    .deferred
                    .entry(b.id)
                    .or_insert_with(|| (p, b.rev, vec![]))
                    .2
                    .push(input);
            }
            return;
        }
        if queue_input(s.id, input.clone()) {
            return;
        }
        if s.set.len() > 1024 {
            let cancel = Arc::new(AtomicBool::new(false));
            PENDING.with_borrow_mut(|all| {
                all.insert(
                    s.id,
                    Pending {
                        cancel: cancel.clone(),
                        queue: Default::default(),
                    },
                )
            });
            // A native paste or IME notification may have already edited the
            // projected page. Restore it while the worker edits its snapshot.
            self.bind(p, i, s.main.start);
            self.spawn_cursor_edit(i, p, s, input, cancel);
            self.notice("Applying file-wide edit…  Esc to cancel");
            return;
        }
        let before = self.buffers[i].doc.version();
        let mut layout = BulkView::capture(self, i);
        let result = compute_edit(
            &mut self.buffers[i].doc,
            &s,
            input,
            &AtomicBool::new(false),
            &mut layout,
        );
        match result {
            Ok((set, at)) => {
                if self.buffers[i].doc.version() != before {
                    let b = &mut self.buffers[i];
                    b.rev += 1;
                    b.dirty = b.doc.dirty();
                    b.synced = b.rev;
                    self.reload_views(i);
                    self.track_edit();
                }
                if self.buffers[i].doc.version() != before {
                    layout.restore(self, i);
                }
                self.install_cursors(p, i, set, at, false);
                self.refresh();
            }
            Err(e) => {
                self.reload_views(i);
                self.notice(e.to_string());
            }
        }
    }
    pub(super) fn finish_prepared_cursors(&mut self, id: u64, applied: bool) {
        let Some((p, rev, inputs)) = self.views.deferred.remove(&id) else {
            return;
        };
        if applied
            && self.assigned[p] == Some(id)
            && self.cursors(p).is_some_and(|c| c.id == id && c.rev == rev)
        {
            for input in inputs {
                self.change_global(p, input);
            }
        } else if applied {
            self.notice("Document or selection changed; queued cursor input was cancelled");
        }
    }
    fn spawn_cursor_edit(
        &self,
        i: usize,
        pane: usize,
        s: Cursors,
        input: Input,
        cancel: Arc<AtomicBool>,
    ) {
        let mut doc = self.buffers[i].doc.clone();
        let mut layout = BulkView::capture(self, i);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = compute_edit(&mut doc, &s, input, &cancel, &mut layout)
                .map(|(set, at)| (doc, set, at, layout));
            let _ = tx.send(Worker::CursorEdit(EditReply {
                id: s.id,
                rev: s.rev,
                pane,
                result,
            }));
        });
    }
    pub(super) fn finish_cursor_edit(&mut self, r: EditReply) {
        let Some(i) = self.idx(r.id) else {
            return;
        };
        let Some(mut pending) = PENDING.with_borrow_mut(|all| all.remove(&r.id)) else {
            return;
        };
        if self.buffers[i].rev != r.rev || pending.cancel.load(Ordering::Relaxed) {
            self.reload_views(i);
            self.notice("File-wide edit cancelled");
            return;
        }
        let before = self.buffers[i].doc.version();
        let result = r.result.and_then(|(doc, set, at, layout)| {
            if doc.version() != self.buffers[i].doc.version() {
                self.buffers[i].doc.apply_snapshot(doc)?;
            }
            Ok((set, at, layout))
        });
        let (set, at, layout) = match result {
            Ok(v) => v,
            Err(e) => {
                self.reload_views(i);
                self.notice(e.to_string());
                return;
            }
        };
        if self.buffers[i].doc.version() != before {
            let b = &mut self.buffers[i];
            b.rev += 1;
            b.dirty = b.doc.dirty();
            b.synced = b.rev;
            self.reload_views(i);
            self.track_edit();
        }
        if self.buffers[i].doc.version() != before {
            layout.restore(self, i);
        }
        let b = &self.buffers[i];
        let cursors = Cursors {
            id: b.id,
            rev: b.rev,
            set: set.clone(),
            main: at.clone(),
            extension_order: None,
        };
        if self.assigned[r.pane] == Some(r.id) {
            let active = self.active;
            self.install_cursors(r.pane, i, set, at.clone(), false);
            self.active = active;
        }
        while let Some(next) = pending.queue.pop_front() {
            if self.assigned[r.pane] != Some(r.id) || self.assigned[self.active] != Some(r.id) {
                break;
            }
            match next {
                Queued::Edit(input) => {
                    let cancel = pending.cancel.clone();
                    PENDING.with_borrow_mut(|all| all.insert(r.id, pending));
                    self.spawn_cursor_edit(i, r.pane, cursors, input, cancel);
                    return;
                }
                Queued::Command(cmd) => {
                    self.command(&cmd);
                    if !pending.queue.is_empty() {
                        // A command can change the revision or start another operation.
                        // Replay remaining input through normal entry points.
                        for action in pending.queue {
                            if self.assigned[r.pane] != Some(r.id)
                                || self.assigned[self.active] != Some(r.id)
                            {
                                break;
                            }
                            match action {
                                Queued::Edit(input) => self.change_global(r.pane, input),
                                Queued::Command(c) => self.command(&c),
                            }
                        }
                        return;
                    }
                }
            }
        }
        self.refresh();
    }
    pub fn global_char(&mut self, h: H, unit: u16) -> bool {
        let p = self.active;
        if h != self.edit().hwnd()
            || self.cursors(p).is_none()
            || unit < 32 && !matches!(unit, 9 | 13)
        {
            return false;
        }
        let bytes = if unit == 13 {
            self.buffers[self.current().unwrap()]
                .doc
                .newline()
                .as_bytes()
                .to_vec()
        } else {
            if (0xd800..=0xdbff).contains(&unit) {
                SURROGATE.set(Some(unit));
                return true;
            }
            let mut utf = [unit, 0];
            let len = if let Some(high) = SURROGATE.take() {
                utf = [high, unit];
                2
            } else {
                1
            };
            String::from_utf16_lossy(&utf[..len]).into_bytes()
        };
        self.change_global(p, Input::Text(bytes));
        true
    }
    pub(super) fn global_key(&mut self, k: u16, ctrl: bool, shift: bool, alt: bool) -> bool {
        let p = self.active;
        if self.cursors(p).is_none() {
            return false;
        }
        if k == VK_ESCAPE {
            if let Some(i) = self.current() {
                PENDING.with_borrow(|all| {
                    if let Some(p) = all.get(&self.buffers[i].id) {
                        p.cancel.store(true, Ordering::Relaxed);
                    }
                });
            }
            self.clear_cursors(p);
            let pos = self.global_pos();
            self.go(pos, 0);
            return true;
        }
        if !alt
            && matches!(
                k,
                VK_UP | VK_DOWN | VK_LEFT | VK_RIGHT | VK_HOME | VK_END | VK_PRIOR | VK_NEXT
            )
        {
            let backward = self.views.panes.borrow()[p].backward;
            let page = self.editors[p].send(LINESONSCREEN, 0, 0).max(1) as u64;
            let folds = self
                .current()
                .and_then(|i| self.projection(self.buffers[i].id, self.buffers[i].rev))
                .map(|(m, _)| m)
                .unwrap_or_default();
            self.change_global(
                p,
                Input::Motion {
                    key: k,
                    shift,
                    ctrl,
                    backward,
                    page,
                    folds,
                },
            );
            self.views.panes.borrow_mut()[p].backward =
                shift && matches!(k, VK_UP | VK_LEFT | VK_PRIOR | VK_HOME);
            return true;
        }
        if !ctrl && !alt && !shift && matches!(k, VK_BACK | VK_DELETE) {
            self.change_global(
                p,
                if k == VK_BACK {
                    Input::Back
                } else {
                    Input::Forward
                },
            );
            return true;
        }
        false
    }
    pub(super) fn global_native_edit(
        &mut self,
        p: usize,
        b: Binding,
        pos: u64,
        del: u64,
        bytes: &[u8],
    ) -> bool {
        let Some(s) = self.cursors(p) else {
            return false;
        };
        let proxy = self.views.panes.borrow()[p].proxy;
        let Some((a, z)) = proxy else {
            return false;
        };
        let start = events::point(p, a as u64);
        if pos != start || del != 0 && pos + del != events::end_point(p, z as u64) {
            self.clear_cursors(p);
            return false;
        }
        let mut insert = bytes.to_vec();
        if del > 0 {
            events::EVENTS.with_borrow_mut(|q|{
            let Some(n)=q.iter().position(|e|matches!(e,Event::Edit(..))) else{return;};
            if matches!(&q[n],Event::Edit(pane,binding,at,0,_) if (*pane,binding.id,*at)==(p,b.id,pos)) {
                if let Some(Event::Edit(_,_,_,_,text))=q.remove(n){insert=text;}
            }
        });
        }
        if s.set.len() == 0 {
            return false;
        }
        self.change_global(p, Input::Text(insert));
        true
    }
    pub(super) fn global_command(&mut self, id: &str) -> bool {
        let Some(i) = self.current() else {
            return false;
        };
        if !self.buffers[i].large {
            return false;
        }
        let p = self.active;
        if PENDING.with_borrow_mut(|all| {
            if let Some(item) = all.get_mut(&self.buffers[i].id) {
                item.queue.push_back(Queued::Command(id.to_owned()));
                true
            } else {
                false
            }
        }) {
            return true;
        }
        match id {
            "fold" | "foldall" => self.toggle_file_fold(self.global_pos(), id == "foldall"),
            "unfold" => {
                self.cancel_job("global selection");
                self.apply_folds(i, vec![], self.global_pos());
            }
            "selmatches" | "selnext" | "selskipped" => {
                if let Err(e) = self.occurrence(i, id) {
                    self.notice(e.to_string());
                }
            }
            "sellines" => {
                let set = self.cursors(p).map(|s| s.set);
                let span = self.wide_span().unwrap_or_else(|| {
                    let (a, b) = self.edit().selection();
                    events::point(p, a.min(b) as u64)..events::point(p, a.max(b) as u64)
                });
                self.start_global(
                    i,
                    move |doc, cancel| {
                        let set = if let Some(old) = set {
                            let mut builder = linework_storage::SelectionBuilder::default();
                            old.visit(0, |r| {
                                SelectionSet::split_lines(&doc, r, &cancel)?.visit(0, |s| {
                                    builder.push(s)?;
                                    Ok(true)
                                })?;
                                Ok(true)
                            })?;
                            builder.finish()?
                        } else {
                            SelectionSet::split_lines(&doc, span, &cancel)?
                        };
                        let main = set.at(0)?;
                        Ok(Output::Cursors(set, main))
                    },
                    "Splitting selection into lines…",
                );
            }
            "caretup" | "caretdown" => {
                let before = self.cursors(p);
                let at = self.global_pos();
                let doc = &self.buffers[i].doc;
                let key = if id == "caretup" { VK_UP } else { VK_DOWN };
                let folds = self
                    .projection(self.buffers[i].id, self.buffers[i].rev)
                    .map(|(m, _)| m)
                    .unwrap_or_default();
                let target = match cursor_move(doc, &(at..at), key, false, false, false, 1, &folds)
                {
                    Ok(r) => r.start,
                    Err(e) => {
                        self.notice(e.to_string());
                        return true;
                    }
                };
                let result = (|| -> io::Result<_> {
                    let set = if let Some(s) = before {
                        s.set
                    } else {
                        SelectionSet::from_ranges(vec![at..at])?
                    };
                    Ok(set.with(target..target, None)?)
                })();
                match result {
                    Ok(s) => self.install_cursors(p, i, s, target..target, true),
                    Err(e) => self.notice(e.to_string()),
                }
            }
            "undoselect" if self.cursors(p).is_some() => {
                let old = self.views.panes.borrow_mut()[p].history.pop().flatten();
                if let Some(s) =
                    old.filter(|s| s.id == self.buffers[i].id && s.rev == self.buffers[i].rev)
                {
                    self.install_cursors(p, i, s.set, s.main, false);
                } else {
                    self.clear_cursors(p);
                }
            }
            "upper" | "lower" if self.cursors(p).is_some() => self.change_global(
                p,
                if id == "upper" {
                    Input::Upper
                } else {
                    Input::Lower
                },
            ),
            "copy" | "cut" if self.cursors(p).is_some() => {
                let set = self.cursors(p).unwrap().set;
                let doc = &self.buffers[i].doc;
                let mut bytes = vec![];
                let mut first = true;
                let result = set.visit(0, |r| {
                    if r.end - r.start + bytes.len() as u64 + 2 > 32 * 1024 * 1024 {
                        return Err(invalid("Selected text exceeds the 32 MiB clipboard limit"));
                    }
                    if !first {
                        bytes.extend_from_slice(doc.newline().as_bytes());
                    }
                    first = false;
                    bytes.extend(doc.read(r.start, (r.end - r.start) as usize)?);
                    Ok(true)
                });
                if let Err(e) = result {
                    self.notice(e.to_string());
                } else {
                    msg(
                        self.edit().hwnd(),
                        COPYTEXT,
                        bytes.len(),
                        bytes.as_ptr() as isize,
                    );
                    if id == "cut" {
                        self.change_global(p, Input::Text(vec![]));
                    }
                }
            }
            "undo" | "redo" | "selall" => {
                self.clear_cursors(p);
                return false;
            }
            "comment" | "rectangle" | "indent" | "outdent" | "dup" | "moveup" | "movedown"
            | "snippets"
                if self.cursors(p).is_some() =>
            {
                self.notice("Finish the file-wide selection with Escape before using this command");
            }
            _ => return false,
        }
        true
    }
    pub(super) fn global_number(&mut self, start: i64, step: i64) -> bool {
        if self.cursors(self.active).is_none() {
            return false;
        }
        self.change_global(self.active, Input::Number(start, step));
        true
    }
}
