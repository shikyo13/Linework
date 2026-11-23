use crate::{App, forms, sci, state::Step, win};
use linework_editor::Editor;
use std::{
    cell::{Cell, RefCell},
    ffi::{CStr, CString},
    time::{Duration, Instant},
};
use windows_sys::Win32::UI::WindowsAndMessaging::{IDYES, WM_APP};

pub const TICK: u32 = WM_APP + 84;
const TEXT_LIMIT: usize = 8 * 1024 * 1024;

pub fn recordable_command(id: &str) -> bool {
    matches!(
        id,
        "comment"
            | "indent"
            | "outdent"
            | "sortlines"
            | "sortdesc"
            | "dedup"
            | "trimlines"
            | "cut"
            | "copy"
            | "paste"
            | "dup"
            | "moveup"
            | "movedown"
            | "upper"
            | "lower"
            | "selall"
            | "selnext"
            | "selmatches"
            | "selskipped"
            | "undoselect"
            | "sellines"
            | "caretup"
            | "caretdown"
            | "rectangle"
            | "fold"
            | "foldall"
            | "unfold"
    )
}

/// Called synchronously inside a genuine SCN_MACRORECORD notification from
/// one of the app's own editor controls. The native buffer expires on return.
/// # Safety
/// For a text command l must point to its documented byte or C-string argument.
pub unsafe fn capture(msg: u32, w: usize, l: isize) -> Option<Step> {
    use sci::*;
    if !RECORDABLE.contains(&msg) {
        return None;
    }
    let string = match msg {
        ADDTEXT | APPENDTEXT => {
            if w > TEXT_LIMIT || l == 0 {
                return None;
            }
            let bytes = unsafe { std::slice::from_raw_parts(l as *const u8, w) };
            Some(std::str::from_utf8(bytes).ok()?.to_owned())
        }
        REPLACESEL | INSERTTEXT | SEARCHNEXT | SEARCHPREV => {
            if l == 0 {
                return None;
            }
            let text = unsafe { CStr::from_ptr(l as *const _) };
            if text.to_bytes().len() > TEXT_LIMIT {
                return None;
            }
            Some(text.to_str().ok()?.to_owned())
        }
        _ => None,
    };
    let arg = match msg {
        ADDTEXT | APPENDTEXT | INSERTTEXT | SEARCHNEXT | SEARCHPREV | GOTOPOS | GOTOLINE
        | SETSELECTIONMODE => w,
        _ => 0,
    };
    Some(Step::Native {
        msg,
        arg,
        text: string,
    })
}

thread_local! {
    static LINE_END: Cell<(isize, usize, usize, u8)> = const { Cell::new((0, 0, 0, 0)) };
}

/// Scintilla reports each byte in each caret's line ending individually.
/// Preserve the original Enter operation so replay follows the target's
/// current carets and LF, CRLF or CR setting.
/// # Safety
/// The arguments must originate from a genuine notification from editor.
pub unsafe fn capture_from(editor: isize, msg: u32, w: usize, l: isize) -> Option<Step> {
    let step = unsafe { capture(msg, w, l) }?;
    if let Step::Native {
        msg: sci::REPLACESEL,
        text: Some(s),
        ..
    } = &step
        && matches!(s.as_bytes(), b"\r" | b"\n")
    {
        let ch = s.as_bytes()[0];
        let (owner, left, width, expected) = LINE_END.get();
        if left > 0 && owner == editor && ch == expected {
            LINE_END.set((
                editor,
                left - 1,
                width,
                if width == 2 {
                    if ch == 13 { 10 } else { 13 }
                } else {
                    ch
                },
            ));
            return None;
        }
        let mode = win::msg(editor, sci::GETEOLMODE, 0, 0);
        let (first, width) = match mode {
            0 => (13, 2),
            1 => (13, 1),
            _ => (10, 1),
        };
        if first == ch {
            let count = win::msg(editor, sci::GETSELECTIONS, 0, 0).max(1) as usize;
            LINE_END.set((
                editor,
                count.saturating_mul(width) - 1,
                width,
                if width == 2 { 10 } else { ch },
            ));
            return Some(Step::Native {
                msg: sci::NEWLINE,
                arg: 0,
                text: None,
            });
        }
    }
    LINE_END.set((0, 0, 0, 0));
    Some(step)
}

