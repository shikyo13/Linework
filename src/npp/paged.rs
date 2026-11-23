//! A plugin-facing Scintilla endpoint for the logical file. Rendering HWNDs
//! remain separate. A plugin command edits persistent piece-tree snapshots;
//! all reads and later messages in the command see the staged changes.
use super::{App, H, with_app};
use linework_editor::sci::*;
use linework_storage::{ChangeMap, Document, SearchOptions};
use std::{cell::RefCell, collections::BTreeMap, io, ops::Range, sync::atomic::AtomicBool};
use windows_sys::Win32::UI::WindowsAndMessaging::GetDlgCtrlID;

#[path = "paged_pointer.rs"]
mod pointer;
const MAX_COPY: u64 = 16 << 20;
const SPILL_AT: u64 = 8 << 20;
const MAX_REGIONS: usize = 65536;

fn err(s: impl ToString) -> io::Error {
    io::Error::other(s.to_string())
}
fn sorted(a: u64, b: u64) -> Range<u64> {
    a.min(b)..a.max(b)
}
fn shifted(pos: u64, start: u64, end: u64, add: u64) -> u64 {
    if pos <= start {
        pos
    } else if pos < end {
        start
    } else {
        pos - (end - start) + add
    }
}

struct Draft {
    id: u64,
    rev: u64,
    doc: Document,
    map: ChangeMap,
    changed: bool,
    unspilled: u64,
    readonly: bool,
    needs_copy: bool,
}
#[derive(Clone, Default)]
struct Pane {
    id: u64,
    regions: Vec<(u64, u64)>,
    main: usize,
    target: (u64, u64),
    search: usize,
    status: i32,
    selection_changed: bool,
    visible_editor: H,
    tab_width: usize,
}
#[derive(Clone, Default)]
struct Remember {
    id: u64,
    rev: u64,
    target: (u64, u64),
    search: usize,
    status: i32,
}
thread_local! {
    static SCOPE:RefCell<Option<Session>>=const{RefCell::new(None)};
    static LAST:RefCell<[Remember;2]>=RefCell::new(Default::default());
}
struct Session {
    docs: BTreeMap<u64, Draft>,
    views: [Option<Pane>; 2],
    error: Option<String>,
    converter: bool,
}
impl Session {
    fn capture(app: &App, converter: bool) -> io::Result<Self> {
        let mut docs = BTreeMap::new();
        let mut views: [Option<Pane>; 2] = [None, None];
        for (p, slot) in views.iter_mut().enumerate() {
            let Some(b) =
                app.assigned[p].and_then(|id| app.buffers.iter().find(|b| b.id == id && b.large))
            else {
                continue;
            };
            let regions = app.extension_selections(p).map_err(err)?;
            if regions.is_empty() {
                return Err(err("Missing file selection"));
            }
            let last = LAST.with_borrow(|v| v[p].clone());
            let mut view = Pane {
                id: b.id,
                regions,
                visible_editor: app.editors[p].hwnd(),
                tab_width: app.editors[p].send(SCI_GETTABWIDTH, 0, 0).max(1) as usize,
                ..Default::default()
            };
            if (last.id, last.rev) == (b.id, b.rev) {
                view.target = last.target;
                view.search = last.search;
                view.status = last.status;
            }
            *slot = Some(view);
            docs.entry(b.id).or_insert_with(|| {
                let mut doc = b.doc.clone();
                doc.begin_undo_group();
                Draft {
                    id: b.id,
                    rev: b.rev,
                    map: ChangeMap::new(doc.len()),
                    doc,
                    changed: false,
                    unspilled: 0,
                    readonly: b.saving || b.follow,
                    needs_copy: false,
                }
            });
        }
        Ok(Self {
            docs,
            views,
            error: None,
            converter,
        })
    }
    fn finish(self, app: &mut App) -> io::Result<()> {
        let Self {
            docs, views, error, ..
        } = self;
        if let Some(message) = error {
            for draft in docs.values().filter(|b| b.needs_copy) {
                if let Some(i) = app.buffers.iter().position(|v| v.id == draft.id) {
                    app.prepare_input(i);
                }
            }
            return Err(err(message));
        }
        for b in docs.values() {
            if !app.buffers.iter().any(|live| {
                live.id == b.id
                    && live.rev == b.rev
                    && (!b.changed || !(live.follow || live.saving))
            }) {
                return Err(err("The file changed while the plugin was running"));
            }
            if b.changed {
                let index = app.buffers.iter().position(|live| live.id == b.id).unwrap();
                app.can_commit_extension_snapshot(index).map_err(err)?;
            }
        }
        for pane in views.iter().flatten().filter(|v| v.selection_changed) {
            App::validate_extension_selections(docs[&pane.id].doc.len(), &pane.regions)
                .map_err(err)?;
        }
        for (_, mut b) in docs {
            if !b.changed {
                continue;
            }
            b.doc.end_undo_group();
            let index = app.buffers.iter().position(|live| live.id == b.id).unwrap();
            app.commit_extension_snapshot(index, b.doc, &b.map.changes())
                .map_err(err)?;
        }
        let active = app.active;
        for (p, pane) in views.into_iter().enumerate() {
            let Some(mut v) = pane else { continue };
            if app.assigned[p] != Some(v.id) {
                continue;
            }
            if v.selection_changed {
                let main = v.regions.remove(v.main);
                v.regions.insert(0, main);
                app.set_extension_selections(p, &v.regions).map_err(err)?;
            }
            if let Some(b) = app.buffers.iter().find(|b| b.id == v.id) {
                LAST.with_borrow_mut(|s| {
                    s[p] = Remember {
                        id: v.id,
                        rev: b.rev,
                        target: v.target,
                        search: v.search,
                        status: v.status,
                    }
                });
            }
        }
        app.active = active;
        Ok(())
    }
    fn replace(&mut self, p: usize, r: Range<u64>, bytes: &[u8]) -> io::Result<()> {
        let id = self.views[p].as_ref().unwrap().id;
        let b = self.docs.get_mut(&id).unwrap();
        if b.readonly {
            return Err(err("Document is read-only"));
        }
        if r.start > r.end || r.end > b.doc.len() {
            return Err(err("Plugin edit is outside the document"));
        }
        if bytes.is_empty() && r.is_empty() {
            return Ok(());
        }
        if !b.doc.prepare_without_copy()? {
            b.needs_copy = true;
            return Err(err(
                "Preparing a safe editable copy. Repeat the plugin command when ready.",
            ));
        }
        let add = bytes.len() as u64;
        b.doc.replace(r.start, r.end - r.start, bytes)?;
        b.map.replace(r.start, r.end - r.start, add)?;
        b.changed = true;
        b.unspilled += add;
        if b.unspilled >= SPILL_AT {
            b.doc.spill_inserted()?;
            b.unspilled = 0;
        }
        for v in self.views.iter_mut().flatten().filter(|v| v.id == id) {
            for (a, b) in &mut v.regions {
                *a = shifted(*a, r.start, r.end, add);
                *b = shifted(*b, r.start, r.end, add);
            }
            v.target = (
                shifted(v.target.0, r.start, r.end, add),
                shifted(v.target.1, r.start, r.end, add),
            );
            v.selection_changed = true;
        }
        pointer::invalidate(id);
        Ok(())
    }
    fn message(&mut self, p: usize, m: u32, w: usize, l: isize) -> io::Result<isize> {
        let Some(v) = &mut self.views[p] else {
            return Err(err("Plugin view is no longer assigned"));
        };
        let b = self.docs.get(&v.id).unwrap();
        let doc = &b.doc;
        let len = doc.len();
        let main = v.main;
        let (anchor, caret) = v.regions[main];
        let selection = sorted(anchor, caret);
        let pos = |x: isize| if x < 0 { len } else { (x as u64).min(len) };
        let index = |i: usize| {
            v.regions
                .get(i)
                .copied()
                .ok_or_else(|| err("Invalid selection index"))
        };
        if m == SCI_GETSTATUS {
            return Ok(if self.error.is_some() {
                1
            } else {
                v.status as isize
            });
        }
        if self.error.is_some() {
            return Err(err("Previous plugin operation failed"));
        }
        let value = match m {
            SCI_GETLENGTH | SCI_GETTEXTLENGTH => len as isize,
            SCI_GETCODEPAGE => 65001,
            SCI_GETREADONLY => b.readonly as isize,
            SCI_GETMODIFY => doc.dirty() as isize,
            SCI_GETEOLMODE => match doc.newline() {
                "\r\n" => 0,
                "\r" => 1,
                _ => 2,
            },
            SCI_GETCOLUMN => {
                let at = pos(w as isize);
                indexed(doc, at)?;
                let mut offset = doc.line_start(doc.line_of(at));
                let mut col = 0usize;
                while offset < at {
                    let block = doc.read(offset, (at - offset).min(65536) as usize)?;
                    for &b in &block {
                        if b == b'\t' {
                            col += v.tab_width - col % v.tab_width;
                        } else if b & 0xc0 != 0x80 {
                            col += 1;
                        }
                    }
                    offset += block.len() as u64;
                }
                col as isize
            }
            SCI_WORDSTARTPOSITION | SCI_WORDENDPOSITION => {
                word_edge(doc, pos(w as isize), m == SCI_WORDSTARTPOSITION, l != 0)? as isize
            }
            SCI_STYLEGETFORE | SCI_STYLEGETBACK => crate::win::msg(v.visible_editor, m, w, l),
            SCI_GETTABWIDTH => v.tab_width as isize,
            SCI_GETUNDOCOLLECTION => 1,
            SCI_SETSTATUS => {
                v.status = w as i32;
                0
            }
            SCI_GETCURRENTPOS => caret as isize,
            SCI_GETANCHOR => anchor as isize,
            SCI_GETSELECTIONSTART => selection.start as isize,
            SCI_GETSELECTIONEND => selection.end as isize,
            SCI_GETSELECTIONEMPTY => v.regions.iter().all(|(a, b)| a == b) as isize,
            SCI_GETSELECTIONS => v.regions.len() as isize,
            SCI_GETMAINSELECTION => main as isize,
            SCI_GETSELECTIONMODE => 0,
            SCI_SELECTIONISRECTANGLE => 0,
            SCI_GETSELECTIONNCARET => index(w)?.1 as isize,
            SCI_GETSELECTIONNANCHOR => index(w)?.0 as isize,
            SCI_GETSELECTIONNSTART => {
                let (a, b) = index(w)?;
                a.min(b) as isize
            }
            SCI_GETSELECTIONNEND => {
                let (a, b) = index(w)?;
                a.max(b) as isize
            }
            SCI_GETSELECTIONNCARETVIRTUALSPACE | SCI_GETSELECTIONNANCHORVIRTUALSPACE => 0,
            SCI_SETSEL
            | SCI_SETSELECTION
            | SCI_SETEMPTYSELECTION
            | SCI_GOTOPOS
            | SCI_SELECTALL
            | SCI_CLEARSELECTIONS => {
                let pair = match m {
                    SCI_SETSEL => {
                        let end = pos(l);
                        (
                            if (w as isize) < 0 {
                                end
                            } else {
                                pos(w as isize)
                            },
                            end,
                        )
                    }
                    SCI_SETSELECTION => (pos(l), pos(w as isize)),
                    SCI_SELECTALL => (0, len),
                    SCI_CLEARSELECTIONS => (caret, caret),
                    _ => {
                        let n = pos(w as isize);
                        (n, n)
                    }
                };
                v.regions.clear();
                v.regions.push(pair);
                v.main = 0;
                v.selection_changed = true;
                0
            }
            SCI_SETMAINSELECTION => {
                index(w)?;
                v.main = w;
                v.selection_changed = true;
                0
            }
            SCI_ADDSELECTION => {
                if v.regions.len() >= MAX_REGIONS {
                    return Err(err("Too many native plugin selections"));
                }
                v.regions.push((pos(l), pos(w as isize)));
                v.main = v.regions.len() - 1;
                v.selection_changed = true;
                0
            }
            SCI_DROPSELECTIONN => {
                if w < v.regions.len() && v.regions.len() > 1 {
                    v.regions.remove(w);
                    v.main = main
                        .saturating_sub((w <= main) as usize)
                        .min(v.regions.len() - 1);
                    v.selection_changed = true;
                }
                0
            }
            SCI_SETANCHOR
            | SCI_SETCURRENTPOS
            | SCI_SETSELECTIONSTART
            | SCI_SETSELECTIONEND
            | SCI_SETSELECTIONNANCHOR
            | SCI_SETSELECTIONNCARET
            | SCI_SETSELECTIONNSTART
            | SCI_SETSELECTIONNEND => {
                let numbered = matches!(
                    m,
                    SCI_SETSELECTIONNANCHOR
                        | SCI_SETSELECTIONNCARET
                        | SCI_SETSELECTIONNSTART
                        | SCI_SETSELECTIONNEND
                );
                let k = if numbered { w } else { main };
                let (a, b) = index(k)?;
                let n = pos(if numbered { l } else { w as isize });
                v.regions[k] = match m {
                    SCI_SETANCHOR | SCI_SETSELECTIONNANCHOR => (n, b),
                    SCI_SETCURRENTPOS | SCI_SETSELECTIONNCARET => (a, n),
                    SCI_SETSELECTIONSTART | SCI_SETSELECTIONNSTART => (n, b.max(a).max(n)),
                    _ => (a.min(b).min(n), n),
                };
                v.selection_changed = true;
                0
            }
            SCI_GETTARGETSTART => v.target.0 as isize,
            SCI_GETTARGETEND => v.target.1 as isize,
            SCI_SETTARGETSTART => {
                v.target.0 = pos(w as isize);
                0
            }
            SCI_SETTARGETEND => {
                v.target.1 = pos(w as isize);
                0
            }
            SCI_SETTARGETRANGE => {
                v.target = (pos(w as isize), pos(l));
                0
            }
            SCI_TARGETFROMSELECTION => {
                v.target = (selection.start, selection.end);
                0
            }
            SCI_TARGETWHOLEDOCUMENT => {
                v.target = (0, len);
                0
            }
            SCI_SETSEARCHFLAGS => {
                v.search = w;
                0
            }
            SCI_GETSEARCHFLAGS => v.search as isize,
            SCI_GETCHARAT => {
                let mut byte = [0];
                if (w as u64) < len {
                    doc.read_into(w as u64, &mut byte)?;
                }
                byte[0] as isize
            }
            SCI_GETSELTEXT => {
                if v.regions.len() != 1 {
                    return Err(err(
                        "Use numbered selections and GETTEXTRANGE for disjoint ranges",
                    ));
                }
                copy_text(doc, selection, l, true)?
            }
            SCI_GETTARGETTEXT => copy_text(doc, sorted(v.target.0, v.target.1), l, false)?,
            SCI_GETTEXT => {
                let size = if l == 0 { len } else { len.min(w as u64) };
                copy_text(doc, 0..size, l, true)?
            }
            SCI_GETTEXTRANGEFULL => {
                let range = unsafe { (l as *const NativeRange).as_ref() }
                    .ok_or_else(|| err("Missing output range"))?;
                let (mut lo, mut hi) = (range.start, range.end);
                // Converter 4.7's SelectedString narrows its own selection to Win32 LONG before calling this 64-bit API.
                if self.converter
                    && lo == selection.start as i32 as isize
                    && hi == selection.end as i32 as isize
                {
                    lo = selection.start as isize;
                    hi = selection.end as isize;
                }
                let r = checked_range(lo, hi, len)?;
                copy_text(doc, r, range.out as isize, true)?
            }
            SCI_GETCHARACTERPOINTER | SCI_GETRANGEPOINTER => {
                let r = if m == SCI_GETCHARACTERPOINTER {
                    0..len
                } else {
                    let start = w as u64;
                    let end = start
                        .checked_add(u64::try_from(l).map_err(|_| err("Invalid pointer length"))?)
                        .ok_or_else(|| err("Invalid range"))?;
                    if end > len {
                        return Err(err("Pointer range exceeds file length"));
                    }
                    start..end
                };
                pointer::address(p, b.id, b.rev, doc, r, m == SCI_GETCHARACTERPOINTER)?
            }
            SCI_GETGAPPOSITION => len as isize,
            SCI_GETLINECOUNT => {
                indexed(doc, len)?;
                doc.line_count() as isize
            }
            SCI_LINEFROMPOSITION => {
                let n = pos(w as isize);
                indexed(doc, n)?;
                doc.line_of(n) as isize
            }
            SCI_POSITIONFROMLINE | SCI_LINELENGTH | SCI_GETLINEENDPOSITION | SCI_GETLINE => {
                let line = w as u64;
                let (start, next) = line_range(doc, line)?;
                match m {
                    SCI_POSITIONFROMLINE => start as isize,
                    SCI_LINELENGTH => (next - start) as isize,
                    SCI_GETLINE => copy_text(doc, start..next, l, false)?,
                    _ => {
                        let tail = doc.read(
                            next.saturating_sub(2).max(start),
                            2.min((next - start) as usize),
                        )?;
                        (next
                            - if tail.ends_with(b"\r\n") {
                                2
                            } else if tail.ends_with(b"\r") || tail.ends_with(b"\n") {
                                1
                            } else {
                                0
                            }) as isize
                    }
                }
            }
            SCI_POSITIONBEFORE | SCI_POSITIONAFTER => {
                adjacent(doc, pos(w as isize), m == SCI_POSITIONBEFORE)? as isize
            }
            SCI_COUNTCHARACTERS => {
                let a = doc.byte_to_char(pos(w as isize))?;
                let b = doc.byte_to_char(pos(l))?;
                b as isize - a as isize
            }
            SCI_POSITIONRELATIVE => {
                let n = doc
                    .byte_to_char(pos(w as isize))?
                    .saturating_add_signed(l as i64)
                    .min(doc.char_len()?);
                doc.char_to_byte(n)? as isize
            }
            SCI_SEARCHINTARGET | SCI_FINDTEXTFULL => {
                let flags = if m == SCI_SEARCHINTARGET { v.search } else { w };
                if flags & !(2 | 4) != 0 {
                    return Err(err(
                        "Paged native search supports literal and whole-word queries",
                    ));
                }
                let (range, bytes, result) = if m == SCI_SEARCHINTARGET {
                    (v.target, unsafe { input(l, w as isize)? }, None)
                } else {
                    let f = unsafe { (l as *mut NativeSearch).as_mut() }
                        .ok_or_else(|| err("Missing search parameters"))?;
                    (
                        (pos(f.start), pos(f.end)),
                        unsafe { input(f.query as isize, -1)? },
                        Some(f),
                    )
                };
                let text = std::str::from_utf8(bytes).map_err(err)?;
                let opt = SearchOptions {
                    case_sensitive: flags & 4 != 0,
                    whole_word: flags & 2 != 0,
                    backwards: range.1 < range.0,
                    wrap: false,
                    regex: false,
                };
                match doc.find_in(
                    text,
                    range.0,
                    opt,
                    sorted(range.0, range.1),
                    &AtomicBool::new(false),
                )? {
                    Some(hit) => {
                        if let Some(out) = result {
                            out.found_start = hit.start as isize;
                            out.found_end = hit.end as isize;
                        } else {
                            v.target = (hit.start, hit.end);
                        }
                        hit.start as isize
                    }
                    None => -1,
                }
            }
            SCI_REPLACETARGET
            | SCI_REPLACETARGETMINIMAL
            | SCI_REPLACESEL
            | SCI_ADDTEXT
            | SCI_APPENDTEXT
            | SCI_INSERTTEXT
            | SCI_SETTEXT
            | SCI_CLEAR
            | SCI_CLEARALL
            | SCI_DELETERANGE => {
                if v.regions.len() > 1 && matches!(m, SCI_ADDTEXT | SCI_REPLACESEL | SCI_CLEAR) {
                    return Err(err(
                        "Use explicit target ranges to edit disjoint selections",
                    ));
                }
                let (r, bytes) = match m {
                    SCI_REPLACETARGET | SCI_REPLACETARGETMINIMAL => {
                        (sorted(v.target.0, v.target.1), unsafe {
                            input(l, w as isize)?
                        })
                    }
                    SCI_REPLACESEL => (selection, unsafe { input(l, -1)? }),
                    SCI_ADDTEXT => (caret..caret, unsafe { input(l, w as isize)? }),
                    SCI_APPENDTEXT => (len..len, unsafe { input(l, w as isize)? }),
                    SCI_INSERTTEXT => {
                        let n = if (w as isize) < 0 {
                            caret
                        } else {
                            pos(w as isize)
                        };
                        (n..n, unsafe { input(l, -1)? })
                    }
                    SCI_SETTEXT => (0..len, unsafe { input(l, -1)? }),
                    SCI_CLEAR => (selection, &[][..]),
                    SCI_CLEARALL => (0..len, &[][..]),
                    _ => {
                        let start = w as u64;
                        let end = start
                            .checked_add(u64::try_from(l).map_err(err)?)
                            .ok_or_else(|| err("Invalid delete range"))?;
                        (start..end, &[][..])
                    }
                };
                let from = r.start;
                let added = bytes.len() as u64;
                self.replace(p, r, bytes)?;
                let view = self.views[p].as_mut().unwrap();
                if matches!(m, SCI_REPLACETARGET | SCI_REPLACETARGETMINIMAL) {
                    view.target = (from, from + added);
                    return Ok(added as isize);
                }
                if matches!(
                    m,
                    SCI_REPLACESEL | SCI_ADDTEXT | SCI_CLEAR | SCI_SETTEXT | SCI_CLEARALL
                ) {
                    let n = if m == SCI_SETTEXT { 0 } else { from + added };
                    view.regions.clear();
                    view.regions.push((n, n));
                    view.main = 0;
                    view.selection_changed = true;
                }
                0
            }
            SCI_BEGINUNDOACTION | SCI_ENDUNDOACTION | SCI_SCROLLCARET | SCI_CHOOSECARETX
            | SCI_ALLOCATE | SCI_ALLOCATELINES => 0,
            _ => {
                return Err(err(format!(
                    "Scintilla message {m} is unavailable on paged files"
                )));
            }
        };
        Ok(value)
    }
}
pub(super) fn invalidate(id: u64) {
    pointer::invalidate(id);
}

