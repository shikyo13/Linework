use super::*;
use crate::{macros, selection};
use linework_project::Diagnostic;
use windows_sys::Win32::Graphics::Gdi::ScreenToClient;

/// The encoding list from Notepad++ and Sublime Text, filterable by typing.
/// Returns the label `Document` takes. The last entry asks for any label.
pub(super) fn choose_encoding(h: H, title: &str, hex: bool) -> Option<String> {
    let list: Vec<&(&str, &str)> = linework_storage::ENCODINGS
        .iter()
        .filter(|e| hex || e.1 != "Hexadecimal")
        .collect();
    let mut names: Vec<String> = list.iter().map(|e| format!("{}    {}", e.0, e.1)).collect();
    names.push("Other encoding label…".into());
    let k = forms::pick(h, title, &names, "")?;
    match list.get(k) {
        Some(e) => Some(e.1.to_string()),
        None => forms::input(h, "Encoding", "Encoding label, for example cp437 or koi8-r", ""),
    }
}
pub(super) fn pick(h: H, title: &str, options: &[&str]) -> Option<usize> {
    forms::pick(
        h,
        title,
        &options.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "",
    )
}
impl App {
    pub(crate) fn sidebar_width(&self) -> i32 {
        let w = client(self.h).0 * 96 / dpi(self.h);
        self.prefs
            .sidebar_width
            .clamp(150, (w - 220).clamp(150, 900))
    }
    pub(crate) fn shortcut(&self, c: &commands::Command) -> String {
        self.prefs
            .shortcuts
            .get(c.id)
            .cloned()
            .unwrap_or_else(|| c.key.into())
    }
    fn command_rows(&self) -> Vec<String> {
        commands::COMMANDS
            .iter()
            .map(|c| format!("{}\t{}\t{}", c.group, c.name, self.shortcut(c)))
            .collect()
    }
    fn command_picker(&self, title: &str) -> Option<&'static commands::Command> {
        forms::pick(self.h, title, &self.command_rows(), "").map(|i| &commands::COMMANDS[i])
    }
    fn palette(&mut self) {
        let mut choices = self.command_rows();
        let builtins = choices.len();
        let native = crate::npp::rows();
        choices.extend(native.iter().map(|(_, cmd)| format!("Notepad++\t{cmd}\t")));
        let packages = crate::sublime::palette_rows();
        choices.extend(packages.iter().map(|cmd| format!("Sublime\t{cmd}\t")));
        if let Some(i) = forms::pick(self.h, "Command palette", &choices, "") {
            if i < builtins {
                let id = commands::COMMANDS[i].id;
                if id != "palette" {
                    self.command(id);
                }
            } else if let Some((id, _)) = native.get(i - builtins) {
                crate::npp::invoke(self, *id);
            } else {
                self.sublime_palette_run(i - builtins - native.len());
            }
        }
    }
    pub fn key(&mut self, v: u32) -> bool {
        let ctrl = down(VK_CONTROL as _);
        let alt = down(VK_MENU as _);
        let shift = down(VK_SHIFT as _);
        let k = v as u16;
        let f = focused();
        if k == VK_ESCAPE
            && let Some(c) = self
                .current()
                .and_then(|i| self.buffers[i].preparing.as_ref())
        {
            c.store(true, Ordering::Relaxed);
            return true;
        }
        if k == VK_ESCAPE && self.cancel_sublime() {
            return true;
        }
        let field = f == self.searchbox || f == self.replacebox;
        if !ctrl && !alt && frame::controls::key(k, shift, self.edit().hwnd()) {
            return true;
        }
        if f == self.edit().hwnd() && self.global_key(k, ctrl, shift, alt) {
            return true;
        }
        if self.playing {
            if k == VK_ESCAPE {
                self.stop_macro()
            }
            return true;
        }
        if self.panel_fields.contains(&f) {
            match k {
                VK_RETURN => {
                    // Enter in the replace field previews, and opens the review; anywhere else it searches now.
                    let preview = f == self.panel_fields[1] || self.preview_mode;
                    self.review_next = preview;
                    self.run_folder_search(preview);
                    return true;
                }
                VK_ESCAPE => {
                    if self.current().is_some() {
                        focus(self.edit().hwnd());
                    }
                    return true;
                }
                VK_TAB => {
                    if let Some(next) = self.next_panel_field(f, shift) {
                        focus(next);
                        msg(next, 0xb1, 0, -1);
                    }
                    return true;
                }
                VK_DOWN => {
                    if !self.rows.is_empty() {
                        focus(self.list);
                        if selected(self.list).is_none() {
                            msg(self.list, LB_SETCURSEL, 0, 0);
                        }
                    }
                    return true;
                }
                _ => {}
            }
        }
        if field {
            if k == VK_ESCAPE {
                self.hide_find();
                return true;
            }
            if k == VK_RETURN {
                self.find(shift, f == self.replacebox);
                return true;
            }
            if k == VK_TAB && self.repbar {
                focus(if f == self.replacebox {
                    self.searchbox
                } else {
                    self.replacebox
                });
                return true;
            }
        }
        if f == self.list {
            match k {
                VK_RETURN => {
                    self.open_row();
                    return true;
                }
                VK_SPACE if self.side == 2 => {
                    self.command("togglehit");
                    return true;
                }
                VK_ESCAPE => {
                    if self.current().is_some() {
                        focus(self.edit().hwnd());
                    }
                    return true;
                }
                _ => {}
            }
        }
        if self.current().is_some()
            && f == self.edit().hwnd()
            && self.edit().send(AUTOCACTIVE, 0, 0) != 0
            && matches!(k, VK_DOWN | VK_UP | VK_RETURN | VK_TAB | VK_ESCAPE)
        {
            return false;
        }
        if self.findbar && k == VK_ESCAPE {
            self.hide_find();
            return true;
        }
        if !ctrl
            && !alt
            && k == VK_TAB
            && !shift
            && self.current().is_some()
            && f == self.edit().hwnd()
            && self.edit().snippet_next()
        {
            return true;
        }
        if !field && self.smart_key(k, ctrl, shift, alt) {
            return true;
        }
        if !field
            && ctrl
            && shift
            && !alt
            && matches!(k, VK_HOME | VK_END)
            && self.current().is_some_and(|i| self.buffers[i].large)
        {
            if self.recording {
                self.macro_steps.push(Step::Native {
                    msg: if k == VK_END {
                        DOCUMENTENDEXTEND
                    } else {
                        DOCUMENTSTARTEXTEND
                    },
                    arg: 0,
                    text: None,
                });
            }
            let at = self.global_pos();
            let dest = if k == VK_END { self.total_len() } else { 0 };
            self.select_span(at.min(dest)..at.max(dest), k == VK_END);
            return true;
        }
        if !field
            && self.current().is_some_and(|i| self.buffers[i].large)
            && ctrl
            && !shift
            && !alt
            && matches!(k, VK_HOME | VK_END)
        {
            if self.recording {
                self.macro_steps.push(Step::Native {
                    msg: if k == VK_END {
                        DOCUMENTEND
                    } else {
                        DOCUMENTSTART
                    },
                    arg: 0,
                    text: None,
                });
            }
            self.go(if k == VK_END { self.total_len() } else { 0 }, 0);
            return true;
        }
        if alt
            && !ctrl
            && !shift
            && matches!(k, 0x46 | 0x45 | 0x53 | 0x56 | 0x47 | 0x4d | 0x54 | 0x48)
        {
            let group = match k {
                0x46 => "File",
                0x45 => "Edit",
                0x53 => "Search",
                0x56 => "View",
                0x47 => "Go",
                0x4d => "Macro",
                0x54 => "Tools",
                _ => "Help",
            };
            events::push(Event::Hit(
                Hit::Menu(group.into()),
                size(self.h, 130),
                size(self.h, 40),
            ));
            return true;
        }
        // The editor pastes on Shift+Insert by itself, without saying that
        // it was a paste. Sending it through the command marks it as one.
        if k == VK_INSERT && shift && !ctrl && !alt && !field && self.current().is_some() && f == self.edit().hwnd() {
            self.command("paste");
            return true;
        }
        let mut chord = commands::chord(v, ctrl, shift, alt);
        if chord.is_empty() {
            return false;
        }
        chord.make_ascii_lowercase();
        if let Some(&c) = self.shortcuts.get(&chord) {
            if field
                && matches!(
                    c.id,
                    "undo" | "redo" | "cut" | "copy" | "paste" | "selall" | "caretup" | "caretdown"
                )
            {
                return false;
            }
            self.command(c.id);
            return true;
        }
        if ctrl || alt {
            return self.project_tool_by_shortcut(&chord);
        }
        false
    }
    pub fn command(&mut self, id: &str) {
        if self.recording && !self.playing && macros::recordable_command(id) {
            self.macro_steps.push(Step::Command(id.into()));
            events::SKIP_MACRO.set(true);
            self.execute(id);
            events::SKIP_MACRO.set(false);
        } else {
            self.execute(id)
        }
        self.refresh();
    }
    fn execute(&mut self, id: &str) {
        if id == "nppinstall" || id == "nppcommands" {
            self.npp_manage(id);
            return;
        }
        if self.sublime_command(id) {
            return;
        }
        if self.global_command(id) {
            return;
        }
        if self.wide_span().is_some()
            && matches!(
                id,
                "comment"
                    | "indent"
                    | "outdent"
                    | "upper"
                    | "lower"
                    | "dup"
                    | "moveup"
                    | "movedown"
                    | "columnfill"
                    | "sellines"
                    | "snippets"
                    | "rectangle"
                    | "selnext"
                    | "selmatches"
                    | "selskipped"
            )
        {
            self.notice("This command needs a selection within the loaded page");
            return;
        }
        match id {
            "docpreview" => {
                if self.preview_kind().is_some() {
                    self.prefs.document_preview = !self.prefs.document_preview;
                    self.save_prefs();
                    self.layout();
                    self.update_preview();
                } else {
                    self.notice("Preview is available for Markdown, HTML, SVG, CSV and TSV");
                }
            }
            "previewcue" | "previewfollow" | "previewshow" | "previewsync" | "previewsource" => {
                self.preview_command(id)
            }
            "editoreffects" | "paragraphfocus" | "mdbold" | "mditalic" => self.polish_command(id),
            "encodingdetect" | "encodingfallback" | "encodingnew" => self.encoding_settings(id),
            "minimap" => {
                self.prefs.document_map = !self.prefs.document_map;
                self.save_prefs();
                self.layout();
            }
            "langrules" => self.language_rules(),
            "importgrammar" => self.import_grammar(),
            "new" => self.new_file(),
            "open" | "openother" => {
                let pane = if id == "openother" {
                    1 - self.active
                } else {
                    self.active
                };
                if let Some(paths) = crate::dialogs::files(self.h)
                    .set_directory(self.root_dir())
                    .pick_files()
                {
                    for p in paths {
                        self.open_in(p, pane, None)
                    }
                }
            }
            "folder" => {
                if let Some(p) = crate::dialogs::files(self.h)
                    .set_directory(self.root_dir())
                    .pick_folder()
                {
                    self.set_root(p)
                }
            }
            "reopen" => {
                if let Some(p) = self.closed.pop() {
                    self.open_path(p)
                } else {
                    self.notice("No recently closed files")
                }
            }
            "saveall" => {
                for i in 0..self.buffers.len() {
                    if self.buffers[i].dirty {
                        self.save(i, false, false)
                    }
                }
            }
            "exit" => self.quit(),
            "quick" => self.quick_open(),
            "opentabs" => {
                let names = self
                    .buffers
                    .iter()
                    .map(|b| {
                        b.path
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| b.name.clone())
                    })
                    .collect::<Vec<_>>();
                if let Some(i) = forms::pick(self.h, "Open editors", &names, "") {
                    self.select(i, self.active, None);
                }
            }
            "palette" => self.palette(),
            "help" => {
                if let Some(c) = self.command_picker("Keyboard shortcuts") {
                    if c.id != "palette" && c.id != "help" {
                        self.command(c.id)
                    }
                }
            }
            "bindings" => self.bind_shortcut(),
            "sidebar" => {
                self.prefs.sidebar = !self.prefs.sidebar;
                self.layout();
                self.save_prefs()
            }
            "theme" => {
                if let Some(i) = pick(self.h, "Color theme", &["Graphite dark", "Paper light"]) {
                    self.prefs.light = i == 1;
                    LIGHT.set(i == 1);
                    dark_chrome(self.h, i == 0);
                    for h in [
                        self.list,
                        self.searchbox,
                        self.replacebox,
                        self.editors[0].hwnd(),
                        self.editors[1].hwnd(),
                        self.scroll[0],
                        self.scroll[1],
                    ]
                    .into_iter()
                    .chain(self.panel_fields)
                    {
                        crate::chrome::theme_child(h, self.prefs.light);
                    }
                    for p in 0..2 {
                        self.style(p)
                    }
                    self.apply_appearance();
                    self.layout();
                    self.save_prefs();
                    unsafe {
                        RedrawWindow(
                            hwnd(self.h),
                            null(),
                            null_mut(),
                            RDW_INVALIDATE | RDW_ALLCHILDREN,
                        );
                    }
                }
            }
            "findopen" | "replaceopen" => self.project_search_open(id == "replaceopen"),
            "comparesaved" | "comparefile" => self.compare_current(id == "comparesaved"),
            "findproject" | "replaceproject" => self.project_search(id == "replaceproject"),
            "applypreview" => self.project_apply(),
            "undofolder" => self.project_undo(),
            "toggleall" => self.project_toggle_all(),
            "togglehit" => {
                if let Some(row) = selected(self.list) {
                    self.project_toggle_row(row)
                }
            }
            "previewhit" => {
                if let Some(Target::Hit(j)) = selected(self.list)
                    .and_then(|i| self.rows.get(i))
                    .map(|r| r.target.clone())
                {
                    self.project_preview_hit(j)
                }
            }
            other if self.search_command(other) => {}
            "build" | "buildconfig" => self.project_build(id == "buildconfig"),
            "tools" | "toolconfig" => self.project_tool(id == "toolconfig"),
            "cancel" => {
                if let Some((_, flag)) = self.jobs.values().max_by_key(|(id, _)| id) {
                    flag.store(true, Ordering::Relaxed);
                }
                if self.playing {
                    self.stop_macro()
                }
                self.notice("Cancel requested")
            }
            "typing" => self.typing(),
            "dictionary" => self.dictionary(),
            "tokencolors" | "highlights" | "markcolor" | "nextcolor" | "clearmarks" => {
                self.appearance_command(id)
            }
            "findselection" => self.show_selection_find(),
            "findoptions" => self.find_options(),
            "wsymbols" => self.project_symbols(true),
            _ => {
                let Some(i) = self.current() else {
                    self.notice("Open a file or create a new document first");
                    return;
                };
                match id {
                    "save" | "saveas" => self.save(i, id == "saveas", false),
                    "close" => self.close_tab(i),
                    "nexttab" | "prevtab" => {
                        let n = self.buffers.len();
                        self.select(
                            if id == "nexttab" {
                                (i + 1) % n
                            } else {
                                (i + n - 1) % n
                            },
                            self.active,
                            None,
                        );
                    }
                    "single" => {
                        let id = self.assigned[self.active];
                        let pos = self.global_pos();
                        if self.recording {
                            self.macro_command("record")
                        }
                        self.split = false;
                        self.assigned[1] = None;
                        self.active = 0;
                        if let Some(i) = id.and_then(|v| self.idx(v)) {
                            self.select(i, 0, Some(pos));
                        }
                        self.layout();
                    }
                    "syncscroll" => {
                        self.prefs.sync_scroll = !self.prefs.sync_scroll;
                        self.save_prefs();
                        self.notice(if self.prefs.sync_scroll {
                            "Linked scrolling on"
                        } else {
                            "Linked scrolling off"
                        });
                    }
                    "split" | "splitbelow" | "other" => {
                        if id != "other" {
                            self.prefs.stacked = id == "splitbelow";
                            self.save_prefs();
                        }
                        let p = 1 - self.active;
                        let at = self.global_pos();
                        if self.split && self.assigned[p].is_some() {
                            self.active = p;
                            focus(self.editors[p].hwnd());
                        } else {
                            self.select(i, p, Some(at));
                        }
                        self.layout();
                    }
                    "find" | "replace" => self.show_find(id == "replace"),
                    "hidefind" => self.hide_find(),
                    "scope" => self.choose_scope(),
                    "case" | "regex" | "word" => {
                        match id {
                            "case" => self.case_ = !self.case_,
                            "regex" => self.regex = !self.regex,
                            _ => self.whole = !self.whole,
                        }
                        self.last_find = None;
                        self.layout();
                    }
                    "findnext" | "findprev" | "replaceone" => {
                        if self.folder_scope && self.findbar {
                            self.run_folder_search(id == "replaceone")
                        } else {
                            self.find(id == "findprev", id == "replaceone")
                        }
                    }
                    "replaceall" => {
                        if self.folder_scope {
                            self.run_folder_search(true)
                        } else {
                            self.replace_all()
                        }
                    }
                    "managemacros" => self.manage_macros(),
                    "record" | "play" | "repeat" | "savemacro" | "loadmacro" => {
                        self.macro_command(id)
                    }
                    "symbols" => self.project_symbols(false),
                    "def" | "refs" => {
                        self.remember_location();
                        self.project_goto(id == "refs");
                    }
                    "goto" => self.goto_prompt(),
                    "back" | "forward" => self.history_move(id == "back"),
                    "nextdiag" => self.next_diagnostic(),
                    "bookmark" | "nextmark" | "marks" => self.bookmark(i, id),
                    "undo" | "redo" => {
                        let redo = id == "redo";
                        let pane = self.active;
                        if self.buffers[i].saving {
                            self.buffers[i].after_save.push_back(AfterSave::History { pane, redo });
                            self.notice(if redo { "Redo queued after save" } else { "Undo queued after save" });
                        } else if self.writable(i) {
                            self.apply_history(i, pane, redo);
                        }
                    }
                    "cut" | "copy" | "paste" | "selall" => {
                        if id == "selall" && self.buffers[i].large {
                            self.select_span(0..self.total_len(), false);
                            return;
                        }
                        if matches!(id, "copy" | "cut") {
                            if let Some(span) = self.wide_span() {
                                self.copy_span(span, id == "cut");
                                return;
                            }
                        }
                        // The changes a paste makes are told apart from
                        // typing by this, until they have all been handled.
                        self.pasting = id == "paste";
                        self.edit().send(
                            match id {
                                "cut" => CUT,
                                "copy" => COPY,
                                "paste" => PASTE,
                                _ => SELECTALL,
                            },
                            0,
                            0,
                        );
                    }
                    "selnext" | "selmatches" | "selskipped" | "undoselect" | "sellines"
                    | "caretup" | "caretdown" | "rectangle" | "upper" | "lower" | "dup"
                    | "moveup" | "movedown" => {
                        if let Err(e) = selection::apply(self.edit(), id) {
                            self.notice(e)
                        }
                    }
                    "columnfill" => {
                        if let Some((v, _)) = forms::form(
                            self.h,
                            "Fill column or selections",
                            &[
                                forms::Field {
                                    label: "First number",
                                    value: "1".into(),
                                },
                                forms::Field {
                                    label: "Increment",
                                    value: "1".into(),
                                },
                            ],
                            &[],
                            "Fill",
                        ) {
                            if let (Ok(start), Ok(step)) =
                                (v[0].trim().parse(), v[1].trim().parse())
                            {
                                if self.global_number(start, step) {
                                    return;
                                }
                                if let Err(e) = selection::number(self.edit(), start, step) {
                                    self.notice(e)
                                }
                            } else {
                                self.notice("Enter two whole numbers")
                            }
                        }
                    }
                    "comment" | "indent" | "outdent" | "sortlines" | "sortdesc" | "dedup"
                    | "trimlines" => self.line_action(id),
                    "managesnippets" => self.manage_snippets(),
                    "complete" => {
                        if !self.edit().complete() {
                            self.notice("No completions at the cursor")
                        }
                    }
                    "snippets" => {
                        let labels = self.edit().snippet_labels();
                        if let Some(n) = forms::pick(self.h, "Insert snippet", &labels, "") {
                            self.edit().insert_snippet(&labels[n]);
                        }
                    }
                    "spell" => {
                        let enabled = !self.prefs.spell;
                        for e in &self.editors {
                            if let Err(err) = e.set_spellcheck(enabled) {
                                self.notice(err);
                                return;
                            }
                        }
                        self.prefs.spell = enabled;
                        self.save_prefs();
                        self.notice(if enabled {
                            "Spell check on"
                        } else {
                            "Spell check off"
                        });
                    }
                    "correct" => self.correct(),
                    "cursor" => self.cursor_options(),
                    "language" => self.choose_language(i),
                    "wrap" | "whitespace" | "markmatches" | "currentline" | "zoom+" | "zoom-"
                    | "zoom0" => {
                        match id {
                            "wrap" => self.prefs.wrap = !self.prefs.wrap,
                            "whitespace" => self.prefs.whitespace = !self.prefs.whitespace,
                            "markmatches" => self.prefs.highlight = !self.prefs.highlight,
                            "currentline" => self.prefs.current_line = !self.prefs.current_line,
                            "zoom+" => self.prefs.zoom = (self.prefs.zoom + 1).min(32),
                            "zoom-" => self.prefs.zoom = (self.prefs.zoom - 1).max(-7),
                            _ => self.prefs.zoom = 0,
                        }
                        for p in 0..2 {
                            self.style(p)
                        }
                        self.apply_appearance();
                        self.save_prefs();
                    }
                    "fold" => {
                        let e = self.edit();
                        let l = e.send(LINEFROMPOSITION, e.caret(), 0).max(0) as usize;
                        let parent = e.send(GETFOLDPARENT, l, 0);
                        let line = if e.send(GETFOLDLEVEL, l, 0) & 0x2000 != 0 {
                            l
                        } else {
                            parent.max(0) as usize
                        };
                        e.send(TOGGLEFOLD, line, 0);
                    }
                    "foldall" | "unfold" => {
                        self.edit().send(FOLDALL, (id == "unfold") as usize, 0);
                    }
                    "encoding" | "recode" | "eol" => self.convert(i, id),
                    "reload" => {
                        if !self.buffers[i].saving
                            && self.buffers[i].path.is_some()
                            && (!self.buffers[i].dirty
                                || confirm(
                                    self.h,
                                    "Reload file",
                                    "Discard unsaved changes and reload from disk?",
                                    false,
                                ) == IDYES)
                        {
                            self.reload_disk(i);
                        }
                    }
                    "follow" => {
                        if self.buffers[i].dirty || self.buffers[i].path.is_none() {
                            self.notice("Save the file before following its changes");
                            return;
                        }
                        self.buffers[i].follow = !self.buffers[i].follow;
                        self.reload_views(i);
                        if self.buffers[i].follow {
                            self.go(self.total_len(), 0);
                            self.notice("Following new text")
                        } else {
                            self.notice("Stopped following")
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    pub(super) fn apply_history(&mut self, i: usize, pane: usize, redo: bool) {
        let b = &mut self.buffers[i];
        if b.large {
            let target = b.doc.history_position(redo).unwrap_or(b.last);
            let changed = if redo { b.doc.redo() } else { b.doc.undo() };
            if changed {
                b.rev += 1;
                b.dirty = b.doc.dirty();
                b.last = target;
                self.reload_views(i);
                if self.assigned[pane] == Some(self.buffers[i].id) {
                    self.bind(pane, i, target);
                }
                self.note_paged_history(i, target, redo);
                self.track_edit();
            }
            return;
        }
        let command = if redo { REDO } else { UNDO };
        if let Some(p) = [pane, 1 - pane].into_iter().find(|&p| self.assigned[p] == Some(b.id)) {
            self.editors[p].send(command, 0, 0);
        } else {
            quiet(|| msg(self.bridge.hwnd(), SETDOCPOINTER, 0, b.native));
            self.bridge.send(SETREADONLY, 0, 0);
            if self.bridge.send(if redo { CANREDO } else { CANUNDO }, 0, 0) != 0 {
                let changes = events::bridge_history(|| { self.bridge.send(command, 0, 0); });
                for (pos, del, add) in changes {
                    b.move_marks(pos, del, add);
                }
                b.rev += 1;
                b.dirty = b.forced_dirty || self.bridge.send(GETMODIFY, 0, 0) != 0;
                self.track_edit();
            }
        }
    }
    pub(super) fn writable(&mut self, i: usize) -> bool {
        if self.buffers[i].saving || self.buffers[i].follow {
            self.notice("This document is read-only while saving or following");
            false
        } else {
            self.prepare_input(i)
        }
    }
    pub(crate) fn remember_location(&mut self) {
        if let Some(id) = self.assigned[self.active] {
            let at = (id, self.global_pos());
            if self.history.last() != Some(&at) {
                self.history.push(at);
            }
            self.future.clear();
        }
    }
    fn history_move(&mut self, back: bool) {
        let from = self.assigned[self.active].map(|id| (id, self.global_pos()));
        loop {
            let item = if back {
                self.history.pop()
            } else {
                self.future.pop()
            };
            let Some((id, at)) = item else {
                self.notice(if back { "No earlier location" } else { "No later location" });
                return;
            };
            if let Some(i) = self.idx(id) {
                if let Some(loc) = from {
                    if back {
                        self.future.push(loc)
                    } else {
                        self.history.push(loc)
                    }
                }
                self.select(i, self.active, Some(at));
                break;
            }
        }
    }
    fn goto_prompt(&mut self) {
        if let Some(s) = forms::input(
            self.h,
            "Go to",
            "Line[:column], byte offset (@12345), or percentage (75%)",
            "",
        ) {
            let s = s.trim();
            self.remember_location();
            if let Some(p) = s.strip_suffix('%').and_then(|v| v.parse::<f64>().ok()) {
                self.go(
                    (self.total_len() as f64 * p.clamp(0., 100.) / 100.) as u64,
                    0,
                )
            } else if let Some(b) = s.strip_prefix('@').and_then(|v| v.parse().ok()) {
                self.go(b, 0)
            } else {
                let (l, c) = s.split_once(':').unwrap_or((s, "1"));
                if let (Ok(line), Ok(col)) = (l.parse(), c.parse()) {
                    self.goto_line_col(line, col)
                } else {
                    self.notice("Enter a line, byte offset or percentage")
                }
            }
        }
    }
    pub(super) fn goto_line_col(&mut self, line: u64, col: u64) {
        let Some(i) = self.current() else { return };
        let l = line.saturating_sub(1);
        let n = col.saturating_sub(1);
        let serial = self.column_request.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        self.indexed_jump.remove(&self.buffers[i].id);
        if self.buffers[i].large {
            let d = &self.buffers[i].doc;
            if !d.indexing_complete() && line >= d.line_count() {
                // Lines of one length let the place be worked out before the
                // count reaches it; the count confirms or corrects it.
                let tabs = self.edit().send(GETTABWIDTH, 0, 0).max(1) as u64;
                let guess = (n <= 4096)
                    .then(|| d.estimate_line_start(l))
                    .flatten()
                    .and_then(|pos| crate::launch::column_offset(d, pos, n, tabs, || false).ok());
                self.indexed_jump.insert(self.buffers[i].id, (line, col, guess));
                if let Some(at) = guess {
                    self.go(at, 0);
                }
                self.notice(format!("Locating line {line}…"));
                return;
            }
            let pos = d.line_start(l.min(d.line_count().saturating_sub(1)));
            let tabs = self.edit().send(GETTABWIDTH, 0, 0).max(1) as u64;
            if n <= 4096 {
                match crate::launch::column_offset(d,pos,n,tabs,|| false) {
                    Ok(at) => self.go(at, 0),
                    Err(e) => self.notice(e.to_string()),
                }
            } else {
                let doc = d.clone();
                let id = self.buffers[i].id;
                let rev = self.buffers[i].rev;
                let caret = self.global_pos();
                let requests = Arc::clone(&self.column_request);
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let result = crate::launch::column_offset(&doc,pos,n,tabs,
                        || requests.load(Ordering::Relaxed) != serial).map_err(|e| e.to_string());
                    let _ = tx.send(Worker::LongColumn(serial,id,rev,caret,result));
                });
            }
            return;
        }
        let e = self.edit();
        let local = (l as usize).min(e.send(GETLINECOUNT, 0, 0).saturating_sub(1) as usize);
        let pos = e
            .send(FINDCOLUMN, local, n.min(isize::MAX as u64) as isize)
            .max(0) as u64;
        self.go(events::point(self.active, pos), 0);
    }
    fn bookmark(&mut self, i: usize, id: &str) {
        let e = self.edit();
        let local = e.send(LINEFROMPOSITION, e.caret(), 0).max(0) as usize;
        let pos = events::point(
            self.active,
            e.send(POSITIONFROMLINE, local, 0).max(0) as u64,
        );
        let end = events::end_point(
            self.active,
            e.send(GETLINEENDPOSITION, local, 0).max(0) as u64,
        );
        if id == "bookmark" {
            let b = &mut self.buffers[i];
            if b.marks.range(pos..=end.max(pos)).next().is_some() {
                b.marks.retain(|&m| m < pos || m > end);
            } else {
                b.marks.insert(pos);
            }
            self.paint_bookmarks(i, self.active);
            self.saved_state = false;
            return;
        }
        let marks = self.buffers[i].marks.iter().copied().collect::<Vec<_>>();
        if marks.is_empty() {
            self.notice("No bookmarks in this file");
            return;
        }
        let at = if id == "nextmark" {
            match marks.iter().copied().find(|b| *b > pos) {
                Some(at) => at,
                None => {
                    // Past the last bookmark the search starts again at the top.
                    self.notice(if marks == [pos] {
                        "The only bookmark"
                    } else {
                        "Wrapped to the first bookmark"
                    });
                    marks[0]
                }
            }
        } else {
            if let Err(e) = self.sync_buffer(i) {
                self.notice(e);
                return;
            }
            let opts = marks
                .iter()
                .map(|at| {
                    format!(
                        "Line {}    {}",
                        self.buffers[i].doc.line_of(*at) + 1,
                        String::from_utf8_lossy(
                            &self.buffers[i].doc.read(*at, 100).unwrap_or_default()
                        )
                        .lines()
                        .next()
                        .unwrap_or("")
                    )
                })
                .collect::<Vec<_>>();
            let Some(j) = forms::pick(self.h, "Bookmarks", &opts, "") else {
                return;
            };
            marks[j]
        };
        self.remember_location();
        self.go(at, 0);
    }
    pub(super) fn paint_bookmarks(&self, i: usize, p: usize) {
        let e = &self.editors[p];
        let base = events::binding(p).base;
        e.send(MARKERDELETEALL, 0, 0);
        for &at in self.buffers[i]
            .marks
            .range(base..=events::point(p, e.len() as u64))
        {
            if events::point(p, events::offset(p, at)) != at {
                continue;
            }
            let line = e.send(LINEFROMPOSITION, events::offset(p, at) as usize, 0);
            e.send(MARKERADD, line as usize, 0);
        }
    }
    fn quick_open(&mut self) {
        let mut paths = self.index.all_files.clone();
        paths.extend(self.prefs.recent.iter().cloned());
        paths.extend(self.buffers.iter().filter_map(|b| b.path.clone()));
        let mut seen = HashSet::new();
        paths.retain(|p| seen.insert(state::key(p)));
        if paths.is_empty() {
            self.command("open");
            return;
        }
        let root = self.root_dir();
        let names = paths
            .iter()
            .map(|p| p.strip_prefix(&root).unwrap_or(p).display().to_string())
            .collect::<Vec<_>>();
        if let Some(i) = forms::pick(self.h, "Quick open", &names, "") {
            self.remember_location();
            self.open_path(paths[i].clone());
        }
    }
    fn next_diagnostic(&mut self) {
        let found: Vec<(usize, Diagnostic)> =
            self.diagnostics.iter().cloned().enumerate().collect();
        if found.is_empty() {
            self.notice("No diagnostics");
            return;
        }
        let current = selected(self.list);
        let (n, d) = found
            .iter()
            .find(|(i, _)| current.is_none_or(|c| *i > c))
            .unwrap_or(&found[0]);
        self.side = 2;
        msg(self.list, LB_SETCURSEL, *n, 0);
        let p = d.path.clone();
        let l = d.line;
        let c = d.column.max(1);
        self.remember_location();
        self.open_line(p, l, c);
    }
    fn choose_language(&mut self, i: usize) {
        let mut names = linework_editor::languages();
        names.insert(0, "Automatic detection".into());
        if let Some(n) = forms::pick(self.h, "Syntax language", &names, "") {
            let lang = if n == 0 {
                let s = self.edit().text_range(0, self.edit().len().min(8192));
                language_for(&self.prefs, self.buffers[i].path.as_deref(), &s)
            } else {
                names[n].to_string()
            };
            self.buffers[i].manual = n != 0;
            self.buffers[i].lang = lang.clone();
            for p in 0..2 {
                if self.assigned[p] == Some(self.buffers[i].id) {
                    self.editors[p].set_language(&lang);
                    self.indentation(p, &lang);
                }
            }
            self.saved_state = false;
            self.notice(format!("Syntax · {lang}"));
        }
    }
    fn cursor_options(&mut self) {
        let Some(n) = pick(self.h, "Cursor shape", &["Line", "Wide", "Block"]) else {
            return;
        };
        let Some((v, _)) = forms::form(
            self.h,
            "Cursor appearance",
            &[
                forms::Field {
                    label: "Width in pixels (1–8)",
                    value: self.prefs.width.to_string(),
                },
                forms::Field {
                    label: "Blink interval in milliseconds (0 = steady)",
                    value: self.prefs.blink.to_string(),
                },
            ],
            &[],
            "Apply",
        ) else {
            return;
        };
        if let (Ok(w), Ok(b)) = (v[0].parse::<usize>(), v[1].parse::<u32>()) {
            self.prefs.caret = ["Line", "Wide", "Block"][n].into();
            self.prefs.width = w.clamp(1, 8);
            self.prefs.blink = b.min(2000);
            for p in 0..2 {
                self.style(p)
            }
            self.apply_appearance();
            self.save_prefs();
        } else {
            self.notice("Enter numeric width and blink interval")
        }
    }
    fn typing(&mut self) {
        if let Some((v, c)) = forms::form(
            self.h,
            "Typing preferences",
            &[
                forms::Field {
                    label: "Suggest after this many characters (1–12)",
                    value: self.prefs.completion_threshold.to_string(),
                },
                forms::Field {
                    label: "Tab and indentation width (1–16)",
                    value: self.prefs.tab_width.to_string(),
                },
            ],
            &[
                ("Suggest completions while typing", self.prefs.auto_complete),
                ("Indent new lines automatically", self.prefs.auto_indent),
                (
                    "Choose tabs or spaces by language (Makefiles and Go use tabs)",
                    self.prefs.use_tabs.is_none(),
                ),
                (
                    "Insert tab characters when language defaults are off",
                    self.prefs.use_tabs == Some(true),
                ),
            ],
            "Apply",
        ) {
            let Ok(n) = v[0].trim().parse::<usize>() else {
                self.notice("Enter a completion length from 1 to 12");
                return;
            };
            let Some(width) = v[1]
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|w| (1..=16).contains(w))
            else {
                self.notice("Enter a tab width between 1 and 16");
                return;
            };
            self.prefs.tab_width = width;
            self.prefs.use_tabs = (!c[2]).then_some(c[3]);
            self.prefs.completion_threshold = n.clamp(1, 12);
            self.prefs.auto_complete = c[0];
            self.prefs.auto_indent = c[1];
            for p in 0..2 {
                self.style(p)
            }
            self.apply_appearance();
            self.save_prefs();
        }
    }
    fn dictionary(&mut self) {
        let Some(n) = pick(
            self.h,
            "Spelling dictionary",
            &[
                "Built-in English",
                "Import Hunspell dictionary (.dic and .aff)",
            ],
        ) else {
            return;
        };
        let path = if n == 0 {
            None
        } else {
            let Some(p) = crate::dialogs::files(self.h)
                .add_filter("Hunspell dictionaries", &["dic"])
                .pick_file()
            else {
                return;
            };
            Some(p)
        };
        for e in &self.editors {
            if let Err(e) = e.set_dictionary(path.as_deref()) {
                self.notice(e);
                return;
            }
        }
        self.prefs.dictionary = path;
        self.save_prefs();
        self.notice("Spelling dictionary updated");
    }
    fn correct(&mut self) {
        if !self.prefs.spell {
            self.notice("Turn on spell check with F6");
            return;
        }
        self.edit().refresh_spelling();
        let mut choices = self.edit().spelling_suggestions();
        let n = choices.len();
        choices.push("Learn this word".into());
        choices.push("Ignore this word for this session".into());
        if let Some(i) = forms::pick(self.h, "Spelling suggestions", &choices, "") {
            let ok = if i < n {
                self.edit().replace_spelling(&choices[i])
            } else if i == n {
                self.edit().learn_word()
            } else {
                self.edit().ignore_word()
            };
            if !ok {
                self.notice("Place the cursor in an underlined word");
            }
        }
    }
    fn bind_shortcut(&mut self) {
        let Some(c) = self.command_picker("Choose command to rebind") else {
            return;
        };
        if let Some(s) = forms::input(
            self.h,
            "Keyboard shortcut",
            "Shortcut, e.g. Ctrl+Alt+J (empty disables this binding)",
            &self.shortcut(c),
        ) {
            let Some(binding) = commands::normalize_chord(&s) else {
                self.notice("Use a supported key and optional Ctrl, Alt or Shift modifiers");
                return;
            };
            let chord = binding.as_str();
            if !chord.is_empty() {
                if let Some(other) = commands::COMMANDS
                    .iter()
                    .find(|x| x.id != c.id && self.shortcut(x).eq_ignore_ascii_case(chord))
                {
                    if confirm(
                        self.h,
                        "Reassign shortcut",
                        &format!("Move {chord} from {} to {}?", other.name, c.name),
                        false,
                    ) != IDYES
                    {
                        return;
                    }
                    self.prefs.shortcuts.insert(other.id.into(), String::new());
                }
            }
            self.prefs.shortcuts.insert(c.id.into(), chord.into());
            self.shortcuts = commands::shortcut_index(&self.prefs.shortcuts);
            self.save_prefs();
            self.notice("Shortcut updated");
        }
    }
    fn convert(&mut self, i: usize, id: &str) {
        if !self.writable(i) {
            return;
        }
        if id == "eol" {
            let Some(k) = pick(
                self.h,
                "Convert line endings",
                &["Windows (CRLF)", "Unix (LF)", "Classic Mac (CR)"],
            ) else {
                return;
            };
            let style = ["\r\n", "\n", "\r"][k];
            if self.buffers[i].large {
                let (job, cancel) = self.new_job("line endings");
                let b = &self.buffers[i];
                let (id, rev) = (b.id, b.rev);
                let mut layout = BulkView::capture(self, i);
                let mut d = b.doc.clone();
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let r = d
                        .convert_eol_observed(style, &cancel, |r, a| layout.edit(r, a))
                        .map(|_| (d, layout))
                        .map_err(|e| e.to_string());
                    let _ = tx.send(Worker::Eol(job, id, rev, style, r));
                });
                self.notice("Converting line endings…");
            } else {
                let native = [0, 2, 1][k];
                self.edit().send(CONVERTEOLS, native, 0);
                self.edit().send(SETEOLMODE, native, 0);
                self.buffers[i].doc.set_newline(style);
            }
            return;
        }
        let now = self.buffers[i].doc.encoding_name();
        let title = if id == "encoding" {
            format!("Save with encoding (now {now})")
        } else {
            format!("Reopen with encoding (now {now})")
        };
        // Saving as hexadecimal would read the text as hex digits, so it is
        // only offered for reopening.
        let Some(enc) = choose_encoding(self.h, &title, id == "recode") else {
            return;
        };
        if id == "recode" {
            if self.buffers[i].dirty {
                self.notice("Save changes before reopening with a different encoding");
                return;
            }
            let Some(path) = self.buffers[i].path.clone() else {
                return;
            };
            let (id, rev) = (self.buffers[i].id, self.buffers[i].rev);
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let r = Document::open_as(&path, &enc).map_err(|e| e.to_string());
                let _ = tx.send(Worker::Recoded(id, rev, r));
            });
            return;
        }
        match self.buffers[i].doc.set_encoding(&enc) {
            Ok(()) => {
                let b = &mut self.buffers[i];
                b.forced_dirty = true;
                b.dirty = true;
                b.rev += 1;
                self.track_edit();
                self.notice(format!("Save as {enc}"));
            }
            Err(e) => self.notice(e.to_string()),
        }
    }
    /// The three encoding settings: guessing on or off, the encoding used
    /// when guessing is off, and the encoding of new files.
    fn encoding_settings(&mut self, id: &str) {
        let e = self.prefs.encoding.clone();
        match id {
            "encodingdetect" => {
                self.prefs.encoding.auto_detect = !e.auto_detect;
                self.notice(if !e.auto_detect {
                    "Files that are not UTF-8 will have their encoding guessed from their bytes"
                } else {
                    "Files that are not UTF-8 will be read as the fallback encoding"
                });
            }
            "encodingfallback" => {
                let title = format!("Encoding for files that are not UTF-8 (now {})", e.fallback);
                let Some(enc) = choose_encoding(self.h, &title, false) else { return };
                if let Err(err) = linework_storage::set_detection(false, &enc) {
                    self.notice(err.to_string());
                    return;
                }
                self.notice(format!(
                    "Fallback encoding: {enc}{}",
                    if e.auto_detect { ". It is used when Detect encoding is off" } else { "" }
                ));
                self.prefs.encoding.fallback = enc;
            }
            _ => {
                let title = format!("Encoding for new files (now {})", e.new_files);
                let Some(enc) = choose_encoding(self.h, &title, false) else { return };
                if let Err(err) = Document::empty_as(&enc) {
                    self.notice(err.to_string());
                    return;
                }
                self.notice(format!("New files will be saved as {enc}"));
                self.prefs.encoding.new_files = enc;
            }
        }
        let e = &self.prefs.encoding;
        let _ = linework_storage::set_detection(e.auto_detect, &e.fallback);
        self.save_prefs();
    }
    pub(super) fn context(&mut self, x: i32, y: i32, source: H) {
        let mut pt = POINT { x, y };
        unsafe {
            if x < 0 {
                GetCursorPos(&mut pt);
            }
            ScreenToClient(hwnd(self.h), &mut pt);
        }
        let ids: Vec<&str> = if source == self.list && self.side == 2 {
            vec!["togglehit", "previewhit", "toggleall", "applypreview"]
        } else {
            if let Some(p) = self.editors.iter().position(|e| e.hwnd() == source) {
                self.active = p;
                let mut at = POINT { x, y };
                unsafe {
                    ScreenToClient(hwnd(source), &mut at);
                }
                let pos = self
                    .edit()
                    .send(POSITIONFROMPOINTCLOSE, at.x as usize, at.y as isize);
                let (a, b) = self.edit().selection();
                if pos >= 0 && (!(a.min(b)..=a.max(b)).contains(&(pos as usize)) || a == b) {
                    self.edit().set_selection(pos as usize, pos as usize);
                }
            }
            vec![
                "cut",
                "copy",
                "paste",
                "selnext",
                "markcolor",
                "def",
                "refs",
                "correct",
                "bookmark",
            ]
        };
        let items = ids
            .iter()
            .enumerate()
            .map(|(i, id)| PopupItem {
                id: (i + 1) as u16,
                label: commands::label(id),
                shortcut: commands::COMMANDS.iter().find(|c| c.id == *id)
                    .map(|c| self.shortcut(c)).unwrap_or_default(),
                checked: self.is_checked(id),
            })
            .collect::<Vec<_>>();
        let r = menu_at(self.h, &items, pt.x, pt.y);
        if r > 0 {
            self.command(ids[r as usize - 1]);
        }
    }
}
