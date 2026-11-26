//! The application: one window holding one editor, with the file commands that
//! need nothing but the editor and the storage crate.
use crate::{menu, selection};
use linework_editor::{Editor, Notification, detect_language, sci::*, set_wake};
use linework_storage::Document;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSAppearance, NSAppearanceCustomization,
    NSAppearanceNameDarkAqua, NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
    NSApplicationTerminateReply, NSAutoresizingMaskOptions, NSBackingStoreType, NSControlStateValueOff, NSMenuItem,
    NSMenuItemValidation, NSModalResponseOK, NSOpenPanel, NSSavePanel, NSView, NSWindow, NSWindowDelegate,
    NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL, ns_string};
use std::cell::{Cell, OnceCell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Windows shows a file above this size through a paged view of the file on
/// disk. The Mac program does not have that view yet.
const LARGE: u64 = 24 * 1024 * 1024;

/// The commands of the shared table that work here so far. The rest are in the
/// menus, greyed, until their parts are built.
const READY: [&str; 25] = [
    "new", "open", "save", "saveas", "exit", "undo", "redo", "cut", "copy", "paste", "complete", "selall", "selnext",
    "selmatches", "selskipped", "undoselect", "sellines", "caretup", "caretdown", "rectangle", "upper", "lower", "dup",
    "moveup", "movedown",
];

struct Open {
    document: Document,
    path: Option<PathBuf>,
    name: String,
}

/// What the application holds once its window exists.
pub struct State {
    window: Retained<NSWindow>,
    editor: Rc<Editor>,
    open: RefCell<Open>,
    untitled: Cell<u32>,
}

thread_local! {
    /// The one application object, for the two calls that arrive with no receiver:
    /// a notification from the editor and a grammar worker's wake.
    static APP: OnceCell<Retained<App>> = const { OnceCell::new() };
}

pub fn run() {
    let mtm = MainThreadMarker::new().expect("the program starts on the main thread");
    let application = NSApplication::sharedApplication(mtm);
    application.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let app = App::new(mtm);
    application.setDelegate(Some(ProtocolObject::from_ref(&*app)));
    application.run();
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `App` has no `Drop`.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    // Boxed: the runtime lays instance variables out to 8 bytes at most, and
    // the document inside asks for 16.
    #[ivars = OnceCell<Box<State>>]
    pub struct App;

    unsafe impl NSObjectProtocol for App {}

    unsafe impl NSApplicationDelegate for App {
        // The window must exist before macOS hands over a file the program was
        // started with, which comes between these two calls.
        #[unsafe(method(applicationWillFinishLaunching:))]
        fn will_finish_launching(&self, _: &NSNotification) {
            self.start();
        }

        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _: &NSNotification) {
            if let Some(path) = std::env::args_os().skip(1).map(PathBuf::from).find(|path| path.is_file()) {
                self.open(path);
            }
            NSApplication::sharedApplication(self.mtm()).activate();
        }

        #[unsafe(method(application:openFile:))]
        fn open_file(&self, _: &NSApplication, file: &NSString) -> bool {
            self.open(PathBuf::from(file.to_string()))
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn ends_with_its_window(&self, _: &NSApplication) -> bool {
            true
        }

        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _: &NSApplication) -> NSApplicationTerminateReply {
            if self.settle() { NSApplicationTerminateReply::TerminateNow } else { NSApplicationTerminateReply::TerminateCancel }
        }
    }

    unsafe impl NSWindowDelegate for App {
        #[unsafe(method(windowShouldClose:))]
        fn should_close(&self, _: &NSWindow) -> bool {
            self.settle()
        }
    }

    unsafe impl NSMenuItemValidation for App {
        #[unsafe(method(validateMenuItem:))]
        fn validate(&self, item: &NSMenuItem) -> bool {
            self.ready(item)
        }
    }

    impl App {
        #[unsafe(method(command:))]
        fn chosen(&self, item: &NSMenuItem) {
            if let Some(command) = menu::command(item) {
                self.command(command.id);
            }
        }
    }
);

