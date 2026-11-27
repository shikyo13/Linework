//! The editor's one search, on lw's engine: the sidebar's search panel. It
//! searches the current file, the open files or the folder. Its fields become
//! lw's words; the search runs again a moment after they change; the results
//! are lines grouped by file under the panel.
use super::*;
use linework_project::ask;

/// How long typing must pause before the search runs again.
const PAUSE: Duration = Duration::from_millis(250);

/// What the search panel searches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SearchScope {
    /// The current file, with its unsaved text. Next and previous match, and
    /// replacing one match or all of them, work on it directly.
    #[default]
    File,
    /// Every open file, with its unsaved text.
    Open,
    /// The folder, with open files read as they are in their tabs.
    Folder,
}

impl App {
    /// The sidebar shows the search panel: its view is open and holds a search, not other results.
    pub(crate) fn search_panel_shown(&self) -> bool {
        self.prefs.sidebar && self.side == 2 && self.search_view
    }

    /// The scope to open the panel with: the last one, unless it has nothing to search.
    pub(crate) fn usable_scope(&self) -> SearchScope {
        match self.search_scope {
            SearchScope::Folder if self.prefs.root.is_none() => SearchScope::File,
            SearchScope::File | SearchScope::Open if self.buffers.is_empty() && self.prefs.root.is_some() => SearchScope::Folder,
            scope => scope,
        }
    }

    /// Find in folder, from a command or a menu.
    pub(crate) fn open_folder_search(&mut self, replace: bool) {
        self.open_search(SearchScope::Folder, replace);
    }

    /// The search panel over `scope`, with the selection as the text to find
    /// when it is one short line, and the find or replace field focused.
    pub(crate) fn open_search(&mut self, scope: SearchScope, replace: bool) {
        if scope == SearchScope::Folder && self.prefs.root.is_none() {
            self.notice("Open a folder to search its files");
            return;
        }
        if scope != self.search_scope {
            self.search_scope = scope;
            self.search_due = Some(Instant::now());
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
            self.run_search(self.preview_mode);
        }
    }

    /// Empties the results, with `note` in the summary line.
    fn clear_search(&mut self, note: &str) {
        self.cancel_job("folder search");
        self.report = None;
        self.preview_mode = false;
        self.search_summary = note.into();
        self.show_rich_rows(String::new(), Vec::new(), Vec::new(), Vec::new());
    }

    /// Searches the panel's scope as the panel asks; with `preview`, with the replacements too.
    pub(super) fn run_search(&mut self, preview: bool) {
        self.search_due = None;
        if self.panel.text.is_empty() {
            self.clear_search("");
            return;
        }
        let mut words = self.panel.words();
        self.drain_edits();
        let (mut req, buffers) = match self.search_scope {
            SearchScope::Folder => {
                let Some(root) = self.prefs.root.clone() else {
                    self.clear_search("Open a folder to search its files");
                    return;
                };
                // Absolute, so the open tabs inside it are recognised whatever form the folder was given in.
                let root = std::path::absolute(&root).unwrap_or(root);
                let buffers = self.workspace_streams(Some(root.as_path()));
                (linework_project::SearchRequest::new(root, words), buffers)
            }
            scope => {
                let only = match (scope, self.current()) {
                    (SearchScope::File, Some(i)) => Some(self.buffers[i].id),
                    (SearchScope::File, None) => {
                        self.clear_search("Open a file to search it");
                        return;
                    }
                    _ => None,
                };
                if self.buffers.is_empty() {
                    self.clear_search("Open a file to search it");
                    return;
                }
                let buffers = self.streams_where(|b| only.is_none_or(|id| b.id == id), None);
                // The files searched, so the copied lw command finds the same lines.
                words.extend(buffers.iter().filter(|s| s.path.is_absolute()).map(|s| s.path.display().to_string()));
                let mut req = linework_project::SearchRequest::new(self.root_dir(), words);
                req.buffers_only = true;
                (req, buffers)
            }
        };
        if preview {
            req.replacement = Some(text(self.panel_fields[1]));
        }
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
            "p:file" | "p:open" | "p:folder" => {
                let scope = match id {
                    "p:file" => SearchScope::File,
                    "p:open" => SearchScope::Open,
                    _ => SearchScope::Folder,
                };
                if scope == SearchScope::Folder && self.prefs.root.is_none() {
                    self.notice("Open a folder to search its files: File, Open folder");
                    return true;
                }
                self.search_scope = scope;
            }
            "p:run" => {}
            "p:more" => {
                self.panel_more = !self.panel_more;
                self.layout();
                return true;
            }
            "p:preview" => {
                let on = !self.preview_mode;
                self.review_next = on;
                self.run_search(on);
                return true;
            }
            "p:copy" => {
                self.copy_search_command();
                return true;
            }
            "p:sort" => {
                self.choose_result_order();
                return true;
            }
            _ => return false,
        }
        // A switch changed: search again now.
        self.run_search(self.preview_mode);
        self.refresh();
        true
    }

    /// Next or previous match: in the current file, from the caret, when the
    /// panel searches it; otherwise the next or previous match in the results.
    pub(crate) fn search_step(&mut self, back: bool) {
        if self.panel.text.is_empty() {
            self.open_search(self.usable_scope(), false);
            return;
        }
        if self.search_scope == SearchScope::File && !self.preview_mode {
            self.find(back, false);
            return;
        }
        if self.search_due.is_some() {
            self.notice("Searching…");
            return;
        }
        let n = self.report.as_ref().map_or(0, |r| r.hits.len());
        if n == 0 {
            self.notice("No matches");
            return;
        }
        let at = selected(self.list).and_then(|row| self.row_hits.get(row)).filter(|r| !r.is_empty()).cloned();
        let hit = match at {
            Some(r) if back => (r.start + n - 1) % n,
            Some(r) => r.end % n,
            None if back => n - 1,
            None => 0,
        };
        // The line's row, not its file's heading, which holds every match of the file.
        if let Some(row) = self.row_hits.iter().rposition(|r| r.contains(&hit)) {
            msg(self.list, LB_SETCURSEL, row, 0);
        }
        if !self.preview_mode {
            self.project_open_hit(hit);
        }
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
