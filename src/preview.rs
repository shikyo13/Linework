//! Live document preview. Source snapshots are bounded even for paged files.
use crate::{app::{App, UiWake}, state::PreviewCues, win::*};
use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
};

#[path = "preview_web.rs"]
mod web;

const WINDOW: usize = 128 * 1024;
/// Text runs on one page that carry a source position. A page with more runs
/// still renders in full; the runs past this count are plain text.
const MAX_RUNS: usize = 30_000;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Markdown,
    Html,
    Svg,
    Csv,
    Tsv,
}
impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Markdown => "Markdown",
            Self::Html => "HTML",
            Self::Svg => "SVG",
            Self::Csv => "CSV",
            Self::Tsv => "TSV",
        }
    }
}
fn kind(name: &str, language: &str) -> Option<Kind> {
    let ext = Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    Some(match ext.to_ascii_lowercase().as_str() {
        "md" | "markdown" | "mdown" | "mkd" | "mkdn" | "mdwn" => Kind::Markdown,
        "html" | "htm" | "shtml" | "xhtml" => Kind::Html,
        "svg" => Kind::Svg,
        "csv" => Kind::Csv,
        "tsv" => Kind::Tsv,
        _ if language.eq_ignore_ascii_case("markdown") => Kind::Markdown,
        _ if language.eq_ignore_ascii_case("html") => Kind::Html,
        _ => return None,
    })
}
/// What produced the newest source change. The preview words its cue and its
/// offscreen button from this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditKind {
    Typed,
    Undo,
    Redo,
}
impl EditKind {
    fn label(self) -> &'static str {
        match self {
            Self::Typed => "type",
            Self::Undo => "undo",
            Self::Redo => "redo",
        }
    }
}
/// The newest edit of one buffer, in file bytes as they are after the edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EditMark {
    pub id: u64,
    /// Buffer revision the edit produced. A page shows the edit only if it
    /// was rendered from exactly this revision.
    pub rev: u64,
    pub at: u64,
    pub del: u64,
    pub add: u64,
    pub kind: EditKind,
    serial: u64,
}
/// Something the reader did in the preview pane. Offsets are file bytes.
pub(crate) enum Signal {
    /// Rendered text was clicked: move the source caret, selecting `len` bytes.
    Jump { id: u64, rev: u64, at: u64, len: u64 },
    /// The controls in the preview header changed.
    Cues(PreviewCues),
    /// Keyboard focus should return to the source.
    Focus,
    /// A shortcut was pressed while the preview had the keyboard.
    Chord { vk: u32, ctrl: bool, shift: bool, alt: bool },
}
#[derive(Clone, PartialEq, Eq)]
struct Key {
    id: u64,
    rev: u64,
    page: u64,
    kind: Kind,
    light: bool,
    path: Option<PathBuf>,
}
/// The edit a page was rendered for, in bytes from the start of the page text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PageEdit {
    at: usize,
    del: u64,
    add: usize,
    kind: EditKind,
    serial: u64,
    /// Last inserted byte: 1 for a blank, 2 for a line break, 0 for anything else.
    tail: u8,
}
struct Job {
    key: Key,
    text: String,
    notice: String,
    /// File offset of the first byte of `text`. `None` when the bytes were not
    /// valid UTF-8, because the repaired text no longer lines up with the file.
    origin: Option<u64>,
    edit: Option<PageEdit>,
    nonce: String,
}
struct Page {
    key: Key,
    content: String,
    notice: String,
    /// Present only when `content` carries source positions.
    origin: Option<u64>,
    /// Bytes of source text the page was rendered from.
    len: usize,
    edit: Option<PageEdit>,
    nonce: String,
}
fn html_text(into: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '&' => into.push_str("&amp;"),
            '<' => into.push_str("&lt;"),
            '>' => into.push_str("&gt;"),
            _ => into.push(ch),
        }
    }
}
/// A value that text inside a document cannot predict, new for every render.
fn nonce() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(0x6c69_6e65_776f_726b);
    format!("{:016x}", hasher.finish())
}
/// Where the newest edit sits in this page, if the page shows it exactly.
fn page_edit(text: &str, origin: u64, mark: EditMark) -> Option<PageEdit> {
    let at = usize::try_from(mark.at.checked_sub(origin)?).ok()?;
    let add = usize::try_from(mark.add).ok()?;
    let end = at.checked_add(add)?;
    if !text.is_char_boundary(at) || !text.is_char_boundary(end) {
        return None;
    }
    let tail = match (add > 0).then(|| text.as_bytes()[end - 1]) {
        Some(b' ' | b'\t') => 1,
        Some(b'\n' | b'\r') => 2,
        _ => 0,
    };
    Some(PageEdit {
        at,
        del: mark.del,
        add,
        kind: mark.kind,
        serial: mark.serial,
        tail,
    })
}
/// One rendered text run with its place in the source: `offset` bytes into the
/// current top-level block, `len` bytes long. `exact` says the run's text is
/// the source bytes unchanged, so a character inside it can be located too.
fn mapped_run<'a>(
    events: &mut Vec<pulldown_cmark::Event<'a>>,
    nonce: &str,
    offset: usize,
    len: usize,
    exact: bool,
    text: pulldown_cmark::CowStr<'a>,
) {
    use pulldown_cmark::{CowStr, Event};
    events.push(Event::Html(CowStr::from(format!(
        "<span data-lw=\"{nonce}\" data-o=\"{offset}\" data-l=\"{len}\"{}>",
        if exact { "" } else { " data-x" }
    ))));
    events.push(Event::Text(text));
    events.push(Event::Html(CowStr::Borrowed("</span>")));
}
fn end_run<'a>(
    events: &mut Vec<pulldown_cmark::Event<'a>>,
    pending: &mut Option<(Range<usize>, String)>,
    runs: &mut usize,
    nonce: &str,
    block: usize,
) {
    if let Some((span, text)) = pending.take() {
        *runs += 1;
        mapped_run(
            events,
            nonce,
            span.start.saturating_sub(block),
            span.len(),
            true,
            pulldown_cmark::CowStr::from(text),
        );
    }
}
/// Find an inline code span's text between its backticks. Returns the file
/// offset of the text, its length and whether it is the source unchanged.
fn code_origin(source: &str, range: &Range<usize>, code: &str) -> (usize, usize, bool) {
    let raw = source.get(range.clone()).unwrap_or("");
    let ticks = raw.bytes().take_while(|b| *b == b'`').count();
    if ticks > 0 && raw.len() >= 2 * ticks {
        let inner = &raw[ticks..raw.len() - ticks];
        if inner == code {
            return (range.start + ticks, code.len(), true);
        }
        // One blank of padding on each side is not part of the code.
        if inner.len() == code.len() + 2
            && inner.starts_with(' ')
            && inner.ends_with(' ')
            && &inner[1..inner.len() - 1] == code
        {
            return (range.start + ticks + 1, code.len(), true);
        }
    }
    (range.start, range.len(), false)
}
/// Markdown as HTML. With a nonce the HTML also records where its text came
/// from: a comment in front of each top-level block carries the block's byte
/// offset in the page text, and each text run sits in a span that carries its
/// offset inside that block and its length.
///
/// Offsets inside a block do not change when text is typed above the block, so
/// its HTML stays byte-identical and the page keeps the existing node, with
/// its decoded images and any selection. The page accepts these marks only
/// with the matching nonce, so HTML written in the document cannot forge one.
fn markdown(source: &str, nonce: Option<&str>) -> String {
    use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_FOOTNOTES;
    let mut out = String::new();
    let Some(nonce) = nonce else {
        html::push_html(&mut out, Parser::new_ext(source, options));
        return out;
    };
    let mut events: Vec<Event> = Vec::new();
    let mut depth = 0usize;
    let mut images = 0usize;
    let mut block = 0usize;
    let mut runs = 0usize;
    // The parser splits text at every character that could have started
    // markup. Adjacent pieces are joined again so a word stays in one run.
    let mut pending: Option<(Range<usize>, String)> = None;
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        // Image text becomes an `alt` attribute and must stay plain.
        let plain = images > 0 || runs >= MAX_RUNS || range.start < block;
        if let (Event::Text(piece), false) = (&event, plain) {
            if source.get(range.clone()) == Some(&**piece) {
                match &mut pending {
                    Some((span, text)) if span.end == range.start => {
                        span.end = range.end;
                        text.push_str(piece);
                    }
                    _ => {
                        end_run(&mut events, &mut pending, &mut runs, nonce, block);
                        pending = Some((range, piece.to_string()));
                    }
                }
            } else {
                // An entity, an escape or normalized white space: the run is
                // located as a whole, not character by character.
                end_run(&mut events, &mut pending, &mut runs, nonce, block);
                runs += 1;
                mapped_run(
                    &mut events,
                    nonce,
                    range.start - block,
                    range.len(),
                    false,
                    piece.clone(),
                );
            }
            continue;
        }
        end_run(&mut events, &mut pending, &mut runs, nonce, block);
        match &event {
            Event::Start(tag) => {
                if depth == 0 {
                    block = range.start;
                    events.push(Event::Html(CowStr::from(format!(
                        "<!--lw{nonce}:{block}-->\n"
                    ))));
                }
                depth += 1;
                if matches!(tag, Tag::Image { .. }) {
                    images += 1;
                }
            }
            Event::End(tag) => {
                depth = depth.saturating_sub(1);
                if matches!(tag, TagEnd::Image) {
                    images = images.saturating_sub(1);
                }
            }
            Event::Rule if depth == 0 => {
                block = range.start;
                events.push(Event::Html(CowStr::from(format!(
                    "<!--lw{nonce}:{block}-->\n"
                ))));
            }
            Event::Code(code) if !plain => {
                let (start, len, exact) = code_origin(source, &range, code);
                events.push(Event::Html(CowStr::Borrowed("<code>")));
                runs += 1;
                mapped_run(&mut events, nonce, start - block, len, exact, code.clone());
                events.push(Event::Html(CowStr::Borrowed("</code>")));
                continue;
            }
            _ => {}
        }
        events.push(event);
    }
    end_run(&mut events, &mut pending, &mut runs, nonce, block);
    html::push_html(&mut out, events.into_iter());
    out
}
fn render(job: Job) -> Page {
    let mut notice = job.notice;
    let mapped = job.key.kind == Kind::Markdown && job.origin.is_some() && !job.nonce.is_empty();
    let len = job.text.len();
    let content = match job.key.kind {
        Kind::Markdown => markdown(&job.text, mapped.then_some(job.nonce.as_str())),
        Kind::Csv | Kind::Tsv => {
            let sep = if job.key.kind == Kind::Tsv {
                b'\t'
            } else {
                b','
            };
            let mut reader = csv::ReaderBuilder::new()
                .has_headers(false)
                .flexible(true)
                .delimiter(sep)
                .from_reader(job.text.as_bytes());
            let mut html = String::from("<table>");
            let mut limited = false;
            for (n, record) in reader.records().enumerate() {
                if n == 500 {
                    limited = true;
                    break;
                }
                let Ok(row) = record else {
                    notice.push_str(" A record could not be displayed.");
                    break;
                };
                html.push_str("<tr>");
                for cell in row.iter().take(80) {
                    let header = n == 0 && job.key.page == 0;
                    html.push_str(if header { "<th>" } else { "<td>" });
                    html_text(&mut html, cell);
                    html.push_str(if header { "</th>" } else { "</td>" });
                }
                limited |= row.len() > 80;
                html.push_str("</tr>");
            }
            if limited {
                notice.push_str(" Table preview shows up to 500 rows and 80 columns.");
            }
            html.push_str("</table>");
            html
        }
        Kind::Html | Kind::Svg => job.text,
    };
    Page {
        key: job.key,
        content,
        notice,
        origin: job.origin.filter(|_| mapped),
        len,
        edit: job.edit.filter(|_| mapped),
        nonce: if mapped { job.nonce } else { String::new() },
    }
}
struct Formatter {
    send: Sender<Job>,
    receive: Receiver<Page>,
}
impl Formatter {
    fn new(wake: UiWake) -> Self {
        let (send, jobs) = mpsc::channel::<Job>();
        let (pages, receive) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(mut job) = jobs.recv() {
                while let Ok(newest) = jobs.try_recv() {
                    job = newest;
                }
                if pages.send(render(job)).is_err() {
                    break;
                }
                wake.notify();
            }
        });
        Self { send, receive }
    }
}
#[derive(Default)]
pub(crate) struct Pane {
    web: Option<web::View>,
    formatter: Option<Formatter>,
    requested: Option<Key>,
    visible: bool,
    needs_update: bool,
    /// Newest located edit of any buffer.
    mark: Option<EditMark>,
    serial: u64,
    /// Buffer and source caret that the cue on screen belongs to. Cleared as
    /// soon as the caret moves on, so the cue folds at once and a page that
    /// arrives late does not replay it.
    cue_caret: Option<(u64, u64)>,
}
impl Pane {
    pub(crate) fn mark_update(&mut self) {
        self.needs_update = self.visible;
    }
    pub(crate) fn take_update(&mut self) -> bool {
        std::mem::take(&mut self.needs_update) && self.visible
    }
    /// Record the newest source edit. One keystroke can report several
    /// changes (a replaced selection is a deletion, then an insertion); the
    /// last one names the text the reader should look at.
    pub(crate) fn note_edit(
        &mut self,
        id: u64,
        rev: u64,
        at: u64,
        del: u64,
        add: u64,
        kind: EditKind,
    ) {
        self.serial += 1;
        self.mark = Some(EditMark {
            id,
            rev,
            at,
            del,
            add,
            kind,
            serial: self.serial,
        });
    }
    pub(crate) fn mark(&self) -> Option<EditMark> {
        self.mark
    }
    /// True while the pane is open on a document.
    pub(crate) fn is_open(&self) -> bool {
        self.visible && self.web.is_some()
    }
    /// Scroll the preview to the newest edit and replay its cue.
    pub(crate) fn show_edit(&self) -> bool {
        self.web.as_ref().is_some_and(|view| view.run("show()"))
    }
    /// Scroll the preview to a file position, if the page on screen holds it.
    pub(crate) fn reveal(&self, id: u64, rev: u64, at: u64) -> bool {
        self.web
            .as_ref()
            .and_then(|view| Some((view, view.page_offset(id, rev, at)?)))
            .is_some_and(|(view, offset)| view.run(&format!("reveal({offset})")))
    }
    /// Ask the page which source position is at the top of the reading view.
    /// The answer arrives as a `Signal::Jump`.
    pub(crate) fn locate_reading(&self) -> bool {
        self.web.as_ref().is_some_and(web::View::locate)
    }
    /// Push changed cue settings to the page without waiting for a render.
    pub(crate) fn set_cues(&self, cues: &PreviewCues) {
        if let Some(view) = &self.web {
            view.run(&format!("options({})", web::cue_json(cues)));
        }
    }
    pub(crate) fn layout(
        &mut self,
        parent: H,
        layout: &mut WindowLayout,
        right: i32,
        left: i32,
        top: i32,
        bottom: i32,
        open: bool,
        light: bool,
        preferred_width: i32,
    ) -> i32 {
        if open && !self.visible { self.needs_update = true; }
        self.visible = open;
        if !open {
            if let Some(view) = &self.web {
                layout.set(view.h, None);
            }
            self.requested = None;
            self.cue_caret = None;
            return 0;
        }
        if self.web.is_none() {
            self.web = web::View::create(parent, light);
        }
        let Some(view) = &self.web else {
            return 0;
        };
        let space = (right - left).max(1);
        let min_width = size(parent, 165).min(space / 2);
        let ideal = if preferred_width > 0 {
            size(parent, preferred_width)
        } else {
            (space / 2).min(size(parent, 760))
        };
        let width = ideal.clamp(min_width, (space - min_width).max(min_width));
        let gap = size(parent, 6).max(1);
        layout.set(view.h, Some(rect(right - width, top, width, bottom - top)));
        width + gap
    }
}
pub(crate) fn divider_offset(host: H, x: i32, y: i32) -> Option<i32> {
    use windows_sys::Win32::{
        Foundation::RECT, Graphics::Gdi::MapWindowPoints, UI::WindowsAndMessaging::*,
    };
    unsafe {
        let pane = GetDlgItem(hwnd(host), 760);
        if pane.is_null() || IsWindowVisible(pane) == 0 {
            return None;
        }
        let mut r: RECT = std::mem::zeroed();
        if GetWindowRect(pane, &mut r) == 0 {
            return None;
        }
        MapWindowPoints(
            std::ptr::null_mut(),
            hwnd(host),
            (&mut r as *mut RECT).cast(),
            2,
        );
        (x >= r.left - size(host, 6).max(1) && x < r.left && y >= r.top && y < r.bottom)
            .then_some(r.left - x)
    }
}
impl App {
    pub(crate) fn preview_kind(&self) -> Option<Kind> {
        let b = &self.buffers[self.current()?];
        kind(&b.name, &b.lang)
    }
    pub(crate) fn update_preview(&mut self) {
        if !self.document_preview.visible {
            return;
        }
        let Some(i) = self.current() else {
            return;
        };
        let Some(kind) = self.preview_kind() else {
            return;
        };
        let b = &self.buffers[i];
        let len = if b.large {
            b.doc.len()
        } else {
            self.edit().len() as u64
        };
        let caret = self.global_pos();
        let page = if len > WINDOW as u64 {
            caret.min(len.saturating_sub(1)) / WINDOW as u64 * WINDOW as u64
        } else {
            0
        };
        let key = Key {
            id: b.id,
            rev: b.rev,
            page,
            kind,
            path: b.path.clone(),
            light: self.prefs.light,
        };
        if self.document_preview.requested.as_ref() != Some(&key) {
            let old = self.document_preview.requested.as_ref();
            if old
                .is_none_or(|old| old.id != key.id || old.path != key.path || old.kind != key.kind)
            {
                if let Some(view) = &self.document_preview.web {
                    view.switch_document(key.id, &key.path);
                }
            }
            // Align sliced UTF-8 input to physical character boundaries without
            // touching the rest of the document or attaching an editor to it.
            let start = if b.large {
                b.doc.char_boundary(page)
            } else {
                page
            };
            let end = (page + WINDOW as u64).min(len);
            let mut notice = String::new();
            let mut origin = None;
            let content = if len > WINDOW as u64 && kind == Kind::Svg {
                notice = format!(
                    "This SVG exceeds the {} KiB diagram preview limit.",
                    WINDOW / 1024
                );
                String::new()
            } else {
                let read = if b.large {
                    b.doc.read(start, (end - start) as usize)
                } else {
                    Ok(self.edit().raw_bytes(start as usize, end as usize))
                };
                match read {
                    Ok(bytes) => {
                        if len > WINDOW as u64 {
                            notice = format!(
                                "Excerpt around the caret · bytes {}–{} of {}",
                                start + 1,
                                end,
                                len
                            );
                        }
                        let left = bytes.iter().take_while(|&&c| c & 0xc0 == 0x80).count();
                        let bytes = &bytes[left..];
                        let last = std::str::from_utf8(bytes)
                            .err()
                            .filter(|e| e.error_len().is_none())
                            .map_or(bytes.len(), |e| e.valid_up_to());
                        match std::str::from_utf8(&bytes[..last]) {
                            Ok(text) => {
                                // Only an exact decode keeps byte positions true.
                                origin = Some(start + left as u64);
                                text.to_owned()
                            }
                            Err(_) => String::from_utf8_lossy(&bytes[..last]).into_owned(),
                        }
                    }
                    Err(err) => {
                        notice = format!("Cannot read this preview: {err}");
                        String::new()
                    }
                }
            };
            let mapped = kind == Kind::Markdown && origin.is_some();
            let edit = if mapped {
                self.document_preview
                    .mark
                    .filter(|m| m.id == key.id && m.rev == key.rev)
                    .and_then(|m| page_edit(&content, origin?, m))
            } else {
                None
            };
            let formatter = self
                .document_preview
                .formatter
                .get_or_insert_with(|| Formatter::new(self.tx.wake.clone()));
            let _ = formatter.send.send(Job {
                key: key.clone(),
                text: content,
                notice,
                origin,
                edit,
                nonce: if mapped { nonce() } else { String::new() },
            });
            self.document_preview.requested = Some(key.clone());
            self.document_preview.cue_caret = edit.map(|_| (key.id, caret));
        } else if let Some((id, at)) = self.document_preview.cue_caret {
            // The cue belongs to the caret that made the edit. Once the caret
            // or the selection moves on without another edit, fold it now.
            let (a, z) = self.edit().selection();
            if id != key.id || at != caret || a != z {
                self.document_preview.cue_caret = None;
                if let Some(view) = &self.document_preview.web {
                    view.run("rest()");
                }
            }
        }
        let pages: Vec<Page> = self
            .document_preview
            .formatter
            .as_ref()
            .map(|formatter| formatter.receive.try_iter().collect())
            .unwrap_or_default();
        for page in pages {
            if page.key != key {
                continue;
            }
            // The caret already left this edit: keep it for "Show edit", but
            // do not play the cue.
            let quiet = page.edit.is_some() && self.document_preview.cue_caret.is_none();
            if let Some(view) = &self.document_preview.web {
                view.show(page, &self.prefs.preview_cues.checked(), quiet);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn job(k: Kind, text: &str, nonce: &str) -> Job {
        Job {
            key: Key {
                id: 1,
                rev: 0,
                page: 0,
                kind: k,
                light: false,
                path: None,
            },
            text: text.into(),
            notice: String::new(),
            origin: Some(0),
            edit: None,
            nonce: nonce.into(),
        }
    }
    fn sample(k: Kind, text: &str) -> String {
        render(job(k, text, "")).content
    }
    #[test]
    fn rich_formats_and_quoted_cells() {
        assert_eq!(kind("IMAGE.SVG", "XML"), Some(Kind::Svg));
        assert_eq!(kind("Untitled 1", "Markdown"), Some(Kind::Markdown));
        assert_eq!(kind("app.rs", "Rust"), None);
        let doc = sample(
            Kind::Markdown,
            "# Design\n\n- [x] Ready\n\n|A|B|\n|-|-|\n|**one**|two|",
        );
        for mark in ["<h1>Design", "checkbox", "<table>", "<strong>one"] {
            assert!(doc.contains(mark), "{mark}");
        }
        let table = sample(Kind::Csv, "A,B\n\"a,b\",\"<draft>\nnext\"\n");
        assert!(table.contains("<td>a,b</td><td>&lt;draft&gt;\nnext</td>"));
    }
    /// Read `data-o` and `data-l` of every mapped run, with the offset of the
    /// block marker in front of it, and return the source slice each names.
    fn mapped_slices<'a>(source: &'a str, html: &str, nonce: &str) -> Vec<(&'a str, bool)> {
        let marker = format!("<!--lw{nonce}:");
        let open = format!("<span data-lw=\"{nonce}\" data-o=\"");
        let mut out = Vec::new();
        let mut block = 0usize;
        let mut rest = html;
        loop {
            let next_marker = rest.find(&marker);
            let next_run = rest.find(&open);
            match (next_marker, next_run) {
                (Some(m), run) if run.is_none_or(|r| m < r) => {
                    rest = &rest[m + marker.len()..];
                    let end = rest.find("-->").unwrap();
                    block = rest[..end].parse().unwrap();
                }
                (_, Some(r)) => {
                    rest = &rest[r + open.len()..];
                    let quote = rest.find('"').unwrap();
                    let offset: usize = rest[..quote].parse().unwrap();
                    rest = &rest[quote..];
                    let from = rest.find("data-l=\"").unwrap() + 8;
                    rest = &rest[from..];
                    let quote = rest.find('"').unwrap();
                    let len: usize = rest[..quote].parse().unwrap();
                    let tag_end = rest.find('>').unwrap();
                    let inexact = rest[..tag_end].contains("data-x");
                    out.push((&source[block + offset..block + offset + len], !inexact));
                }
                _ => return out,
            }
        }
    }
    #[test]
    fn markdown_runs_name_their_source_bytes() {
        let source = "# Title é\n\nsnake_case and `code` with **bold** &amp; more\n\n\
                      ![alt text](a.png)\n\n- item one\n- item `` two ``\n\n---\n\nlast\n";
        let page = render(job(Kind::Markdown, source, "n1"));
        assert_eq!(page.origin, Some(0));
        let html = &page.content;
        let slices = mapped_slices(source, html, "n1");
        let exact: Vec<&str> = slices.iter().filter(|s| s.1).map(|s| s.0).collect();
        // A word with underscores is one run, and inline code maps to its text.
        for text in ["Title é", "snake_case and ", "code", "bold", "item one", "two", "last"] {
            assert!(exact.contains(&text), "{text}: {exact:?}");
        }
        // The entity is located as a whole.
        assert!(slices.contains(&("&amp;", false)), "{slices:?}");
        // Image text stays a plain attribute.
        assert!(html.contains("alt=\"alt text\""), "{html}");
        // Every exact run's rendered text is the source slice it names.
        for (text, _) in slices.iter().filter(|s| s.1) {
            let mut escaped = String::new();
            html_text(&mut escaped, text);
            assert!(html.contains(&format!(">{escaped}</span>")), "{text}");
        }
        // A rule is a block of its own.
        assert!(html.contains(&format!("<!--lwn1:{}-->", source.find("---").unwrap())));
    }
    #[test]
    fn typing_above_a_block_leaves_its_html_unchanged() {
        let tail = "Second paragraph with *emphasis* and a [link](#x).\n";
        let before = render(job(Kind::Markdown, &format!("First\n\n{tail}"), "same")).content;
        let after = render(job(Kind::Markdown, &format!("First line grew\n\n{tail}"), "same")).content;
        let block = |html: &str| html[html.rfind("<p>").unwrap()..].to_owned();
        assert_eq!(block(&before), block(&after));
        assert_ne!(before, after);
    }
    #[test]
    fn edits_are_placed_inside_the_page_or_dropped() {
        let mark = |at, del, add| EditMark {
            id: 1,
            rev: 1,
            at,
            del,
            add,
            kind: EditKind::Typed,
            serial: 7,
        };
        let text = "aé b\n";
        let edit = page_edit(text, 100, mark(101, 0, 2)).unwrap();
        assert_eq!((edit.at, edit.add, edit.tail, edit.serial), (1, 2, 0, 7));
        assert_eq!(page_edit(text, 100, mark(103, 0, 1)).unwrap().tail, 1);
        assert_eq!(page_edit(text, 100, mark(105, 0, 1)).unwrap().tail, 2);
        assert_eq!(page_edit(text, 100, mark(104, 3, 0)).unwrap().tail, 0);
        // Before the page, past its end, or inside a character.
        assert!(page_edit(text, 100, mark(99, 0, 1)).is_none());
        assert!(page_edit(text, 100, mark(105, 0, 5)).is_none());
        assert!(page_edit(text, 100, mark(102, 0, 1)).is_none());
    }
    #[test]
    fn unmapped_pages_carry_no_positions() {
        let mut lossy = job(Kind::Markdown, "plain *text*", "n2");
        lossy.origin = None;
        let page = render(lossy);
        assert!(page.origin.is_none() && page.nonce.is_empty());
        assert!(!page.content.contains("data-lw") && !page.content.contains("<!--lw"));
        let html = render(job(Kind::Html, "<p>kept as written</p>", "")).content;
        assert_eq!(html, "<p>kept as written</p>");
    }
}
