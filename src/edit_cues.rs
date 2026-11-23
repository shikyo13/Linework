//! Feedback that follows source edits: the newest edit for the preview's cue,
//! what the reader does in the preview pane, and a short flash in the editor
//! that shows where an undo or a redo landed.
use super::*;
use crate::{
    preview::{Arrival, EditKind, Signal},
    state::PreviewCues,
};

// Scintilla's modification flags that say who changed the text.
const BY_UNDO: i32 = 0x20;
const BY_REDO: i32 = 0x40;
const LAST_STEP: i32 = 0x100;
/// The editor indicator the flash draws with. 8 to 14 belong to spelling,
/// search and phrase marks, and plug-ins are handed 16 and up, so 15 is the
/// one number nothing else can ask for.
const MARK: usize = 15;
// Indicator looks: a rounded box, a point under the start of its range, and
// a box as tall as the line for a whole line.
const BOX: isize = 7;
const POINT: isize = 18;
const FULL: isize = 16;
/// How long the flash stays. The maintenance timer clears it, so the real
/// time is this plus up to one timer period.
const FLASH: Duration = Duration::from_millis(1000);
/// The line a click in the preview landed on is marked for a shorter time.
const LANDING: Duration = Duration::from_millis(480);
/// What the flash in the editor is showing.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum FlashKind {
    #[default]
    Undo,
    Redo,
    /// Text that was just pasted.
    Paste,
    /// The line a click in the preview led to.
    Landing,
}
/// Places drawn for one undo or redo. Further places are counted, not drawn.
const MAX_SPANS: usize = 256;

/// The places one undo or redo changed, while they are collected and shown.
#[derive(Default)]
pub(super) struct HistoryFlash {
    id: u64,
    kind: FlashKind,
    /// `start..end` in bytes of the text as it is now. An empty place is
    /// where text was removed.
    spans: Vec<(u64, u64)>,
    /// Places past `MAX_SPANS`.
    more: usize,
    shown: Option<Instant>,
}

