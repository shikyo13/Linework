//! Native Windows editing surface. File storage and workspace policy live in the host.
mod completion;
mod grammar;
mod sublime_v2;
pub use grammar::{import_user_grammar, load_user_grammars};
mod language;
pub mod sci;
mod spelling;
mod style;
mod user_snippet;
pub use user_snippet::UserSnippet;

pub use language::{detect_language, languages};
pub use memchr::memmem::find_iter as literal_matches;
use sci::*;
use std::cell::{Cell, RefCell};
use std::ffi::CString;
use std::marker::PhantomData;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::OnceLock;
pub use style::{CaretStyle, ColorRole, ColorScheme, HighlightOptions, MARK_COLORS, Theme};
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NotifyHeader {
    pub hwnd_from: isize,
    pub id_from: usize,
    pub code: u32,
}

/// Scintilla's SCNotification ABI. Copy notification text during WM_NOTIFY if
/// the host defers a notification. No borrowed pointer is retained by the editor.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Notification {
    pub nmhdr: NotifyHeader,
    pub position: isize,
    pub ch: i32,
    pub modifiers: i32,
    pub modification_type: i32,
    pub text: *const i8,
    pub length: isize,
    pub lines_added: isize,
    pub message: i32,
    pub w_param: usize,
    pub l_param: isize,
    pub line: isize,
    pub fold_level_now: i32,
    pub fold_level_prev: i32,
    pub margin: i32,
    pub list_type: i32,
    pub x: i32,
    pub y: i32,
    pub token: i32,
    pub annotation_lines_added: isize,
    pub updated: i32,
    pub list_completion_method: i32,
    pub character_source: i32,
}

type CreateLexer = unsafe extern "system" fn(*const i8) -> *mut std::ffi::c_void;
struct Native {
    create_lexer: CreateLexer,
}
fn native() -> Result<&'static Native, String> {
    static NATIVE: OnceLock<Result<Native, String>> = OnceLock::new();
    NATIVE.get_or_init(|| {
        let exe = std::env::current_exe().map_err(|e|e.to_string())?;
        let folder = exe.parent().ok_or("Cannot locate the executable folder")?;
        let candidates = [folder.join("runtime"), folder.to_path_buf()];
        let mut dirs:Vec<PathBuf> = candidates.into_iter().collect();
        if let Some(path) = std::env::var_os("LINEWORK_RUNTIME") { dirs.push(PathBuf::from(path)); }
        let dir=dirs.iter().find(|p|p.join("Scintilla.dll").is_file() && p.join("Lexilla.dll").is_file())
            .ok_or("Scintilla.dll and Lexilla.dll must be beside Linework.exe or in its runtime folder.")?;
        let load = |name:&str| {
            let wide:Vec<u16>=dir.join(name).as_os_str().encode_wide().chain(Some(0)).collect();
            let h=unsafe { LoadLibraryExW(wide.as_ptr(),std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR|LOAD_LIBRARY_SEARCH_SYSTEM32) };
            if h.is_null() { Err(format!("Cannot load {name}: {}",std::io::Error::last_os_error())) } else { Ok(h) }
        };
        load("Scintilla.dll")?;
        let lex=load("Lexilla.dll")?;
        let entry=unsafe { GetProcAddress(lex,c"CreateLexer".as_ptr().cast()) }.ok_or("Incompatible Lexilla.dll")?;
        Ok(Native{create_lexer:unsafe {std::mem::transmute::<unsafe extern "system" fn()->isize,CreateLexer>(entry)} })
    }).as_ref().map_err(Clone::clone)
}

#[repr(C)]
struct TextRange {
    start: isize,
    end: isize,
    text: *mut u8,
}

