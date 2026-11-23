//! Effects that live in the source editor: edit heat in the gutter, pair
//! glow, paragraph focus and Markdown typing help, with their settings. The
//! two glides are drawn by `crate::glide`; this module switches them.
use super::*;
use crate::{
    glide,
    mdsmart::{self, Enter, Span},
    state::EditorEffects,
};
use linework_editor::sci as s;

/// Margin markers 1 to 5: a just-edited line, then four cooler steps.
/// 0 is the bookmark and 21 and up belong to change history and folding.
const HEAT_MARK: usize = 1;
const HEAT_STEPS: usize = 5;
const HEAT_MASK: isize = 0b11_1110;
/// How long a mark takes to cool away.
const HEAT: Duration = Duration::from_secs(10);
/// Marks kept at once, and lines marked by one edit.
const HEAT_LIMIT: usize = 512;
const HEAT_LINES: usize = 200;
/// Indicators. 8 to 15 are spelling, search, phrase marks and the history
/// flash; plug-ins are handed 16 to 29.
const PAIR: usize = 30;
const DIM: usize = 31;
/// Lines looked at around the caret, in bytes and in number.
const LINE_LIMIT: usize = 4096;
const PARAGRAPH_LINES: usize = 400;

struct HeatMark {
    id: u64,
    handle: isize,
    born: Instant,
    step: usize,
}

/// What the editor panes are showing of these effects.
#[derive(Default)]
pub(super) struct Polish {
    heat: Vec<HeatMark>,
    /// The document and the two glowing ranges, per pane.
    pair: [Option<(isize, Span, Span)>; 2],
}

fn mix(from: u32, to: u32, toward: f32) -> isize {
    (0..3).fold(0u32, |color, i| {
        let (a, b) = ((from >> (8 * i) & 255) as f32, (to >> (8 * i) & 255) as f32);
        color | ((a + (b - a) * toward).round() as u32) << (8 * i)
    }) as isize
}

impl App {
    /// Colors and looks of the effects for one pane, and the glide switches.
    /// Runs after every restyle, which resets what it sets.
    pub(crate) fn effect_styles(&self, p: usize) {
        let h = self.editors[p].hwnd();
        let fx = self.prefs.editor_effects;
        quiet(|| {
            let gutter = msg(h, s::SCI_STYLEGETBACK, 33, 0) as u32;
            let page = msg(h, s::SCI_STYLEGETBACK, 32, 0) as u32;
            let ink = msg(h, s::SCI_STYLEGETFORE, 32, 0) as u32;
            let accent = msg(h, s::SCI_GETELEMENTCOLOUR, 40, 0) as u32 & 0xff_ffff;
            // Amber, as blue-green-red.
            let warm = if self.prefs.light { 0x10_78_D0 } else { 0x3A_A8_F0 };
            for step in 0..HEAT_STEPS {
                let color = mix(warm, gutter, step as f32 / HEAT_STEPS as f32);
                msg(h, s::SCI_MARKERDEFINE, HEAT_MARK + step, s::SC_MARK_LEFTRECT as isize);
                msg(h, s::SCI_MARKERSETFORE, HEAT_MARK + step, color);
                msg(h, s::SCI_MARKERSETBACK, HEAT_MARK + step, color);
            }
            msg(h, s::SCI_SETMARGINMASKN, 1, 1 | HEAT_MASK);
            msg(h, s::SCI_INDICSETSTYLE, PAIR, s::INDIC_ROUNDBOX as isize);
            msg(h, s::SCI_INDICSETFORE, PAIR, accent as isize);
            msg(h, s::SCI_INDICSETALPHA, PAIR, 64);
            msg(h, s::SCI_INDICSETOUTLINEALPHA, PAIR, 255);
            // Over the text: the glow is always on the caret's line, whose
            // highlight would cover anything drawn under it.
            msg(h, s::SCI_INDICSETUNDER, PAIR, 0);
            msg(h, s::SCI_INDICSETSTYLE, DIM, s::INDIC_TEXTFORE as isize);
            msg(h, s::SCI_INDICSETFORE, DIM, mix(page, ink, 0.36));
        });
        let calm = self.prefs.preview_cues.calm || !system_animations();
        glide::configure(glide::Config {
            caret: fx.glide_caret && !calm,
            line: fx.glide_line && !calm && self.prefs.current_line,
        });
    }
    /// The pane is about to show other text.
    pub(super) fn polish_leaving(&mut self, p: usize) {
        if self.polish.pair[p].take().is_some() {
            let h = self.editors[p].hwnd();
            quiet(|| {
                msg(h, SETINDICATORCURRENT, PAIR, 0);
                msg(h, INDICATORCLEARRANGE, 0, msg(h, s::SCI_GETLENGTH, 0, 0));
            });
        }
    }
    /// The pane shows other text now.
    pub(super) fn polish_bound(&mut self, p: usize) {
        glide::forget();
        self.pair_glow(p);
        self.paragraph_focus(p);
    }
    /// The caret, the selection or the text of a pane changed.
    pub(super) fn polish_update(&mut self, p: usize, updated: i32) {
        if updated & 3 != 0 {
            self.pair_glow(p);
            self.paragraph_focus(p);
        }
    }
    fn markdown(&self, p: usize) -> bool {
        self.assigned[p]
            .and_then(|id| self.idx(id))
            .is_some_and(|i| self.buffers[i].lang == "Markdown")
    }
    /// The caret's line as bytes, with where it starts and the caret's
    /// offset in it. None for more than one caret or a very long line.
    fn caret_line(&self, p: usize) -> Option<(usize, usize, Vec<u8>)> {
        let e = &self.editors[p];
        let h = e.hwnd();
        if msg(h, GETSELECTIONS, 0, 0) != 1 {
            return None;
        }
        let caret = e.caret();
        let line = msg(h, LINEFROMPOSITION, caret, 0).max(0) as usize;
        let a = msg(h, POSITIONFROMLINE, line, 0).max(0) as usize;
        let z = msg(h, GETLINEENDPOSITION, line, 0).max(0) as usize;
        (z - a <= LINE_LIMIT && caret >= a && caret <= z).then(|| (a, caret - a, e.raw_bytes(a, z)))
    }