pub(super) fn invoke(p: usize, adapter: Option<(u64, bool, bool)>, call: impl Fn()) {
    let Some((wrap, encode, decode)) = adapter else {
        return call();
    };
    let range = SCOPE.with_borrow(|s| {
        let v = s.as_ref()?.views[p].as_ref()?;
        let (a, b) = v.regions[v.main];
        Some(sorted(a, b))
    });
    let Some(range) = range else { return call() };
    let fail = |reason: &str| {
        SCOPE.with_borrow_mut(|s| s.as_mut().unwrap().error = Some(reason.to_owned()))
    };
    let mut remaining = range.end - range.start;
    if decode && remaining > MAX_COPY {
        fail("Converter 4.7 hex decoding requires a selection of 16 MiB or less");
        return;
    }
    if !encode || remaining == 0 {
        return call();
    }
    let mut at = range.start;
    while remaining > 0 {
        let Some(n) = super::converter::chunk(wrap, remaining) else {
            fail("The configured Converter output line exceeds its safe buffer size");
            return;
        };
        if n == remaining && at == range.start {
            return call();
        }
        active_request(p, SCI_SETSEL, at as usize, (at + n) as isize);
        if SCOPE.with_borrow(|s| s.as_ref().unwrap().error.is_some()) {
            return;
        }
        call();
        if SCOPE.with_borrow(|s| s.as_ref().unwrap().error.is_some()) {
            return;
        }
        let end = active_request(p, SCI_GETSELECTIONEND, 0, 0) as u64;
        let start = active_request(p, SCI_GETSELECTIONSTART, 0, 0) as u64;
        if start != at || end <= at + n {
            fail("Converter did not return the complete converted range");
            return;
        }
        at = end;
        remaining -= n;
    }
    active_request(p, SCI_SETSEL, range.start as usize, at as isize);
}
#[repr(C)]
struct NativeRange {
    start: isize,
    end: isize,
    out: *mut u8,
}
#[repr(C)]
struct NativeSearch {
    start: isize,
    end: isize,
    query: *const u8,
    found_start: isize,
    found_end: isize,
}
fn checked_range(start: isize, end: isize, len: u64) -> io::Result<Range<u64>> {
    let start = u64::try_from(start).map_err(err)?;
    let end = if end == -1 {
        len
    } else {
        u64::try_from(end).map_err(err)?
    };
    if start > end || end > len {
        Err(err("Invalid file range"))
    } else {
        Ok(start..end)
    }
}
fn copy_text(doc: &Document, range: Range<u64>, out: isize, terminate: bool) -> io::Result<isize> {
    let n = range.end - range.start;
    if n > MAX_COPY {
        if out != 0 && terminate {
            unsafe {
                *(out as *mut u8) = 0;
            }
        }
        return Err(err(
            "This plugin requested a contiguous copy larger than 16 MiB. Use range reads or a mapped pointer.",
        ));
    }
    if out != 0 {
        let bytes = unsafe { std::slice::from_raw_parts_mut(out as *mut u8, n as usize) };
        doc.read_into(range.start, bytes)?;
        if terminate {
            unsafe {
                *(out as *mut u8).add(n as usize) = 0;
            }
        }
    }
    Ok(n as isize)
}
unsafe fn input<'a>(ptr: isize, length: isize) -> io::Result<&'a [u8]> {
    if length == 0 {
        return Ok(&[]);
    }
    if ptr == 0 {
        return Err(err("Missing plugin text"));
    }
    let p = ptr as *const u8;
    let n = if length == -1 {
        let mut n = 0;
        while n <= MAX_COPY as usize && unsafe { *p.add(n) } != 0 {
            n += 1;
        }
        n
    } else {
        usize::try_from(length).map_err(err)?
    };
    if n > MAX_COPY as usize {
        return Err(err("Plugin write exceeds the 16 MiB per-call budget"));
    }
    Ok(unsafe { std::slice::from_raw_parts(p, n) })
}
fn indexed(doc: &Document, at: u64) -> io::Result<()> {
    if at > doc.indexed_bytes() && !doc.indexing_complete() {
        return Err(err(
            "The line index is still building. Retry after indexing.",
        ));
    }
    Ok(())
}
fn line_range(doc: &Document, line: u64) -> io::Result<(u64, u64)> {
    if line >= doc.line_count() {
        indexed(doc, doc.len())?;
        if line > doc.line_count() {
            return Err(err("Unknown line number"));
        }
    }
    let start = doc.line_start(line);
    let end = doc.line_start(line.saturating_add(1));
    indexed(doc, end)?;
    Ok((start, end))
}
fn word_edge(doc: &Document, mut at: u64, back: bool, word_only: bool) -> io::Result<u64> {
    let mut first = None;
    while (back && at > 0) || (!back && at < doc.len()) {
        let (lo, hi) = if back {
            (doc.char_boundary(at.saturating_sub(65536)), at)
        } else {
            (at, doc.char_boundary((at + 65536).min(doc.len())))
        };
        let bytes = doc.read(lo, (hi - lo) as usize)?;
        let text = std::str::from_utf8(&bytes).map_err(err)?;
        let mut consume = |c: char| {
            let class = if c.is_alphanumeric() || c == '_' {
                1
            } else if c.is_whitespace() {
                0
            } else {
                2
            };
            if word_only && class != 1 || *first.get_or_insert(class) != class {
                return false;
            }
            if back {
                at -= c.len_utf8() as u64;
            } else {
                at += c.len_utf8() as u64;
            }
            true
        };
        let exhausted = if back {
            text.chars().rev().all(&mut consume)
        } else {
            text.chars().all(&mut consume)
        };
        if !exhausted || bytes.is_empty() {
            break;
        }
    }
    Ok(at)
}
fn adjacent(doc: &Document, at: u64, back: bool) -> io::Result<u64> {
    if back {
        let lo = at.saturating_sub(4);
        let b = doc.read(lo, (at - lo) as usize)?;
        let mut n = b.len().saturating_sub(1);
        while n > 0 && b[n] & 0xc0 == 0x80 {
            n -= 1;
        }
        Ok(lo + n as u64)
    } else {
        let b = doc.read(at, 4)?;
        Ok(at
            + if b.is_empty() {
                0
            } else {
                1 + b[1..].iter().take_while(|b| **b & 0xc0 == 0x80).count()
            } as u64)
    }
}
fn failure_value(m: u32) -> isize {
    if matches!(
        m,
        SCI_FINDTEXTFULL
            | SCI_SEARCHINTARGET
            | SCI_REPLACETARGET
            | SCI_REPLACETARGETMINIMAL
            | SCI_REPLACETARGETRE
            | SCI_POSITIONFROMLINE
    ) {
        -1
    } else {
        0
    }
}
fn active_request(p: usize, m: u32, w: usize, l: isize) -> isize {
    SCOPE.with_borrow_mut(|s| {
        let Some(s) = s.as_mut() else {
            return failure_value(m);
        };
        match s.message(p, m, w, l) {
            Ok(n) => n,
            Err(e) => {
                if s.error.is_none() {
                    s.error = Some(e.to_string());
                }
                failure_value(m)
            }
        }
    })
}
/// True only for a detached, plugin-owned HWND. Linework's own paging and
/// initialization messages always go to the real control.
pub(super) fn request(h: H, m: u32, w: usize, l: isize) -> Option<isize> {
    if !(2000..=5000).contains(&m) || crate::events::is_quiet() {
        return None;
    }
    let p = match unsafe { GetDlgCtrlID(super::hwnd(h)) } {
        720 => 0,
        721 => 1,
        _ => return None,
    };
    if SCOPE.with_borrow(|s| s.is_some()) {
        return Some(active_request(p, m, w, l));
    }
    Some(
        with_app(|a| {
            let mut value = failure_value(m);
            if command(a, false, || value = active_request(p, m, w, l)) {
                value
            } else {
                failure_value(m)
            }
        })
        .unwrap_or_else(|| failure_value(m)),
    )
}
pub(super) fn command(app: &mut App, converter: bool, f: impl FnOnce()) -> bool {
    if SCOPE.with_borrow(|s| s.is_some())
        || !app
            .assigned
            .iter()
            .flatten()
            .any(|id| app.buffers.iter().any(|b| b.id == *id && b.large))
    {
        super::dispatch(app, f);
        return true;
    }
    app.drain_edits();
    let scope = match Session::capture(app, converter) {
        Ok(s) => s,
        Err(e) => {
            app.notice(e.to_string());
            Session {
                docs: BTreeMap::new(),
                views: [None, None],
                error: Some(e.to_string()),
                converter,
            }
        }
    };
    SCOPE.with_borrow_mut(|s| *s = Some(scope));
    super::dispatch(app, f);
    let session = SCOPE.with_borrow_mut(Option::take).unwrap();
    if let Err(e) = session.finish(app) {
        LAST.with_borrow_mut(|saved| {
            for (p, view) in saved.iter_mut().enumerate() {
                if let Some(b) = app.assigned[p]
                    .and_then(|id| app.buffers.iter().find(|b| b.id == id && b.large))
                {
                    if view.id != b.id || view.rev != b.rev {
                        *view = Remember {
                            id: b.id,
                            rev: b.rev,
                            ..Default::default()
                        };
                    }
                    view.status = 1;
                }
            }
        });
        app.notice(format!("Paged plugin: {e}"));
        return false;
    }
    true
}
/// A native command may explicitly save. Commit accepted operations before
/// the save, then capture a fresh revision for any subsequent plugin reads.
pub(crate) fn checkpoint(app: &mut App) -> bool {
    let old = SCOPE.with_borrow_mut(Option::take);
    let Some(old) = old else { return true };
    let converter = old.converter;
    let result = old.finish(app);
    let next = Session::capture(app, converter);
    let failure = result
        .err()
        .or_else(|| next.as_ref().err().map(|e| err(e.to_string())));
    let mut next = next.unwrap_or_else(|_| Session {
        docs: BTreeMap::new(),
        views: [None, None],
        error: None,
        converter,
    });
    if let Some(e) = failure {
        next.error = Some(e.to_string());
    }
    let ok = next.error.is_none();
    SCOPE.with_borrow_mut(|s| *s = Some(next));
    ok
}
