use super::*;
pub(super) type Versions = Vec<(u64, u64, bool)>;

impl App {
    pub(super) fn versions(&self) -> Versions {
        self.buffers
            .iter()
            .map(|b| (b.id, b.rev, b.dirty))
            .collect()
    }
    pub(super) fn recovery_path(&self, id: u64, rev: u64) -> PathBuf {
        self.recovery_dir.join(format!("{id}-{rev}.lwr"))
    }
    pub(super) fn schedule_checkpoint(&mut self) {
        if self.checkpoint_running {
            return;
        }
        for p in 0..2 {
            if let Some(i) = self.assigned[p].and_then(|id| self.idx(id)) {
                self.buffers[i].last = events::point(p,self.editors[p].caret() as u64);
            }
        }
        let mut writes = vec![];
        let mut states = vec![];
        let mut obsolete = vec![];
        for i in 0..self.buffers.len() {
            if self.buffers[i].dirty && self.buffers[i].recovered_rev != Some(self.buffers[i].rev) {
                if let Err(e) = self.sync_buffer(i) {
                    self.notice(format!("Recovery checkpoint: {e}"));
                    return;
                }
                let b = &self.buffers[i];
                writes.push((
                    self.recovery_path(b.id, b.rev),
                    b.doc.clone(),
                    b.recovery_cancel.clone(),
                ));
            }
            let b = &self.buffers[i];
            if let Some(rev) = b.recovered_rev {
                if rev != b.rev || !b.dirty {
                    obsolete.push(self.recovery_path(b.id, rev));
                }
            }
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
                recovery: b.dirty.then(|| {
                    self.recovery_path(b.id, b.rev)
                        .to_string_lossy()
                        .into_owned()
                }),
            });
        }
        self.checkpoint_running = true;
        let versions = self.versions();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = writes
                .iter()
                .try_for_each(
                    |(p, d, cancelled)| match d.save_recovery_cancel(p, cancelled) {
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(()),
                        r => r,
                    },
                )
                .map_err(|e| e.to_string());
            let created = writes.into_iter().map(|(p, _, _)| p).collect();
            let _ = tx.send(Worker::Checkpoint(
                versions, states, created, obsolete, result,
            ));
        });
    }
    pub(super) fn checkpoint_finished(
        &mut self,
        versions: Versions,
        states: Vec<FileState>,
        created: Vec<PathBuf>,
        obsolete: Vec<PathBuf>,
        result: Result<(), String>,
    ) {
        self.checkpoint_running = false;
        if let Err(e) = result {
            self.notice(format!("Recovery checkpoint: {e}"));
            if std::mem::take(&mut self.quit_after_checkpoint)
                && confirm(
                    self.h,
                    "Session could not be saved",
                    &format!("{e}\nExit anyway? Changes after the last checkpoint may be lost."),
                    false,
                ) == IDYES
            {
                self.exit_now();
            }
            return;
        }
        let current = self.versions();
        // Publish the captured revision even when typing has continued. Exclude closed
        // tabs and replace saved entries so a stale worker cannot resurrect a draft.
        let mut published = Vec::new();
        for ((id, rev, captured_dirty), mut state) in versions.iter().zip(states) {
            let Some(i) = self.idx(*id) else {
                continue;
            };
            let b = &mut self.buffers[i];
            if !b.dirty {
                state.path = b.path.clone();
                state.name = b.name.clone();
                state.recovery = None;
            } else if b.save_epoch.is_some_and(|saved| saved >= *rev) {
                // A Save and subsequent edit overtook this worker. The previous
                // manifest remains valid until a checkpoint of the new base.
                state.recovery = None;
                state.path = b.path.clone();
            }
            published.push((
                *id,
                if *captured_dirty {
                    state.recovery.as_ref().map(|_| *rev)
                } else {
                    None
                },
                state,
            ));
        }
        let states: Vec<_> = published.iter().map(|(_, _, s)| s.clone()).collect();
        if let Err(e) = state::store(&self.recovery_dir.join("recovery.json"), &states) {
            if std::mem::take(&mut self.quit_after_checkpoint)
                && confirm(
                    self.h,
                    "Session could not be saved",
                    &format!("{e}\nExit anyway and leave the last checkpoint on disk?"),
                    false,
                ) == IDYES
            {
                self.exit_now();
                return;
            }
            self.notice(format!("Recovery checkpoint: {e}"));
            return;
        }
        for (id, rev, _) in published {
            if let Some(i) = self.idx(id) {
                self.buffers[i].recovered_rev = rev;
            }
        }
        if let Some(dir) = self.restored.take() {
            let _ = fs::write(dir.join("closed"), b"recovered");
        }
        self.restore_guard.take();
        let _ = (obsolete, created);
        let claimed_persisted = !self.claimed_drafts.is_empty()
            && state::save_settings(
                &self.prefs,
                &self.pref_baseline.borrow(),
                false,
                Some(&self.claimed_drafts),
            )
            .is_ok();
        if claimed_persisted {
            self.claimed_drafts.clear();
            self.pref_baseline.replace(self.prefs.clone());
        }
        self.prune_drafts();
        self.saved_state = current == versions;
        if std::mem::take(&mut self.quit_after_checkpoint) {
            self.quit();
        }
    }
}

