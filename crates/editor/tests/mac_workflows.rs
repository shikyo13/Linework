//! The real control on macOS. AppKit views belong to the main thread, so this is
//! a program of its own: `cargo test` runs its `main` there.
#[cfg(target_os = "macos")]
mod workflows;

#[cfg(target_os = "macos")]
fn main() {
    use linework_editor::{Editor, Notification, import_user_grammar, sci::*, set_wake};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    let e = Rc::new(Editor::create(0, 100).expect("native editor"));
    workflows::run(&e, || e.set_frame(0.0, 0.0, 840.0, 550.0));

    // What the shared workflows hand over by hand, Scintilla must deliver itself
    // here: a typed line end reaches the handler, the handler passes it on, and
    // the editor's answer (the indent) lands back in the text.
    let weak = Rc::downgrade(&e);
    e.listen(Some(Rc::new(move |notice: &Notification| {
        let editor = weak.upgrade().expect("a notification after the editor was gone");
        assert_eq!(notice.nmhdr.hwnd_from, editor.hwnd());
        assert_eq!(notice.nmhdr.id_from, 100);
        editor.on_notify(notice);
    })));
    e.set_language("Rust");
    e.send(SCI_SETEOLMODE, 2, 0);
    e.set_text("    fn main() {");
    e.set_selection(e.len(), e.len());
    e.send(SCI_NEWLINE, 0, 0);
    assert_eq!(e.text(), "    fn main() {\n        ");
    e.listen(None);
    e.send(SCI_NEWLINE, 0, 0);
    assert_eq!(e.text(), "    fn main() {\n        \n", "a handler taken away still heard");

    // A grammar worker with styles ready wakes the main thread, where the host
    // pumps. Nothing here pumps until the wake arrives.
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: *const std::ffi::c_void;
        fn CFRunLoopRunInMode(mode: *const std::ffi::c_void, seconds: f64, one: u8) -> i32;
    }
    static WOKEN: AtomicBool = AtomicBool::new(false);
    fn woken() {
        WOKEN.store(true, Ordering::Relaxed);
    }
    set_wake(woken);
    let folder = std::env::temp_dir().join(format!("linework-wake-test-{}", std::process::id()));
    std::fs::create_dir_all(&folder).unwrap();
    let file = folder.join("Wake.sublime-syntax");
    let syntax = "%YAML 1.2\n---\nname: Wake\nscope: source.wake\nfile_extensions: [wake]\ncontexts:\n  main:\n    - match: '\\btrue\\b'\n      scope: keyword.control.wake\n";
    std::fs::write(&file, syntax).unwrap();
    let name = import_user_grammar(&folder.join("installed"), &file).unwrap();
    let source = "{\"key\": true}\n";
    e.set_text(source);
    assert!(e.set_language(name));
    assert!(e.is_custom_grammar());
    let word = source.find("true").unwrap();
    assert_eq!(e.send(SCI_GETSTYLEAT, word, 0), 0, "styled before any worker answered");
    let until = Instant::now() + Duration::from_secs(6);
    while !WOKEN.load(Ordering::Relaxed) {
        assert!(Instant::now() < until, "no wake reached the main thread");
        unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.01, 0) };
    }
    e.pump_grammar();
    assert_ne!(e.send(SCI_GETSTYLEAT, word, 0), 0);
    e.set_language("Plain Text");
    std::fs::remove_dir_all(folder).unwrap();

    drop(Rc::try_unwrap(e).ok().expect("the editor is still held somewhere"));
    println!("native editor workflows: ok");
}

#[cfg(not(target_os = "macos"))]
fn main() {}