    // Pair glow.

    fn pair_glow(&mut self, p: usize) {
        let h = self.editors[p].hwnd();
        let mut next = None;
        if self.prefs.editor_effects.pair_glow
            && self.markdown(p)
            && let Some((a, caret, text)) = self.caret_line(p)
            && let Some((open, close)) = mdsmart::pair(&text, caret)
        {
            next = Some((
                msg(h, s::SCI_GETDOCPOINTER, 0, 0),
                (a + open.0, a + open.1),
                (a + close.0, a + close.1),
            ));
        }
        if self.polish.pair[p] == next {
            return;
        }
        let shown = std::mem::replace(&mut self.polish.pair[p], next);
        quiet(|| {
            msg(h, SETINDICATORCURRENT, PAIR, 0);
            if shown.is_some() {
                msg(h, INDICATORCLEARRANGE, 0, msg(h, s::SCI_GETLENGTH, 0, 0));
            }
            if let Some((_, open, close)) = next {
                for (a, b) in [open, close] {
                    msg(h, INDICATORFILLRANGE, a, (b - a) as isize);
                }
            }
        });
    }

    // Paragraph focus.

    /// Dim everything outside the caret's paragraph. The dimming is an
    /// indicator that recolors text, so no text moves and none changes.
    fn paragraph_focus(&mut self, p: usize) {
        let h = self.editors[p].hwnd();
        let len = msg(h, s::SCI_GETLENGTH, 0, 0).max(0) as usize;
        let dimmed = |at: usize| msg(h, s::SCI_INDICATORVALUEAT, DIM, at as isize) != 0;
        let run_end = |at: usize| msg(h, s::SCI_INDICATOREND, DIM, at as isize).max(0) as usize;
        if !self.prefs.editor_effects.paragraph_focus {
            if len > 0 && (dimmed(0) || run_end(0) < len) {
                quiet(|| {
                    msg(h, SETINDICATORCURRENT, DIM, 0);
                    msg(h, INDICATORCLEARRANGE, 0, len as isize);
                });
            }
            return;
        }
        let caret = self.editors[p].caret();
        let line = msg(h, LINEFROMPOSITION, caret, 0).max(0) as usize;
        let count = msg(h, GETLINECOUNT, 0, 0).max(1) as usize;
        let blank = |l: usize| {
            msg(h, GETLINEINDENTPOSITION, l, 0) >= msg(h, GETLINEENDPOSITION, l, 0)
        };
        let (mut top, mut bottom) = (line, line);
        if !blank(line) {
            while top > 0 && line - top < PARAGRAPH_LINES && !blank(top - 1) {
                top -= 1;
            }
            while bottom + 1 < count && bottom - line < PARAGRAPH_LINES && !blank(bottom + 1) {
                bottom += 1;
            }
        }
        let start = msg(h, POSITIONFROMLINE, top, 0).max(0) as usize;
        let end = if bottom + 1 < count {
            msg(h, POSITIONFROMLINE, bottom + 1, 0).max(0) as usize
        } else {
            len
        };
        // Typing inside the paragraph moves the dimmed ranges with the text,
        // so most of the time everything is already where it belongs.
        let before = start == 0
            || (dimmed(start - 1) && msg(h, s::SCI_INDICATORSTART, DIM, start as isize - 1) == 0);
        let inside = start == end || (!dimmed(start) && run_end(start) == end);
        let after = end >= len || (dimmed(end) && run_end(end) == len);
        if before && inside && after {
            return;
        }
        quiet(|| {
            msg(h, SETINDICATORCURRENT, DIM, 0);
            msg(h, INDICATORCLEARRANGE, 0, len as isize);
            if start > 0 {
                msg(h, INDICATORFILLRANGE, 0, start as isize);
            }
            if end < len {
                msg(h, INDICATORFILLRANGE, end, (len - end) as isize);
            }
        });
    }

