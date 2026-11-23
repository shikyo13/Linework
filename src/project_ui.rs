use crate::app::{App, Row, Target, Worker};
use crate::win::{alert, set_text, text};
use crate::{forms, sci, state};
use linework_project::{
    self as project, ApplyReport, BuildReport, Index, OpenBuffer, OpenStream, SearchReport,
    SearchRequest, ToolConfig, ToolMode, ToolResult, UserTool,
};
use std::cell::RefCell;
use std::collections::HashSet;
use std::io::{self, BufReader, Cursor, Read};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

thread_local! {
    static SHORTCUTS: RefCell<Option<(Instant, ToolConfig)>> = const { RefCell::new(None) };
    static INDEX_JOB: RefCell<IndexJob> = RefCell::new(IndexJob::default());
    static LOCAL_SYMBOLS: RefCell<Option<LocalSymbols>> = const { RefCell::new(None) };
}

struct LocalSymbols {
    id: u64,
    revision: u64,
    path: PathBuf,
    language: String,
    index: Rc<Index>,
}
#[derive(Default)]
struct IndexJob {
    generation: u64,
    root: Option<PathBuf>,
    cancel: Arc<AtomicBool>,
}

fn short_path(root: &Path, path: &Path) -> String {
    project::relative_path(path, root)
        .unwrap_or(path)
        .display()
        .to_string()
}

struct DocumentReader {
    doc: linework_storage::Document,
    position: u64,
}
impl Read for DocumentReader {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        let n = self
            .doc
            .len()
            .saturating_sub(self.position)
            .min(dst.len() as u64) as usize;
        if n > 0 {
            self.doc.read_into(self.position, &mut dst[..n])?;
        }
        self.position += n as u64;
        Ok(n)
    }
}