pub(crate) fn validate(step: &Step) -> Result<(), String> {
    use sci::*;
    match step {
        Step::Native { msg, arg, text } => {
            if *msg != NEWLINE && !RECORDABLE.contains(msg) {
                return Err("Unknown native macro command".into());
            }
            match *msg {
                ADDTEXT | APPENDTEXT => {
                    let t = text.as_deref().ok_or("Missing text in macro")?;
                    if t.len() > TEXT_LIMIT || t.len() != *arg {
                        return Err("Invalid macro text length".into());
                    }
                }
                REPLACESEL | INSERTTEXT | SEARCHNEXT | SEARCHPREV => {
                    let t = text.as_deref().ok_or("Missing text in macro")?;
                    if t.len() > TEXT_LIMIT || t.as_bytes().contains(&0) {
                        return Err("Invalid macro text".into());
                    }
                    let valid = match *msg {
                        REPLACESEL => *arg == 0,
                        INSERTTEXT => *arg == usize::MAX || *arg <= isize::MAX as usize,
                        _ => *arg & !0x00f0_0006 == 0,
                    };
                    if !valid {
                        return Err("Invalid macro text argument".into());
                    }
                }
                _ => {
                    if text.is_some() {
                        return Err("Unexpected text in macro command".into());
                    }
                    match *msg {
                        GOTOPOS | GOTOLINE if *arg <= isize::MAX as usize => {}
                        SETSELECTIONMODE if *arg <= 4 => {}
                        _ if *arg == 0 => {}
                        _ => return Err("Invalid macro argument".into()),
                    }
                }
            }
        }
        Step::Command(id) if !recordable_command(id) => {
            return Err(format!("Unsupported recorded command: {id}"));
        }
        Step::Find { query, replace, .. }
            if query.is_empty()
                || query.len() > 1024 * 1024
                || replace.as_ref().is_some_and(|s| s.len() > TEXT_LIMIT) =>
        {
            return Err("Invalid recorded search".into());
        }
        _ => {}
    }
    Ok(())
}

pub fn replay_native(editor: &Editor, step: &Step) -> Result<(), String> {
    use sci::*;
    validate(step)?;
    let Step::Native { msg, arg, text } = step else {
        return Err("Expected a native edit".into());
    };
    // Native addresses are constructed from the validated, owned value for
    // this one synchronous call. No address is ever serialized or re-used.
    match *msg {
        ADDTEXT | APPENDTEXT => {
            let value = text.as_ref().unwrap().as_bytes();
            unsafe {
                editor.send_ptr(*msg, value.len(), value.as_ptr() as isize);
            }
        }
        REPLACESEL
            if editor.send(GETSELECTIONS, 0, 0) > 1 || editor.send(GETOVERTYPE, 0, 0) != 0 =>
        {
            replace_at_carets(editor, text.as_deref().unwrap())?;
        }
        REPLACESEL | INSERTTEXT | SEARCHNEXT | SEARCHPREV => {
            let value = CString::new(text.as_ref().unwrap().as_str())
                .map_err(|_| "Invalid embedded NUL")?;
            let arg = if *msg == INSERTTEXT && *arg != usize::MAX {
                (*arg).min(editor.len())
            } else {
                *arg
            };
            let result = unsafe { editor.send_ptr(*msg, arg, value.as_ptr() as isize) };
            if matches!(*msg, SEARCHNEXT | SEARCHPREV) && result < 0 {
                return Err("No further matches".into());
            }
        }
        _ => {
            editor.send(*msg, *arg, 0);
        }
    }
    Ok(())
}