    // Edit heat.

    /// An editor that has the document, for work on its line markers.
    fn with_document<T>(&self, i: usize, f: impl FnOnce(H) -> T) -> Option<T> {
        let b = &self.buffers[i];
        if b.large || b.native == 0 {
            return None;
        }
        if let Some(p) = (0..2).find(|&p| self.assigned[p] == Some(b.id) && events::binding(p).id == b.id) {
            return Some(quiet(|| f(self.editors[p].hwnd())));
        }
        Some(quiet(|| {
            msg(self.bridge.hwnd(), SETDOCPOINTER, 0, b.native);
            f(self.bridge.hwnd())
        }))
    }
    /// Text was added at `at`, or removed there: warm the lines it touched.
    pub(super) fn heat_edit(&mut self, i: usize, at: u64, add: u64) {
        if !self.prefs.editor_effects.heat {
            return;
        }
        let id = self.buffers[i].id;
        let now = Instant::now();
        let fresh = self.with_document(i, |h| {
            let first = msg(h, LINEFROMPOSITION, at as usize, 0).max(0) as usize;
            let last = msg(h, LINEFROMPOSITION, (at + add) as usize, 0).max(0) as usize;
            (first..=last.min(first + HEAT_LINES))
                .filter_map(|line| {
                    if msg(h, s::SCI_MARKERGET, line, 0) & HEAT_MASK != 0 {
                        for step in 0..HEAT_STEPS {
                            msg(h, s::SCI_MARKERDELETE, line, (HEAT_MARK + step) as isize);
                        }
                    }
                    let handle = msg(h, MARKERADD, line, HEAT_MARK as isize);
                    (handle >= 0).then_some(HeatMark { id, handle, born: now, step: 0 })
                })
                .collect::<Vec<_>>()
        });
        self.polish.heat.extend(fresh.unwrap_or_default());
        if self.polish.heat.len() > HEAT_LIMIT {
            let extra = self.polish.heat.len() - HEAT_LIMIT;
            let old: Vec<_> = self.polish.heat.drain(..extra).collect();
            self.heat_remove(old);
        }
    }
    fn heat_remove(&self, marks: Vec<HeatMark>) {
        for mark in marks {
            if let Some(i) = self.idx(mark.id) {
                self.with_document(i, |h| msg(h, s::SCI_MARKERDELETEHANDLE, mark.handle as usize, 0));
            }
        }
    }
    /// Called from the maintenance timer: cool each mark one step at a time.
    pub(super) fn heat_tick(&mut self) {
        if self.polish.heat.is_empty() {
            return;
        }
        let marks = std::mem::take(&mut self.polish.heat);
        let mut keep = Vec::with_capacity(marks.len());
        for mut mark in marks {
            let age = mark.born.elapsed();
            let step = (age.as_millis() * HEAT_STEPS as u128 / HEAT.as_millis()) as usize;
            if step == mark.step {
                keep.push(mark);
                continue;
            }
            let Some(i) = self.idx(mark.id) else { continue };
            let moved = self.with_document(i, |h| {
                let line = msg(h, s::SCI_MARKERLINEFROMHANDLE, mark.handle as usize, 0);
                msg(h, s::SCI_MARKERDELETEHANDLE, mark.handle as usize, 0);
                if line < 0 || step >= HEAT_STEPS {
                    return -1;
                }
                msg(h, MARKERADD, line as usize, (HEAT_MARK + step) as isize)
            });
            if let Some(handle) = moved.filter(|handle| *handle >= 0) {
                mark.handle = handle;
                mark.step = step;
                keep.push(mark);
            }
        }
        self.polish.heat = keep;
    }

