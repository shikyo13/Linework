//! Searching a folder from the editor, on lw's engine: the sidebar's search
//! panel. Its fields become lw's words; the search runs again a moment after
//! they change; the results are lines grouped by file under the panel.
use super::*;
use linework_project::ask;

/// How long typing must pause before the search runs again.
const PAUSE: Duration = Duration::from_millis(250);

impl App {
    /// The sidebar shows the search panel: its view is open and holds a search, not other results.
    pub(crate) fn search_panel_shown(&self) -> bool {
        self.prefs.sidebar && self.side == 2 && self.search_view
    }

    /// Find in folder, from a command or a menu: the search panel, with the
    /// selection as the text to find when it is one short line.
    pub(crate) fn open_folder_search(&mut self, replace: bool) {
        if self.prefs.root.is_none() {
            self.notice("Open a folder to search its files");
            return;
        }
        let selected = self.short_selection();
        self.prefs.sidebar = true;
        if self.prefs.sidebar_width < frame::PANEL_WIDTH {
            self.prefs.sidebar_width = frame::PANEL_WIDTH;
        }
        if !self.search_panel_shown() {
            self.side = 2;
            if self.report.is_some() {
                self.project_refresh_rows();
            } else {
                self.show_rich_rows(String::new(), Vec::new(), Vec::new(), Vec::new());
            }
        }
        if let Some(selected) = selected {
            set_text(self.panel_fields[0], &selected);
            self.panel.text = selected;
            self.search_due = Some(Instant::now());
        }
        self.layout();
        let field = self.panel_fields[usize::from(replace)];
        focus(field);
        msg(field, 0xb1, 0, -1);
    }

    /// The selected text, when it is one short line.
    fn short_selection(&self) -> Option<String> {
        self.current()?;
        let (a, b) = self.edit().selection();
        if a == b || a.abs_diff(b) > 2048 {
            return None;
        }
        let text = self.edit().selected_text();
        (!text.contains(['\r', '\n', '\0'])).then_some(text)
    }

    /// Text changed in a panel field.
    pub(super) fn search_input(&mut self, id: i32) {
        let Some(&field) = self.panel_fields.get((id - 640) as usize) else { return };
        let value = text(field);
        let p = &mut self.panel;
        match id {
            640 => p.text = value,
            // The replacement matters only to a preview.
            641 if !self.preview_mode => return,
            641 => {}
            642 => p.files = value,
            643 => p.skip = value,
            644 => p.types = value,
            645 => p.near = value,
            646 => p.also = value,
            _ => p.lacks = value,
        }
        self.search_due = Some(Instant::now() + PAUSE);
    }

    /// Runs a search that has waited long enough since the last change.
    pub(super) fn search_tick(&mut self) {
        if self.search_due.is_some_and(|due| Instant::now() >= due) {
            self.search_due = None;
            self.run_folder_search(self.preview_mode);
        }
    }

    /// Searches the folder as the panel asks; with `preview`, with the replacements too.
    pub(super) fn run_folder_search(&mut self, preview: bool) {
        let Some(root) = self.prefs.root.clone() else {
            self.notice("Open a folder to search its files");
            return;
        };
        // Absolute, so the open tabs inside it are recognised whatever form the folder was given in.
        let root = std::path::absolute(&root).unwrap_or(root);
        self.search_due = None;
        if self.panel.text.is_empty() {
            self.cancel_job("folder search");
            self.report = None;
            self.preview_mode = false;
            self.search_summary.clear();
            self.show_rich_rows(String::new(), Vec::new(), Vec::new(), Vec::new());
            return;
        }
        let mut req = linework_project::SearchRequest::new(root, self.panel.words());
        if preview {
            req.replacement = Some(text(self.panel_fields[1]));
        }
        self.drain_edits();
        let buffers = self.workspace_streams(Some(req.root.as_path()));
        self.preview_mode = preview;
        self.checked.clear();
        self.prefs.sidebar = true;
        self.search_summary = "Searching…".into();
        self.show_rich_rows(String::new(), Vec::new(), Vec::new(), Vec::new());
        let (job, cancel) = self.new_job("folder search");
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = linework_project::search_with_streams(&req, buffers, &cancel);
            let _ = tx.send(Worker::Report(job, result));
        });
    }

    /// A command from the panel's switches.
    pub(super) fn search_command(&mut self, id: &str) -> bool {
        let p = &mut self.panel;
        match id {
            "p:case" => p.match_case = !p.match_case,
            "p:word" => p.whole_word = !p.whole_word,
            "p:regex" => p.regex = !p.regex,
            "p:scope" => p.scope = !p.scope,
            "p:hidden" => p.hidden = !p.hidden,
            "p:ignored" => p.ignored = !p.ignored,
            "p:ctx-" => p.context = p.context.saturating_sub(1),
            "p:ctx+" => p.context = (p.context + 1).min(9),
            "p:near-" => p.near_lines = p.near_lines.saturating_sub(1).max(1),
            "p:near+" => p.near_lines = (p.near_lines + 1).min(99),
            "p:more" => {
                self.panel_more = !self.panel_more;
                self.layout();
                return true;
            }
            "p:preview" => {
                let on = !self.preview_mode;
                self.review_next = on;
                self.run_folder_search(on);
                return true;
            }
            "p:copy" => {
                self.copy_search_command();
                return true;
            }
            _ => return false,
        }
        // A switch changed: search again now.
        self.run_folder_search(self.preview_mode);
        self.refresh();
        true
    }

    /// The panel's field after `field`, among those shown, for Tab.
    pub(super) fn next_panel_field(&self, field: H, back: bool) -> Option<H> {
        let shown: Vec<H> = self.panel_fields.iter().copied().enumerate().filter(|(i, _)| *i < 2 || self.panel_more).map(|(_, h)| h).collect();
        let at = shown.iter().position(|&h| h == field)?;
        let n = shown.len();
        Some(shown[if back { (at + n - 1) % n } else { (at + 1) % n }])
    }

    /// Copies the lw command that finds the same lines.
    fn copy_search_command(&mut self) {
        let Some(report) = self.report.as_ref() else {
            self.notice("Search first, then copy its command");
            return;
        };
        let command = format!("lw {}", ask::join(&report.command));
        if copy_text(self.h, &command) {
            self.notice(format!("Copied: {command}"));
        } else {
            self.notice("The clipboard is in use by another program");
        }
    }
}

/// Puts `text` on the clipboard as Unicode text.
fn copy_text(owner: H, text: &str) -> bool {
    use windows_sys::Win32::System::{DataExchange::*, Memory::*};
    let wide = wide(text);
    unsafe {
        if OpenClipboard(hwnd(owner)) == 0 {
            return false;
        }
        EmptyClipboard();
        let mut done = false;
        let memory = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
        if !memory.is_null() {
            let target = GlobalLock(memory) as *mut u16;
            if !target.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), target, wide.len());
                GlobalUnlock(memory);
                // CF_UNICODETEXT; the clipboard owns the memory once it takes it.
                done = !SetClipboardData(13, memory).is_null();
            }
            if !done {
                GlobalFree(memory);
            }
        }
        CloseClipboard();
        done
    }
}