// SCI_REPLACESEL clears the extra carets. Reproduce ordinary typed input
// across all selections and preserve the primary caret for later commands.
fn replace_at_carets(e: &Editor, text: &str) -> Result<(), String> {
    use sci::*;
    let spans = e.selections();
    let overwrite = e.send(GETOVERTYPE, 0, 0) != 0;
    let main = e.send(GETMAINSELECTION, 0, 0).max(0) as usize;
    let mut edits = Vec::with_capacity(spans.len());
    for (i, (a, b)) in spans.into_iter().enumerate() {
        let start = a.min(b);
        let mut end = a.max(b);
        let padding = e.send(GETSELECTIONNSTARTVIRTUALSPACE, i, 0).max(0) as usize;
        if padding > 1024 * 1024 {
            return Err("Virtual column exceeds the editing limit".into());
        }
        if overwrite && padding == 0 && a == b && !text.is_empty() {
            let line = e.send(LINEFROMPOSITION, start, 0) as usize;
            if start < e.send(GETLINEENDPOSITION, line, 0) as usize {
                end = e.send(POSITIONAFTER, start, 0) as usize;
            }
        }
        edits.push((start, end, padding, i));
    }
    edits.sort_unstable();
    if edits.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err("Overlapping selections cannot be replaced independently".into());
    }
    let mut delta = 0isize;
    let mut carets = vec![0; edits.len()];
    for &(a, b, pad, i) in &edits {
        carets[i] = a.saturating_add_signed(delta) + pad + text.len();
        delta += (pad + text.len()) as isize - (b - a) as isize;
    }
    for &(a, b, pad, _) in edits.iter().rev() {
        if pad == 0 {
            e.replace_range(a, b, text);
        } else {
            let mut padded = " ".repeat(pad);
            padded.push_str(text);
            e.replace_range(a, b, &padded);
        }
    }
    if let Some(&p) = carets.first() {
        e.send(SETSELECTION, p, p as isize);
        for &p in &carets[1..] {
            e.send(ADDSELECTION, p, p as isize);
        }
        e.send(SETMAINSELECTION, main.min(carets.len() - 1), 0);
        e.send(SCROLLCARET, 0, 0);
    }
    Ok(())
}

enum Repeat {
    Count(usize),
    ToEnd,
}
struct Playback {
    editor: isize,
    native_undo: bool,
    id: u64,
    steps: Vec<Step>,
    repeat: Repeat,
    pc: usize,
    passes: usize,
    remaining: u64,
    last_notice: Instant,
}
thread_local! {
    static PLAYBACK:RefCell<Option<Playback>>=const{RefCell::new(None)};
    static WAITING_FIND:Cell<bool>=const{Cell::new(false)};
    static FOUND:RefCell<Option<Result<bool,String>>>=const{RefCell::new(None)};
}
pub(crate) fn wait_for_find() {
    WAITING_FIND.set(true);
}
impl App {
    pub(crate) fn align_macro_page(&mut self) {
        let Some(i) = self.current() else { return };
        if !self.buffers[i].large || self.wide_span().is_some() {
            return;
        }
        let editor = self.edit();
        let (a, b) = editor.selection();
        if a != b || editor.send(sci::GETSELECTIONS, 0, 0) != 1 {
            return;
        }
        let base = self.pane_base();
        let loaded = editor.len();
        let near_start = base > 0 && a < 4096;
        let near_end = crate::events::end_point(self.active, loaded as u64) < self.total_len()
            && a.saturating_add(4096) >= loaded;
        if near_start || near_end {
            self.bind(self.active, i, crate::events::point(self.active, a as u64));
        }
    }
    fn play_native(&mut self, step: &Step) -> Result<(), String> {
        use sci::*;
        let Step::Native { msg, arg, .. } = step else {
            return Err("Expected a native macro command".into());
        };
        if self.current().is_some_and(|i| self.buffers[i].large) {
            let total = self.total_len();
            let at = match *msg {
                DOCUMENTEND | DOCUMENTENDEXTEND => Some(total),
                DOCUMENTSTART | DOCUMENTSTARTEXTEND => Some(0),
                GOTOPOS => Some((*arg as u64).min(total)),
                GOTOLINE => {
                    let doc = &self.buffers[self.current().unwrap()].doc;
                    if !doc.indexing_complete() && *arg as u64 >= doc.line_count() {
                        return Err("This line has not been indexed yet".into());
                    }
                    Some(doc.line_start(*arg as u64))
                }
                _ => None,
            };
            if let Some(at) = at {
                if matches!(*msg, DOCUMENTSTARTEXTEND | DOCUMENTENDEXTEND) {
                    let anchor = self.extension_selections(self.active)?[0].0;
                    self.select_span(anchor.min(at)..anchor.max(at), at >= anchor);
                } else {
                    self.go(at, 0);
                }
                return Ok(());
            }
        }
        replay_native(self.edit(), step)
    }
    pub(super) fn macro_command(&mut self, id: &str) {
        if self.playing {
            self.stop_macro();
            return;
        }
        if self.current().is_none() {
            self.notice("Open a document to use macros");
            return;
        }
        if id == "record" {
            if self.recording {
                self.edit().send(sci::STOPRECORD, 0, 0);
                self.recording = false;
                let count = self.macro_steps.len();
                self.prefs.last_macro = self.macro_steps.clone();
                self.save_prefs();
                self.notice(format!("Recorded {count} macro steps"));
            } else {
                self.macro_steps.clear();
                self.recording = true;
                self.edit().send(sci::STARTRECORD, 0, 0);
                self.notice("Recording macro · Ctrl+Shift+M to finish");
            }
            return;
        }
        if self.recording {
            self.notice("Stop recording before managing or replaying a macro");
            return;
        }
        if id == "loadmacro" {
            let names: Vec<_> = self.prefs.macros.keys().cloned().collect();
            if names.is_empty() {
                self.notice("No named macros yet");
                return;
            }
            if let Some(i) = forms::pick(self.h, "Load macro", &names, "") {
                let name = &names[i];
                self.macro_steps = self.prefs.macros[name].clone();
                self.prefs.last_macro = self.macro_steps.clone();
                self.save_prefs();
                self.notice(format!("Loaded {name}"));
            }
            return;
        }
        if self.macro_steps.is_empty() {
            self.macro_steps = self.prefs.last_macro.clone();
        }
        if self.macro_steps.is_empty() {
            self.notice("Record or load a macro first");
            return;
        }
        if id == "savemacro" {
            if let Some(input) = forms::input(self.h, "Save macro", "Macro name", "") {
                let name = input.trim();
                if name.is_empty() || name.len() > 160 {
                    self.notice("Enter a name of 1–160 bytes");
                    return;
                }
                if self.prefs.macros.contains_key(name)
                    && win::confirm(
                        self.h,
                        "Save macro",
                        &format!("Replace the saved macro “{name}”?"),
                        false,
                    ) != IDYES
                {
                    return;
                }
                self.prefs
                    .macros
                    .insert(name.into(), self.macro_steps.clone());
                self.save_prefs();
                self.notice(format!("Saved macro {name}"));
            }
            return;
        }
        let repeat = match id {
            "play" => Repeat::Count(1),
            "repeat" => {
                let Some(value) = forms::input(
                    self.h,
                    "Repeat macro",
                    "Repetitions (1–1,000,000), or enter EOF to repeat through the document",
                    "2",
                ) else {
                    return;
                };
                let value = value.trim();
                if value.eq_ignore_ascii_case("EOF") {
                    Repeat::ToEnd
                } else if let Ok(count @ 1..=1_000_000) = value.replace(',', "").parse::<usize>() {
                    Repeat::Count(count)
                } else {
                    self.notice("Use a positive count up to 1,000,000, or EOF");
                    return;
                }
            }
            _ => return,
        };
        self.start_macro(repeat);
    }