pub struct Editor {
    hwnd: isize,
    native: &'static Native,
    theme: Cell<Theme>,
    colors: RefCell<ColorScheme>,
    highlights: Cell<HighlightOptions>,
    mark_terms: RefCell<[String; 5]>,
    mark_view: Cell<Option<(isize, usize, usize)>>,
    language: Cell<&'static language::Language>,
    grammar: RefCell<Option<std::rc::Rc<RefCell<grammar::Session>>>>,
    prose_styles: Cell<[bool; 256]>,
    workspace_words: RefCell<Vec<String>>,
    snippet: RefCell<Option<completion::SnippetSession>>,
    user_snippets: RefCell<Vec<UserSnippet>>,
    spelling: RefCell<spelling::Spelling>,
    occurrences: Cell<bool>,
    occurrence_key: Cell<(usize, usize, usize, usize)>,
    auto_complete: Cell<bool>,
    completion_threshold: Cell<usize>,
    auto_indent: Cell<bool>,
    epoch: Cell<u64>,
    caret_style: Cell<CaretStyle>,
    dpi: Cell<u32>,
    _ui_thread: PhantomData<Rc<()>>,
}

impl Editor {
    pub fn create(parent: isize, id: i32) -> Result<Self, String> {
        let native = native()?;
        let class: Vec<u16> = "Scintilla".encode_utf16().chain(Some(0)).collect();
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                std::ptr::null(),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_CLIPSIBLINGS,
                0,
                0,
                1,
                1,
                parent as _,
                id as isize as _,
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err(format!(
                "Cannot create text editor: {}",
                std::io::Error::last_os_error()
            ));
        }
        let editor = Self {
            hwnd: hwnd as isize,
            native,
            theme: Cell::new(Theme::Dark),
            colors: RefCell::new(ColorScheme::default()),
            highlights: Cell::new(HighlightOptions::default()),
            mark_terms: RefCell::new(Default::default()),
            mark_view: Cell::new(None),
            language: Cell::new(&language::registry()[0]),
            grammar: RefCell::new(None),
            prose_styles: Cell::new([true; 256]),
            workspace_words: RefCell::new(Vec::new()),
            snippet: RefCell::new(None),
            user_snippets: RefCell::new(Vec::new()),
            spelling: RefCell::new(Default::default()),
            occurrences: Cell::new(true),
            occurrence_key: Cell::new((usize::MAX, 0, 0, 0)),
            auto_complete: Cell::new(true),
            completion_threshold: Cell::new(2),
            auto_indent: Cell::new(true),
            epoch: Cell::new(0),
            caret_style: Cell::new(CaretStyle::Line),
            dpi: Cell::new(96),
            _ui_thread: PhantomData,
        };
        for (msg, value) in [
            (SCI_SETCODEPAGE, 65001),
            (SCI_SETTECHNOLOGY, usize::from(parent != -3)),
            (SCI_SETFONTQUALITY, SC_EFF_QUALITY_ANTIALIASED as usize),
            (SCI_SETLAYOUTCACHE, 2),
            (SCI_SETMULTIPLESELECTION, 1),
            (SCI_SETADDITIONALSELECTIONTYPING, 1),
            (SCI_SETADDITIONALCARETSBLINK, 1),
            (SCI_SETVIRTUALSPACEOPTIONS, 1),
            (SCI_SETMULTIPASTE, 1),
            (SCI_AUTOCSETMULTI, 1),
            (SCI_SETTABWIDTH, 4),
            (SCI_SETINDENT, 4),
            (SCI_SETUSETABS, 0),
            (SCI_SETINDENTATIONGUIDES, 3),
            (SCI_SETTABINDENTS, 1),
            (SCI_SETBACKSPACEUNINDENTS, 1),
            (SCI_SETAUTOMATICFOLD, 7),
            (SCI_AUTOCSETIGNORECASE, 1),
            (SCI_AUTOCSETMAXHEIGHT, 9),
            (SCI_AUTOCSETMAXWIDTH, 64),
            (SCI_AUTOCSETSEPARATOR, b'\n' as usize),
            (SCI_SETSCROLLWIDTHTRACKING, 1),
            (SCI_SETSCROLLWIDTH, 80),
        ] {
            editor.send(msg, value, 0);
        }
        editor.set_theme(Theme::Dark);
        editor.set_language("Plain Text");
        Ok(editor)
    }

    pub fn hwnd(&self) -> isize {
        self.hwnd
    }
    /// Numeric interface for built-in editing commands. Pointer-bearing and
    /// unknown messages are rejected before they reach the native component.
    pub fn send(&self, msg: u32, w: usize, l: isize) -> isize {
        if !sci::scalar_message(msg) {
            return 0;
        }
        unsafe { self.send_ptr(msg, w, l) }
    }
    /// # Safety
    /// All pointer arguments must satisfy the documented Scintilla message ABI
    /// and remain valid throughout the synchronous dispatch.
    pub unsafe fn send_ptr(&self, msg: u32, w: usize, l: isize) -> isize {
        unsafe { SendMessageW(self.hwnd as _, msg, w, l) }
    }
    pub fn len(&self) -> usize {
        self.send(SCI_GETLENGTH, 0, 0).max(0) as usize
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn document_epoch(&self) -> u64 {
        self.epoch.get()
    }
    /// A shared Scintilla document changed through another editor, or a new
    /// document was attached without editing the previous view's text.
    pub fn document_changed_outside_view(&self, earliest: usize) {
        self.epoch.set(self.epoch.get().wrapping_add(1));
        self.snippet.replace(None);
        self.spelling.borrow_mut().invalidate();
        self.occurrence_key.set((usize::MAX, 0, 0, 0));
        self.mark_view.set(None);
        self.invalidate_grammar(earliest);
    }
    pub fn caret(&self) -> usize {
        self.send(SCI_GETCURRENTPOS, 0, 0).max(0) as usize
    }
    pub fn selection(&self) -> (usize, usize) {
        (self.send(SCI_GETANCHOR, 0, 0).max(0) as usize, self.caret())
    }
    pub fn set_selection(&self, anchor: usize, caret: usize) {
        self.send(SCI_SETSEL, anchor, caret as isize);
    }
    pub fn selections(&self) -> Vec<(usize, usize)> {
        let count = self.send(SCI_GETSELECTIONS, 0, 0).max(0) as usize;
        (0..count)
            .map(|i| {
                (
                    self.send(SCI_GETSELECTIONNANCHOR, i, 0) as usize,
                    self.send(SCI_GETSELECTIONNCARET, i, 0) as usize,
                )
            })
            .collect()
    }
    pub fn set_text(&self, text: &str) {
        self.set_bytes(text.as_bytes());
    }
    pub fn set_bytes(&self, text: &[u8]) {
        self.invalidate_grammar(0);
        self.mark_view.set(None);
        self.epoch.set(self.epoch.get().wrapping_add(1));
        self.snippet.replace(None);
        self.send(SCI_SETUNDOCOLLECTION, 0, 0);
        self.send(SCI_CLEARALL, 0, 0);
        unsafe {
            self.send_ptr(SCI_ADDTEXT, text.len(), text.as_ptr() as isize);
        }
        self.send(SCI_SETUNDOCOLLECTION, 1, 0);
        self.send(SCI_EMPTYUNDOBUFFER, 0, 0);
        self.send(SCI_SETSAVEPOINT, 0, 0);
        self.set_selection(0, 0);
        self.occurrence_key.set((usize::MAX, 0, 0, 0));
        self.spelling.borrow_mut().invalidate();
        self.refresh_margins();
    }
    fn bytes(&self, start: usize, end: usize, styled: bool) -> Vec<u8> {
        let end = end.min(self.len());
        let start = start.min(end);
        let length = (end - start) * if styled { 2 } else { 1 };
        let mut out = vec![0; length + 2];
        let mut range = TextRange {
            start: start as isize,
            end: end as isize,
            text: out.as_mut_ptr(),
        };
        unsafe {
            self.send_ptr(
                if styled {
                    SCI_GETSTYLEDTEXTFULL
                } else {
                    SCI_GETTEXTRANGEFULL
                },
                0,
                &mut range as *mut _ as isize,
            );
        }
        out.truncate(length);
        out
    }
    pub fn text_range(&self, start: usize, end: usize) -> String {
        String::from_utf8_lossy(&self.bytes(start, end, false)).into_owned()
    }
    pub fn text(&self) -> String {
        self.text_range(0, self.len())
    }
    pub fn selected_text(&self) -> String {
        let (a, b) = self.selection();
        self.text_range(a.min(b), a.max(b))
    }
    pub fn bytes_of_selection(&self) -> Vec<u8> {
        let (a, b) = self.selection();
        self.raw_bytes(a.min(b), a.max(b))
    }
    pub fn raw_bytes(&self, start: usize, end: usize) -> Vec<u8> {
        self.bytes(start, end, false)
    }
    pub fn replace_range(&self, start: usize, end: usize, text: &str) {
        self.replace_bytes(start, end, text.as_bytes());
    }
    pub fn replace_bytes(&self, start: usize, end: usize, text: &[u8]) {
        self.invalidate_grammar(start);
        self.send(SCI_SETTARGETRANGE, start, end as isize);
        unsafe {
            self.send_ptr(SCI_REPLACETARGET, text.len(), text.as_ptr() as isize);
        }
    }
    pub fn language(&self) -> std::borrow::Cow<'static, str> {
        self.grammar.borrow().as_ref().map_or_else(
            || std::borrow::Cow::Borrowed(self.language.get().name.as_str()),
            |s| std::borrow::Cow::Owned(s.borrow().name.clone()),
        )
    }
    pub fn set_indentation(&self, width: usize, tabs: bool) {
        self.send(SCI_SETTABWIDTH, width, 0);
        self.send(SCI_SETINDENT, width, 0);
        self.send(SCI_SETUSETABS, tabs as usize, 0);
    }
    pub fn set_language(&self, name: impl AsRef<str>) -> bool {
        let name = name.as_ref();
        let Some(lang) = language::lookup(name) else {
            return self.use_grammar(name);
        };
        let c = CString::new(lang.lexer.as_str()).unwrap();
        let lexer = unsafe { (self.native.create_lexer)(c.as_ptr()) };
        if lexer.is_null() {
            return false;
        }
        unsafe {
            self.send_ptr(SCI_SETILEXER, 0, lexer as isize);
        }
        self.grammar.replace(None);
        self.language.set(lang);
        for (k, v) in [
            ("fold", "1"),
            ("fold.compact", "0"),
            ("fold.comment", "1"),
            ("fold.preprocessor", "1"),
            ("fold.html", "1"),
            ("fold.hypertext.comment", "1"),
            ("fold.quotes.python", "1"),
            ("fold.comment.python", "1"),
            ("fold.rust.comment.multiline", "1"),
            ("lexer.cpp.track.preprocessor", "0"),
            ("lexer.cpp.allow.dollars", "1"),
            (
                "lexer.cpp.backquoted.strings",
                if matches!(name, "JavaScript" | "TypeScript" | "Go") {
                    "1"
                } else {
                    "0"
                },
            ),
            ("lexer.json.allow.comments", "1"),
            ("lexer.json.escape.sequence", "1"),
            ("lexer.python.strings.f", "1"),
            (
                "lexer.cpp.triplequoted.strings",
                if matches!(name, "Kotlin" | "Java" | "Groovy") {
                    "1"
                } else {
                    "0"
                },
            ),
        ] {
            self.property(k, v);
        }
        for (slot, words) in &lang.keywords {
            self.put_str(SCI_SETKEYWORDS, *slot, words);
        }
        self.apply_lexer_styles();
        self.send(SCI_CLEARDOCUMENTSTYLE, 0, 0);
        self.occurrence_key.set((usize::MAX, 0, 0, 0));
        self.spelling.borrow_mut().invalidate();
        self.refresh_visible_lexer();
        true
    }
    pub fn detect(&self, path: Option<&Path>) -> String {
        let name = detect_language(path, &self.text_range(0, 8192));
        self.set_language(&name);
        name
    }
    fn put_str(&self, msg: u32, w: usize, s: &str) -> isize {
        let Ok(s) = CString::new(s) else {
            return 0;
        };
        unsafe { self.send_ptr(msg, w, s.as_ptr() as isize) }
    }
    fn property(&self, key: &str, value: &str) {
        let key = CString::new(key).unwrap();
        let v = CString::new(value).unwrap();
        unsafe {
            self.send_ptr(SCI_SETPROPERTY, key.as_ptr() as usize, v.as_ptr() as isize);
        }
    }
    fn visible_range(&self) -> (usize, usize) {
        let first = self.send(SCI_GETFIRSTVISIBLELINE, 0, 0).max(0) as usize;
        let rows = self.send(SCI_LINESONSCREEN, 0, 0).clamp(8, 400) as usize;
        let top = self.send(SCI_DOCLINEFROMVISIBLE, first, 0).max(0) as usize;
        let bottom = self.send(SCI_DOCLINEFROMVISIBLE, first + rows + 2, 0);
        let start = self.send(SCI_POSITIONFROMLINE, top, 0).max(0) as usize;
        let end = if bottom < 0 {
            self.len()
        } else {
            let p = self.send(SCI_POSITIONFROMLINE, bottom as usize + 1, 0);
            if p < 0 { self.len() } else { p as usize }
        };
        (start.min(self.len()), end.min(self.len()))
    }
    fn refresh_visible_lexer(&self) {
        let (start, end) = self.visible_range();
        self.send(
            SCI_COLOURISE,
            start,
            (end.min(start.saturating_add(128 * 1024))) as isize,
        );
    }
    pub fn set_workspace_words(&self, words: Vec<String>) {
        self.workspace_words.replace(words);
    }
    pub fn set_auto_complete(&self, on: bool) {
        self.auto_complete.set(on);
        if !on {
            self.send(SCI_AUTOCCANCEL, 0, 0);
        }
    }
    pub fn set_completion_threshold(&self, length: usize) {
        self.completion_threshold.set(length.clamp(1, 16));
    }
    pub fn set_auto_indent(&self, on: bool) {
        self.auto_indent.set(on);
    }
    pub fn set_occurrence_highlight(&self, on: bool) {
        self.occurrences.set(on);
        self.occurrence_key.set((usize::MAX, 0, 0, 0));
        self.refresh_occurrences();
    }
    pub fn on_notify(&self, n: &Notification) {
        if n.nmhdr.hwnd_from != self.hwnd {
            return;
        }
        match n.nmhdr.code {
            SCN_CHARADDED if n.ch == 10 || (n.ch == 13 && self.send(SCI_GETEOLMODE, 0, 0) == 1) => {
                self.indent_new_line();
            }
            SCN_CHARADDED
                if self.auto_complete.get()
                    && n.ch >= 0
                    && char::from_u32(n.ch as u32)
                        .is_some_and(|c| c == '_' || c.is_alphanumeric()) =>
            {
                self.completions(false);
            }
            SCN_UPDATEUI if n.updated & 15 != 0 => {
                self.refresh_occurrences();
                self.refresh_marks();
                if n.updated & 3 != 0 {
                    self.refresh_braces();
                }
            }
            SCN_MODIFIED if n.modification_type & 3 != 0 => {
                self.invalidate_grammar(n.position.max(0) as usize);
                self.mark_view.set(None);
                self.occurrence_key.set((usize::MAX, 0, 0, 0));
                self.spelling.borrow_mut().invalidate();
                self.track_snippet_edit(n);
                if n.lines_added != 0 {
                    self.refresh_margins();
                }
            }
            SCN_AUTOCCOMPLETED => {
                self.expand_completed_snippet(n.position);
            }
            _ => {}
        }
    }
    fn indent_new_line(&self) {
        if !self.auto_indent.get() || self.send(SCI_GETSELECTIONS, 0, 0) != 1 {
            return;
        }
        let pos = self.caret();
        let line = self.send(SCI_LINEFROMPOSITION, pos, 0) as usize;
        if line == 0 || self.send(SCI_POSITIONFROMLINE, line, 0) as usize != pos {
            return;
        }
        if self.send(SCI_GETLINEINDENTATION, line, 0) > 0 {
            return;
        }
        let end = self.send(SCI_GETLINEENDPOSITION, line - 1, 0) as usize;
        let start = self.send(SCI_POSITIONFROMLINE, line - 1, 0) as usize;
        let tail = self.text_range(start.max(end.saturating_sub(512)), end);
        let tail = tail.trim_end();
        let lang = self.language();
        let nested = match tail.as_bytes().last() {
            Some(b':') => matches!(lang.as_ref(), "Python" | "YAML"),
            Some(b'{' | b'[' | b'(') => {
                !matches!(lang.as_ref(), "Markdown" | "Plain Text" | "HTML" | "XML")
            }
            _ => false,
        };
        let width = self.send(SCI_GETINDENT, 0, 0).clamp(1, 32);
        let indent =
            self.send(SCI_GETLINEINDENTATION, line - 1, 0) + if nested { width } else { 0 };
        if indent == 0 {
            return;
        }
        self.send(SCI_SETLINEINDENTATION, line, indent);
        let at = self.send(SCI_GETLINEINDENTPOSITION, line, 0) as usize;
        self.set_selection(at, at);
    }
    fn refresh_braces(&self) {
        if !self.highlights.get().brace_pairs {
            self.send(SCI_BRACEHIGHLIGHT, usize::MAX, -1);
            return;
        }
        let p = self.caret();
        let chars = b"()[]{}";
        let candidate = p
            .checked_sub(1)
            .into_iter()
            .chain(Some(p))
            .find(|&i| chars.contains(&(self.send(SCI_GETCHARAT, i, 0) as u8)));
        if let Some(i) = candidate {
            let partner = self.send(SCI_BRACEMATCH, i, 0);
            if partner >= 0 {
                self.send(SCI_BRACEHIGHLIGHT, i, partner);
            } else {
                self.send(SCI_BRACEBADLIGHT, i, 0);
            }
        } else {
            self.send(SCI_BRACEHIGHLIGHT, usize::MAX, -1);
        }
    }
    /// Assign one literal phrase to each persistent mark color.
    pub fn set_mark_terms(&self, terms: &[String; 5]) {
        let changed = *self.mark_terms.borrow() != *terms;
        if changed {
            self.mark_terms.replace(terms.clone());
            for slot in 10..15 {
                self.send(SCI_SETINDICATORCURRENT, slot, 0);
                self.send(SCI_INDICATORCLEARRANGE, 0, self.len() as isize);
            }
        }
        self.mark_view.set(None);
        self.refresh_marks();
    }
    fn refresh_marks(&self) {
        if self.mark_terms.borrow().iter().all(String::is_empty) {
            return;
        }
        let (mut start, mut end) = self.visible_range();
        if end.saturating_sub(start) > 65536 {
            start = self
                .caret()
                .saturating_sub(16384)
                .clamp(start, end.saturating_sub(65536));
            end = start + 65536;
        }
        let key = (self.send(SCI_GETDOCPOINTER, 0, 0), start, end);
        if self.mark_view.replace(Some(key)) == Some(key) {
            return;
        }
        let terms = self.mark_terms.borrow().clone();
        let overlap = terms.iter().map(String::len).max().unwrap_or(0);
        let base = start.saturating_sub(overlap);
        let data = self.raw_bytes(base, end.saturating_add(overlap));
        for (i, term) in terms.iter().enumerate().filter(|(_, t)| !t.is_empty()) {
            self.send(SCI_SETINDICATORCURRENT, 10 + i, 0);
            self.send(
                SCI_INDICATORCLEARRANGE,
                start,
                end.saturating_sub(start) as isize,
            );
            for p in literal_matches(&data, term.as_bytes()).take(4096) {
                self.send(SCI_INDICATORFILLRANGE, base + p, term.len() as isize);
            }
        }
    }
    fn refresh_occurrences(&self) {
        let (a, b) = self.selection();
        let (start, end) = self.visible_range();
        let key = (a, b, start, end);
        if self.occurrence_key.replace(key) == key {
            return;
        }
        self.send(SCI_SETINDICATORCURRENT, 9, 0);
        self.send(SCI_INDICATORCLEARRANGE, 0, self.len() as isize);
        if !self.occurrences.get() || a == b || a.abs_diff(b) > 160 {
            return;
        }
        let query = self.selected_text();
        if query.is_empty() || !query.chars().all(|c| c == '_' || c.is_alphanumeric()) {
            return;
        }
        let local = self.text_range(start, end.min(start + 65536));
        for (i, _) in local.match_indices(&query).take(4096) {
            let is_word = |c: char| c == '_' || c.is_alphanumeric();
            if !local[..i].chars().next_back().is_some_and(is_word)
                && !local[i + query.len()..].chars().next().is_some_and(is_word)
            {
                self.send(SCI_INDICATORFILLRANGE, start + i, query.len() as isize);
            }
        }
    }
}