    // Markdown typing help.

    /// The document Markdown typing help may act on through the focused
    /// editor: Markdown, held in the editor, one selection, nothing recording.
    fn markdown_target(&self, h: H) -> Option<usize> {
        let i = self.current()?;
        let b = &self.buffers[i];
        (h == self.edit().hwnd()
            && !b.large
            && b.lang == "Markdown"
            && !self.recording
            && !self.playing
            && msg(h, GETSELECTIONS, 0, 0) == 1)
            .then_some(i)
    }
    /// A character was typed. With text selected, `*`, `_`, `~` and a
    /// backtick wrap it and `[` makes it the text of a link. True when the
    /// character was used up.
    pub fn smart_char(&mut self, h: H, unit: u16) -> bool {
        let Ok(mark @ (b'*' | b'_' | b'~' | b'`' | b'[')) = u8::try_from(unit) else {
            return false;
        };
        if !self.prefs.editor_effects.smart_typing {
            return false;
        }
        let Some(i) = self.markdown_target(h) else { return false };
        let (anchor, caret) = self.edit().selection();
        let (a, b) = (anchor.min(caret), anchor.max(caret));
        if a == b || b - a > 1 << 20 || !self.writable(i) {
            return false;
        }
        let e = self.edit();
        let mut text = vec![mark];
        text.extend(e.raw_bytes(a, b));
        if mark == b'[' {
            text.extend(b"]()");
            e.replace_bytes(a, b, &text);
            // Between the brackets, ready for the address.
            e.set_selection(b + 3, b + 3);
        } else {
            text.push(mark);
            e.replace_bytes(a, b, &text);
            // The same words stay selected, so a second `*` makes bold.
            if anchor < caret {
                e.set_selection(a + 1, b + 1);
            } else {
                e.set_selection(b + 1, a + 1);
            }
        }
        true
    }
    /// A key went down in the focused editor. True when it was used up.
    pub(super) fn smart_key(&mut self, k: u16, ctrl: bool, shift: bool, alt: bool) -> bool {
        if alt || !self.prefs.editor_effects.smart_typing || self.current().is_none() {
            return false;
        }
        let h = self.edit().hwnd();
        if focused() != h {
            return false;
        }
        let Some(i) = self.markdown_target(h) else { return false };
        match k {
            VK_RETURN if !ctrl && !shift => self.smart_enter(i),
            VK_TAB if !ctrl => self.smart_tab(i, shift),
            0x42 | 0x49 if ctrl && !shift => {
                self.emphasis(i, if k == 0x42 { 2 } else { 1 });
                true
            }
            _ => false,
        }
    }
    /// Enter on a list item or a quote line starts the next one. On an empty
    /// one it removes the markup instead, which ends the list.
    fn smart_enter(&mut self, i: usize) -> bool {
        let (anchor, caret) = self.edit().selection();
        if anchor != caret {
            return false;
        }
        let Some((a, column, line)) = self.caret_line(self.active) else { return false };
        let Some(action) = mdsmart::enter(&line, column) else { return false };
        if !self.writable(i) {
            return false;
        }
        let e = self.edit();
        match action {
            Enter::Continue(lead) => {
                let mut text: Vec<u8> = match e.send(GETEOLMODE, 0, 0) {
                    0 => b"\r\n".to_vec(),
                    1 => b"\r".to_vec(),
                    _ => b"\n".to_vec(),
                };
                text.extend(lead.as_bytes());
                e.replace_bytes(caret, caret, &text);
                e.set_selection(caret + text.len(), caret + text.len());
            }
            Enter::End { from } => {
                e.replace_bytes(a + from, a + line.len(), b"");
                e.set_selection(a + from, a + from);
            }
        }
        e.send(SCROLLCARET, 0, 0);
        true
    }
    /// Tab at the start of a list item nests it one level, Shift+Tab lifts
    /// it. Anywhere else Tab does what it always did.
    fn smart_tab(&mut self, i: usize, back: bool) -> bool {
        let (anchor, caret) = self.edit().selection();
        if anchor != caret {
            return false;
        }
        let Some((a, column, line)) = self.caret_line(self.active) else { return false };
        let Some(lead) = mdsmart::lead(&line) else { return false };
        let empty = line[lead.end..].iter().all(u8::is_ascii_whitespace);
        if lead.step == 0 || lead.quote > lead.indent || (column > lead.end && !empty) {
            return false;
        }
        let h = self.edit().hwnd();
        let row = msg(h, LINEFROMPOSITION, caret, 0).max(0) as usize;
        let indent = msg(h, s::SCI_GETLINEINDENTATION, row, 0).max(0) as usize;
        if (back && indent == 0) || !self.writable(i) {
            return false;
        }
        let next = if back { indent.saturating_sub(lead.step) } else { indent + lead.step };
        msg(h, BEGINUNDOACTION, 0, 0);
        msg(h, s::SCI_SETLINEINDENTATION, row, next as isize);
        msg(h, ENDUNDOACTION, 0, 0);
        let grown = msg(h, GETLINEENDPOSITION, row, 0) - (a + line.len()) as isize;
        let at = (caret as isize + grown).max(a as isize) as usize;
        self.edit().set_selection(at, at);
        true
    }
    /// Bold (`width` 2) or italic (`width` 1) on or off, for the selection
    /// or else the word at the caret. With neither, the markers are typed
    /// and the caret waits between them.
    fn emphasis(&mut self, i: usize, width: usize) {
        if !self.writable(i) {
            return;
        }
        let e = self.edit();
        let (anchor, caret) = e.selection();
        let (mut a, mut b) = (anchor.min(caret), anchor.max(caret));
        if a == b {
            a = e.send(WORDSTARTPOSITION, caret, 1).max(0) as usize;
            b = (e.send(WORDENDPOSITION, caret, 1).max(0) as usize).max(a);
        }
        // Up to three stars each side decide what is already on.
        let from = a.saturating_sub(3);
        let near = e.raw_bytes(from, (b + 3).min(e.len()));
        let (span, text, keep) = mdsmart::toggle(&near, a - from, b - from, width);
        e.replace_bytes(from + span.0, from + span.1, &text);
        let (start, end) = (from + keep.0, from + keep.1);
        if anchor == caret {
            // The caret stays on the letter it was on.
            let at = (start + caret.saturating_sub(a)).min(end);
            e.set_selection(at, at);
        } else if anchor < caret {
            e.set_selection(start, end);
        } else {
            e.set_selection(end, start);
        }
    }