    fn start_macro(&mut self, repeat: Repeat) {
        let Some(tab) = self.current() else {
            return;
        };
        for s in &self.macro_steps {
            if let Err(e) = validate(s) {
                self.notice(e);
                return;
            }
        }
        let remaining = self.total_len().saturating_sub(self.global_pos());
        if matches!(repeat, Repeat::ToEnd) && remaining == 0 {
            self.notice("Already at the end of the document");
            return;
        }
        self.drain_edits();
        let native_undo = !self.buffers[tab].large;
        if native_undo {
            self.edit().send(sci::BEGINUNDOACTION, 0, 0);
        } else {
            self.buffers[tab].doc.begin_undo_group();
        }
        let editor = self.edit().hwnd();
        self.playing = true;
        WAITING_FIND.set(false);
        FOUND.with_borrow_mut(|r| *r = None);
        PLAYBACK.with(|state| {
            state.replace(Some(Playback {
                editor,
                native_undo,
                id: self.buffers[tab].id,
                steps: self.macro_steps.clone(),
                repeat,
                pc: 0,
                passes: 0,
                remaining,
                last_notice: Instant::now(),
            }))
        });
        self.macro_tick();
    }

    pub(super) fn macro_tick(&mut self) {
        let Some(mut p) = PLAYBACK.with(|s| s.take()) else {
            return;
        };
        let began = Instant::now();
        let outcome = loop {
            if self.assigned[self.active] != Some(p.id) || self.edit().hwnd() != p.editor {
                break Some("Macro stopped after changing editors".to_owned());
            }
            let result = match &p.steps[p.pc] {
                s @ Step::Native { .. } => self.play_native(s),
                Step::Command(id) => {
                    self.command(id);
                    Ok(())
                }
                s @ Step::Find { .. } => {
                    let found = match FOUND.with(|r| r.take()) {
                        Some(result) => result,
                        None if WAITING_FIND.get() => {
                            PLAYBACK.with(|s| s.replace(Some(p)));
                            return;
                        }
                        None => self.macro_find(s, matches!(p.repeat, Repeat::ToEnd)),
                    };
                    if WAITING_FIND.get() {
                        PLAYBACK.with(|s| s.replace(Some(p)));
                        return;
                    }
                    found.and_then(|f| {
                        if f {
                            Ok(())
                        } else {
                            Err("No further matches".into())
                        }
                    })
                }
            };
            self.drain_edits();
            self.align_macro_page();
            if let Err(reason) = result {
                break Some(format!("Macro stopped at step {} · {reason} · {} complete passes", p.pc + 1, p.passes));
            }
            p.pc += 1;
            if p.pc == p.steps.len() {
                p.passes += 1;
                p.pc = 0;
                let remaining = self.total_len().saturating_sub(self.global_pos());
                let finished = match p.repeat {
                    Repeat::Count(n) => p.passes >= n,
                    Repeat::ToEnd => remaining == 0,
                };
                if finished {
                    break Some(format!(
                        "Macro completed · {} {}",
                        p.passes,
                        if p.passes == 1 { "pass" } else { "passes" }
                    ));
                }
                if matches!(p.repeat, Repeat::ToEnd) && remaining >= p.remaining {
                    break Some(format!(
                        "Stopped after {} passes · macro made no progress toward EOF",
                        p.passes
                    ));
                }
                p.remaining = remaining;
            }
            if began.elapsed() >= Duration::from_millis(6) {
                break None;
            }
        };
        if let Some(message) = outcome {
            self.finish_macro(p, message);
        } else {
            if p.last_notice.elapsed() > Duration::from_millis(250) {
                self.notice(format!("Replaying · {} passes · Esc to stop", p.passes));
                p.last_notice = Instant::now();
            }
            PLAYBACK.with(|s| s.replace(Some(p)));
            win::post(self.h, TICK, 0, 0);
        }
    }
    fn finish_macro(&mut self, p: Playback, message: String) {
        if WAITING_FIND.replace(false) {
            self.cancel_job("find");
        }
        FOUND.with_borrow_mut(|r| *r = None);
        self.drain_edits();
        if let Some(i) = self.idx(p.id) {
            if p.native_undo && self.buffers[i].native != 0 {
                crate::events::quiet(|| {
                    win::msg(
                        self.bridge.hwnd(),
                        sci::SETDOCPOINTER,
                        0,
                        self.buffers[i].native,
                    );
                    self.bridge.send(sci::ENDUNDOACTION, 0, 0);
                });
            } else if !p.native_undo {
                self.buffers[i].doc.end_undo_group();
            }
        }
        self.playing = false;
        self.notice(message);
    }
    pub(super) fn stop_macro(&mut self) {
        if let Some(p) = PLAYBACK.with(|s| s.take()) {
            let count = p.passes;
            self.finish_macro(p, format!("Macro stopped · {count} complete passes"));
        }
    }
    pub(crate) fn complete_macro_find(&mut self, result: Result<bool, String>) {
        if self.playing && WAITING_FIND.replace(false) {
            FOUND.with_borrow_mut(|r| *r = Some(result));
            win::post(self.h, TICK, 0, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_owns_text_and_checks_count() {
        let mut bytes = b"one\0two".to_vec();
        let value = unsafe { capture(sci::ADDTEXT, bytes.len(), bytes.as_ptr() as isize) }.unwrap();
        bytes.fill(0);
        assert!(matches!(&value,Step::Native{text:Some(s),..}if s.as_bytes()==b"one\0two"));
        assert!(validate(&value).is_ok());
        assert!(
            validate(&Step::Native {
                msg: sci::ADDTEXT,
                arg: 123,
                text: Some("a".into())
            })
            .is_err()
        );
        assert!(
            validate(&Step::Native {
                msg: sci::SETDOCPOINTER,
                arg: 0,
                text: None
            })
            .is_err()
        );
        assert!(validate(&Step::Command("build".into())).is_err());
    }
}