impl App {
    pub(super) fn prune_drafts(&self) {
        let profile = state::profile();
        let mut keep = HashSet::new();
        for path in [profile.join("settings.json"), profile.join("settings.bak")] {
            if path.is_file() {
                let Some(p) = state::read::<Prefs>(&path) else {
                    return;
                };
                keep.extend(
                    p.tabs
                        .into_iter()
                        .filter_map(|s| s.recovery)
                        .map(|p| state::key(Path::new(&p))),
                );
            }
        }
        let Ok(entries) = fs::read_dir(profile.join("sessions")) else {
            return;
        };
        let mut candidates = vec![];
        for entry in entries.flatten() {
            if !entry
                .file_type()
                .is_ok_and(|t| t.is_dir() && !t.is_symlink())
            {
                continue;
            }
            let dir = entry.path();
            let own = dir == self.recovery_dir;
            let archived = dir.join("closed").exists() || dir.join("dismissed").exists();
            if !archived || own {
                let path = dir.join("recovery.json");
                if path.exists() {
                    let Some(tabs) = state::read::<Vec<FileState>>(&path) else {
                        return;
                    };
                    keep.extend(
                        tabs.into_iter()
                            .filter_map(|s| s.recovery)
                            .map(|p| state::key(Path::new(&p))),
                    );
                }
            }
            if own || archived {
                candidates.push((dir, own));
            }
        }
        for (dir, own) in candidates {
            let lock = if own {
                None
            } else {
                match OpenOptions::new()
                    .read(true)
                    .share_mode(0)
                    .open(dir.join("active.lock"))
                {
                    Ok(lock) => Some(lock),
                    Err(_) => continue,
                }
            };
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            let files: Vec<_> = entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .map(|e| e.path())
                .collect();
            let mut bases = HashSet::new();
            let mut retained = false;
            for p in files
                .iter()
                .filter(|p| p.extension().is_some_and(|e| e == "lwr"))
            {
                if keep.contains(&state::key(p)) {
                    retained = true;
                    match Document::recovery_archive(p) {
                        Ok(Some(path)) => {
                            bases.insert(state::key(&path));
                        }
                        Ok(None) => {}
                        Err(_) => return,
                    }
                } else if fs::remove_file(p).is_err() {
                    return;
                }
            }
            for p in files
                .iter()
                .filter(|p| p.extension().is_some_and(|e| e == "lwbase"))
            {
                if !bases.contains(&state::key(p)) {
                    let _ = fs::remove_file(p);
                }
            }
            if !own && !retained {
                for name in ["recovery.json", "dismissed", "closed"] {
                    let _ = fs::remove_file(dir.join(name));
                }
                drop(lock);
                let _ = fs::remove_file(dir.join("active.lock"));
                let _ = fs::remove_dir(&dir);
            }
        }
    }
}