impl App {
    fn workspace_streams(&self, root: Option<&Path>) -> Vec<OpenStream<'static>> {
        self.buffers
            .iter()
            .enumerate()
            .filter_map(|(i, buffer)| {
                let path = match &buffer.path {
                    Some(path)
                        if root.is_none_or(|r| project::relative_path(path, r).is_some()) =>
                    {
                        path.clone()
                    }
                    None if root.is_none() => PathBuf::from(&buffer.name),
                    _ => return None,
                };
                let reader: Box<dyn io::BufRead + Send> = if buffer.dirty && !buffer.large {
                    Box::new(Cursor::new(self.buffer_text(i).into_bytes()))
                } else {
                    Box::new(BufReader::with_capacity(
                        256 * 1024,
                        DocumentReader {
                            doc: buffer.doc.clone(),
                            position: 0,
                        },
                    ))
                };
                Some(OpenStream {
                    path,
                    buffer_id: Some(buffer.id),
                    revision: buffer.rev,
                    reader,
                })
            })
            .collect()
    }

    pub(super) fn project_index(&mut self) {
        let (job, cancel, changed_root) = INDEX_JOB.with_borrow_mut(|state| {
            state.cancel.store(true, Ordering::Relaxed);
            state.generation += 1;
            state.cancel = Arc::default();
            let changed = state.root != self.prefs.root;
            state.root = self.prefs.root.clone();
            (state.generation, state.cancel.clone(), changed)
        });
        if changed_root {
            self.index = Index::default();
            for editor in &self.editors {
                editor.set_workspace_words(Vec::new());
            }
        }
        let Some(root) = self.prefs.root.clone() else {
            return;
        };
        self.drain_edits();
        let buffers: Vec<_> = self
            .buffers
            .iter()
            .enumerate()
            .filter_map(|(i, b)| {
                let path = b
                    .path
                    .as_ref()
                    .filter(|p| project::relative_path(p, &root).is_some())?;
                (b.dirty && !b.large)
                    .then(|| OpenBuffer::new(path.clone(), self.buffer_text(i), b.rev))
            })
            .collect();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let idx = project::index_with_buffers(&root, &buffers, &cancel);
            let _ = tx.send(Worker::Index(job, idx));
        });
    }

    pub(super) fn project_index_result(&mut self, job: u64, mut index: Index) {
        if index.cancelled
            || !INDEX_JOB
                .with_borrow(|state| state.generation == job && state.root == self.prefs.root)
        {
            return;
        }
        let words = std::mem::take(&mut index.completion_words);
        for editor in &self.editors {
            editor.set_workspace_words(words.clone());
        }
        self.index = index;
        if self.side != 2 {
            self.notice(format!(
                "Indexed {} workspace files",
                self.index.all_files.len()
            ));
        }
    }

    pub(super) fn project_search(&mut self, preview: bool) {
        self.search_scope(preview, false);
    }
    pub(super) fn project_search_open(&mut self, preview: bool) {
        self.search_scope(preview, true);
    }
    fn search_scope(&mut self, preview: bool, tabs_only: bool) {
        if !tabs_only && self.prefs.root.is_none() {
            self.notice("Open a folder to search its files");
            return;
        }
        if tabs_only && self.buffers.is_empty() {
            self.notice("Open a file or create a tab to search");
            return;
        }
        let selection = if self.current().is_some() {
            let (a, b) = self.edit().selection();
            if a.abs_diff(b) <= 2048 {
                self.edit().selected_text()
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        let query = if !selection.is_empty() && !selection.contains(['\r', '\n']) {
            selection
        } else {
            text(self.searchbox)
        };
        let mut fields = vec![forms::Field {
            label: "Find",
            value: query,
        }];
        if preview {
            fields.push(forms::Field {
                label: "Replace with",
                value: text(self.replacebox),
            });
        }
        if !tabs_only {
            fields.push(forms::Field {
                label: "Include files   ·   *.rs;*.toml   (blank for all)",
                value: self.project_include.clone(),
            });
            fields.push(forms::Field {
                label: "Exclude files   ·   tests/**;*.min.js",
                value: self.project_exclude.clone(),
            });
        }
        let title = match (tabs_only, preview) {
            (true, true) => "Preview replacements in open tabs",
            (true, false) => "Find in open tabs",
            (false, true) => "Preview replacements in folder",
            (false, false) => "Find in folder",
        };
        let Some((values, options)) = forms::form(
            self.h,
            title,
            &fields,
            &[
                ("Match case", self.case_),
                ("Whole words", self.whole),
                ("Regular expression", self.regex),
            ],
            if preview { "Preview" } else { "Find" },
        ) else {
            return;
        };
        let mut req = SearchRequest::new(self.root_dir(), values[0].clone());
        req.buffers_only = tabs_only;
        if req.query.is_empty() {
            self.notice("Enter text to search for");
            return;
        }
        set_text(self.searchbox, &req.query);
        if !tabs_only {
            let i = 1 + usize::from(preview);
            self.project_include = values[i].clone();
            self.project_exclude = values[i + 1].clone();
            req.include = self.project_include.clone();
            req.exclude = self.project_exclude.clone();
        }
        (self.case_, self.whole, self.regex) = (options[0], options[1], options[2]);
        (req.match_case, req.whole_word, req.regex) = (self.case_, self.whole, self.regex);
        if preview {
            req.replacement = Some(values[1].clone());
            set_text(self.replacebox, &values[1]);
        }
        self.drain_edits();
        let buffers = self.workspace_streams((!tabs_only).then_some(req.root.as_path()));
        self.report = None;
        self.checked.clear();
        self.preview_mode = preview;
        self.prefs.sidebar = true;
        self.show_rows(
            2,
            if tabs_only {
                "Searching open tabs…"
            } else {
                "Searching folder…"
            }
            .into(),
            vec![],
        );
        let (job, cancel) = self.new_job("folder search");
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = project::search_with_streams(&req, buffers, &cancel);
            let _ = tx.send(Worker::Report(job, result));
        });
    }

    pub(super) fn project_report(&mut self, report: SearchReport) {
        self.checked = vec![self.preview_mode; report.hits.len()];
        self.report = Some(report);
        self.project_refresh_rows();
        if self.preview_mode { self.review_replacements(0); }
    }
    pub(super) fn project_open_hit(&mut self, index: usize) {
        if self.preview_mode { self.review_replacements(index); return; }
        let Some(hit) = self.report.as_ref().and_then(|r| r.hits.get(index)) else {
            return;
        };
        let (id, path, offset, length) = (hit.buffer_id, hit.path.clone(), hit.offset, hit.length);
        if let Some(id) = id {
            let Some(i) = self.buffers.iter().position(|b| b.id == id) else {
                self.notice("This tab has closed. Refresh the search.");
                return;
            };
            self.select(i, self.active, Some(offset));
            self.go(offset, length);
        } else {
            self.jump_path(&path, offset);
            if self.current().is_some_and(|i| {
                self.buffers[i]
                    .path
                    .as_deref()
                    .is_some_and(|p| state::key(p) == state::key(&path))
            }) {
                self.go(offset, length);
            }
        }
    }
    pub(super) fn project_refresh_rows(&mut self) {
        let Some(report) = self.report.as_ref() else {
            return;
        };
        let root = self.root_dir();
        let n = report.hits.len();
        let checked = self.checked.iter().filter(|v| **v).count();
        let mut caption = if self.preview_mode {
            format!("{checked} of {n} checked · Space toggles")
        } else {
            format!("{n} matches · {} files searched", report.files_scanned)
        };
        if report.truncated {
            caption.push_str(" · result limit");
        }
        if report.cancelled {
            caption.push_str(" · canceled");
        }
        if !report.errors.is_empty() {
            caption.push_str(&format!(" · {} issues", report.errors.len()));
        }
        let mut rows = Vec::with_capacity(n + report.errors.len());
        for (i, h) in report.hits.iter().enumerate() {
            let marker = if self.preview_mode {
                if self.checked[i] { "☑  " } else { "☐  " }
            } else {
                ""
            };
            let name = h.path.file_name().unwrap_or_default().to_string_lossy();
            let content = if let Some(after) = &h.preview_after {
                format!("{} → {}", h.preview.trim(), after.trim())
            } else {
                h.preview.trim().to_owned()
            };
            rows.push(Row {
                label: format!(
                    "{marker}{name}:{}  ·  {content}   {}",
                    h.line,
                    short_path(&root, &h.path)
                ),
                target: Target::Hit(i),
            });
        }
        rows.extend(report.errors.iter().map(|e| Row {
            label: e.clone(),
            target: Target::None,
        }));
        self.show_rows(2, caption, rows);
    }
    pub(super) fn project_toggle_hit(&mut self, index: usize) {
        if self.preview_mode {
            if let Some(v) = self.checked.get_mut(index) {
                *v = !*v;
                self.project_refresh_rows();
            }
        }
    }
    pub(super) fn project_toggle_all(&mut self) {
        if !self.preview_mode {
            return;
        }
        let select = self.checked.iter().any(|v| !*v);
        self.checked.fill(select);
        self.project_refresh_rows();
    }
    pub(super) fn project_preview_hit(&self, index: usize) {
        if let Some(h) = self.report.as_ref().and_then(|r| r.hits.get(index)) {
            let detail = if let Some(after) = &h.preview_after {
                format!("Before\n{}\n\nAfter\n{}", h.preview, after)
            } else {
                h.preview.clone()
            };
            alert(
                self.h,
                "Match details",
                &format!(
                    "{} · line {}, column {}\n\n{detail}",
                    h.path.display(),
                    h.line,
                    h.column
                ),
            );
        }
    }
    pub(super) fn project_apply(&mut self) {
        if !self.commit_paths.is_empty() {
            self.notice("Finishing the previous replacement");
            return;
        }
        let indices: Vec<_> = self
            .checked
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.then_some(i))
            .collect();
        if !self.preview_mode || indices.is_empty() {
            self.notice("Preview replacements and check the matches to apply");
            return;
        }
        if self.buffers.iter().any(|b| b.saving) {
            self.notice("Save in progress. Apply the checked replacements once saving finishes.");
            return;
        }
        if self.report.as_ref().is_some_and(|report| {
            indices.iter().any(|i| {
                report
                    .hits
                    .get(*i)
                    .is_some_and(|h| self.pending_open(&h.path))
            })
        }) {
            self.notice("A matching file is opening. Refresh the preview when it opens.");
            return;
        }
        let Some(report) = self.report.take() else {
            return;
        };
        let open: Vec<_> = self.buffers.iter().filter_map(|b| b.path.clone()).collect();
        self.commit_paths = indices
            .iter()
            .filter_map(|i| report.hits.get(*i).map(|h| h.path.clone()))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        self.checked.clear();
        self.preview_mode = false;
        self.cancel_job("folder search");
        let tx = self.tx.clone();
        self.show_rows(2, "Applying checked replacements…".into(), vec![]);
        thread::spawn(move || {
            let applied = report.apply_selected(&indices, &open);
            let _ = tx.send(Worker::Applied(applied));
        });
    }
    pub(super) fn project_applied(&mut self, mut report: ApplyReport) {
        self.commit_paths.clear();
        let mut count = report.applied;
        for plan in report.open {
            let key = state::key(&plan.path);
            let Some(i) = self.buffers.iter().position(|b| match plan.buffer_id {
                Some(id) => b.id == id,
                None => b.path.as_deref().is_some_and(|p| state::key(p) == key),
            }) else {
                report
                    .errors
                    .push(format!("{} closed since preview", plan.path.display()));
                continue;
            };
            if self.sync_buffer(i).is_err() || self.buffers[i].rev != plan.revision {
                report
                    .errors
                    .push(format!("{} changed since preview", plan.path.display()));
                continue;
            }
            let valid = if self.buffers[i].large {
                plan.edits.iter().all(|e| {
                    self.buffers[i]
                        .doc
                        .read(e.offset, e.matched.len())
                        .is_ok_and(|v| v == e.matched.as_bytes())
                })
            } else {
                plan.validate(&self.buffer_text(i), self.buffers[i].rev)
            };
            if !valid {
                report
                    .errors
                    .push(format!("{} changed since preview", plan.path.display()));
                continue;
            }
            let n = plan.edits.len();
            let edits = plan
                .edits
                .into_iter()
                .map(|e| {
                    (
                        e.offset as usize,
                        e.offset as usize + e.matched.len(),
                        e.replacement,
                    )
                })
                .collect();
            match self.change_open(i, edits) {
                Ok(()) => count += n,
                Err(e) => report.errors.push(e),
            }
        }
        let mut rows: Vec<_> = report
            .changed_files
            .iter()
            .map(|p| Row {
                label: format!("Saved  ·  {}", p.display()),
                target: Target::File(p.clone()),
            })
            .collect();
        rows.extend(report.errors.iter().map(|e| Row {
            label: e.clone(),
            target: Target::None,
        }));
        let status = format!(
            "Applied {count} replacements{}",
            if report.errors.is_empty() {
                String::new()
            } else {
                format!(" · {} skipped", report.errors.len())
            }
        );
        self.show_rows(2, status.clone(), rows);
        self.notice(status);
    }

    fn active_symbol_path(&self, i: usize) -> PathBuf {
        self.buffers[i]
            .path
            .clone()
            .unwrap_or_else(|| PathBuf::from(&self.buffers[i].name))
    }
    fn active_symbol_index(&self, i: usize, path: &Path) -> Rc<Index> {
        let buffer = &self.buffers[i];
        let language = self.edit().language().to_ascii_lowercase();
        LOCAL_SYMBOLS.with_borrow_mut(|cached| {
            if let Some(c) = cached
                && c.id == buffer.id
                && c.revision == buffer.rev
                && state::key(&c.path) == state::key(&path)
                && c.language == language
            {
                return c.index.clone();
            }
            let key = match language.as_str() {
                "c++" => "cpp",
                "c#" => "csharp",
                "typescript"
                    if path
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("tsx")) =>
                {
                    "tsx"
                }
                other => other,
            };
            let index = Rc::new(project::index_file_as(
                path,
                &self.buffer_text(i),
                Some(key),
            ));
            *cached = Some(LocalSymbols {
                id: buffer.id,
                revision: buffer.rev,
                path: path.into(),
                language,
                index: index.clone(),
            });
            index
        })
    }
    pub(super) fn project_symbols(&mut self, workspace: bool) {
        if workspace {
            if self.index.symbols.is_empty() {
                self.notice("No indexed definitions in this workspace");
                return;
            }
            let root = self.root_dir();
            let labels = self
                .index
                .symbols
                .iter()
                .map(|s| {
                    format!(
                        "{}    {}  ·  {}:{}",
                        s.name,
                        s.kind,
                        short_path(&root, &s.path),
                        s.line
                    )
                })
                .collect::<Vec<_>>();
            if let Some(n) = forms::pick(self.h, "Go to workspace symbol", &labels, "") {
                let s = self.index.symbols[n].clone();
                self.jump_path(&s.path, s.offset);
            }
        } else {
            self.prefs.sidebar = true;
            let Some(i) = self.current() else {
                self.show_rows(1, "Open a file for its symbols".into(), vec![]);
                return;
            };
            if self.buffers[i].large || self.edit().len() > project::MAX_SYMBOL_SOURCE {
                self.show_rows(
                    1,
                    "Whole-file symbols require a source file under 4 MiB".into(),
                    vec![],
                );
                return;
            }
            let file = self.active_symbol_path(i);
            let id = self.buffers[i].id;
            let index = self.active_symbol_index(i, &file);
            let rows = index
                .symbols
                .iter()
                .map(|s| Row {
                    label: format!("{}    {} · {}", s.name, s.kind, s.line),
                    target: Target::BufferAt(id, s.offset),
                })
                .collect();
            self.show_rows(
                1,
                format!("{} symbols · {}", index.symbols.len(), self.buffers[i].name),
                rows,
            );
        }
    }
    pub(super) fn project_goto(&mut self, references: bool) {
        let Some(i) = self.current() else {
            return;
        };
        let ed = self.edit();
        let here = ed.caret();
        let a = ed.send(sci::WORDSTARTPOSITION, here, 1).max(0) as usize;
        let b = ed.send(sci::WORDENDPOSITION, here, 1).max(0) as usize;
        let name = ed.text_range(a, b);
        if name.trim().is_empty() {
            self.notice("Place the caret on an identifier");
            return;
        }
        let local_path = self.active_symbol_path(i);
        let query = |idx: &Index| {
            if references {
                idx.references(&name)
            } else {
                idx.definitions(&name)
            }
        };
        let mut matches = query(&self.index);
        if !self.buffers[i].large && self.edit().len() <= project::MAX_SYMBOL_SOURCE {
            let local = self.active_symbol_index(i, &local_path);
            let key = state::key(&local_path);
            matches.retain(|s| state::key(&s.path) != key);
            matches.splice(0..0, query(&local));
        }
        if matches.is_empty() {
            self.notice(format!(
                "No {} for {name}",
                if references {
                    "indexed uses"
                } else {
                    "definitions"
                }
            ));
            return;
        }
        if !references && matches.len() == 1 {
            let s = &matches[0];
            if state::key(&s.path) == state::key(&local_path) {
                self.go(s.offset, 0);
            } else {
                self.jump_path(&s.path, s.offset);
            }
            return;
        }
        let root = self.root_dir();
        let rows = matches
            .iter()
            .map(|s| Row {
                label: format!("{}:{}    {}", short_path(&root, &s.path), s.line, s.name),
                target: if state::key(&s.path) == state::key(&local_path) {
                    Target::BufferAt(self.buffers[i].id, s.offset)
                } else {
                    Target::Location(s.path.clone(), s.offset)
                },
            })
            .collect();
        self.prefs.sidebar = true;
        self.show_rows(
            2,
            format!(
                "{} {} for {name}",
                matches.len(),
                if references { "uses" } else { "definitions" }
            ),
            rows,
        );
    }

    pub(super) fn project_build(&mut self, configure: bool) {
        let root = self.root_dir();
        let default = self
            .prefs
            .builds
            .get(&root)
            .cloned()
            .or_else(|| project::default_build(&root))
            .unwrap_or_default();
        let command = if configure || default.is_empty() {
            let Some(input) = forms::input(
                self.h,
                "Build command",
                &format!("Run in {}", root.display()),
                &default,
            ) else {
                return;
            };
            if input.trim().is_empty() {
                self.notice("Enter a command to run");
                return;
            }
            self.prefs.builds.insert(root.clone(), input.clone());
            self.save_prefs();
            if configure {
                self.notice("Build command saved · F7 to run");
                return;
            }
            input
        } else {
            default
        };
        let (job, cancel) = self.new_job("build");
        let tx = self.tx.clone();
        self.prefs.sidebar = true;
        self.show_rows(2, format!("Running  ·  {command}"), vec![]);
        thread::spawn(move || {
            let result = project::build(&root, &command, &cancel);
            let _ = tx.send(Worker::Build(job, result));
        });
    }
    pub(super) fn project_diagnostics(&mut self, report: BuildReport) {
        let root = self.root_dir();
        self.diagnostics = report.diagnostics;
        let mut rows: Vec<_> = self
            .diagnostics
            .iter()
            .map(|d| Row {
                label: format!(
                    "{}:{}:{}  {}",
                    short_path(&root, &d.path),
                    d.line,
                    d.column,
                    d.message
                ),
                target: Target::Line(d.path.clone(), d.line, d.column),
            })
            .collect();
        rows.extend(report.output.lines().take(3000).map(|line| Row {
            label: line.into(),
            target: Target::None,
        }));
        if report.output.lines().count() > 3000 {
            rows.push(Row {
                label: "Output panel displays the first 3,000 lines".into(),
                target: Target::None,
            });
        }
        let status = if report.cancelled {
            "Canceled"
        } else if report.success {
            "Finished"
        } else {
            "Command failed"
        };
        let title = format!("{status} · {} diagnostics", self.diagnostics.len());
        self.prefs.sidebar = true;
        self.show_rows(2, title.clone(), rows);
        self.notice(title);
    }
    pub(super) fn project_tool(&mut self, configure: bool) {
        let path = state::profile().join("tools.json");
        if let Err(e) = ToolConfig::ensure_example(&path) {
            self.notice(e);
            return;
        }
        if configure {
            self.open_path(path);
            return;
        }
        let config = match ToolConfig::load(&path) {
            Ok(c) => c,
            Err(e) => {
                self.notice(e);
                return;
            }
        };
        let labels = config
            .tools
            .iter()
            .map(|t| format!("{}    {}", t.name, t.shortcut.as_deref().unwrap_or("")))
            .collect::<Vec<_>>();
        if labels.is_empty() {
            self.notice("Add a tool under Tools → Configure external tools");
            return;
        }
        if let Some(i) = forms::pick(self.h, "Run external tool", &labels, "") {
            self.run_project_tool(config.tools[i].clone());
        }
    }
    pub(super) fn project_tool_by_shortcut(&mut self, chord: &str) -> bool {
        if chord.is_empty() {
            return false;
        }
        let tool = SHORTCUTS.with_borrow_mut(|cached| {
            if cached
                .as_ref()
                .is_none_or(|(at, _)| at.elapsed() >= Duration::from_secs(2))
            {
                let config =
                    ToolConfig::load(&state::profile().join("tools.json")).unwrap_or_default();
                *cached = Some((Instant::now(), config));
            }
            cached
                .as_ref()?
                .1
                .tools
                .iter()
                .find(|tool| {
                    tool.shortcut
                        .as_deref()
                        .is_some_and(|s| s.eq_ignore_ascii_case(chord))
                })
                .cloned()
        });
        if let Some(t) = tool {
            self.run_project_tool(t);
            true
        } else {
            false
        }
    }
    pub(crate) fn run_project_tool(&mut self, tool: UserTool) {
        let filter = tool.mode == ToolMode::Filter;
        let root = self.root_dir();
        let (i, id, rev, file) = if let Some(i) = self.current() {
            let b = &self.buffers[i];
            (Some(i), b.id, b.rev, b.path.clone())
        } else {
            (None, 0, 0, None)
        };
        let range = self.wide_span().unwrap_or_else(|| {
            let (a, b) = self.edit().selection();
            crate::events::range(self.active, a.min(b) as u64, a.max(b) as u64)
        });
        if filter && (i.is_none() || range.is_empty()) {
            self.notice("Select text to run this filter");
            return;
        }
        let size = range.end - range.start;
        if size > 16 * 1024 * 1024 {
            self.notice("External tools accept selections up to 16 MiB");
            return;
        }
        let snapshot = if let Some(i) = i {
            if let Err(e) = self.sync_buffer(i) {
                self.notice(e);
                return;
            }
            Some(self.buffers[i].doc.clone())
        } else {
            None
        };
        let (start, end) = (range.start as usize, range.end as usize);
        let (job, cancel) = self.new_job("tool");
        let tx = self.tx.clone();
        self.notice(format!("Running {}", tool.name));
        thread::spawn(move || {
            let result = (|| {
                let bytes = match snapshot {
                    Some(d) => d
                        .read(range.start, size as usize)
                        .map_err(|e| e.to_string())?,
                    None => vec![],
                };
                let selected = std::str::from_utf8(&bytes)
                    .map_err(|_| "The selection contains invalid UTF-8")?;
                project::run_tool(&tool, &root, file.as_deref(), selected, &cancel)
            })();
            let _ = tx.send(Worker::Tool(job, id, rev, start, end, filter, result));
        });
    }
    pub(super) fn project_tool_result(
        &mut self,
        id: u64,
        rev: u64,
        start: usize,
        end: usize,
        filter: bool,
        result: Result<ToolResult, String>,
    ) {
        let report = match result {
            Ok(r) => r,
            Err(e) => {
                self.notice(e);
                return;
            }
        };
        if !filter || !report.success {
            self.project_diagnostics(report);
            return;
        }
        let Some(i) = self.buffers.iter().position(|b| b.id == id && b.rev == rev) else {
            self.notice("The document changed while the filter ran. Run the filter again.");
            return;
        };
        match self.change_open(i, vec![(start, end, report.output)]) {
            Ok(()) => self.notice("Selection replaced"),
            Err(e) => self.notice(e),
        }
    }
}
