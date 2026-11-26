use super::*;
impl App {
    pub(crate) fn save_prefs(&self) {
        let result = state::save_settings(&self.prefs, &self.pref_baseline.borrow(), false, None);
        if let Err(e) = result {
            alert(self.h, "Could not save settings", &e.to_string());
        } else {
            self.pref_baseline.replace(self.prefs.clone());
        }
    }
    pub(super) fn restore(&mut self, restore_all: bool) {
        let previous = self.prefs.tabs.clone();
        let mut states = if restore_all {
            previous
        } else {
            previous
                .into_iter()
                .filter(|s| s.recovery.is_some())
                .collect()
        };
        let sessions = state::profile().join("sessions");
        let clean = fs::metadata(state::profile().join("last-exit"))
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        let mut crashes = Vec::new();
        if let Ok(it) = fs::read_dir(sessions) {
            for entry in it.flatten() {
                let dir = entry.path();
                let manifest = dir.join("recovery.json");
                if dir == self.recovery_dir
                    || !manifest.exists()
                    || dir.join("closed").is_file()
                    || dir.join("dismissed").is_file()
                {
                    continue;
                }
                if OpenOptions::new()
                    .read(true)
                    .share_mode(0)
                    .open(dir.join("active.lock"))
                    .is_err()
                {
                    continue;
                }
                if let Ok(stamp) = fs::metadata(&manifest).and_then(|m| m.modified()) {
                    if let Some(files) = state::read::<Vec<FileState>>(&manifest) {
                        if files.iter().any(|x| x.recovery.is_some()) || stamp > clean {
                            crashes.push((stamp, dir, files));
                        }
                    }
                }
            }
        }
        crashes.sort_by_key(|p| p.0);
        if let Some((_, dir, crash)) = crashes.last()
            && let Ok(guard) = OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(dir.join("active.lock"))
        {
            if confirm(
                self.h,
                "Recover last session",
                "Linework found an interrupted session. Restore its tabs and recovered edits?",
                false,
            ) == IDYES
            {
                states = crash.clone();
                self.restored = Some(dir.clone());
                self.restore_guard = Some(guard);
            } else {
                let _ = fs::write(dir.join("dismissed"), b"dismissed");
            }
        }
        let crash_manifest = self.restored.as_ref().map(|dir| dir.join("recovery.json"));
        let active = self.prefs.active;
        let second = self.prefs.second;
        // Restore in manifest order, using original byte-backed documents for unmodified files.
        for st in states {
            let result = if let Some(file) = &st.recovery {
                match Document::recover(Path::new(file)) {
                    Err(e)
                        if e.to_string() == "Legacy draft uses an unverified original"
                            && confirm(
                                self.h,
                                "Open older draft",
                                "This older draft needs the original file. Its contents could have changed. Try opening it as a separate copy?",
                                false,
                            ) == IDYES =>
                    {
                        match Document::recover_legacy(Path::new(file)) {
                            Ok(mut d) => {
                                d.detach_path();
                                Ok(d)
                            }
                            Err(e)
                                if confirm(
                                    self.h,
                                    "Recover available edits",
                                    "The original is unavailable. Open the preserved edits with missing original ranges marked in brackets?",
                                    false,
                                ) == IDYES =>
                            {
                                let _ = e;
                                Document::salvage_legacy(Path::new(file))
                            }
                            Err(e) => Err(e),
                        }
                    }
                    result => result,
                }
            } else if let Some(p) = &st.path {
                Document::open(p)
            } else {
                Ok(Document::empty())
            };
            match result {
                Ok(doc) => {
                    if let Some(name) = &st.recovery {
                        self.claimed_drafts.insert(name.clone());
                    }
                    let detached =
                        st.recovery.is_some() && st.path.is_some() && doc.path().is_none();
                    let path = if detached { None } else { st.path.clone() };
                    let name = if detached {
                        format!("{} (recovered copy)", st.name)
                    } else {
                        st.name.clone()
                    };
                    self.add(doc, path, name, 0, Some(st));
                    if detached {
                        self.notice("Opened a recovered copy. Save As to choose its destination.");
                    }
                }
                Err(e) => {
                    if let Some(file) = &st.recovery {
                        if confirm(
                            self.h,
                            "Cannot restore draft",
                            &format!(
                                "{}: {e}\n\nDismiss this draft from future sessions? Choose No to keep the recovery files.",
                                st.name
                            ),
                            false,
                        ) == IDYES
                        {
                            let mut dismissed = HashSet::new();
                            dismissed.insert(file.clone());
                            let retire = (|| -> io::Result<()> {
                                if let Some(manifest) = &crash_manifest {
                                    let mut tabs = state::read::<Vec<FileState>>(manifest)
                                        .ok_or_else(|| {
                                            io::Error::other("Could not read the recovery manifest")
                                        })?;
                                    tabs.retain(|tab| tab.recovery.as_ref() != Some(file));
                                    state::store(manifest, &tabs)?;
                                }
                                state::save_settings(
                                    &self.prefs,
                                    &self.pref_baseline.borrow(),
                                    false,
                                    Some(&dismissed),
                                )
                            })();
                            if let Err(e) = retire {
                                self.restored = None;
                                self.notice(format!("Could not dismiss the draft: {e}"));
                            } else {
                                self.claimed_drafts.insert(file.clone());
                            }
                        } else {
                            self.restored = None;
                        }
                    }
                }
            }
        }
        if !self.buffers.is_empty() {
            let positions = self.prefs.view_positions;
            let pane = self.prefs.active_pane;
            let left = active.min(self.buffers.len() - 1);
            self.select(left, 0, positions.map(|p| p[0]));
            if let Some(i) = second.filter(|i| *i < self.buffers.len()) {
                self.select(i, 1, positions.map(|p| p[1]));
                if pane == 0 {
                    self.active = 0;
                    focus(self.editors[0].hwnd());
                }
            }
        }
    }
    fn snapshot(&mut self, final_save: bool) -> Result<(), String> {
        for p in 0..2 {
            if let Some(i) = self.assigned[p].and_then(|id| self.idx(id)) {
                self.buffers[i].last = events::point(p, self.editors[p].caret() as u64);
            }
        }
        let mut states = Vec::new();
        for i in 0..self.buffers.len() {
            let dirty = self.buffers[i].dirty;
            let rec = if dirty {
                self.sync_buffer(i)?;
                let b = &mut self.buffers[i];
                let file = self.recovery_dir.join(format!("{}-{}.lwr", b.id, b.rev));
                if b.recovered_rev != Some(b.rev) {
                    b.doc.save_recovery(&file).map_err(|e| e.to_string())?;
                    b.recovered_rev = Some(b.rev);
                }
                Some(file.to_string_lossy().into_owned())
            } else {
                None
            };
            let b = &self.buffers[i];
            states.push(FileState {
                path: b.path.clone(),
                name: b.name.clone(),
                offset: b.last,
                language: if b.manual {
                    b.lang.clone()
                } else {
                    String::new()
                },
                marks: b.marks.iter().copied().collect(),
                recovery: rec,
            });
        }
        state::store(&self.recovery_dir.join("recovery.json"), &states)
            .map_err(|e| e.to_string())?;
        if let Some(dir) = self.restored.take() {
            let _ = fs::write(dir.join("closed"), b"recovered");
        }
        self.restore_guard.take();
        if final_save {
            self.prefs.active_pane = self.active;
            self.prefs.view_positions =
                Some([0usize, 1].map(|p| events::point(p, self.editors[p].caret() as u64)));
            self.prefs.tabs = states;
            self.prefs.active = self.assigned[0].and_then(|id| self.idx(id)).unwrap_or(0);
            self.prefs.second = if self.split {
                self.assigned[1].and_then(|id| self.idx(id))
            } else {
                None
            };
            state::save_settings(
                &self.prefs,
                &self.pref_baseline.borrow(),
                true,
                Some(&self.claimed_drafts),
            )
            .map_err(|e| e.to_string())?;
            fs::write(self.recovery_dir.join("closed"), b"closed").map_err(|e| e.to_string())?;
            let _ = fs::write(state::profile().join("last-exit"), b"");
        }
        self.prune_drafts();
        self.saved_state = true;
        Ok(())
    }
    pub(super) fn quit(&mut self) {
        if self.buffers.iter().any(|b| b.preparing.is_some()) {
            self.notice("Wait for the editable copy or press Esc to cancel");
            return;
        }
        if global_edit::busy() {
            self.notice("A file-wide edit is finishing. Press Escape to cancel it before closing.");
            return;
        }
        if self.buffers.iter().any(|b| b.saving) || !self.commit_paths.is_empty() {
            self.notice("Finish the pending save before closing");
            return;
        }
        if self.playing {
            self.stop_macro()
        }
        if self.recording {
            self.macro_command("record");
        }
        self.drain_edits();
        if self.checkpoint_running
            || self
                .buffers
                .iter()
                .any(|b| b.dirty && b.recovered_rev != Some(b.rev))
        {
            self.quit_after_checkpoint = true;
            self.schedule_checkpoint();
            if self.checkpoint_running {
                self.notice("Saving the session before closing…");
                return;
            }
            self.quit_after_checkpoint = false;
        }
        if let Err(e) = self.snapshot(true) {
            if confirm(
                self.h,
                "Session could not be saved",
                &format!("{e}\nExit anyway? Changes after the last checkpoint may be lost."),
                false,
            ) != IDYES
            {
                return;
            }
        }
        self.exit_now();
    }
    pub(super) fn exit_now(&mut self) {
        for (_, flag) in self.jobs.values() {
            flag.store(true, Ordering::Relaxed);
        }
        self.exiting = true;
        self.tx.wake.close();
        crate::npp::shutdown(self);
        destroy(self.h);
    }
    pub(super) fn save(&mut self, i: usize, choose: bool, close_after: bool) {
        if self.buffers[i].preparing.is_some() {
            self.notice("Wait for the editable copy or press Esc to cancel");
            return;
        }
        if global_edit::pending(self.buffers[i].id) {
            self.notice("Finish or cancel the file-wide edit before saving");
            return;
        }
        if self.buffers[i].saving {
            let pending = &mut self.buffers[i].after_save;
            if !choose && !close_after && matches!(pending.back(), Some(AfterSave::History { .. }))
            {
                pending.push_back(AfterSave::Save);
            }
            return;
        }
        let mut path = self.buffers[i].path.clone();
        if choose || path.is_none() {
            let mut dlg = crate::dialogs::files(self.h)
                .set_title("Save file")
                .set_file_name(&self.buffers[i].name);
            if let Some(dir) = path.as_deref().and_then(Path::parent) {
                dlg = dlg.set_directory(dir)
            }
            path = dlg.save_file();
        }
        let Some(path) = path else { return };
        self.save_to(i, path, close_after, None);
    }
    fn save_to(
        &mut self,
        i: usize,
        path: PathBuf,
        close_after: bool,
        overwrite: Option<linework_storage::OverwriteTarget>,
    ) {
        if self
            .commit_paths
            .iter()
            .any(|p| state::key(p) == state::key(&path))
            || self.buffers.iter().enumerate().any(|(j, b)| {
                j != i
                    && b.path
                        .as_ref()
                        .is_some_and(|p| state::key(p) == state::key(&path))
            })
        {
            self.notice("That file is in use by another tab or replacement");
            return;
        }
        crate::npp::file_event(self, 1007, self.buffers[i].id);
        if let Err(e) = self.sync_buffer(i) {
            alert(self.h, "Save file", &e);
            return;
        }
        let b = &mut self.buffers[i];
        b.saving = true;
        let id = b.id;
        let mut doc = b.doc.clone();
        for p in 0..2 {
            if self.assigned[p] == Some(id) {
                self.editors[p].send(SETREADONLY, 1, 0);
            }
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let r = match overwrite {
                Some(target) => doc.overwrite(target),
                None => doc.save(&path),
            }
            .map(|_| doc);
            let _ = tx.send(Worker::Saved(id, r, path, close_after));
        });
        self.notice("Saving…");
    }
    pub(super) fn reload_disk(&mut self, i: usize) {
        let b = &self.buffers[i];
        let Some(path) = b.path.clone() else { return };
        let (id, rev, tx) = (b.id, b.rev, self.tx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Worker::Recoded(
                id,
                rev,
                Document::open(&path).map_err(|e| e.to_string()),
            ));
        });
    }
    fn adopt_reloaded(&mut self, i: usize, doc: Document) -> io::Result<()> {
        let id = self.buffers[i].id;
        let target = self.buffers[i].path.as_deref().map(state::key);
        if target != doc.path().map(state::key) || doc.disk_changed()? {
            return Err(io::Error::other(
                "File changed while reloading. Reload again to read its current version.",
            ));
        }
        let old_native = self.buffers[i].native;
        let large = doc.len() > LARGE;
        let sample = doc.read(0, doc.len().min(8192) as usize)?;
        let language = if self.buffers[i].manual {
            self.buffers[i].lang.clone()
        } else {
            language_for(
                &self.prefs,
                self.buffers[i].path.as_deref(),
                &String::from_utf8_lossy(&sample),
            )
        };
        let positions = [0, 1].map(|p| {
            (self.assigned[p] == Some(id))
                .then(|| events::point(p, self.editors[p].caret() as u64).min(doc.len()))
        });
        let native = if large {
            0
        } else {
            let bytes = doc.read(0, doc.len() as usize)?;
            let handle = if old_native != 0 {
                old_native
            } else {
                msg(self.bridge.hwnd(), CREATEDOCUMENT, 0, 0)
            };
            if handle == 0 {
                return Err(io::Error::other("Could not create the reloaded document"));
            }
            quiet(|| {
                msg(self.bridge.hwnd(), SETDOCPOINTER, 0, handle);
                self.bridge.send(SETREADONLY, 0, 0);
                self.bridge.set_bytes(&bytes);
                self.bridge.send(EMPTYUNDOBUFFER, 0, 0);
                self.bridge.send(SETSAVEPOINT, 0, 0);
            });
            handle
        };
        self.forget_view(id);
        let b = &mut self.buffers[i];
        b.recovery_cancel.store(true, Ordering::Relaxed);
        b.recovery_cancel = Arc::default();
        b.rev += 1;
        b.synced = b.rev;
        b.save_epoch = Some(b.rev);
        b.dirty = false;
        b.forced_dirty = false;
        b.lang = language;
        b.last = b.last.min(doc.len());
        b.marks.retain(|m| *m <= doc.len());
        b.doc = doc;
        b.native = native;
        b.large = large;
        for (p, pos) in positions.into_iter().enumerate() {
            if let Some(at) = pos {
                self.editors[p].document_changed_outside_view(0);
                self.bind(p, i, at);
            }
        }
        if old_native != 0 && old_native != native {
            msg(self.bridge.hwnd(), RELEASEDOCUMENT, 0, old_native);
        }
        self.saved_state = false;
        self.layout();
        Ok(())
    }
    pub(super) fn close_tab(&mut self, i: usize) {
        if i >= self.buffers.len() {
            return;
        }
        if self.buffers[i].preparing.is_some() {
            self.notice("Wait for the editable copy or press Esc to cancel");
            return;
        }
        if self.buffers[i].saving
            || self.buffers[i].path.as_ref().is_some_and(|p| {
                self.commit_paths
                    .iter()
                    .any(|other| state::key(other) == state::key(p))
            })
        {
            self.notice("File is being saved");
            return;
        }
        if self.buffers[i].dirty {
            match confirm(
                self.h,
                "Close tab",
                &format!("Save changes to {}?", self.buffers[i].name),
                true,
            ) {
                IDYES => {
                    self.save(i, false, true);
                    return;
                }
                IDNO => {}
                _ => return,
            }
        }
        self.remove(i);
    }
    pub(super) fn remove(&mut self, i: usize) {
        if self.playing {
            self.stop_macro();
        }
        crate::npp::file_event(self, 1003, self.buffers[i].id);
        self.buffers[i]
            .recovery_cancel
            .store(true, Ordering::Relaxed);
        self.sublime_closed(i);
        let b = self.buffers.remove(i);
        self.indexed_jump.remove(&b.id);
        self.forget_view(b.id);
        if let Some(p) = b.path {
            self.closed.push(p)
        }
        for p in 0..2 {
            if self.assigned[p] == Some(b.id) {
                self.assigned[p] = None;
                events::install(p, Binding::default(), Default::default());
                quiet(|| {
                    msg(self.editors[p].hwnd(), SETDOCPOINTER, 0, self.scratch[p]);
                });
            }
        }
        if b.native != 0 {
            msg(self.bridge.hwnd(), RELEASEDOCUMENT, 0, b.native);
        }
        if self.buffers.is_empty() {
            self.split = false;
            self.active = 0;
        } else {
            for p in 0..if self.split { 2 } else { 1 } {
                if self.assigned[p].is_none() {
                    self.select(i.min(self.buffers.len() - 1), p, None);
                }
            }
        }
        self.saved_state = false;
        self.rebuild_side();
        self.layout();
    }
    pub fn layout(&mut self) {
        self.layout_children(WindowLayout::new(self.h));
        self.refresh();
        invalidate(self.h);
    }
    pub(crate) fn resize_client(&mut self) {
        let side = self.sidebar_width();
        frame::FRAME.with_borrow_mut(|f| f.sidebar_width = side);
        let mut layout = WindowLayout::new(self.h);
        let damage = frame::rendering::stage_layout(self.h, &mut layout);
        self.layout_children(layout);
        if self.prefs.document_map { self.map_refresh(); }
        frame::rendering::request_layout(self.h, damage);
    }
    fn layout_children(&mut self, mut layout: WindowLayout) {
        let h = self.h;
        let (w, ht) = client(h);
        crate::chrome::rail_tips(h);
        let dpi = dpi(h);
        let scale = |n| n * dpi / 96;
        let sx = if self.prefs.sidebar {
            scale(self.sidebar_width())
        } else {
            scale(frame::RAIL)
        };
        crate::sidebar_grip::layout(self.h, self.prefs.sidebar, sx, self.prefs.light);
        let y = scale(frame::HEAD + frame::TABS);
        // The bar about zero bytes, when the part of the file shown holds a long run of them.
        let area = y + scale(8) + if self.zeros_caption().is_some() { scale(frame::ZERO_BAR) } else { 0 };
        let bottom = ht - scale(frame::FOOT);
        layout.set(
            self.status_left,
            Some(rect(scale(29), bottom, (w / 2 - scale(34)).max(1), ht - bottom)),
        );
        layout.set(
            self.status_right,
            Some(rect(w / 2, bottom, w / 2 - scale(12), ht - bottom)),
        );
        let has_folder = self.prefs.root.is_some();
        let panel = self.search_panel_shown();
        let list_y = scale(frame::list_y(
            self.side,
            has_folder,
            self.buffers.len(),
            !self.report_label.is_empty(),
            panel.then_some(self.panel_more),
        ));
        // The search panel's fields: find and replace always, the rest while More is open.
        let field_x = scale(frame::RAIL + 15);
        let field_w = sx - scale(frame::RAIL + 27);
        for (i, field) in self.panel_fields.into_iter().enumerate() {
            let shown = panel && (i < 2 || self.panel_more);
            // Room for the previous and next buttons beside find, the Preview
            // button beside replace, and the line count beside near.
            let narrower = match i {
                0 => scale(68),
                1 => scale(76),
                5 => scale(76),
                _ => 0,
            };
            // The More fields sit after their labels.
            let label = if i >= 2 { scale(frame::PANEL_LABEL) } else { 0 };
            layout.set(field, shown.then(|| rect(field_x + label, scale(frame::panel_field_y(i)), field_w - narrower - label, scale(frame::FIELD_H))));
        }
        let room = bottom
            - list_y
            - scale(if self.side == 2 && self.preview_mode {
                42
            } else {
                8
            });
        let list_h = if self.side == 0 && !has_folder {
            scale(frame::ROW * self.buffers.len().min(6) as i32).min(room)
        } else {
            room
        };
        layout.set(self.list, (self.prefs.sidebar && list_h > 0).then(||
            rect(scale(frame::RAIL + 9), list_y, sx - scale(frame::RAIL + 19), list_h)));
        let row_height = scale(frame::ROW) as isize;
        if msg(self.list, LB_GETITEMHEIGHT, 0, 0) != row_height {
            msg(self.list, LB_SETITEMHEIGHT, 0, row_height);
        }
        let count = if self.split { 2 } else { 1 };
        let format = self.preview_kind();
        let preview_width = self.document_preview.layout(self.h, &mut layout, w, sx, area, bottom,
            self.prefs.document_preview && format.is_some(), self.prefs.light, self.prefs.preview_width);
        let source_right = w - preview_width;
        let map_width = self.map_layout(&mut layout, source_right, sx, area, bottom);
        let right = source_right - map_width;
        let stacked = self.split && self.prefs.stacked;
        for p in 0..2 {
            let visible = p < count && self.assigned[p].is_some();
            let large = visible
                && self.assigned[p]
                    .and_then(|id| self.idx(id))
                    .is_some_and(|i| self.buffers[i].large);
            if !visible {
                layout.set(self.editors[p].hwnd(), None);
                layout.set(self.pane_headers[p], None);
                layout.set(self.scroll[p], None);
                continue;
            }
            let span = (right - sx) / if stacked { 1 } else { count as i32 };
            let x = if stacked { sx } else { sx + p as i32 * span };
            let dw = if stacked || p + 1 == count {
                right - x
            } else {
                span - scale(3)
            };
            let top = if stacked && p == 1 {
                (area + bottom) / 2 + scale(2)
            } else {
                area
            };
            let edge = if stacked && p == 0 {
                (area + bottom) / 2 - scale(1)
            } else {
                bottom
            };
            let sb = if large { scale(15) } else { 0 };
            let header = if self.split { scale(29) } else { 0 };
            layout.set(self.pane_headers[p], self.split.then(|| rect(x, top, dw, header)));
            let top = top + header;
            layout.set(self.editors[p].hwnd(), Some(rect(x, top, dw - sb, edge - top)));
            layout.set(self.scroll[p], large.then(|| rect(x + dw - sb, top, sb, edge - top)));
        }
        layout.apply();
        for p in 0..count {
            self.margins(p);
        }
    }
    pub fn refresh(&mut self) {
        self.refresh_after_input = false;
        let idx = self.current();
        let mut details = String::new();
        let mut status = if self.status_at.elapsed() < Duration::from_secs(5) {
            self.status.clone()
        } else {
            String::new()
        };
        if let Some(i) = idx {
            let e = self.edit();
            let b = &self.buffers[i];
            let pos = self.global_pos();
            let local = e.caret();
            let line = if b.large {
                b.doc.line_of(pos) + 1
            } else {
                e.send(LINEFROMPOSITION, local, 0) as u64 + 1
            };
            let total = if b.large { b.doc.len() } else { e.len() as u64 };
            let n = self
                .global_count()
                .unwrap_or_else(|| e.send(GETSELECTIONS, 0, 0) as u64);
            let col = e.send(GETCOLUMN, local, 0) + 1;
            details = format!(
                "{}    {}    {}    {}",
                if n > 1 {
                    format!("{n} carets")
                } else if b.large && pos > b.doc.indexed_bytes() {
                    "Indexing line number".into()
                } else if b.large
                    && self.pane_base() > 0
                    && e.send(LINEFROMPOSITION, local, 0) == 0
                    && b.doc.line_start(line.saturating_sub(1)) < self.pane_base()
                {
                    format!(
                        "Ln {}   Byte col {}",
                        state::pretty(line),
                        state::pretty(pos - b.doc.line_start(line.saturating_sub(1)) + 1)
                    )
                } else {
                    format!(
                        "Ln {}   Col {}",
                        state::pretty(line),
                        state::pretty(col.max(1) as u64)
                    )
                },
                if b.large {
                    format!("Byte {} of {}", state::pretty(pos), state::pretty(total))
                } else {
                    state::size(total)
                },
                b.doc.encoding_name(),
                if b.large && e.is_custom_grammar() {
                    format!("{} (page context)", b.lang)
                } else {
                    b.lang.clone()
                }
            );
            if status.is_empty() {
                status = if b.follow {
                    "Following file · read-only".into()
                } else if b.large && !b.doc.indexing_complete() {
                    format!(
                        "Indexing · {:.0}%",
                        100. * b.doc.indexed_bytes() as f64 / b.doc.len().max(1) as f64
                    )
                } else {
                    format!(
                        "{} lines",
                        state::pretty(if b.large {
                            b.doc.line_count()
                        } else {
                            e.send(GETLINECOUNT, 0, 0).max(1) as u64
                        })
                    )
                };
            }
        } else if status.is_empty() {
            status = "Ready".into();
        }
        if let Some(range) = self.wide_span() {
            status = format!("{} selected", state::size(range.end - range.start));
        }
        if self.recording {
            status = format!("● Recording · {} steps", self.macro_steps.len());
        }
        if self.split {
            let positions = if self.prefs.stacked { ["Top", "Bottom"] } else { ["Left", "Right"] };
            for (pane, position) in positions.into_iter().enumerate() {
                if let Some(b) = self.assigned[pane].and_then(|id| self.idx(id)).map(|ix| &self.buffers[ix]) {
                    let state = if b.dirty { "Unsaved · " } else { "" };
                    let role = if pane == self.active {
                        format!("Active {} pane", position.to_ascii_lowercase())
                    } else {
                        format!("{position} pane")
                    };
                    let caption = format!("{role} · {state}{}", b.name);
                    let header = self.pane_headers[pane];
                    if text(header) != caption {
                        set_text(header, &caption);
                        invalidate(header);
                    }
                }
            }
        }
        let title = idx
            .map(|i| {
                format!(
                    "{}{} — Linework",
                    self.buffers[i].name,
                    if self.buffers[i].dirty { " · Unsaved" } else { "" }
                )
            })
            .unwrap_or_else(|| "Linework".into());
        if title != self.displayed_title {
            set_text(self.h, &title);
            self.displayed_title = title;
        }
        if text(self.status_left) != status {
            set_text(self.status_left, &status)
        }
        if text(self.status_right) != details {
            set_text(self.status_right, &details)
        }
        let f = Frame {
            light: self.prefs.light,
            sidebar: self.prefs.sidebar,
            sidebar_width: self.sidebar_width(),
            side: self.side,
            workspace: self
                .prefs
                .root
                .as_deref()
                .and_then(Path::file_name)
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            tabs: self
                .buffers
                .iter()
                .map(|b| (b.name.clone(), b.dirty))
                .collect(),
            active: idx,
            alternate: if self.split {
                self.assigned[1 - self.active].and_then(|id| self.idx(id))
            } else {
                None
            },
            first_tab: self.tab_first,
            split: self.split,
            recent: self
                .prefs
                .recent
                .iter()
                .take(4)
                .map(|p| p.display().to_string())
                .collect(),
            recording: self.recording,
            preview: self.preview_mode && self.report.is_some(),
            can_preview: self.preview_kind().is_some(),
            document_preview: self.prefs.document_preview && self.preview_kind().is_some(),
            report: self.report_label.clone(),
            search_panel: self.search_view,
            panel: frame::PanelState {
                case_: self.panel.match_case,
                whole: self.panel.whole_word,
                regex: self.panel.regex,
                scope: self.panel.scope,
                hidden: self.panel.hidden,
                ignored: self.panel.ignored,
                more: self.panel_more,
                context: self.panel.context,
                near_lines: self.panel.near_lines,
                searching: match self.search_scope {
                    SearchScope::File => 0,
                    SearchScope::Open => 1,
                    SearchScope::Folder => 2,
                },
                folder: self.prefs.root.is_some(),
                selection: self.selection_search_active(),
            },
            summary: self.search_summary.clone(),
            zeros: self.zeros_caption(),
        };
        let (tab_state_changed, frame_changed, preview_changed) = frame::FRAME.with_borrow(|old|
            (old.tabs != f.tabs || old.active != f.active, old != &f, old.document_preview != f.document_preview));
        if frame_changed {
            let content_changed=crate::sidebar_grip::captured() && frame::FRAME.with_borrow(|old| {
                let mut old=old.clone();old.sidebar_width=f.sidebar_width;old!=f
            });
            frame::FRAME.replace(f);
            if content_changed {crate::sidebar_grip::artwork_changed();}
        }
        if preview_changed { self.layout_children(WindowLayout::new(self.h)); }
        self.document_preview.mark_update();
        if self.prefs.document_map {
            self.map_refresh();
        }
        if tab_state_changed {
            invalidate(self.list);
        }
        if frame_changed {
            invalidate(self.h);
        }
    }
    pub fn show_rows(&mut self, side: u8, caption: String, rows: Vec<Row>) {
        frame::RICH.with_borrow_mut(Vec::clear);
        self.row_hits.clear();
        // Other results in the search view (definitions, uses, build output) take the panel's place.
        if side == 2 {
            self.search_view = false;
        }
        self.show_list(side, caption, rows);
    }
    /// Folder search results: rows drawn as `rich` describes them, each with the matches it holds.
    pub fn show_rich_rows(&mut self, caption: String, rows: Vec<Row>, rich: Vec<frame::Rich>, hits: Vec<Range<usize>>) {
        frame::RICH.replace(rich);
        self.row_hits = hits;
        self.search_view = true;
        self.show_list(2, caption, rows);
    }
    fn show_list(&mut self, side: u8, caption: String, rows: Vec<Row>) {
        let idx = selected(self.list);
        self.side = side;
        self.report_label = caption;
        self.rows = rows;
        // Drawn rows are cut to the list's width; plain ones scroll sideways.
        let rich = frame::RICH.with_borrow(|r| !r.is_empty());
        msg(
            self.list,
            LB_SETHORIZONTALEXTENT,
            if rich {
                0
            } else {
                self.rows
                    .iter()
                    .map(|r| size(self.h, (r.label.chars().count().min(2500) * 8 + 12) as i32) as usize)
                    .max()
                    .unwrap_or(0)
            },
            0,
        );
        list_set(self.list, self.rows.iter().map(|r| r.label.as_str()),
            idx.filter(|i| *i < self.rows.len()));
        self.layout();
        crate::sidebar_grip::artwork_changed();
    }
    pub fn rebuild_side(&mut self) {
        if self.side != 0 {
            return;
        }
        let mut rows = vec![];
        let report;
        if self.prefs.root.is_some() {
            report = self
                .dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if self
                .directory_loading
                .as_ref()
                .is_some_and(|(p, _)| p == &self.dir)
            {
                return;
            }
            if let Some((_, old)) = self.directory_loading.take() {
                old.store(true, Ordering::Relaxed);
            }
            let cancelled = Arc::new(AtomicBool::new(false));
            self.directory_loading = Some((self.dir.clone(), cancelled.clone()));
            self.directory_job += 1;
            let (serial, path, tx) = (self.directory_job, self.dir.clone(), self.tx.clone());
            self.show_rows(0, report, vec![]);
            std::thread::spawn(move || {
                let result = (|| -> io::Result<Vec<Row>> {
                    let mut files = Vec::new();
                    for e in fs::read_dir(&path)? {
                        if cancelled.load(Ordering::Relaxed) {
                            return Err(io::ErrorKind::Interrupted.into());
                        }
                        let e = e?;
                        let t = e.file_type()?;
                        if t.is_symlink() {
                            continue;
                        }
                        let name = e.file_name().to_string_lossy().into_owned();
                        files.push((!t.is_dir(), name.to_lowercase(), name, e.path()));
                    }
                    files.sort_unstable();
                    Ok(files
                        .into_iter()
                        .map(|(file, _, name, path)| Row {
                            label: if file {
                                format!("    {name}")
                            } else {
                                format!("›  {name}")
                            },
                            target: if file {
                                Target::File(path)
                            } else {
                                Target::Directory(path)
                            },
                        })
                        .collect())
                })();
                let _ = tx.send(Worker::Directory(serial, path, result));
            });
            return;
        } else {
            report = "OPEN EDITORS".into();
            rows.extend(self.buffers.iter().map(|b| Row {
                label: format!("{}{}", if b.dirty { "●  " } else { "    " }, b.name),
                target: Target::BufferAt(b.id, b.last),
            }));
            if rows.is_empty() {
                rows.push(Row {
                    label: "Open a folder…".into(),
                    target: Target::None,
                });
            }
        }
        self.show_rows(0, report, rows);
    }
    fn directory_page(&mut self, start: usize) {
        let total = self.directory_rows.len();
        let end = (start + 512).min(total);
        let mut rows = Vec::new();
        if self.prefs.root.as_ref() != Some(&self.dir) {
            rows.push(Row {
                label: "‹  Parent folder".into(),
                target: Target::Parent,
            });
        }
        if start > 0 {
            rows.push(Row {
                label: "‹  Previous entries".into(),
                target: Target::DirectoryPage(start.saturating_sub(512)),
            });
        }
        rows.extend(self.directory_rows[start.min(end)..end].iter().cloned());
        if end < total {
            rows.push(Row {
                label: format!("Next entries  ›  ({end} of {total})"),
                target: Target::DirectoryPage(end),
            });
        }
        self.show_rows(
            0,
            self.dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            rows,
        );
    }
    pub(super) fn open_row(&mut self) {
        let Some(row) = selected(self.list).and_then(|i| self.rows.get(i)).cloned() else {
            return;
        };
        match row.target {
            Target::Location(p, b) => self.jump_path(&p, b),
            Target::File(p) => self.open_path(p),
            Target::Directory(p) => {
                self.dir = p;
                self.rebuild_side()
            }
            Target::DirectoryPage(start) => self.directory_page(start),
            Target::Parent => {
                if let Some(p) = self.dir.parent() {
                    self.dir = p.to_owned();
                    self.rebuild_side();
                }
            }
            Target::BufferAt(id, pos) => {
                if let Some(i) = self.idx(id) {
                    self.select(i, self.active, Some(pos));
                }
            }
            Target::Line(p, line, col) => {
                self.remember_location();
                self.open_line(p, line, col);
            }
            Target::Hit(n) => self.project_open_hit(n),
            Target::None => {
                if self.side == 0 {
                    self.command("folder");
                }
            }
        }
    }
    pub fn event(&mut self, event: Event) {
        match event {
            Event::Painted => {
                for (id, _) in std::mem::take(&mut self.count_after_paint) {
                    self.start_count(id);
                }
            }
            Event::Activated => {
                let docs: Vec<_> = self
                    .buffers
                    .iter()
                    .filter(|b| !b.saving && !b.follow && b.path.is_some())
                    .map(|b| (b.id, b.rev, b.doc.clone()))
                    .collect();
                if !docs.is_empty() {
                    let tx = self.tx.clone();
                    std::thread::spawn(move || {
                        let changes = docs
                            .into_iter()
                            .filter(|(_, _, d)| d.disk_changed().unwrap_or(false))
                            .map(|(id, r, _)| (id, r))
                            .collect();
                        let _ = tx.send(Worker::DiskChanged(changes));
                    });
                }
            }
            Event::SideWidth(px, done) => {
                let before = self.sidebar_width();
                self.prefs.sidebar_width = (px * 96 / dpi(self.h)).clamp(150, 900);
                if self.sidebar_width() != before { self.resize_client(); }
                if done {
                    self.save_prefs();
                }
            }
            Event::MapJump(f) => self.map_jump(f),
            Event::PreviewWidth(x, done) => {
                let scale = dpi(self.h);
                let width = (((client(self.h).0 - x) * 96 + scale / 2) / scale).max(1);
                if self.prefs.preview_width != width {
                    self.prefs.preview_width = width;
                    self.resize_client();
                }
                if done { self.save_prefs(); }
            }
            Event::MapReady => self.map_refresh(),
            Event::MapWheel(w, l) => {
                if self.current().is_some() {
                    msg(self.edit().hwnd(), WM_MOUSEWHEEL, w, l);
                }
            }
            Event::Size => self.layout(),
            Event::ScaleChanged => {
                for h in [self.status_left, self.status_right] {
                    msg(h, WM_SETFONT, font(size(self.h, 11), 400, true) as usize, 1);
                }
                for h in [self.list] {
                    msg(
                        h,
                        WM_SETFONT,
                        font(size(self.h, 14), 400, false) as usize,
                        1,
                    );
                }
                for (p, editor) in self.editors.iter().enumerate() {
                    editor.set_dpi(dpi(self.h) as u32);
                    self.margin_cache[p].set(None);
                }
                self.layout();
            }
            Event::Close => self.quit(),
            Event::Timer => self.tick(),
            Event::WorkReady => {
                self.tx.wake.received();
                self.drain_workers();
                self.document_preview.mark_update();
            }
            Event::MacroTick => self.macro_tick(),
            Event::SideOpen => self.open_row(),
            Event::Hit(hit, x, y) => match hit {
                Hit::Cmd(id) => self.command(&id),
                Hit::Menu(g) => {
                    // The menu's commands in `commands::MENUS` order; "-" is a separator.
                    let ids = commands::MENUS.iter().find(|(name, _)| *name == g).map_or(&[][..], |m| m.1);
                    let choices: Vec<_> = ids
                        .iter()
                        .filter_map(|id| {
                            if *id == "-" {
                                return Some(PopupItem { id: 0, label: "", shortcut: String::new(), checked: false });
                            }
                            let (i, c) = commands::COMMANDS.iter().enumerate().find(|(_, c)| c.id == *id)?;
                            Some(PopupItem {
                                id: (i + 1) as u16,
                                label: c.name,
                                shortcut: self.shortcut(c),
                                checked: self.is_checked(c.id),
                            })
                        })
                        .collect();
                    let r = menu_at(self.h, &choices, x, y);
                    if r > 0 {
                        self.command(commands::COMMANDS[r as usize - 1].id);
                    }
                }
                Hit::Tab(i) => self.select(i, self.active, None),
                Hit::Close(i) => self.close_tab(i),
                Hit::Recent(i) => {
                    if let Some(p) = self.prefs.recent.get(i).cloned() {
                        self.open_path(p)
                    }
                }
                Hit::ScrollTabs(delta) => {
                    self.tab_first = (self.tab_first as i32 + delta)
                        .clamp(0, self.buffers.len().saturating_sub(1) as i32)
                        as usize;
                    self.refresh();
                }
                Hit::Side(s) => {
                    self.prefs.sidebar = true;
                    self.side = s;
                    match s {
                        0 => self.rebuild_side(),
                        1 => self.project_symbols(false),
                        // The search panel, over what it last searched, or what there is.
                        _ => self.open_search(self.usable_scope(), false),
                    };
                }
            },
            Event::Launch(args) => self.open_targets(args),
            Event::Drop(paths) => {
                for p in paths {
                    if p.is_dir() {
                        self.set_root(p)
                    } else {
                        self.open_path(p)
                    }
                }
            }
            Event::Scroll(p, kind, thumb) => {
                if p > 1 {
                    return;
                }
                self.active = p;
                let (at, total) = (self.global_pos(), self.total_len());
                let n = match kind as i32 {
                    SB_THUMBTRACK | SB_THUMBPOSITION => events::physical(
                        p,
                        ((thumb as u128 * events::view(p, total) as u128) / 30000) as u64,
                    ),
                    SB_LINEUP => self.current().map_or(at, |i| {
                        let d = &self.buffers[i].doc;
                        events::physical(
                            p,
                            events::view(p, d.line_start(d.line_of(at).saturating_sub(1)))
                                .min(events::view(p, at).saturating_sub(1)),
                        )
                    }),
                    SB_LINEDOWN => self.current().map_or(at, |i| {
                        let d = &self.buffers[i].doc;
                        events::physical(p, events::view(p, d.line_start(d.line_of(at) + 1)))
                    }),
                    SB_PAGEUP => events::physical(p, events::view(p, at).saturating_sub(8192)),
                    SB_PAGEDOWN => {
                        events::physical(p, events::view(p, at).saturating_add(8192)).min(total)
                    }
                    _ => return,
                };
                self.go(n, 0);
            }
            Event::Macro(s) => {
                if self.recording && !self.playing && self.macro_steps.len() < 100000 {
                    self.macro_steps.push(s);
                }
            }
            Event::Edit(p, b, pos, del, bytes) => {
                if self.global_native_edit(p, b, pos, del, &bytes) {
                    return;
                }
                if let Some(i) = self.idx(b.id) {
                    if !self.prepare_input(i) {
                        if self.buffers[i].preparing.is_some() {
                            let mut edit = (pos, del, bytes);
                            if del > 0
                                && let Some((id, rev, all, shown)) = self.wide_selection[p].as_ref()
                                && (*id, *rev) == (b.id, self.buffers[i].rev)
                                && *shown == (pos..pos + del)
                            {
                                edit.0 = all.start;
                                edit.1 = all.end - all.start;
                                if let Some(insert) = Self::matching_insert(p, b.id, pos) {
                                    edit.2 = insert;
                                }
                                self.wide_selection[p] = None;
                            }
                            self.buffers[i].pending_edits.push(edit);
                        } else {
                            self.reload_views(i);
                        }
                        return;
                    }
                }
                if self.take_span_edit(p, b, pos, del) {
                    return;
                }
                if let Some(i) = self.idx(b.id) {
                    if let Err(e) = self.buffers[i].doc.replace(pos, del, &bytes) {
                        self.reload_views(i);
                        self.notice(e.to_string());
                        return;
                    }
                    let shifted = self.shifted_folds(i, pos, del, bytes.len() as u64);
                    let buf = &mut self.buffers[i];
                    buf.move_marks(pos, del, bytes.len() as u64);
                    buf.rev += 1;
                    buf.dirty = buf.doc.dirty();
                    buf.synced = buf.rev;
                    let rev = buf.rev;
                    self.note_paged_edit(b.id, rev, pos, del, bytes.len() as u64);
                    if let Some(folds) = shifted {
                        self.advance_folds(i, p, folds, pos, del, &bytes);
                    }
                    let other = 1 - p;
                    if self.split && self.assigned[other] == Some(b.id) {
                        let at = events::point(other, self.editors[other].caret() as u64);
                        self.bind(other, i, at);
                    }
                    self.track_edit();
                    self.refresh_after_input = true;
                }
            }
            Event::Modified(p, id, at, del, add, flags) => {
                if self.split && self.assigned[self.active] == Some(id) && p != self.active {
                    return;
                }
                if let Some(i) = self.idx(id) {
                    self.buffers[i].move_marks(at, del, add);
                    self.buffers[i].rev += 1;
                    if self.assigned[p] == Some(id) {
                        self.buffers[i].dirty = self.buffers[i].forced_dirty
                            || self.editors[p].send(GETMODIFY, 0, 0) != 0;
                    }
                    self.note_native_edit(i, at, del, add, flags);
                    self.track_edit();
                    self.refresh_after_input = true;
                }
            }
            Event::Preview(signal) => self.preview_signal(signal),
            Event::SearchInput(id) => self.search_input(id),
            Event::Notify(p, id, mut n, txt) => {
                if self.assigned[p] != Some(id) {
                    return;
                }
                if !txt.is_empty() {
                    n.text = txt.as_ptr() as _;
                }
                if n.nmhdr.code == sn::SCN_MARGINCLICK
                    && n.margin == 2
                    && n.position >= 0
                    && events::binding(p).large
                {
                    self.active = p;
                    let at = events::point(p, n.position as u64);
                    self.toggle_file_fold(at, false);
                    return;
                }
                if n.nmhdr.code == sn::SCN_MARGINCLICK && n.margin == 1 && n.position >= 0 {
                    self.active = p;
                    let e = &self.editors[p];
                    let l = e.send(LINEFROMPOSITION, n.position as usize, 0) as usize;
                    let at = events::point(p, e.send(POSITIONFROMLINE, l, 0).max(0) as u64);
                    if let Some(i) = self.idx(id) {
                        let end =
                            events::end_point(p, e.send(GETLINEENDPOSITION, l, 0).max(0) as u64);
                        let marks = &mut self.buffers[i].marks;
                        if marks.range(at..=end.max(at)).next().is_some() {
                            marks.retain(|&m| m < at || m > end);
                        } else {
                            marks.insert(at);
                        }
                        self.paint_bookmarks(i, p);
                        self.saved_state = false;
                    }
                }
                if n.nmhdr.code == sn::SCN_FOCUSIN {
                    self.active = p;
                    self.refresh_after_input = true;
                }
                if n.nmhdr.code == sn::SCN_SAVEPOINTLEFT || n.nmhdr.code == sn::SCN_SAVEPOINTREACHED
                {
                    if let Some(i) = self.idx(id) {
                        if !self.buffers[i].large {
                            self.buffers[i].dirty = self.buffers[i].forced_dirty
                                || self.editors[p].send(GETMODIFY, 0, 0) != 0;
                            self.refresh_after_input = true;
                        }
                    }
                }
                self.editors[p].on_notify(&n);
                if n.nmhdr.code == sn::SCN_UPDATEUI {
                    self.polish_update(p, n.updated);
                }
                if n.nmhdr.code == sn::SCN_UPDATEUI
                    && n.updated & 4 != 0
                    && self.prefs.sync_scroll
                    && self.split
                    && p == self.active
                {
                    self.link_pane(p);
                }
                if n.nmhdr.code == sn::SCN_UPDATEUI {
                    if n.updated & 2 != 0 {
                        self.check_wide_selection(p);
                        self.check_global_selection(p);
                    }
                    if n.updated & 4 != 0 && self.global_count().is_some() {
                        self.render_cursors(p, false);
                    }
                    // Scrolling a resident document changes the viewport only.
                    // The caret, tab strip, status and file identity are unchanged.
                    if n.updated != 0 && n.updated & !12 == 0 && !events::binding(p).large {
                        if p == self.active {
                            self.map_refresh();
                        }
                        return;
                    }
                    self.margins(p);
                    if self.recording && p == self.active && n.updated & 2 != 0 {
                        self.align_macro_page();
                    }
                    self.page_edge(p, n.updated);
                    self.refresh_after_input = true;
                }
            }
            Event::Context(x, y, h) => {
                self.context(x, y, h);
            }
        }
    }
    fn page_edge(&mut self, p: usize, updated: i32) {
        let Some(i) = self.assigned[p].and_then(|id| self.idx(id)) else {
            return;
        };
        if !self.buffers[i].large || self.playing || self.recording {
            return;
        }
        let bind = events::binding(p);
        let e = &self.editors[p];
        let local = e.caret();
        let length = e.len() as u64;
        let total = self.buffers[i].doc.len();
        let (a, b) = e.selection();
        if a != b {
            return;
        }
        if updated & 2 != 0
            && ((local == 0 && bind.base > 0)
                || (local as u64 >= length && events::point(p, length) < total))
        {
            let at = events::point(p, local as u64);
            self.bind(p, i, at);
            return;
        }
        if updated & 4 != 0 && length > 12000 {
            let first = e.send(GETFIRSTVISIBLELINE, 0, 0) as usize;
            let line = e.send(DOCLINEFROMVISIBLE, first, 0).max(0) as usize;
            let pos = e.send(POSITIONFROMLINE, line, 0).max(0) as u64;
            let end = e
                .send(
                    POSITIONFROMLINE,
                    (line + e.send(LINESONSCREEN, 0, 0) as usize)
                        .min(e.send(GETLINECOUNT, 0, 0).saturating_sub(1) as usize),
                    0,
                )
                .max(0) as u64;
            if (end + 3072 > length && events::point(p, length) < total)
                || (first == 0 && bind.base > 0 && local < 4096)
            {
                let target = events::point(p, pos);
                self.bind(p, i, target);
                let line =
                    self.editors[p].send(LINEFROMPOSITION, events::offset(p, target) as usize, 0);
                self.editors[p].send(SETFIRSTVISIBLELINE, line as usize, 0);
            }
        }
    }
    fn drain_workers(&mut self) {
        let start = Instant::now();
        for _ in 0..32 {
            let Ok(worker) = self.rx.try_recv() else { return };
            self.worker(worker);
            if self.exiting { return; }
            if start.elapsed() >= Duration::from_millis(3) { break; }
        }
        self.tx.wake.notify();
    }
    /// Start the background line count of buffer `id`.
    fn start_count(&mut self, id: u64) {
        if let Some(i) = self.idx(id) {
            if let Err(e) = self.buffers[i].doc.start_indexing() {
                self.notice(e.to_string());
            }
        }
    }
    fn tick(&mut self) {
        // A file whose view has not painted within a second counts its lines anyway.
        let late: Vec<u64> = self
            .count_after_paint
            .iter()
            .filter(|(_, at)| at.elapsed() >= Duration::from_secs(1))
            .map(|(id, _)| *id)
            .collect();
        if !late.is_empty() {
            self.count_after_paint.retain(|(id, _)| !late.contains(id));
            for id in late {
                self.start_count(id);
            }
        }
        if let Some(i) = self.current() {
            let buf = &self.buffers[i];
            if let Some(&(line, col, guess)) = self.indexed_jump.get(&buf.id) {
                if buf.doc.indexing_complete() || line < buf.doc.line_count() {
                    // The line is found: its "Locating line" notice is done too.
                    if self.status.starts_with("Locating line") {
                        self.status_at = Instant::now() - Duration::from_secs(6);
                    }
                    let exact = guess.and_then(|_| {
                        let start = buf.doc.line_start(line.saturating_sub(1));
                        let tabs = self.edit().send(GETTABWIDTH, 0, 0).max(1) as u64;
                        crate::launch::column_offset(&buf.doc, start, col.saturating_sub(1), tabs, || false).ok()
                    });
                    if guess.is_some_and(|at| at != self.global_pos() || exact == Some(at)) {
                        // Moved away from the estimate since, or the estimate was
                        // right: the view stays as it is.
                        self.indexed_jump.remove(&buf.id);
                        self.refresh();
                    } else {
                        // The exact place: the same as a right estimate.
                        self.goto_line_col(line, col)
                    }
                } else if let Some(err) = buf.doc.index_error() {
                    self.indexed_jump.remove(&buf.id);
                    self.notice(err)
                }
            }
        }
        self.sublime_tick();
        self.tick_history_flash();
        self.search_tick();
        self.heat_tick();
        crate::glide::watchdog();
        if !self.playing && !self.recording {
            if let Some(i) = self.current() {
                if !self.buffers[i].large
                    && !self.buffers[i].saving
                    && !self.buffers[i].follow
                    && self.edit().len() as u64 > LARGE
                {
                    if self.sync_buffer(i).is_ok() {
                        self.promote(i);
                    }
                }
            }
        }
        for e in &self.editors {
            e.pump_grammar();
        }
        self.drain_workers();
        if self.exiting { return; }
        if !self.saved_state
            && (self.changed_at.elapsed() > Duration::from_millis(1100)
                || self.auto_at.elapsed() >= Duration::from_secs(5))
            && self.auto_at.elapsed() > Duration::from_secs(2)
        {
            self.auto_at = Instant::now();
            self.schedule_checkpoint();
        }
        if self.spell_at.elapsed() > Duration::from_millis(490) {
            self.spell_at = Instant::now();
            for p in 0..if self.split { 2 } else { 1 } {
                if self.assigned[p].is_some() {
                    if self.prefs.spell {
                        self.editors[p].refresh_spelling();
                    }
                }
            }
            if self.changed_at.elapsed() > Duration::from_millis(450) {
                if let Some(i) = self.current() {
                    if !self.buffers[i].manual
                        && self.buffers[i].path.is_none()
                        && self.pane_base() == 0
                    {
                        let e = self.edit();
                        let s = e.text_range(0, e.len().min(4096));
                        let lang = detect_language(None, &s);
                        if lang != self.buffers[i].lang {
                            self.edit().set_language(&lang);
                            self.indentation(self.active, &lang);
                            self.buffers[i].lang = lang;
                        }
                    }
                }
            }
        }
        if self.tail_at.elapsed() > Duration::from_millis(750) {
            self.tail_at = Instant::now();
            for i in 0..self.buffers.len() {
                if !self.buffers[i].follow {
                    continue;
                }
                let b = &mut self.buffers[i];
                if b.following_read {
                    continue;
                }
                b.following_read = true;
                let (id, rev, mut doc, tx) = (b.id, b.rev, b.doc.clone(), self.tx.clone());
                std::thread::spawn(move || {
                    let result = doc
                        .refresh_from_disk()
                        .map(|changed| changed.then_some(doc));
                    let _ = tx.send(Worker::Follow(id, rev, result));
                });
            }
        }
        self.refresh();
    }
    pub(super) fn worker(&mut self, w: Worker) {
        let done = match &w {
            Worker::Global(r) => Some(r.job_id()),
            Worker::Compared(j, _)
            | Worker::Report(j, _)
            | Worker::Build(j, _)
            | Worker::Tool(j, ..)
            | Worker::Found(j, ..)
            | Worker::Replaced(j, ..)
            | Worker::Eol(j, ..)
            | Worker::Lines(j, ..) => Some(*j),
            _ => None,
        };
        match w {
            Worker::PrepareProgress(id, at, total) => {
                if self
                    .idx(id)
                    .is_some_and(|i| self.buffers[i].preparing.is_some())
                {
                    self.notice(format!(
                        "Preparing editable copy · {} of {} · Esc to cancel",
                        state::size(at),
                        state::size(total)
                    ));
                }
            }
            Worker::Prepared(id, rev, result) => {
                let Some(i) = self.idx(id) else { return };
                let b = &mut self.buffers[i];
                b.preparing = None;
                let edits = std::mem::take(&mut b.pending_edits);
                let mut target = None;
                let applied = (|| -> Result<(), String> {
                    result?;
                    if b.rev != rev || b.follow || b.saving {
                        return Err("File changed while preparing the edit".into());
                    }
                    let mut draft = b.doc.clone();
                    draft.begin_undo_group();
                    for (at, del, bytes) in &edits {
                        draft.replace(*at, *del, bytes).map_err(|e| e.to_string())?;
                        target = Some(*at + bytes.len() as u64);
                    }
                    draft.end_undo_group();
                    if target.is_some() {
                        b.doc.apply_snapshot(draft).map_err(|e| e.to_string())?;
                        for (at, del, bytes) in edits {
                            b.move_marks(at, del, bytes.len() as u64);
                        }
                        b.rev += 1;
                        b.synced = b.rev;
                        b.dirty = b.doc.dirty();
                    }
                    Ok(())
                })();
                self.reload_views(i);
                let ready = applied.is_ok();
                match applied {
                    Ok(()) => {
                        if let Some(at) = target {
                            if self.current() == Some(i) {
                                self.go(at, 0);
                            }
                            self.track_edit();
                        }
                        self.notice("Editable copy ready");
                    }
                    Err(e) => self.notice(e),
                }
                self.finish_prepared_cursors(id, ready);
            }
            Worker::Directory(serial, path, r) if self.directory_job == serial => {
                self.directory_loading = None;
                if self.side != 0 || self.dir != path || self.prefs.root.is_none() {
                    return;
                }
                match r {
                    Ok(rows) => {
                        self.directory_rows = rows;
                        self.directory_page(0);
                    }
                    Err(e) => self.notice(e.to_string()),
                }
            }
            Worker::DiskChanged(changes) => {
                let names = changes
                    .into_iter()
                    .filter_map(|(id, rev)| {
                        self.idx(id)
                            .map(|i| &self.buffers[i])
                            .filter(|b| b.rev == rev && !b.saving)
                            .map(|b| b.name.as_str())
                    })
                    .collect::<Vec<_>>();
                if !names.is_empty() {
                    self.notice(format!(
                        "Changed on disk: {} · File → Compare changes or Reload from disk",
                        names.join(", ")
                    ));
                }
            }
            Worker::Follow(id, rev, result) => {
                let Some(i) = self.idx(id) else { return };
                self.buffers[i].following_read = false;
                if self.buffers[i].rev != rev || !self.buffers[i].follow || self.buffers[i].dirty {
                    return;
                }
                match result {
                    Ok(Some(doc)) => {
                        let b = &mut self.buffers[i];
                        let end = doc.len();
                        if !b.large && end <= LARGE {
                            let bytes = match doc.read(0, end as usize) {
                                Ok(bytes) => bytes,
                                Err(e) => {
                                    self.notice(e.to_string());
                                    return;
                                }
                            };
                            quiet(|| {
                                msg(self.bridge.hwnd(), SETDOCPOINTER, 0, b.native);
                                self.bridge.send(SETREADONLY, 0, 0);
                                self.bridge.set_bytes(&bytes);
                                self.bridge.send(EMPTYUNDOBUFFER, 0, 0);
                                self.bridge.send(SETSAVEPOINT, 0, 0);
                            });
                            for p in 0..2 {
                                if self.assigned[p] == Some(id) {
                                    self.editors[p].document_changed_outside_view(0);
                                }
                            }
                        }
                        b.doc = doc;
                        b.rev += 1;
                        b.synced = b.rev;
                        if !b.large && end > LARGE {
                            self.promote(i);
                        }
                        for p in 0..2 {
                            if self.assigned[p] == Some(id) {
                                self.bind(p, i, end);
                            }
                        }
                        self.layout();
                    }
                    Err(e) => {
                        self.buffers[i].follow = false;
                        self.reload_views(i);
                        self.notice(format!("Following stopped: {e}"));
                    }
                    Ok(None) => {}
                }
            }
            Worker::LongColumn(serial, id, rev, caret, result) => {
                if self.column_request.load(Ordering::Relaxed) == serial
                    && self
                        .current()
                        .is_some_and(|i| self.buffers[i].id == id && self.buffers[i].rev == rev)
                    && self.global_pos() == caret
                {
                    match result {
                        Ok(at) => self.go(at, 0),
                        Err(e) => self.notice(e),
                    }
                }
            }
            Worker::Lines(job, id, rev, start, end, result)
                if self.current_job("line tools", job) =>
            {
                if let Some(i) = self.idx(id) {
                    if self.buffers[i].rev != rev {
                        self.notice("Document changed. Run the line transformation again.");
                        return;
                    }
                    match result {
                        Ok(Some((new, layout))) => {
                            match self.change_open(i, vec![(start, end, new)]) {
                                Ok(()) => {
                                    layout.restore(self, i);
                                    self.notice("Updated lines");
                                }
                                Err(e) => self.notice(e),
                            }
                        }
                        Ok(None) => self.notice("Lines already match"),
                        Err(e) => self.notice(e),
                    }
                }
            }
            Worker::Checkpoint(v, s, c, o, r) => self.checkpoint_finished(v, s, c, o, r),
            Worker::Info(s) => self.notice(s),
            Worker::Global(result) => self.global_result(result),
            Worker::CursorEdit(result) => self.finish_cursor_edit(result),
            Worker::Open(path, r, pane, st) => {
                self.pending.remove(&state::key(&path));
                let jump = self.pending_jump.remove(&state::key(&path));
                match r {
                    Ok(doc) => {
                        let name = path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        let enc = doc.encoding_name();
                        self.add(doc, Some(path.clone()), name, pane, st);
                        self.status_at = Instant::now() - Duration::from_secs(6);
                        if !enc.starts_with("UTF-") {
                            // Say so when a file is not Unicode, so a wrong guess
                            // is noticed and fixed before anything is saved.
                            self.notice(if enc == "Hexadecimal" {
                                "Showing raw bytes. Saving writes the hex digits back as bytes".to_string()
                            } else {
                                format!("Read as {enc}: the file is not UTF-8. Encoding, Reopen with encoding changes it")
                            });
                        }
                        if let Some((at,col)) = jump {
                            if col > 0 {
                                self.goto_line_col(at, col)
                            } else {
                                self.go(at, 0)
                            }
                        }
                        // The text first, then the line count beside it: counting a
                        // large file while the window first paints slows the paint.
                        // A jump waiting for the count without an estimate starts it at once.
                        let key = state::key(&path);
                        if let Some(i) = self.buffers.iter().position(|b| b.path.as_ref().is_some_and(|p| state::key(p) == key)) {
                            let id = self.buffers[i].id;
                            if self.indexed_jump.get(&id).is_some_and(|j| j.2.is_none()) {
                                self.start_count(id);
                            } else {
                                self.count_after_paint.push((id, Instant::now()));
                                events::AWAIT_PAINT.set(true);
                            }
                        }
                    }
                    Err(e)
                        if e.contains("binary data")
                            || e.contains("Reopen with its original encoding") =>
                    {
                        // Binary files read as text are gibberish whatever the
                        // encoding, so the hexadecimal view leads the list.
                        let binary = e.contains("binary data");
                        let title = if binary {
                            "This is a binary file, not text. Open it as hexadecimal or as text in"
                        } else {
                            "Open as text with encoding"
                        };
                        let enc = if binary {
                            match actions::pick(
                                self.h,
                                title,
                                &["Hexadecimal (raw bytes, safe to edit and save)", "Text in an encoding…"],
                            ) {
                                Some(0) => Some("Hexadecimal".to_string()),
                                Some(_) => actions::choose_encoding(self.h, "Open as text with encoding", false),
                                None => None,
                            }
                        } else {
                            actions::choose_encoding(self.h, title, true)
                        };
                        if let Some(enc) = enc {
                            let tx = self.tx.clone();
                            self.pending.insert(state::key(&path));
                            self.notice(format!(
                                "Opening {} as {enc}…",
                                path.file_name().unwrap_or_default().to_string_lossy()
                            ));
                            std::thread::spawn(move || {
                                let r = Document::open_as(&path, &enc)
                                    .map_err(|e| format!("Could not decode as {enc}: {e}"));
                                let _ = tx.send(Worker::Open(path, r, pane, st));
                            });
                        }
                    }
                    Err(e) => alert(self.h, "Open file", &format!("{}\n{e}", path.display())),
                }
            }
            Worker::Saved(id, r, path, close) => {
                if let Some(i) = self.idx(id) {
                    self.buffers[i].saving = false;
                    for p in 0..2 {
                        if self.assigned[p] == Some(id) {
                            self.editors[p].send(SETREADONLY, self.buffers[i].follow as _, 0);
                        }
                    }
                    if r.is_err() {
                        self.buffers[i].after_save.clear();
                    }
                    match r {
                        Ok(doc) => {
                            let b = &mut self.buffers[i];
                            b.path = Some(path.clone());
                            b.name = path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned();
                            b.doc.adopt_save(doc);
                            b.recovery_cancel.store(true, Ordering::Relaxed);
                            b.recovery_cancel = Arc::default();
                            b.save_epoch = Some(b.rev);
                            b.dirty = false;
                            b.forced_dirty = false;
                            b.synced = b.rev;
                            // The durable manifest still owns the prior checkpoint until its successor is published.
                            if !b.large {
                                quiet(|| {
                                    msg(self.bridge.hwnd(), SETDOCPOINTER, 0, b.native);
                                    self.bridge.send(SETSAVEPOINT, 0, 0);
                                });
                            }
                            if !b.manual {
                                let sample = b.doc.read(0, 4096).unwrap_or_default();
                                b.lang = language_for(
                                    &self.prefs,
                                    Some(&path),
                                    &String::from_utf8_lossy(&sample),
                                );
                                for p in 0..2 {
                                    if self.assigned[p] == Some(id) {
                                        self.editors[p].set_language(&b.lang);
                                        let lang = b.lang.clone();
                                        let width = self.prefs.tab_width.clamp(1, 16);
                                        let tabs = self
                                            .prefs
                                            .use_tabs
                                            .unwrap_or(matches!(lang.as_str(), "Makefile" | "Go"));
                                        self.editors[p].set_indentation(width, tabs);
                                    }
                                }
                            }
                            self.prefs
                                .recent
                                .retain(|p| state::key(p) != state::key(&path));
                            self.prefs.recent.insert(0, path);
                            self.prefs.recent.truncate(40);
                            self.saved_state = false;
                            self.sublime_saved(i);
                            crate::npp::file_event(self, 1008, id);
                            if close && self.buffers[i].after_save.is_empty() {
                                self.remove(i);
                            }
                            self.notice("Saved");
                        }
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                            self.notice("File changed on disk · draft kept");
                            let target = linework_storage::OverwriteTarget::inspect(&path).ok();
                            use crate::dialogs::ConflictAction;
                            match crate::dialogs::save_conflict(
                                self.h,
                                &self.buffers[i].name,
                                path.is_file(),
                                target.is_some(),
                            ) {
                                ConflictAction::Copy => self.save(i, true, close),
                                ConflictAction::Overwrite => {
                                    if let Some(t) = target {
                                        self.save_to(i, path, close, Some(t));
                                    }
                                }
                                ConflictAction::Compare => {
                                    drop(target);
                                    self.select(i, self.active, None);
                                    self.compare_current(true);
                                }
                                ConflictAction::Reload => {
                                    drop(target);
                                    self.reload_disk(i);
                                }
                                ConflictAction::Keep => {}
                            }
                        }
                        Err(e) => {
                            self.notice("Save failed · draft kept");
                            alert(self.h, "Save file", &e.to_string());
                        }
                    }
                    if let Some(i) = self.idx(id) {
                        while !self.buffers[i].saving {
                            match self.buffers[i].after_save.pop_front() {
                                Some(AfterSave::History { pane, redo }) => {
                                    self.apply_history(i, pane, redo);
                                    self.drain_edits();
                                }
                                Some(AfterSave::Save) => self.save(i, false, false),
                                None => break,
                            }
                        }
                        self.refresh();
                    }
                }
            }
            Worker::Index(j, idx) => self.project_index_result(j, idx),
            Worker::Compared(j, result) if self.current_job("compare", j) => {
                self.show_comparison(result)
            }
            Worker::Report(j, r) if self.current_job("folder search", j) => match r {
                Ok(r) => self.project_report(r),
                Err(e) => self.notice(e),
            },
            Worker::Build(j, r) if self.current_job("build", j) => match r {
                Ok(r) => self.project_diagnostics(r),
                Err(e) => self.notice(e),
            },
            Worker::Applied(r) => self.project_applied(r),
            Worker::Zeros(job, id, rev, run, done) => self.zeros_measured(job, id, rev, run, done),
            Worker::ZerosFailed(job, e) => self.zeros_failed(job, e),
            Worker::Undone(r) => self.project_undone(r),
            Worker::Tool(j, id, rev, a, b, filter, r) if self.current_job("tool", j) => {
                self.project_tool_result(id, rev, a, b, filter, r)
            }
            Worker::Found(j, id, rev, r, replace) if self.current_job("find", j) => {
                self.finish_find(j, id, rev, r, replace)
            }
            Worker::Replaced(j, id, rev, r) if self.current_job("replace", j) => {
                self.finish_replace(id, rev, r)
            }
            Worker::Recoded(id, rev, r) => {
                if let Some(i) = self.idx(id) {
                    if self.buffers[i].rev == rev && !self.buffers[i].saving {
                        match r {
                            Ok(doc) => match self.adopt_reloaded(i, doc) {
                                Ok(()) => self.notice("Reloaded file from disk"),
                                Err(e) => self.notice(e.to_string()),
                            },
                            Err(e) => self.notice(e),
                        }
                    }
                }
            }
            Worker::Eol(j, id, rev, style, r) if self.current_job("line endings", j) => {
                if let Some(i) = self.idx(id) {
                    if self.buffers[i].rev == rev
                        && !self.buffers[i].saving
                        && !self.buffers[i].follow
                    {
                        match r {
                            Ok((doc, layout)) => match self.buffers[i].doc.apply_snapshot(doc) {
                                Ok(()) => {
                                    self.buffers[i].doc.set_newline(style);
                                    self.buffers[i].rev += 1;
                                    self.buffers[i].dirty = true;
                                    self.reload_views(i);
                                    self.track_edit();
                                    layout.restore(self, i);
                                    self.notice("Converted line endings");
                                }
                                Err(e) => self.notice(e.to_string()),
                            },
                            Err(e) => self.notice(e),
                        }
                    }
                }
            }
            _ => {}
        }
        if let Some(id) = done {
            self.jobs.retain(|_, (j, _)| *j != id);
        }
    }
}