    // Settings and commands.

    pub(super) fn polish_command(&mut self, id: &str) {
        let mut fx = self.prefs.editor_effects;
        match id {
            "paragraphfocus" => {
                fx.paragraph_focus = !fx.paragraph_focus;
                self.notice(if fx.paragraph_focus {
                    "Paragraph focus is on: the text around the caret's paragraph is dimmed"
                } else {
                    "Paragraph focus is off"
                });
            }
            "mdbold" | "mditalic" => {
                let h = self.current().map_or(0, |_| self.edit().hwnd());
                match self.markdown_target(h) {
                    Some(i) => self.emphasis(i, if id == "mdbold" { 2 } else { 1 }),
                    None => self.notice("Bold and italic work in a Markdown document"),
                }
                return;
            }
            _ => {
                let Some((_, checks)) = forms::form(
                    self.h,
                    "Editor effects",
                    &[],
                    &[
                        ("Gliding cursor: the caret slides to its new place", fx.glide_caret),
                        ("Active line glide: the line highlight slides between lines", fx.glide_line),
                        ("Edit heat: edited lines get a gutter mark that cools over 10 seconds", fx.heat),
                        ("Pair glow: light both ends of the Markdown markup around the caret", fx.pair_glow),
                        ("Paragraph focus: dim everything outside the caret's paragraph", fx.paragraph_focus),
                        ("Smart Markdown typing: wrap selections, continue lists, Ctrl+B, Ctrl+I", fx.smart_typing),
                    ],
                    "Apply",
                ) else {
                    return;
                };
                fx = EditorEffects {
                    glide_caret: checks[0],
                    glide_line: checks[1],
                    heat: checks[2],
                    pair_glow: checks[3],
                    paragraph_focus: checks[4],
                    smart_typing: checks[5],
                };
            }
        }
        if self.prefs.editor_effects == fx {
            return;
        }
        self.prefs.editor_effects = fx;
        self.save_prefs();
        if !fx.heat {
            let marks = std::mem::take(&mut self.polish.heat);
            self.heat_remove(marks);
        }
        for p in 0..2 {
            self.effect_styles(p);
            if self.assigned[p].is_some() && (p == 0 || self.split) {
                self.pair_glow(p);
                self.paragraph_focus(p);
            }
        }
    }
}

/// Whether Windows is set to animate controls at all.
fn system_animations() -> bool {
    const SPI_GETCLIENTAREAANIMATION: u32 = 0x1042;
    let mut on: i32 = 1;
    unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&mut on as *mut i32).cast(), 0) };
    on != 0
}