impl App {
    /// A resident document changed in an editor pane. The preview learns
    /// where, and the steps of an undo or redo are gathered for the flash.
    pub(super) fn note_native_edit(&mut self, i: usize, at: u64, del: u64, add: u64, flags: i32) {
        let (id, rev) = (self.buffers[i].id, self.buffers[i].rev);
        self.heat_edit(i, at, add);
        let kind = if flags & BY_UNDO != 0 {
            EditKind::Undo
        } else if flags & BY_REDO != 0 {
            EditKind::Redo
        } else if self.pasting && add > 0 {
            EditKind::Paste
        } else {
            EditKind::Typed
        };
        let arrival = match kind {
            EditKind::Paste => Arrival::Pasted,
            EditKind::Typed if add > 0 => self.typed_arrival(i, at, add),
            _ => Arrival::Nothing,
        };
        self.document_preview.track_change(id, at, del, add, arrival);
        self.document_preview.note_edit(id, rev, at, del, add, kind);
        let flash = match kind {
            EditKind::Typed => {
                self.clear_history_flash();
                return;
            }
            EditKind::Undo => FlashKind::Undo,
            EditKind::Redo => FlashKind::Redo,
            EditKind::Paste => FlashKind::Paste,
        };
        if self.history_flash.shown.is_some()
            || self.history_flash.id != id
            || self.history_flash.kind != flash
        {
            // The first step of a new undo, redo or paste.
            self.clear_history_flash();
            self.history_flash.id = id;
            self.history_flash.kind = flash;
        }
        // Carry the places found so far through this step, then add its own.
        let flash = &mut self.history_flash;
        let end = at + del;
        for span in &mut flash.spans {
            if span.1 <= at {
                continue;
            }
            if span.0 >= end {
                span.0 = span.0 - del + add;
                span.1 = span.1 - del + add;
            } else {
                span.0 = span.0.min(at);
                span.1 = if span.1 >= end { span.1 - del + add } else { at + add };
            }
        }
        let new = (at, at + add);
        // Undoing a typed word arrives as one step per character, each next to
        // the one before: those grow one place instead of adding many.
        let joined = match flash.spans.last_mut() {
            Some(last) if new.0 <= last.1 && last.0 <= new.1 => {
                last.0 = last.0.min(new.0);
                last.1 = last.1.max(new.1);
                true
            }
            _ => false,
        };
        if !joined {
            if flash.spans.len() < MAX_SPANS {
                flash.spans.push(new);
            } else {
                flash.more += 1;
            }
        }
        if flags & LAST_STEP != 0 {
            // The preview marks all of the text the last step belongs to,
            // not only its final character.
            let whole = flash
                .spans
                .iter()
                .copied()
                .find(|span| span.0 <= at && at <= span.1)
                .unwrap_or(new);
            self.document_preview.note_edit(
                id,
                rev,
                whole.0,
                u64::from(whole.0 == whole.1),
                whole.1 - whole.0,
                kind,
            );
            self.show_history_flash();
        }
    }
    /// Whether typed text begins with, or is nothing but, white space. A
    /// blank ends the word before it, so the preview starts a new trace.
    fn typed_arrival(&self, i: usize, at: u64, add: u64) -> Arrival {
        let id = self.buffers[i].id;
        let pane = [self.active, 1 - self.active]
            .into_iter()
            .find(|&p| self.assigned[p] == Some(id) && (p == 0 || self.split || p == self.active));
        let Some(p) = pane.filter(|_| !self.buffers[i].large) else {
            return Arrival::Typed { blank_first: false, blank: false };
        };
        let e = &self.editors[p];
        let end = (at + add.min(64)).min(e.len() as u64);
        let bytes = e.raw_bytes(at as usize, end as usize);
        let blank = |b: &u8| matches!(*b, b' ' | b'\t' | b'\r' | b'\n');
        Arrival::Typed {
            blank_first: bytes.first().is_some_and(blank),
            blank: add <= 64 && !bytes.is_empty() && bytes.iter().all(blank),
        }
    }
    /// A paste is complete once every change it made has been reported:
    /// show where it landed. Called after the pending events are handled.
    pub(super) fn finish_paste(&mut self) {
        if !std::mem::take(&mut self.pasting) {
            return;
        }
        let flash = &self.history_flash;
        if flash.kind != FlashKind::Paste || flash.shown.is_some() || flash.spans.is_empty() {
            return;
        }
        if self.prefs.preview_cues.echo {
            self.show_history_flash();
        } else {
            self.history_flash = HistoryFlash::default();
        }
    }
    /// Mark the line the caret was just sent to from the preview.
    fn flash_landing(&mut self) {
        let Some(i) = self.current() else { return };
        let (p, e) = (self.active, self.edit());
        let line = e.send(LINEFROMPOSITION, e.caret(), 0).max(0) as usize;
        let a = e.send(POSITIONFROMLINE, line, 0).max(0) as u64;
        let z = e.send(GETLINEENDPOSITION, line, 0).max(0) as u64;
        if z <= a {
            return;
        }
        let span = (events::point(p, a), events::point(p, z));
        self.clear_history_flash();
        self.history_flash = HistoryFlash {
            id: self.buffers[i].id,
            kind: FlashKind::Landing,
            spans: vec![span],
            more: 0,
            shown: None,
        };
        self.show_history_flash();
    }
    /// A paged document changed through an editor pane.
    pub(super) fn note_paged_edit(&mut self, id: u64, rev: u64, at: u64, del: u64, add: u64) {
        self.clear_history_flash();
        self.document_preview.track_change(id, at, del, add, Arrival::Nothing);
        self.document_preview
            .note_edit(id, rev, at, del, add, EditKind::Typed);
    }
    /// A paged document went back or forward in its history. Storage reports
    /// the place, not the extent, so the place is what gets marked.
    pub(super) fn note_paged_history(&mut self, i: usize, at: u64, redo: bool) {
        let (id, rev) = (self.buffers[i].id, self.buffers[i].rev);
        let kind = if redo { EditKind::Redo } else { EditKind::Undo };
        self.clear_history_flash();
        self.document_preview.note_edit(id, rev, at, 0, 0, kind);
        self.history_flash = HistoryFlash {
            id,
            kind: if redo { FlashKind::Redo } else { FlashKind::Undo },
            spans: vec![(at, at)],
            more: 0,
            shown: None,
        };
        self.show_history_flash();
    }
    fn show_history_flash(&mut self) {
        let mut spans = std::mem::take(&mut self.history_flash.spans);
        if spans.is_empty() {
            return;
        }
        spans.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(spans.len());
        for span in spans {
            match merged.last_mut() {
                Some(last) if span.0 <= last.1 => last.1 = last.1.max(span.1),
                _ => merged.push(span),
            }
        }
        let places = merged.len() + self.history_flash.more;
        self.history_flash.spans = merged;
        self.history_flash.shown = Some(Instant::now());
        self.paint_history_flash();
        if places > 1 {
            // Most of them may be off screen, so say how many there were.
            let what = match self.history_flash.kind {
                FlashKind::Undo => "Undo",
                FlashKind::Redo => "Redo",
                FlashKind::Paste | FlashKind::Landing => "Paste",
            };
            self.notice(format!("{what} changed {places} places"));
        }
    }
    fn paint_history_flash(&self) {
        let flash = &self.history_flash;
        let Some(i) = self.idx(flash.id) else { return };
        let large = self.buffers[i].large;
        let accent = colors(self.prefs.light).accent as isize;
        for p in 0..2 {
            if self.assigned[p] != Some(flash.id) || (p == 1 && !self.split) {
                continue;
            }
            let e = &self.editors[p];
            let len = e.len();
            quiet(|| {
                e.send(SETINDICATORCURRENT, MARK, 0);
                e.send(INDICATORCLEARRANGE, 0, len as isize);
                // One indicator has one look. Text that came back is boxed. A
                // place where text went away gets a point when a character
                // follows it on its line, which is where a point can sit, and
                // every place of this flash is such a seam. Otherwise the
                // character beside the seam is boxed.
                let mut marks = Vec::with_capacity(flash.spans.len());
                let mut seams = true;
                for &(start, end) in &flash.spans {
                    let (a, b) = if large {
                        // A pane holds one page of a paged document.
                        let limit = events::point(p, len as u64);
                        if start < events::binding(p).base || start > limit {
                            continue;
                        }
                        (
                            events::offset(p, start) as usize,
                            events::offset(p, end.min(limit)) as usize,
                        )
                    } else {
                        (start as usize, end as usize)
                    };
                    let (a, b) = (a.min(len), b.min(len));
                    if b > a {
                        seams = false;
                        marks.push((a, b));
                        continue;
                    }
                    let line = e.send(LINEFROMPOSITION, a, 0).max(0) as usize;
                    let line_end = (e.send(GETLINEENDPOSITION, line, 0).max(0) as usize).min(len);
                    if a < line_end {
                        let next = e.send(POSITIONAFTER, a, 0).max(0) as usize;
                        marks.push((a, next.clamp(a + 1, len)));
                        continue;
                    }
                    // Nothing is drawn past the end of a line, the end of the
                    // text included, so the character before carries the mark.
                    // An empty line has none: the caret is what shows there.
                    seams = false;
                    let line_start = e.send(POSITIONFROMLINE, line, 0).max(0) as usize;
                    if a > line_start {
                        let before =
                            e.send(linework_editor::sci::SCI_POSITIONBEFORE, a, 0).max(0) as usize;
                        marks.push((before.clamp(line_start, a - 1), a));
                    }
                }
                let landing = flash.kind == FlashKind::Landing;
                let look = if landing { FULL } else if seams { POINT } else { BOX };
                e.send(INDICSETSTYLE, MARK, look);
                e.send(INDICSETFORE, MARK, accent);
                e.send(INDICSETALPHA, MARK, if landing { 44 } else { 80 });
                for (a, b) in marks {
                    e.send(INDICATORFILLRANGE, a, (b - a) as isize);
                }
            });
        }
    }
    /// Remove the flash, wherever its document is now.
    pub(super) fn clear_history_flash(&mut self) {
        let flash = std::mem::take(&mut self.history_flash);
        if flash.shown.is_none() {
            return;
        }
        let Some(i) = self.idx(flash.id) else { return };
        let wipe = |e: &Editor| {
            e.send(SETINDICATORCURRENT, MARK, 0);
            e.send(INDICATORCLEARRANGE, 0, e.len() as isize);
        };
        let mut seen = false;
        for p in 0..2 {
            if self.assigned[p] == Some(flash.id) {
                seen = true;
                quiet(|| wipe(&self.editors[p]));
            }
        }
        // Indicators belong to the document, so a resident document that left
        // its pane would bring the marks back with it.
        let buffer = &self.buffers[i];
        if !seen && !buffer.large && buffer.native != 0 {
            quiet(|| {
                msg(self.bridge.hwnd(), SETDOCPOINTER, 0, buffer.native);
                wipe(&self.bridge);
            });
        }
    }
    /// Called from the maintenance timer.
    pub(super) fn tick_history_flash(&mut self) {
        let stay = if self.history_flash.kind == FlashKind::Landing { LANDING } else { FLASH };
        match self.history_flash.shown {
            Some(since) if since.elapsed() >= stay => self.clear_history_flash(),
            // The last step of a history action was not reported as such.
            None if !self.history_flash.spans.is_empty() => self.show_history_flash(),
            _ => {}
        }
    }

