//! The first window, checked from inside: the real application object with its
//! menu bar, window and editor, without the event loop. A window appears while
//! this runs.
#[cfg(target_os = "macos")]
fn main() {
    use linework_editor::sci::*;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenu};

    let mtm = MainThreadMarker::new().expect("the main thread");
    let app = linework_mac::App::new(mtm);
    app.start();

    // The menu bar is the Windows one, after the menu macOS puts first.
    let bar = NSApplication::sharedApplication(mtm).mainMenu().expect("a menu bar");
    let menu = |at: isize| bar.itemAtIndex(at).and_then(|item| item.submenu()).expect("a menu");
    let titles: Vec<String> = (1..bar.numberOfItems()).map(|at| menu(at).title().to_string()).collect();
    assert_eq!(titles, ["File", "Edit", "Selection", "Search", "Go", "View", "Macro", "Tools", "Help"]);
    let chord = |menu: &NSMenu, title: &str| {
        let item = menu.itemWithTitle(&objc2_foundation::NSString::from_str(title)).expect(title);
        (item.keyEquivalent().to_string(), item.keyEquivalentModifierMask())
    };
    let (control, shift, option) = (NSEventModifierFlags::Control, NSEventModifierFlags::Shift, NSEventModifierFlags::Option);
    assert_eq!(chord(&menu(1), "Open folder…"), ("o".into(), control | shift));
    assert_eq!(chord(&menu(1), "Save all"), ("\u{f70f}".into(), control | shift));
    assert_eq!(chord(&menu(1), "Exit Linework"), ("\u{f707}".into(), option));
    assert_eq!(chord(&menu(2), "Move selected lines up"), ("\u{f700}".into(), option));
    assert_eq!(chord(&menu(2), "Toggle comment on selected lines"), ("/".into(), control));

    let folder = std::env::temp_dir().join(format!("linework-first-window-{}", std::process::id()));
    std::fs::create_dir_all(&folder).unwrap();
    let editor = app.editor();
    let title = || app.editor_window_title();

    // A file with Windows line ends: shown as it is, a typed line end is a
    // Windows one, and saving writes exactly the edited text.
    let windows = folder.join("windows.txt");
    std::fs::write(&windows, b"first line\r\nsecond line\r\n").unwrap();
    assert!(app.open(windows.clone()));
    assert_eq!(editor.raw_bytes(0, editor.len()), b"first line\r\nsecond line\r\n");
    assert_eq!(title(), "windows.txt — Linework");
    editor.replace_range(editor.len(), editor.len(), "third line");
    editor.set_selection(editor.len(), editor.len());
    editor.send(SCI_NEWLINE, 0, 0);
    assert_eq!(title(), "windows.txt · Unsaved — Linework");
    assert!(app.save(false));
    assert_eq!(std::fs::read(&windows).unwrap(), b"first line\r\nsecond line\r\nthird line\r\n");
    assert_eq!(title(), "windows.txt — Linework");

    // A second save after another edit still writes the whole text right: the
    // document took over the first save's place on disk.
    editor.replace_range(0, 5, "FIRST");
    assert!(app.save(false));
    assert_eq!(std::fs::read(&windows).unwrap(), b"FIRST line\r\nsecond line\r\nthird line\r\n");

    // A file that is not UTF-8 is shown as text and goes back in its own encoding.
    let latin = folder.join("latin.txt");
    std::fs::write(&latin, b"caf\xe9 au lait, na\xefve, d\xe9j\xe0 vu\n").unwrap();
    assert!(app.open(latin.clone()));
    assert_eq!(editor.text(), "café au lait, naïve, déjà vu\n");
    editor.replace_range(editor.len(), editor.len(), "fin\n");
    assert!(app.save(false));
    assert_eq!(std::fs::read(&latin).unwrap(), b"caf\xe9 au lait, na\xefve, d\xe9j\xe0 vu\nfin\n");

    // A source file gets its language, and the commands reach the editor.
    let source = folder.join("sample.rs");
    std::fs::write(&source, "fn main() {\n    let name = 1;\n}\n").unwrap();
    assert!(app.open(source));
    assert_eq!(editor.language(), "Rust");
    editor.send(SCI_COLOURISE, 0, -1);
    assert_ne!(editor.send(SCI_GETSTYLEAT, 0, 0), 0, "the keyword has no style");
    app.command("selall");
    assert_eq!(editor.selected_text(), editor.text());
    app.command("upper");
    assert!(editor.text().starts_with("FN MAIN()"));
    app.command("undo");
    assert!(editor.text().starts_with("fn main()"));
    // Nothing is unsaved after the undo, so ending here asks nothing.
    assert_eq!(editor.send(SCI_GETMODIFY, 0, 0), 0);

    std::fs::remove_dir_all(folder).unwrap();
    println!("first window: ok");
}

#[cfg(not(target_os = "macos"))]
fn main() {}
