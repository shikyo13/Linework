use super::*;

pub(crate) fn language_for(prefs: &Prefs, path: Option<&Path>, sample: &str) -> String {
    let rule = path.and_then(|p| {
        let name = p.file_name()?.to_str()?.to_lowercase();
        prefs.language_rules.get(&name).or_else(|| {
            prefs
                .language_rules
                .get(&format!("*.{}", p.extension()?.to_str()?.to_lowercase()))
        })
    });
    rule.cloned()
        .unwrap_or_else(|| detect_language(path, sample).into())
}

impl App {
    pub(super) fn language_rules(&mut self) {
        let keys: Vec<_> = self.prefs.language_rules.keys().cloned().collect();
        let mut rows = vec!["Add filename or extension rule…".into()];
        rows.extend(
            keys.iter()
                .map(|k| format!("{k}    {}", self.prefs.language_rules[k])),
        );
        let Some(i) = forms::pick(self.h, "Filename and language rules", &rows, "") else {
            return;
        };
        let mut pattern = keys.get(i.wrapping_sub(1)).cloned().unwrap_or_default();
        if i > 0 {
            let opts = vec!["Change language".into(), "Remove rule".into()];
            match forms::pick(self.h, &pattern, &opts, "") {
                Some(1) => {
                    self.prefs.language_rules.remove(&pattern);
                    self.apply_rules();
                    return;
                }
                Some(0) => {}
                _ => return,
            }
        } else {
            let init = self
                .current()
                .and_then(|i| self.buffers[i].path.as_deref()?.file_name()?.to_str())
                .unwrap_or("");
            let Some(s) = forms::input(
                self.h,
                "Match files",
                "Exact filename or extension pattern, e.g. Jenkinsfile or *.inc",
                init,
            ) else {
                return;
            };
            pattern = s.trim().to_lowercase();
            let tail = pattern.strip_prefix("*.").unwrap_or(&pattern);
            if tail.is_empty() || tail.contains(['*', '?', ':', '/', '\\']) {
                self.notice("Use an exact filename or an extension such as *.inc");
                return;
            }
        }
        let names = linework_editor::languages();
        let choices = names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        if let Some(n) = forms::pick(self.h, "Highlight matching files as", &choices, "") {
            self.prefs
                .language_rules
                .insert(pattern, choices[n].clone());
            self.apply_rules();
        }
    }
    fn apply_rules(&mut self) {
        for i in 0..self.buffers.len() {
            if self.buffers[i].manual {
                continue;
            }
            let b = &self.buffers[i];
            let bytes = b.doc.read(0, 8192).unwrap_or_default();
            let lang = language_for(
                &self.prefs,
                b.path.as_deref(),
                &String::from_utf8_lossy(&bytes),
            );
            if lang == b.lang {
                continue;
            }
            let id = b.id;
            self.buffers[i].lang = lang.clone();
            for p in 0..2 {
                if self.assigned[p] == Some(id) {
                    self.editors[p].set_language(&lang);
                    self.indentation(p, &lang);
                }
            }
        }
        self.save_prefs();
        self.notice("Updated file associations");
    }
    pub(super) fn link_pane(&mut self, p: usize) {
        let q = 1 - p;
        let (Some(i), Some(j)) = (
            self.assigned[p].and_then(|id| self.idx(id)),
            self.assigned[q].and_then(|id| self.idx(id)),
        ) else {
            return;
        };
        let e = &self.editors[p];
        let top = e
            .send(
                DOCLINEFROMVISIBLE,
                e.send(GETFIRSTVISIBLELINE, 0, 0).max(0) as usize,
                0,
            )
            .max(0) as u64;
        let global = if self.buffers[i].large {
            self.buffers[i].doc.line_of(events::point(p,e.send(POSITIONFROMLINE,top as usize,0).max(0) as u64))
        } else {
            top
        };
        let e = &self.editors[q];
        let target = if self.buffers[j].large {
            let d = &self.buffers[j].doc;
            let pos = d.line_start(global.min(d.line_count().saturating_sub(1)));
            let bind = events::binding(q);
            if !(bind.base..events::point(q, e.len() as u64)).contains(&pos) {
                self.bind(q, j, pos);
            }
            self.editors[q]
                .send(LINEFROMPOSITION, events::offset(q, pos) as usize, 0)
                .max(0) as usize
        } else {
            (global as usize).min(e.send(GETLINECOUNT, 0, 0).saturating_sub(1) as usize)
        };
        let e = &self.editors[q];
        let first = e.send(VISIBLEFROMDOCLINE, target, 0);
        if first >= 0 && first != e.send(GETFIRSTVISIBLELINE, 0, 0) {
            e.send(SETFIRSTVISIBLELINE, first as usize, 0);
        }
    }
    pub(super) fn choose_scope(&mut self) {
        let opts = ["Current file", "Selected text", "Open tabs…", "Folder…"].map(String::from);
        let Some(n) = forms::pick(self.h, "Search scope", &opts, "") else {
            return;
        };
        let replace = self.repbar;
        let query = text(self.searchbox);
        match n {
            0 => {
                self.show_find(false);
                self.repbar = replace;
                set_text(self.searchbox, &query);
                self.layout();
            }
            1 => {
                self.show_selection_find();
                self.repbar = replace;
                set_text(self.searchbox, &query);
                self.layout();
            }
            2 => self.project_search_open(replace),
            _ => self.project_search(replace),
        }
    }
    pub(super) fn is_checked(&self, id: &str) -> bool {
        match id {
            "spell" => self.prefs.spell,
            "wrap" => self.prefs.wrap,
            "sidebar" => self.prefs.sidebar,
            "minimap" => self.prefs.document_map,
            "docpreview" => self.prefs.document_preview && self.preview_kind().is_some(),
            "previewfollow" => self.prefs.preview_cues.follow,
            "whitespace" => self.prefs.whitespace,
            "markmatches" => self.prefs.highlight,
            "currentline" => self.prefs.current_line,
            "split" => self.split && !self.prefs.stacked,
            "splitbelow" => self.split && self.prefs.stacked,
            "syncscroll" => self.prefs.sync_scroll,
            "single" => !self.split,
            "record" => self.recording,
            "follow" => self.current().is_some_and(|i| self.buffers[i].follow),
            _ => false,
        }
    }
}