    /// The reader did something in the preview pane.
    pub(super) fn preview_signal(&mut self, signal: Signal) {
        match signal {
            Signal::Jump { id, rev, at, len } => self.preview_jump(id, rev, at, len),
            Signal::Cues(cues) => {
                if self.prefs.preview_cues != cues {
                    self.prefs.preview_cues = cues;
                    self.save_prefs();
                    self.document_preview.mark_update();
                }
            }
            Signal::Focus => {
                if self.current().is_some() {
                    focus(self.edit().hwnd());
                }
            }
            Signal::Chord {
                vk,
                ctrl,
                shift,
                alt,
            } => self.preview_chord(vk, ctrl, shift, alt),
            Signal::Task { id, rev, at } => self.preview_task(id, rev, at),
            Signal::Peek { id, rev, at, len, token } => self.preview_peek(id, rev, at, len, token),
        }
    }
    /// A task box was clicked in the preview: tick or clear it in the source.
    /// The change is an ordinary edit, so Undo takes it back.
    fn preview_task(&mut self, id: u64, rev: u64, at: u64) {
        let Some(i) = self.current() else { return };
        if self.buffers[i].id != id {
            return;
        }
        if self.buffers[i].rev != rev {
            self.notice("The preview was still catching up. Click again.");
            return;
        }
        if self.buffers[i].large {
            self.notice("Task boxes can be ticked in the source of a file this large");
            return;
        }
        let at = at as usize;
        if at + 3 > self.edit().len() {
            return;
        }
        let next: &[u8] = match self.edit().raw_bytes(at, at + 3).as_slice() {
            b"[ ]" => b"x",
            b"[x]" | b"[X]" => b" ",
            _ => return,
        };
        if self.writable(i) {
            self.edit().replace_bytes(at + 1, at + 2, next);
        }
    }
    /// The reader asked what Markdown a rendered block came from.
    fn preview_peek(&mut self, id: u64, rev: u64, at: u64, len: u64, token: u64) {
        let Some(i) = self.current() else { return };
        let b = &self.buffers[i];
        if b.id != id || b.rev != rev {
            return;
        }
        let len = len.min(4096);
        let bytes = if b.large {
            b.doc.read(at, len as usize).unwrap_or_default()
        } else {
            let e = self.edit();
            let end = (at + len).min(e.len() as u64);
            e.raw_bytes(at.min(end) as usize, end as usize)
        };
        // A cut in the middle of a character is dropped, not shown as damage.
        let whole = match std::str::from_utf8(&bytes) {
            Ok(_) => bytes.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => bytes.len(),
        };
        let text = String::from_utf8_lossy(&bytes[..whole]);
        self.document_preview.peeked(token, &text);
    }
    /// Rendered text was clicked: put the source caret on the same text.
    fn preview_jump(&mut self, id: u64, rev: u64, mut at: u64, mut len: u64) {
        let Some(i) = self.current() else { return };
        let buffer = &self.buffers[i];
        if buffer.id != id {
            return;
        }
        if buffer.rev != rev {
            // The page that was clicked is one edit old. Carry the position
            // through that edit. An older page is not trusted with a position.
            // Only a typed edit records exactly what it removed and added.
            let newest = self.document_preview.mark().filter(|mark| {
                mark.id == id
                    && mark.rev == buffer.rev
                    && rev + 1 == buffer.rev
                    && mark.kind == EditKind::Typed
            });
            let Some(mark) = newest else {
                self.notice("The preview was still catching up. Click again.");
                return;
            };
            let end = mark.at + mark.del;
            if (at < mark.at && at + len > mark.at) || (at >= mark.at && at < end) {
                len = 0;
            }
            if at >= end {
                at = at - mark.del + mark.add;
            } else if at > mark.at {
                at = mark.at + mark.add;
            }
        }
        self.remember_location();
        self.go(at, len as usize);
        focus(self.edit().hwnd());
        if len == 0 {
            self.flash_landing();
        }
    }
    /// A shortcut was pressed while the preview had the keyboard.
    fn preview_chord(&mut self, vk: u32, ctrl: bool, shift: bool, alt: bool) {
        let mut chord = commands::chord(vk, ctrl, shift, alt);
        if chord.is_empty() {
            return;
        }
        chord.make_ascii_lowercase();
        let Some(&command) = self.shortcuts.get(&chord) else {
            if ctrl || alt {
                self.project_tool_by_shortcut(&chord);
            }
            return;
        };
        // These act on the text under the keyboard, which is the preview's.
        if matches!(
            command.id,
            "undo" | "redo" | "cut" | "copy" | "paste" | "selall" | "caretup" | "caretdown"
        ) {
            return;
        }
        self.command(command.id);
    }
    /// The preview commands of the View menu and the palette.
    pub(super) fn preview_command(&mut self, id: &str) {
        if id == "previewcue" {
            self.preview_cue_settings();
            return;
        }
        if id == "previewfollow" {
            let mut cues = self.prefs.preview_cues.checked();
            cues.follow = !cues.follow;
            self.document_preview.set_cues(&cues);
            self.document_preview.mark_update();
            let now = if cues.follow {
                "The preview follows the source caret"
            } else {
                "The preview keeps its own place"
            };
            self.prefs.preview_cues = cues;
            self.save_prefs();
            self.notice(now);
            return;
        }
        if !self.document_preview.is_open() || self.preview_kind().is_none() {
            self.notice("Open the document preview first: View, Show document preview to the right");
            return;
        }
        let Some(i) = self.current() else { return };
        let done = match id {
            "previewshow" => self.document_preview.show_edit(),
            "previewsync" => {
                let (id, rev) = (self.buffers[i].id, self.buffers[i].rev);
                self.document_preview.reveal(id, rev, self.global_pos())
            }
            "previewsource" => self.document_preview.locate_reading(),
            _ => return,
        };
        if !done {
            self.notice(if self.preview_kind() == Some(crate::preview::Kind::Markdown) {
                "The preview is still drawing this page. Try again in a moment."
            } else {
                "Only a Markdown preview knows where its text is in the source"
            });
        }
    }
    /// The same settings as the Edit cue button in the preview header, for
    /// the keyboard and for when the preview is closed.
    fn preview_cue_settings(&mut self) {
        let now = self.prefs.preview_cues.checked();
        let Some((values, checks)) = forms::form(
            self.h,
            "Preview effects",
            &[
                forms::Field {
                    label: "Mark edited text with: pill, beam, underline, block or off",
                    value: now.style.clone(),
                },
                forms::Field {
                    label: "Dim the rest of the page, 0 to 45 percent",
                    value: now.dim.to_string(),
                },
            ],
            &[
                ("Mark the changed characters instead of the whole word", now.letters),
                ("Marker in the margin", now.margin),
                ("Reduce motion", now.calm),
                ("Click preview text to move the source caret", now.click_source),
                ("Fresh ink: new text starts warm and settles", now.ink),
                ("Change echo: mark pasted text in the source and the preview", now.echo),
                ("Deletion ghosts: deleted text lingers above the join", now.ghosts),
                ("Peek: hover a link, or Ctrl+hover a block for its Markdown", now.peek),
                ("Selection mirror: show the source selection in the preview", now.mirror),
                ("Follow cursor: scroll the preview with the source caret", now.follow),
                ("Task boxes in the preview can be ticked", now.tasks),
            ],
            "Apply",
        ) else {
            return;
        };
        let style = values[0].trim().to_ascii_lowercase();
        if !PreviewCues::STYLES.contains(&style.as_str()) {
            self.notice("The style is one of: pill, beam, underline, block, off");
            return;
        }
        let Some(dim) = values[1]
            .trim()
            .trim_end_matches('%')
            .trim()
            .parse::<u8>()
            .ok()
            .filter(|dim| *dim <= PreviewCues::MAX_DIM)
        else {
            self.notice("The dim is a whole number from 0 to 45");
            return;
        };
        let cues = PreviewCues {
            style,
            letters: checks[0],
            margin: checks[1],
            dim,
            calm: checks[2],
            click_source: checks[3],
            ink: checks[4],
            echo: checks[5],
            ghosts: checks[6],
            peek: checks[7],
            mirror: checks[8],
            follow: checks[9],
            tasks: checks[10],
        };
        if self.prefs.preview_cues != cues {
            self.document_preview.set_cues(&cues);
            self.prefs.preview_cues = cues;
            self.save_prefs();
        }
    }
}
