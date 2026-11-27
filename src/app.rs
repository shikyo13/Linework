use crate::{
    commands,
    events::{self, Binding, Event, quiet},
    forms,
    frame::{self, Frame, Hit},
    sci::*,
    state::{self, FileState, Prefs, Step},
    win::*,
};
use linework_editor::{CaretStyle, Editor, Theme, detect_language, sci as sn};
use linework_project::{ApplyReport, BuildReport, Diagnostic, Index, SearchReport, ToolResult};
use linework_storage::{Document, SearchOptions};
use std::{
    collections::{BTreeSet, HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io,
    ops::Range,
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender, channel},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};
/// The search panel's fields: their control numbers and the hint each shows while empty.
pub(crate) const PANEL_CUES: [(i32, &str); 8] = [
    (640, "Find"),
    (641, "Replace with"),
    (642, "Files: *.rs, src/**"),
    (643, "Skip: tests/**, *.min.js"),
    (644, "Types: rust, ts, md"),
    (645, "Near: a pattern within some lines"),
    (646, "Files that also have"),
    (647, "Files that lack"),
];
const LARGE: u64 = 24 * 1024 * 1024;
const PAGE: usize = 384 * 1024;

// Many workers can finish between two UI messages. One outstanding wake covers
// all of them, including preview renders. Closing disarms every sender clone.
#[derive(Clone)]
pub(crate) struct UiWake(Arc<Mutex<(H, bool)>>);
impl UiWake {
    fn new(host: H) -> Self {
        Self(Arc::new(Mutex::new((host, false))))
    }
    pub(crate) fn notify(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.0 != 0 && !state.1 {
            state.1 = unsafe { PostMessageW(hwnd(state.0), events::WORK_READY, 0, 0) } != 0;
        }
    }
    fn received(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).1 = false;
    }
    fn close(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = (0, false);
    }
}
#[derive(Clone)]
pub(crate) struct WorkerSender {
    queue: Sender<Worker>,
    pub(crate) wake: UiWake,
}
impl WorkerSender {
    pub(crate) fn send(&self, result: Worker) -> Result<(), std::sync::mpsc::SendError<Worker>> {
        self.queue.send(result)?;
        self.wake.notify();
        Ok(())
    }
}
enum AfterSave {
    History { pane: usize, redo: bool },
    Save,
}
struct TabView {
    revision: u64,
    binding: Binding,
    caret: u64,
    top_line: usize,
    wrapped_line: usize,
    horizontal: usize,
    selections: Vec<u8>,
}
impl TabView {
    fn capture(editor: &Editor, pane: usize, revision: u64) -> Self {
        let binding = events::binding(pane);
        let first = editor.send(GETFIRSTVISIBLELINE, 0, 0).max(0) as usize;
        let top_line = editor.send(DOCLINEFROMVISIBLE, first, 0).max(0) as usize;
        let start = editor.send(VISIBLEFROMDOCLINE, top_line, 0).max(0) as usize;
        // Paged selections have their own file-wide representation. For resident
        // documents Scintilla also preserves the main range, orientation and virtual space.
        let mut selections = Vec::new();
        if !binding.large {
            // Both calls run on the owning UI thread. The null query reports
            // the exact byte length; reserve the extra byte for the terminator.
            let len = unsafe { editor.send_ptr(sn::SCI_GETSELECTIONSERIALIZED, 0, 0) };
            if len > 0 {
                selections.resize(len as usize + 1, 0);
                unsafe { editor.send_ptr(sn::SCI_GETSELECTIONSERIALIZED, 0, selections.as_mut_ptr() as isize); }
            }
        }
        Self {
            revision, binding, caret: events::point(pane, editor.caret() as u64),
            top_line, wrapped_line: first.saturating_sub(start),
            horizontal: editor.send(sn::SCI_GETXOFFSET, 0, 0).max(0) as usize,
            selections,
        }
    }
    fn restore(&self, editor: &Editor, pane: usize) {
        let binding = events::binding(pane);
        if binding.large != self.binding.large || binding.base != self.binding.base { return; }
        quiet(|| {
            if !self.selections.is_empty() {
                // The captured buffer is NUL-terminated and lives through dispatch.
                unsafe { editor.send_ptr(sn::SCI_SETSELECTIONSERIALIZED, 0, self.selections.as_ptr() as isize); }
            }
            let line = editor.send(VISIBLEFROMDOCLINE, self.top_line, 0).max(0) as usize;
            let wraps = editor.send(sn::SCI_WRAPCOUNT, self.top_line, 0).max(1) as usize;
            editor.send(SETFIRSTVISIBLELINE, line + self.wrapped_line.min(wraps - 1), 0);
            editor.send(sn::SCI_SETXOFFSET, self.horizontal, 0);
        });
    }
}
pub struct Buffer {
    pub id: u64,
    pub name: String,
    pub path: Option<PathBuf>,
    pub doc: Document,
    pub large: bool,
    pub dirty: bool,
    pub rev: u64,
    pub lang: String,
    pub manual: bool,
    pub native: isize,
    pub synced: u64,
    pub marks: BTreeSet<u64>,
    pub last: u64,
    tab_views: [Option<TabView>; 2],
    save_epoch: Option<u64>,
    pub saving: bool,
    after_save: VecDeque<AfterSave>,
    recovery_cancel: Arc<AtomicBool>,
    pub follow: bool,
    following_read: bool,
    preparing: Option<Arc<AtomicBool>>,
    pending_edits: Vec<(u64, u64, Vec<u8>)>,
    pub recovered_rev: Option<u64>,
    pub forced_dirty: bool,
}
impl Buffer {
    fn move_marks(&mut self, pos: u64, del: u64, add: u64) {
        if self.marks.is_empty() {
            return;
        }
        self.marks = self
            .marks
            .iter()
            .map(|at| {
                if *at <= pos {
                    *at
                } else if *at < pos + del {
                    pos
                } else {
                    at - del + add
                }
            })
            .collect();
    }
}
#[derive(Clone)]
pub enum Target {
    Location(PathBuf, u64),
    BufferAt(u64, u64),
    Line(PathBuf, u64, u64),
    Hit(usize),
    File(PathBuf),
    Directory(PathBuf),
    DirectoryPage(usize),
    Parent,
    None,
}
#[derive(Clone)]
pub struct Row {
    pub label: String,
    pub target: Target,
}
pub enum Worker {
    Prepared(u64, u64, Result<(), String>),
    PrepareProgress(u64, u64, u64),
    Follow(u64, u64, io::Result<Option<Document>>),
    Directory(u64, PathBuf, io::Result<Vec<Row>>),
    DiskChanged(Vec<(u64, u64)>),
    Compared(u64, Result<crate::compare::Comparison, String>),
    Global(global_edit::Reply),
    CursorEdit(global_edit::EditReply),
    Lines(
        u64,
        u64,
        u64,
        usize,
        usize,
        Result<Option<(String, BulkView)>, String>,
    ),
    Checkpoint(
        checkpoint::Versions,
        Vec<FileState>,
        Vec<PathBuf>,
        Vec<PathBuf>,
        Result<(), String>,
    ),
    Info(String),
    /// A sort through lw finished: the job, document, revision, sorted range and result.
    Sorted(u64, u64, u64, Range<u64>, sort_lines::SortResult),
    Recoded(u64, u64, Result<Document, String>),
    Eol(
        u64,
        u64,
        u64,
        &'static str,
        Result<(Document, BulkView), String>,
    ),
    Open(PathBuf, Result<Document, String>, usize, Option<FileState>),
    LongColumn(u64, u64, u64, u64, Result<u64, String>),
    Saved(u64, Result<Document, io::Error>, PathBuf, bool),
    Index(u64, Index),
    Report(u64, Result<SearchReport, String>),
    Build(u64, Result<BuildReport, String>),
    Applied(ApplyReport),
    /// How far a run of zero bytes reaches: the measurement, document, revision, run, and whether it is done.
    Zeros(u64, u64, u64, Range<u64>, bool),
    ZerosFailed(u64, String),
    /// A folder replace undone, or why not.
    Undone(Result<linework_project::change::Undone, String>),
    Tool(
        u64,
        u64,
        u64,
        usize,
        usize,
        bool,
        Result<ToolResult, String>,
    ),
    Found(u64, u64, u64, Result<Option<Range<u64>>, String>, bool),
    Replaced(u64, u64, u64, Result<(Document, usize, BulkView), String>),
}
pub struct App {
    pub h: H,
    pub editors: [Editor; 2],
    pub(crate) bridge: Editor,
    scratch: [isize; 2],
    views: global_edit::ViewState,
    pub assigned: [Option<u64>; 2],
    pub active: usize,
    pub split: bool,
    pub buffers: Vec<Buffer>,
    next_id: u64,
    untitled: u32,
    tab_first: usize,
    pref_baseline: std::cell::RefCell<Prefs>,
    pub prefs: Prefs,
    shortcuts: std::collections::HashMap<String, &'static commands::Command>,
    pub side: u8,
    pub rows: Vec<Row>,
    pub report_label: String,
    pub index: Index,
    pub report: Option<SearchReport>,
    pub checked: Vec<bool>,
    pub diagnostics: Vec<Diagnostic>,
    pub commit_paths: Vec<PathBuf>,
    directory_job: u64,
    directory_loading: Option<(PathBuf, Arc<AtomicBool>)>,
    directory_rows: Vec<Row>,
    pub dir: PathBuf,
    pub(crate) tx: WorkerSender,
    rx: Receiver<Worker>,
    refresh_after_input: bool,
    superseded: Option<&'static str>,
    jobs: std::collections::HashMap<&'static str, (u64, Arc<AtomicBool>)>,
    job: u64,
    /// What the search panel searches.
    pub(crate) search_scope: SearchScope,
    /// The search panel's fields: find, replace, files, skip, types, near, also has, lacks.
    pub panel_fields: [H; 8],
    /// Long runs of zero bytes in the parts of large files on screen.
    pub(crate) zeros: zero_runs::Zeros,
    /// What the search panel asks for.
    pub panel: linework_project::ask::Panel,
    /// When the folder search should run again after typing stops.
    pub search_due: Option<Instant>,
    /// For each results row, the matches it holds, as indices into the report's hits.
    pub row_hits: Vec<Range<usize>>,
    /// The panel's files, conditions and context fields are shown.
    pub panel_more: bool,
    /// The search view holds the search panel and its results, not other results.
    pub search_view: bool,
    /// The next preview's results open the review window: the preview was asked
    /// for, not re-run because the search panel's text changed.
    pub review_next: bool,
    /// The last folder search's totals and its lw command, as the panel shows them.
    pub search_summary: String,
    /// The order the search results are shown in.
    pub result_sort: crate::result_order::ResultSort,
    status_left: H,
    status_right: H,
    pane_headers: [H; 2],
    pub list: H,
    scroll: [H; 2],
    pub preview_mode: bool,
    pub(crate) document_preview: crate::preview::Pane,
    /// Where the last undo or redo changed text, while the editor shows it.
    history_flash: edit_cues::HistoryFlash,
    /// Edit heat and pair glow, as the editor panes show them.
    polish: polish::Polish,
    /// A paste command is running and its changes are still arriving.
    pub(crate) pasting: bool,
    /// The document and revision the latest file comparison read as its revised side.
    pub(crate) compared: Option<(u64, u64)>,
    pub recording: bool,
    pub playing: bool,
    pub macro_steps: Vec<Step>,
    pub exiting: bool,
    status: String,
    status_at: Instant,
    displayed_title: String,
    changed_at: Instant,
    spell_at: Instant,
    tail_at: Instant,
    auto_at: Instant,
    saved_state: bool,
    checkpoint_running: bool,
    quit_after_checkpoint: bool,
    margin_cache: [std::cell::Cell<Option<(u64, u64, usize, usize, u64, u64)>>; 2],
    wide_selection: [Option<(u64, u64, Range<u64>, Range<u64>)>; 2],
    pending: HashSet<String>,
    closed: Vec<PathBuf>,
    history: Vec<(u64, u64)>,
    future: Vec<(u64, u64)>,
    recovery_dir: PathBuf,
    restored: Option<PathBuf>,
    restore_guard: Option<File>,
    claimed_drafts: HashSet<String>,
    _lock: File,
    last_find: Option<(u64, u64, u64, String)>,
    last_job_hit: Option<Range<u64>>,
    pending_jump: std::collections::HashMap<String, (u64, u64)>,
    /// A jump waiting for the line count: line, column, and where an
    /// estimate (`Document::estimate_line_start`) already put the caret.
    indexed_jump: std::collections::HashMap<u64, (u64, u64, Option<u64>)>,
    column_request: Arc<std::sync::atomic::AtomicU64>,
    /// Opened files whose line count waits for their first paint, and since when.
    pub(crate) count_after_paint: Vec<(u64, Instant)>,
}
impl App {
    pub fn new(h: H) -> Result<Self, String> {
        let (prefs, settings_warning) = state::load_prefs();
        if linework_storage::set_detection(prefs.encoding.auto_detect, &prefs.encoding.fallback).is_err() {
            let _ = linework_storage::set_detection(prefs.encoding.auto_detect, "windows-1252");
        }
        // Line counts of very large files, so opening one again skips the count.
        linework_storage::set_index_cache(Some(&state::profile().join("line-index")));
        // Records that let a small change be written straight into a large
        // file. One left by a crash is settled here, before any file is opened.
        linework_storage::set_save_journal(Some(&state::profile().join("save-records")));
        LIGHT.set(prefs.light);
        dark_chrome(h, !prefs.light);
        let editors = [Editor::create(h, 700)?, Editor::create(h, 701)?];
        for e in &editors {
            crate::editor_resize::install(e.hwnd());
            e.set_dpi(dpi(h) as u32);
        }
        let bridge = Editor::create(h, 702)?;
        show(bridge.hwnd(), false);
        let scratch = [
            msg(editors[0].hwnd(), CREATEDOCUMENT, 0, 0),
            msg(editors[1].hwnd(), CREATEDOCUMENT, 0, 0),
        ];
        let list = child(
            h,
            601,
            "LISTBOX",
            "",
            WS_VISIBLE
                | WS_TABSTOP
                | WS_VSCROLL
                | LBS_OWNERDRAWFIXED as u32
                | LBS_HASSTRINGS as u32
                | LBS_NOTIFY as u32
                | LBS_NOINTEGRALHEIGHT as u32,
        );
        crate::frame::controls::buffer_list(list, false);
        // The search panel's fields, shown in the sidebar's search view.
        let panel_fields = PANEL_CUES.map(|(id, cue)| {
            let field = child(h, id, "EDIT", "", WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32);
            msg(field, 0x1501, 1, wide(cue).as_ptr() as _);
            field
        });
        let status_left = child(h, 620, "STATIC", "Ready", WS_VISIBLE | 0x280);
        let status_right = child(h, 621, "STATIC", "", WS_VISIBLE | 0x282);
        // The labels expose the same pane state to MSAA and to the owner-drawn UI.
        let pane_headers = [622, 623].map(|id| child(h, id, "STATIC", "", 0x0d | 0x100));
        for status in [status_left, status_right] {
            msg(status, WM_SETFONT, font(size(h, 11), 400, true) as usize, 1);
        }
        let scroll = [
            child(h, 710, "SCROLLBAR", "", SBS_VERT as _),
            child(h, 711, "SCROLLBAR", "", SBS_VERT as _),
        ];
        for child in [
            list,
            editors[0].hwnd(),
            editors[1].hwnd(),
            scroll[0],
            scroll[1],
        ]
        .into_iter()
        .chain(panel_fields)
        {
            crate::chrome::theme_child(child, prefs.light);
        }
        let recovery_dir = state::profile().join("sessions").join(format!(
            "{}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            std::process::id()
        ));
        fs::create_dir_all(&recovery_dir).map_err(|e| e.to_string())?;
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .share_mode(0)
            .open(recovery_dir.join("active.lock"))
            .map_err(|e| e.to_string())?;
        let (queue, rx) = channel();
        let tx = WorkerSender { queue, wake: UiWake::new(h) };
        let now = Instant::now();
        let mut a = Self {
            h,
            editors,
            bridge,
            scratch,
            views: Default::default(),
            assigned: [None, None],
            active: 0,
            split: false,
            buffers: vec![],
            next_id: 1,
            untitled: 0,
            tab_first: 0,
            pref_baseline: std::cell::RefCell::new(prefs.clone()),
            shortcuts: commands::shortcut_index(&prefs.shortcuts),
            prefs,
            side: 0,
            rows: vec![],
            report_label: String::new(),
            index: Index::default(),
            report: None,
            checked: vec![],
            diagnostics: vec![],
            commit_paths: vec![],
            directory_job: 0,
            directory_loading: None,
            directory_rows: vec![],
            dir: PathBuf::new(),
            tx,
            rx,
            refresh_after_input: false,
            superseded: None,
            jobs: Default::default(),
            job: 0,
            search_scope: SearchScope::File,
            panel_fields,
            zeros: Default::default(),
            panel: Default::default(),
            search_due: None,
            row_hits: Vec::new(),
            panel_more: false,
            search_view: false,
            review_next: false,
            search_summary: String::new(),
            result_sort: Default::default(),
            status_left,
            status_right,
            pane_headers,
            list,
            scroll,
            preview_mode: false,
            document_preview: Default::default(),
            history_flash: Default::default(),
            polish: Default::default(),
            pasting: false,
            compared: None,
            recording: false,
            playing: false,
            macro_steps: vec![],
            exiting: false,
            status: "Ready".into(),
            status_at: now,
            displayed_title: String::new(),
            changed_at: now,
            spell_at: now,
            tail_at: now,
            auto_at: now,
            saved_state: true,
            checkpoint_running: false,
            quit_after_checkpoint: false,
            margin_cache: Default::default(),
            wide_selection: [None, None],
            pending: HashSet::new(),
            closed: vec![],
            history: vec![],
            future: vec![],
            recovery_dir,
            restored: None,
            restore_guard: None,
            claimed_drafts: HashSet::new(),
            _lock: lock,
            last_find: None,
            last_job_hit: None,
            pending_jump: Default::default(),
            indexed_jump: Default::default(),
            column_request: Default::default(),
            count_after_paint: Vec::new(),
        };
        if let Some(path) = &a.prefs.dictionary {
            for e in &a.editors {
                let _ = e.set_dictionary(Some(path));
            }
        }
        a.macro_steps = a.prefs.last_macro.clone();
        for p in 0..2 {
            a.style(p);
        }
        a.apply_appearance();
        a.apply_user_snippets();
        a.rebuild_side();
        if let Some(message) = settings_warning {
            alert(h, "Recovered settings", &message);
        }
        Ok(a)
    }
    pub fn root_dir(&self) -> PathBuf {
        self.prefs
            .root
            .clone()
            .or_else(|| {
                self.current().and_then(|i| {
                    self.buffers[i]
                        .path
                        .as_ref()?
                        .parent()
                        .map(Path::to_path_buf)
                })
            })
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
    }
    pub fn current(&self) -> Option<usize> {
        self.assigned[self.active].and_then(|id| self.idx(id))
    }
    pub(crate) fn idx(&self, id: u64) -> Option<usize> {
        self.buffers.iter().position(|b| b.id == id)
    }
    pub fn edit(&self) -> &Editor {
        &self.editors[self.active]
    }
    pub fn pane_base(&self) -> u64 {
        events::binding(self.active).base
    }
    pub fn global_pos(&self) -> u64 {
        events::point(self.active, self.edit().caret() as u64)
    }
    pub fn total_len(&self) -> u64 {
        self.current().map_or(0, |i| {
            if self.buffers[i].large {
                self.buffers[i].doc.len()
            } else {
                self.edit().len() as u64
            }
        })
    }
    pub fn notice(&mut self, s: impl Into<String>) {
        let message = s.into();
        self.status = match self.superseded.take() {
            Some(kind) => format!("{message} · Previous {kind} request superseded"),
            None => message,
        };
        self.status_at = Instant::now();
        self.refresh();
    }
    pub fn new_job(&mut self, kind: &'static str) -> (u64, Arc<AtomicBool>) {
        self.job += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some((_, old)) = self.jobs.insert(kind, (self.job, cancel.clone())) {
            if !old.swap(true, Ordering::Relaxed) {
                self.superseded = Some(kind);
            }
        }
        (self.job, cancel)
    }
    fn current_job(&self, kind: &str, id: u64) -> bool {
        self.jobs
            .get(kind)
            .is_some_and(|(current, _)| *current == id)
    }
    pub(crate) fn cancel_job(&mut self, kind: &str) {
        if let Some((_, flag)) = self.jobs.remove(kind) {
            flag.store(true, Ordering::Relaxed);
        }
    }
    pub fn start(&mut self, args: Vec<crate::launch::Target>) {
        self.load_grammars();
        let root = self.prefs.root.clone();
        if let Some(r) = root.filter(|p| p.is_dir()) {
            self.dir = r;
            self.rebuild_side();
            self.project_index();
        }
        self.prune_drafts();
        self.restore(args.is_empty());
        self.open_targets(args);
        self.refresh();
    }
    fn open_targets(&mut self, args: Vec<crate::launch::Target>) {
        for target in args {
            if target.path.is_dir() {
                self.set_root(target.path)
            } else if let Some((line,column)) = target.position {
                self.remember_location();
                self.open_line(target.path, line, column);
            } else {
                self.open_path(target.path)
            }
        }
    }
    fn open_line(&mut self, path: PathBuf, line: u64, col: u64) {
        let key = state::key(&path);
        if let Some(i) = self.buffers.iter().position(|b| b.path.as_ref().is_some_and(|p| state::key(p)==key)) {
            self.select(i, self.active, None);
            self.goto_line_col(line, col);
        } else {
            self.pending_jump.insert(key, (line, col.max(1)));
            self.open_path(path);
        }
    }
    fn set_root(&mut self, path: PathBuf) {
        self.dir = path.clone();
        self.prefs.root = Some(path);
        self.prefs.sidebar = true;
        self.side = 0;
        self.report = None;
        self.index = Index::default();
        self.rebuild_side();
        self.project_index();
        self.layout();
    }
    pub(crate) fn pending_open(&self, p: &Path) -> bool {
        self.pending.contains(&state::key(p))
    }
    pub fn open_path(&mut self, path: PathBuf) {
        self.open_in(path, self.active, None);
    }
    fn open_in(&mut self, path: PathBuf, pane: usize, restore: Option<FileState>) {
        let path = std::path::absolute(&path).unwrap_or(path);
        if self
            .commit_paths
            .iter()
            .any(|p| state::key(p) == state::key(&path))
        {
            self.notice("Replacement in progress for this file");
            return;
        }
        if let Some(i) = self.buffers.iter().position(|b| {
            b.path
                .as_ref()
                .is_some_and(|p| state::key(p) == state::key(&path))
        }) {
            self.select(i, pane, None);
            return;
        }
        if !self.pending.insert(state::key(&path)) {
            return;
        }
        let tx = self.tx.clone();
        self.notice(format!(
            "Opening {}…",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        std::thread::spawn(move || {
            // The line count starts once the text is shown (`Worker::Open`).
            let result = Document::open_deferred(&path).map_err(|e| e.to_string());
            let _ = tx.send(Worker::Open(path, result, pane, restore));
        });
    }
    fn new_file(&mut self) {
        self.untitled += 1;
        let mut name = format!("Untitled {}", self.untitled);
        while self.buffers.iter().any(|b| b.name == name) {
            self.untitled += 1;
            name = format!("Untitled {}", self.untitled);
        }
        let doc = Document::empty_as(&self.prefs.encoding.new_files).unwrap_or_else(|_| Document::empty());
        self.add(doc, None, name, self.active, None);
    }
    fn add(
        &mut self,
        doc: Document,
        path: Option<PathBuf>,
        name: String,
        pane: usize,
        st: Option<FileState>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        let large = doc.len() > LARGE;
        let sample = doc
            .read(0, doc.len().min(8192) as usize)
            .unwrap_or_default();
        let detected = language_for(
            &self.prefs,
            path.as_deref(),
            &String::from_utf8_lossy(&sample),
        );
        let requested = st
            .as_ref()
            .map(|s| s.language.as_str())
            .filter(|s| !s.is_empty() && linework_editor::languages().iter().any(|n| n == *s));
        let manual = requested.is_some();
        let lang = requested.map(str::to_owned).unwrap_or(detected);
        let last = st.as_ref().map_or(0, |s| s.offset).min(doc.len());
        let mut native = 0;
        if !large {
            let bytes = match doc.read(0, doc.len() as usize) {
                Ok(b) => b,
                Err(e) => {
                    self.notice(e.to_string());
                    return;
                }
            };
            native = msg(self.bridge.hwnd(), CREATEDOCUMENT, 0, 0);
            quiet(|| {
                msg(self.bridge.hwnd(), SETDOCPOINTER, 0, native);
                self.bridge.set_bytes(&bytes);
                self.bridge.send(EMPTYUNDOBUFFER, 0, 0);
                self.bridge.send(SETSAVEPOINT, 0, 0);
            });
        }
        let dirty = doc.dirty() || st.as_ref().is_some_and(|s| s.recovery.is_some());
        let marks = st
            .as_ref()
            .map(|s| s.marks.iter().copied().collect())
            .unwrap_or_default();
        self.buffers.push(Buffer {
            id,
            name,
            path: path.clone(),
            doc,
            large,
            dirty,
            rev: 0,
            lang,
            manual,
            native,
            synced: 0,
            marks,
            last,
            tab_views: Default::default(),
            save_epoch: None,
            saving: false,
            after_save: VecDeque::new(),
            recovery_cancel: Arc::default(),
            follow: false,
            following_read: false,
            preparing: None,
            pending_edits: vec![],
            recovered_rev: None,
            forced_dirty: dirty,
        });
        if let Some(p) = path {
            self.prefs
                .recent
                .retain(|x| state::key(x) != state::key(&p));
            self.prefs.recent.insert(0, p);
            self.prefs.recent.truncate(40);
        }
        self.select(self.buffers.len() - 1, pane, Some(last));
        self.saved_state = false;
        self.rebuild_side();
    }
    pub(crate) fn indentation(&self, p: usize, lang: &str) {
        let width = self.prefs.tab_width.clamp(1, 16);
        let tabs = self
            .prefs
            .use_tabs
            .unwrap_or(matches!(lang, "Makefile" | "Go"));
        self.editors[p].set_indentation(width, tabs);
    }
    fn style(&self, p: usize) {
        let e = &self.editors[p];
        self.indentation(p, &e.language());
        let pref = &self.prefs;
        quiet(|| {
            e.set_theme(if pref.light {
                Theme::Light
            } else {
                Theme::Dark
            });
            e.set_caret(
                match pref.caret.as_str() {
                    "Block" => CaretStyle::Block,
                    "Wide" => CaretStyle::Wide,
                    _ => CaretStyle::Line,
                },
                pref.width,
                pref.blink,
            );
            e.set_current_line_highlight(pref.current_line);
            e.set_auto_indent(pref.auto_indent);
            e.set_auto_complete(pref.auto_complete);
            e.set_completion_threshold(pref.completion_threshold);
            e.set_occurrence_highlight(pref.highlight);
            e.send(SETWRAPMODE, pref.wrap as usize, 0);
            e.send(SETVIEWWS, pref.whitespace as usize, 0);
            e.send(SETZOOM, pref.zoom as usize, 0);
            e.send(SETMODEVENTMASK, 3, 0);
            e.send(USEPOPUP, 0, 0);
            let _ = e.set_spellcheck(pref.spell);
        });
        self.effect_styles(p);
    }
    pub(crate) fn select(&mut self, i: usize, p: usize, offset: Option<u64>) {
        if i >= self.buffers.len() {
            return;
        }
        if offset.is_none() && p == self.active && (p == 0 || self.split)
            && self.assigned[p] == Some(self.buffers[i].id) && events::binding(p).id == self.buffers[i].id
        {
            focus(self.editors[p].hwnd());
            return;
        }
        if self.playing && (p != self.active || self.assigned[p] != Some(self.buffers[i].id)) {
            self.stop_macro();
        }
        if self.recording && self.assigned[self.active] != Some(self.buffers[i].id) {
            self.macro_command("record");
        }
        if let Some(old) = self.assigned[p].and_then(|id| self.idx(id)) {
            self.buffers[old].last = events::point(p, self.editors[p].caret() as u64);
            if old != i && events::binding(p).id == self.buffers[old].id {
                self.buffers[old].tab_views[p] =
                    Some(TabView::capture(&self.editors[p], p, self.buffers[old].rev));
            }
        }
        self.active = p;
        if p == 1 {
            self.split = true
        };
        // A search of the current file follows the file shown.
        if self.assigned[p] != Some(self.buffers[i].id)
            && self.search_scope == SearchScope::File
            && self.search_panel_shown()
            && !self.panel.text.is_empty()
        {
            self.search_due = Some(Instant::now());
        }
        self.assigned[p] = Some(self.buffers[i].id);
        let saved_view = self.buffers[i].tab_views[p].take()
            .filter(|v| offset.is_none() && v.revision == self.buffers[i].rev);
        let at = offset.or_else(|| saved_view.as_ref().map(|v| v.caret))
            .unwrap_or(self.buffers[i].last)
            .min(if self.buffers[i].large {
                self.buffers[i].doc.len()
            } else {
                usize::MAX as u64
            });
        self.bind(p, i, at);
        if let Some(view) = saved_view { view.restore(&self.editors[p], p); }
        self.tab_first = self.tab_first.min(i);
        let (width, _) = client(self.h);
        let left = if self.prefs.sidebar {
            self.sidebar_width()
        } else {
            frame::RAIL
        };
        let available = (width * 96 / dpi(self.h) - left - frame::tab_reserve(self.preview_kind().is_some())).max(1);
        let mut used = 0;
        let mut first = i;
        for index in (self.tab_first..=i).rev() {
            used += frame::tab_width(&self.buffers[index].name);
            if used + if index > 0 { 25 } else { 0 } > available && index < i {
                break;
            }
            first = index;
        }
        self.tab_first = first;
        invalidate(self.list);
        self.layout();
        focus(self.editors[p].hwnd());
        self.saved_state = false;
    }
    pub(crate) fn bind(&mut self, p: usize, i: usize, at: u64) {
        let at = if self.buffers[i].large {
            self.buffers[i].doc.char_boundary(at)
        } else {
            at
        };
        self.polish_leaving(p);
        crate::npp::switch_pane(self, p, self.buffers[i].large);
        self.margin_cache[p].set(None);
        self.wide_selection[p] = None;
        let b = &self.buffers[i];
        let e = &self.editors[p];
        let old = events::binding(p);
        let mut base = 0;
        let mut bound_map = Default::default();
        let mut caret = at as usize;
        // A long run of zero bytes in the part shown, in the document's own positions.
        let mut zeros = None;
        quiet(|| {
            if b.large {
                let projected = self.projection(b.id, b.rev);
                let map = projected
                    .as_ref()
                    .map(|(map, _)| map.clone())
                    .unwrap_or_default();
                let view = projected.as_ref().map_or(&b.doc, |(_, view)| view);
                let visible = map.visible(at);
                let (offset, bytes) = window_bytes(view, visible).unwrap_or_else(|_| (0, vec![]));
                base = map.physical(offset);
                caret = visible.saturating_sub(offset) as usize;
                zeros = zero_runs::long_zeros(&bytes, caret)
                    .map(|r| map.physical(offset + r.start as u64)..map.physical(offset + r.end as u64));
                bound_map = map;
                msg(e.hwnd(), SETDOCPOINTER, 0, self.scratch[p]);
                e.send(SETREADONLY, 0, 0);
                e.set_bytes(&bytes);
                e.send(EMPTYUNDOBUFFER, 0, 0);
            } else {
                msg(e.hwnd(), SETDOCPOINTER, 0, b.native);
                if old.id != b.id {
                    e.document_changed_outside_view(0);
                }
            }
            if old.id != b.id || e.language() != b.lang || b.large {
                e.set_language(&b.lang);
            }
            self.indentation(p, &b.lang);
            e.send(
                SETEOLMODE,
                match b.doc.newline() {
                    "\r\n" => 0,
                    "\r" => 1,
                    _ => 2,
                },
                0,
            );
            e.send(SETAUTOMATICFOLD, if b.large { 0 } else { 7 }, 0);
            e.send(SETVSCROLLBAR, (!b.large) as usize, 0);
            e.send(SETMARGINTYPEN, 0, if b.large { 4 } else { 1 });
            if b.large {
                e.send(SETMARGINWIDTHN, 0, size(self.h, 92) as isize);
            } else {
                e.refresh_margins();
            }
            e.send(
                SETREADONLY,
                (b.saving || b.follow || b.preparing.is_some()) as usize,
                0,
            );
            e.set_selection(caret.min(e.len()), caret.min(e.len()));
            e.send(SCROLLCARET, 0, 0);
        });
        events::install(
            p,
            Binding {
                id: b.id,
                large: b.large,
                base,
            },
            bound_map,
        );
        self.margins(p);
        self.sync_marks(p);
        self.paint_bookmarks(i, p);
        self.paint_global(p);
        self.paint_fold_headers(i, p);
        self.polish_bound(p);
        self.zeros_in_view(p, i, zeros);
    }
    fn margins(&self, p: usize) {
        let Some(i) = self.assigned[p].and_then(|id| self.idx(id)) else {
            return;
        };
        let b = &self.buffers[i];
        let e = &self.editors[p];
        if !b.large {
            return;
        }
        let base = events::binding(p).base;
        let first = e
            .send(
                DOCLINEFROMVISIBLE,
                e.send(GETFIRSTVISIBLELINE, 0, 0) as _,
                0,
            )
            .max(0) as usize;
        let lines = e.send(LINESONSCREEN, 0, 0).clamp(2, 150) as usize;
        let ready = base <= b.doc.indexed_bytes();
        let sig = (b.id, base, first, lines, b.rev, ready as u64);
        if self.margin_cache[p].get() != Some(sig) {
            self.margin_cache[p].set(Some(sig));
            e.send(SETMARGINTYPEN, 0, 4);
            e.send(SETMARGINWIDTHN, 0, size(self.h, 92) as isize);
            let first_global = b.doc.line_of(base);
            let folded = events::view(p, b.doc.len()) != b.doc.len();
            for n in first..(first + lines + 2).min(e.send(GETLINECOUNT, 0, 0) as usize) {
                let physical = if folded {
                    events::point(p, e.send(POSITIONFROMLINE, n, 0).max(0) as u64)
                } else {
                    base
                };
                let s = if physical <= b.doc.indexed_bytes() {
                    format!(
                        "{} \0",
                        if folded {
                            b.doc.line_of(physical) + 1
                        } else {
                            first_global + n as u64 + 1
                        }
                    )
                } else {
                    "… \0".into()
                };
                msg(e.hwnd(), MARGINSETTEXT, n, s.as_ptr() as isize);
            }
        }
        let pos = events::view(p, base) + e.caret() as u64;
        msg(self.scroll[p], 0xE2, 0, 30000);
        msg(
            self.scroll[p],
            0xE0,
            (u128::from(pos) * 30000 / u128::from(events::view(p, b.doc.len()).max(1))) as usize,
            1,
        );
    }
    pub fn go(&mut self, position: u64, len: usize) {
        let Some(i) = self.current() else { return };
        let pos = if self.buffers[i].large {
            self.buffers[i].doc.char_boundary(position)
        } else {
            position.min(self.total_len())
        };
        let p = self.active;
        self.wide_selection[p] = None;
        self.reveal_global(p, i, pos);
        let base = self.pane_base();
        if self.buffers[i].large
            && (pos < base
                || pos + len as u64 > events::point(p, self.edit().len() as u64)
                || pos == events::point(p, self.edit().len() as u64) && pos < self.total_len())
        {
            self.bind(p, i, pos);
        }
        if self.buffers[i].large
            && len > 0
            && pos.saturating_add(len as u64) > events::point(p, self.edit().len() as u64)
        {
            self.select_span(
                pos..pos.saturating_add(len as u64).min(self.total_len()),
                false,
            );
            return;
        }
        let e = self.edit();
        let a = events::offset(p, pos) as usize;
        e.set_selection(
            a.min(e.len()),
            (events::offset(p, pos.saturating_add(len as u64)) as usize).min(e.len()),
        );
        e.send(SCROLLCARET, 0, 0);
        self.refresh();
    }
    pub fn jump_path(&mut self, path: &Path, offset: u64) {
        if let Some(i) = self.buffers.iter().position(|b| {
            b.path
                .as_ref()
                .is_some_and(|p| state::key(p) == state::key(path))
        }) {
            self.select(i, self.active, Some(offset));
            return;
        }
        self.pending_jump.insert(state::key(path), (offset, 0));
        self.open_path(path.into());
    }
    pub fn buffer_text(&self, i: usize) -> String {
        if self.buffers[i].large {
            return String::new();
        }
        quiet(|| {
            msg(self.bridge.hwnd(), SETDOCPOINTER, 0, self.buffers[i].native);
            self.bridge.text()
        })
    }
    pub fn sync_buffer(&mut self, i: usize) -> Result<(), String> {
        if self.buffers[i].large || self.buffers[i].synced == self.buffers[i].rev {
            return Ok(());
        }
        let new = quiet(|| {
            msg(self.bridge.hwnd(), SETDOCPOINTER, 0, self.buffers[i].native);
            self.bridge.raw_bytes(0, self.bridge.len())
        });
        let b = &mut self.buffers[i];
        let old = b
            .doc
            .read(0, b.doc.len() as usize)
            .map_err(|e| e.to_string())?;
        let mut head = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
        while head > 0 && new.get(head).is_some_and(|x| x & 0xc0 == 0x80) {
            head -= 1;
        }
        let mut tail = old[head..]
            .iter()
            .rev()
            .zip(new[head..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        while tail > 0 && new.get(new.len() - tail).is_some_and(|x| x & 0xc0 == 0x80) {
            tail -= 1;
        }
        if head + tail < old.len() || head + tail < new.len() {
            b.doc
                .replace(
                    head as u64,
                    (old.len() - head - tail) as u64,
                    &new[head..new.len() - tail],
                )
                .map_err(|e| e.to_string())?
        }
        b.synced = b.rev;
        Ok(())
    }
    pub(crate) fn prepare_input(&mut self, i: usize) -> bool {
        if !self.buffers[i].large {
            return true;
        }
        if self.buffers[i].preparing.is_some() {
            return false;
        }
        match self.buffers[i].doc.prepare_without_copy() {
            Ok(true) => return true,
            Err(e) => {
                self.notice(e.to_string());
                return false;
            }
            Ok(false) => {}
        }
        let flag = Arc::new(AtomicBool::new(false));
        let b = &mut self.buffers[i];
        let (id, rev, mut doc, tx) = (b.id, b.rev, b.doc.clone(), self.tx.clone());
        b.preparing = Some(flag.clone());
        for p in 0..2 {
            if self.assigned[p] == Some(id) {
                self.editors[p].send(SETREADONLY, 1, 0);
            }
        }
        self.notice("Preparing a safe editable copy · Esc to cancel");
        std::thread::spawn(move || {
            let mut last = Instant::now();
            let r = doc
                .prepare_edit(|at, total| {
                    if flag.load(Ordering::Relaxed) {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "Editable copy cancelled",
                        ));
                    }
                    if last.elapsed() > Duration::from_millis(300) {
                        let _ = tx.send(Worker::PrepareProgress(id, at, total));
                        last = Instant::now();
                    }
                    Ok(())
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(Worker::Prepared(id, rev, r));
        });
        false
    }
    pub fn change_open(
        &mut self,
        i: usize,
        mut edits: Vec<(usize, usize, String)>,
    ) -> Result<(), String> {
        if edits.is_empty() {
            return Ok(());
        }
        if self.buffers[i].saving || self.buffers[i].follow {
            return Err("The document is currently read-only.".into());
        }
        if !self.prepare_input(i) {
            return Err("Preparing editable copy. Repeat this command when ready.".into());
        }
        edits.sort_by_key(|e| std::cmp::Reverse(e.0));
        let offsets = edits
            .iter()
            .map(|(a, b, s)| (*a as u64, (b - a) as u64, s.len() as u64))
            .collect::<Vec<_>>();
        if self.buffers[i].large {
            let mut patches = edits
                .into_iter()
                .map(|(a, b, s)| (a as u64, (b - a) as u64, s.into_bytes()))
                .collect::<Vec<_>>();
            patches.sort_by_key(|p| p.0);
            let mut candidate = self.buffers[i].doc.clone();
            candidate
                .replace_many(&patches)
                .map_err(|e| e.to_string())?;
            let changes = patches
                .iter()
                .map(|(at, del, text)| (*at, *del, text.len() as u64))
                .collect::<Vec<_>>();
            return self.commit_extension_snapshot(i, candidate, &changes);
        } else {
            quiet(|| {
                msg(self.bridge.hwnd(), SETDOCPOINTER, 0, self.buffers[i].native);
                self.bridge.send(BEGINUNDOACTION, 0, 0);
                for (a, b, s) in edits {
                    self.bridge.replace_range(a, b, &s)
                }
                self.bridge.send(ENDUNDOACTION, 0, 0);
            });
        }
        if !self.buffers[i].large {
            let earliest = offsets.last().map_or(0, |x| x.0 as usize);
            for p in 0..2 {
                if self.assigned[p] == Some(self.buffers[i].id) {
                    self.editors[p].document_changed_outside_view(earliest);
                }
            }
        }
        for (pos, del, add) in offsets {
            self.buffers[i].move_marks(pos, del, add);
        }
        self.buffers[i].rev += 1;
        self.buffers[i].dirty = true;
        self.track_edit();
        Ok(())
    }
    fn promote(&mut self, i: usize) {
        let native = self.buffers[i].native;
        self.buffers[i].large = true;
        self.reload_views(i);
        if native != 0 {
            quiet(|| {
                msg(self.bridge.hwnd(), SETDOCPOINTER, 0, self.scratch[0]);
                msg(self.bridge.hwnd(), RELEASEDOCUMENT, 0, native);
            });
            self.buffers[i].native = 0;
        }
        self.layout();
    }
    fn reload_views(&mut self, i: usize) {
        for p in 0..2 {
            if self.assigned[p] == Some(self.buffers[i].id) {
                let at = events::point(p, self.editors[p].caret() as u64);
                self.bind(p, i, at.min(self.buffers[i].doc.len()));
            }
        }
    }
    fn track_edit(&mut self) {
        self.saved_state = false;
        self.changed_at = Instant::now();
        self.last_find = None;
    }
    pub fn dispatch_pending(&mut self) {
        let mut groups = Vec::new();
        while let Some(event) = events::pop() {
            if let Event::Edit(_, binding, ..) = &event {
                if !groups.contains(&binding.id) {
                    if let Some(i) = self.idx(binding.id) {
                        self.buffers[i].doc.begin_undo_group();
                        groups.push(binding.id);
                    }
                }
            }
            self.event(event);
            if self.exiting {
                break;
            }
        }
        for id in groups {
            if let Some(i) = self.idx(id) {
                self.buffers[i].doc.end_undo_group();
            }
        }
        if !self.exiting {
            self.finish_paste();
            if self.refresh_after_input { self.refresh(); }
            if self.document_preview.take_update() { self.update_preview(); }
        }
    }
    pub fn drain_edits(&mut self) {
        let mut todo = Vec::new();
        events::EVENTS.with_borrow_mut(|q| {
            let mut keep = std::collections::VecDeque::new();
            while let Some(ev) = q.pop_front() {
                if matches!(ev, Event::Edit(..) | Event::Modified(..)) {
                    todo.push(ev)
                } else {
                    keep.push_back(ev)
                }
            }
            *q = keep
        });
        for e in todo {
            self.event(e)
        }
    }
}
impl Drop for App {
    fn drop(&mut self) {
        self.tx.wake.close();
    }
}
fn window_bytes(doc: &Document, at: u64) -> io::Result<(u64, Vec<u8>)> {
    let n = doc.len();
    let mut start = at
        .saturating_sub((PAGE / 3) as u64)
        .min(n.saturating_sub(PAGE as u64));
    let end = (start + PAGE as u64).min(n);
    if start > 0 && start <= doc.indexed_bytes() {
        let l = doc.line_start(doc.line_of(start));
        if start - l < 32768 {
            start = l;
        }
    }
    let mut bytes = doc.read(start, (end - start) as usize)?;
    let mut k = 0;
    while start > 0 && k < bytes.len() && k < 3 && (bytes[k] & 0xc0) == 0x80 {
        k += 1
    }
    if k > 0 {
        bytes.drain(..k);
        start += k as u64;
    }
    if start + (bytes.len() as u64) < n {
        if let Some(p) = bytes.iter().rposition(|b| *b == 10) {
            if p + 32768 > bytes.len() && start + (p as u64 + 1) > at {
                bytes.truncate(p + 1)
            }
        }
        if let Err(err) = std::str::from_utf8(&bytes) {
            if err.error_len().is_none() {
                bytes.truncate(err.valid_up_to());
            }
        }
    }
    Ok((start, bytes))
}
#[path = "actions.rs"]
mod actions;
#[path = "bulk.rs"]
mod bulk;
use bulk::BulkView;
#[path = "checkpoint.rs"]
mod checkpoint;
#[path = "edit_cues.rs"]
mod edit_cues;
pub(crate) use edit_cues::LANDING_TIMER;
#[path = "global_edit.rs"]
pub(crate) mod global_edit;
#[path = "lifecycle.rs"]
mod lifecycle;
#[path = "line_tools.rs"]
mod line_tools;
#[path = "sort_lines.rs"]
mod sort_lines;
#[path = "npp_app.rs"]
mod npp_app;
#[path = "polish.rs"]
mod polish;
#[path = "search_ui.rs"]
mod search_ui;
#[path = "zero_runs.rs"]
pub(crate) mod zero_runs;
#[path = "search_panel.rs"]
mod search_panel;
pub(crate) use search_panel::SearchScope;
#[path = "spans.rs"]
mod spans;

#[path = "navigation.rs"]
mod navigation;
pub(crate) use navigation::language_for;

#[cfg(test)]
#[path = "review_regressions.rs"]
mod review_regressions;

#[cfg(test)]
mod window_tests {
    use super::*;
    #[test]
    fn aligned_tail_still_contains_eof_and_requested_match() {
        let mut d = Document::empty();
        let mut bytes = b"0123456789abcdef row\r\n".repeat(PAGE / 5);
        bytes.extend_from_slice("tail Î¼ marker".as_bytes());
        d.replace(0, 0, &bytes).unwrap();
        for at in [d.len(), d.len() - 6, d.len() - PAGE as u64 / 2, 0] {
            let (base, view) = window_bytes(&d, at).unwrap();
            assert!(base <= at && at <= base + view.len() as u64);
            assert_eq!(&view[..], &bytes[base as usize..base as usize + view.len()]);
            if at == d.len() {
                assert!(view.ends_with(b"marker"));
            }
        }
    }
}