impl App {
    /// The application object. There is one for the life of the program.
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let app: Retained<Self> = unsafe { msg_send![super(Self::alloc(mtm).set_ivars(OnceCell::new())), init] };
        APP.with(|cell| cell.set(app.clone()).ok());
        app
    }

    pub fn editor(&self) -> &Rc<Editor> {
        &self.state().editor
    }

    fn state(&self) -> &State {
        self.ivars().get().expect("the window is made before anything uses it")
    }

    /// Whether a menu item can be chosen now.
    fn ready(&self, item: &NSMenuItem) -> bool {
        item.setState(NSControlStateValueOff);
        let (Some(command), Some(state)) = (menu::command(item), self.ivars().get()) else {
            return false;
        };
        match command.id {
            "undo" => state.editor.send(SCI_CANUNDO, 0, 0) != 0,
            "redo" => state.editor.send(SCI_CANREDO, 0, 0) != 0,
            id => READY.contains(&id),
        }
    }

    /// Makes the menu bar and the window. `run` does this as the program starts.
    pub fn start(&self) {
        let mtm = self.mtm();
        menu::install(&NSApplication::sharedApplication(mtm), self, mtm);
        // Linework draws its own tabs, so the system's window tabs are off.
        NSWindow::setAllowsAutomaticWindowTabbing(false, mtm);
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1100.0, 720.0)),
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable
                    | NSWindowStyleMask::Resizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // Without a window controller the window must outlive its closing.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setContentMinSize(NSSize::new(480.0, 320.0));
        window.center();
        window.setFrameAutosaveName(ns_string!("Linework"));
        window.setAppearance(NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }).as_deref());
        window.setDelegate(Some(ProtocolObject::from_ref(self)));
        let content = window.contentView().expect("a window has a content view");
        let editor = match Editor::create(Retained::as_ptr(&content) as isize, 1) {
            Ok(editor) => Rc::new(editor),
            Err(problem) => {
                alert(mtm, "Linework", &problem);
                std::process::exit(1);
            }
        };
        let bounds = content.bounds();
        editor.set_frame(0.0, 0.0, bounds.size.width, bounds.size.height);
        // SAFETY: the editor's view is an NSView that lives as long as the editor.
        let view = unsafe { &*(editor.view() as *const NSView) };
        view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
        let text: Retained<NSView> = unsafe { msg_send![view, content] };
        window.makeFirstResponder(Some(&text));

        let weak = Rc::downgrade(&editor);
        editor.listen(Some(Rc::new(move |notice: &Notification| {
            let Some(editor) = weak.upgrade() else { return };
            editor.on_notify(notice);
            if matches!(notice.nmhdr.code, SCN_SAVEPOINTREACHED | SCN_SAVEPOINTLEFT) {
                with_app(App::retitle);
            }
        })));
        set_wake(|| with_app(|app| app.state().editor.pump_grammar()));

        let open = Open { document: Document::empty(), path: None, name: "Untitled 1".into() };
        let state = State { window, editor, open: RefCell::new(open), untitled: Cell::new(1) };
        self.ivars().set(Box::new(state)).ok().expect("the window is made once");
        self.retitle();
        self.state().window.makeKeyAndOrderFront(None);
    }

    /// Runs a command of the shared table by its id.
    pub fn command(&self, id: &str) {
        let state = self.state();
        let editor = &state.editor;
        match id {
            "new" => {
                if self.settle() {
                    state.untitled.set(state.untitled.get() + 1);
                    self.show(Document::empty(), None, format!("Untitled {}", state.untitled.get()));
                }
            }
            "open" => {
                let panel = NSOpenPanel::openPanel(self.mtm());
                if panel.runModal() == NSModalResponseOK
                    && let Some(path) = panel.URLs().firstObject().and_then(|url| url.path())
                {
                    self.open(PathBuf::from(path.to_string()));
                }
            }
            "save" | "saveas" => {
                self.save(id == "saveas");
            }
            "exit" => NSApplication::sharedApplication(self.mtm()).terminate(None),
            "undo" => drop(editor.send(SCI_UNDO, 0, 0)),
            "redo" => drop(editor.send(SCI_REDO, 0, 0)),
            "cut" => drop(editor.send(SCI_CUT, 0, 0)),
            "copy" => drop(editor.send(SCI_COPY, 0, 0)),
            "paste" => drop(editor.send(SCI_PASTE, 0, 0)),
            "selall" => drop(editor.send(SCI_SELECTALL, 0, 0)),
            "complete" => drop(editor.complete()),
            id => {
                if let Err(problem) = selection::apply(editor, id) {
                    alert(self.mtm(), crate::commands::label(id), &problem);
                }
            }
        }
    }

    /// Opens a file in place of what is shown. False when nothing was opened.
    pub fn open(&self, path: PathBuf) -> bool {
        let path = std::path::absolute(&path).unwrap_or(path);
        if !self.settle() {
            return false;
        }
        match Document::open(&path) {
            Ok(document) if document.len() > LARGE => {
                alert(self.mtm(), "Open file", "Files over 24 MB need the paged view, which Linework for macOS does not have yet.");
                false
            }
            Ok(document) => {
                let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                self.show(document, Some(path), name)
            }
            Err(problem) => {
                alert(self.mtm(), "Open file", &format!("{}: {problem}", path.display()));
                false
            }
        }
    }

    fn show(&self, document: Document, path: Option<PathBuf>, name: String) -> bool {
        let state = self.state();
        let bytes = match document.read(0, document.len() as usize) {
            Ok(bytes) => bytes,
            Err(problem) => {
                alert(self.mtm(), "Open file", &problem.to_string());
                return false;
            }
        };
        let language = detect_language(path.as_deref(), &String::from_utf8_lossy(&bytes[..bytes.len().min(8192)]));
        let line_ends = match document.newline() {
            "\r\n" => 0,
            "\r" => 1,
            _ => 2,
        };
        state.open.replace(Open { document, path, name });
        let editor = &state.editor;
        editor.set_bytes(&bytes);
        editor.send(SCI_EMPTYUNDOBUFFER, 0, 0);
        editor.send(SCI_SETSAVEPOINT, 0, 0);
        editor.set_language(&language);
        editor.send(SCI_SETEOLMODE, line_ends, 0);
        editor.set_selection(0, 0);
        self.retitle();
        true
    }

    /// Saves what is shown, asking where when it has no file or `choose` is set.
    /// False when nothing was saved.
    pub fn save(&self, choose: bool) -> bool {
        let state = self.state();
        let (known, name) = {
            let open = state.open.borrow();
            (open.path.clone(), open.name.clone())
        };
        let path = match known {
            Some(path) if !choose => path,
            known => {
                let panel = NSSavePanel::savePanel(self.mtm());
                panel.setNameFieldStringValue(&NSString::from_str(&name));
                if let Some(folder) = known.as_deref().and_then(Path::parent) {
                    panel.setDirectoryURL(Some(&NSURL::fileURLWithPath(&NSString::from_str(&folder.to_string_lossy()))));
                }
                if panel.runModal() != NSModalResponseOK {
                    return false;
                }
                match panel.URL().and_then(|url| url.path()) {
                    Some(path) => PathBuf::from(path.to_string()),
                    None => return false,
                }
            }
        };
        let text = state.editor.raw_bytes(0, state.editor.len());
        // The editor notifies while its save point moves, and the notification
        // reads `open`, so the borrow ends before the editor is told.
        let saved = {
            let mut open = state.open.borrow_mut();
            carry(&mut open.document, &text).and_then(|()| {
                // As on Windows: a copy is saved, then the document takes over
                // the copy's place on disk and keeps its own history.
                let mut copy = open.document.clone();
                copy.save(&path)?;
                open.document.adopt_save(copy);
                open.name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                open.path = Some(path);
                Ok(())
            })
        };
        match saved {
            Ok(()) => {
                state.editor.send(SCI_SETSAVEPOINT, 0, 0);
                self.retitle();
                true
            }
            Err(problem) => {
                alert(self.mtm(), "Save file", &problem.to_string());
                false
            }
        }
    }

    /// Before what is shown goes away: offers to save unsaved changes. False
    /// when the person cancels or the save fails.
    fn settle(&self) -> bool {
        let state = self.state();
        if state.editor.send(SCI_GETMODIFY, 0, 0) == 0 {
            return true;
        }
        let question = NSAlert::new(self.mtm());
        question.setMessageText(&NSString::from_str(&format!("Save changes to {}?", state.open.borrow().name)));
        for button in ["Save", "Don't Save", "Cancel"] {
            question.addButtonWithTitle(&NSString::from_str(button));
        }
        let answer = question.runModal();
        if answer == NSAlertFirstButtonReturn {
            return self.save(false);
        }
        if answer == NSAlertSecondButtonReturn {
            // Given up for good, so closing the window and then ending the
            // program asks once.
            state.editor.send(SCI_SETSAVEPOINT, 0, 0);
            return true;
        }
        false
    }

    /// The window's title as shown.
    pub fn editor_window_title(&self) -> String {
        self.state().window.title().to_string()
    }

    fn retitle(&self) {
        let Some(state) = self.ivars().get() else { return };
        let unsaved = state.editor.send(SCI_GETMODIFY, 0, 0) != 0;
        let open = state.open.borrow();
        let title = format!("{}{} — Linework", open.name, if unsaved { " · Unsaved" } else { "" });
        state.window.setTitle(&NSString::from_str(&title));
        state.window.setDocumentEdited(unsaved);
        let file = open.path.as_deref().map(|path| NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy())));
        state.window.setRepresentedURL(file.as_deref());
    }
}

fn with_app(run: impl FnOnce(&App)) {
    APP.with(|cell| cell.get().map(|app| run(app)));
}

/// Puts the editor's text into the document as one change covering only what
/// differs, so the document's history records the edit and not the whole file.
fn carry(document: &mut Document, new: &[u8]) -> std::io::Result<()> {
    let old = document.read(0, document.len() as usize)?;
    let inside = |at: usize| new.get(at).is_some_and(|byte| byte & 0xc0 == 0x80);
    let mut head = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    while head > 0 && inside(head) {
        head -= 1;
    }
    let mut tail = old[head..].iter().rev().zip(new[head..].iter().rev()).take_while(|(a, b)| a == b).count();
    while tail > 0 && inside(new.len() - tail) {
        tail -= 1;
    }
    if head + tail < old.len() || head + tail < new.len() {
        document.replace(head as u64, (old.len() - head - tail) as u64, &new[head..new.len() - tail])?;
    }
    Ok(())
}

fn alert(mtm: MainThreadMarker, title: &str, message: &str) {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(message));
    alert.runModal();
}
