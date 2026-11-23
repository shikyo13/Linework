#![cfg(windows)]
use linework_editor::{
    CaretStyle, ColorRole, ColorScheme, Editor, HighlightOptions, Notification, Theme,
    detect_language, languages, sci::*,
};
use std::{path::Path, ptr};
use windows_sys::Win32::{System::LibraryLoader::GetModuleHandleW, UI::WindowsAndMessaging::*};
#[path = "../../../src/selection.rs"]
mod selection;

#[test]
fn detection_prefers_explicit_content_and_safe_defaults() {
    assert_eq!(
        detect_language(
            Some(Path::new("config.txt")),
            "#!/usr/bin/env -S python3 -u\n"
        ),
        "Python"
    );
    assert_eq!(
        detect_language(Some(Path::new("tools")), "// vim: set ft=rust :\n"),
        "Rust"
    );
    assert_eq!(
        detect_language(Some(Path::new("README.MD")), ""),
        "Markdown"
    );
    assert_eq!(
        detect_language(Some(Path::new("CMakeLists.txt")), ""),
        "CMake"
    );
    assert_eq!(detect_language(None, "{\"hello\":42}"), "JSON");
    assert_eq!(
        detect_language(None, "this is ordinary writing"),
        "Plain Text"
    );
}

// Create and test the real DLL control on an isolated, never-switched desktop.
// A completion list opened during this test cannot appear on the user's desktop.
#[test]
fn native_editor_workflows() {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn CreateDesktopW(
            name: *const u16,
            device: *const u16,
            devmode: *const std::ffi::c_void,
            flags: u32,
            access: u32,
            sa: *const std::ffi::c_void,
        ) -> isize;
        fn SetThreadDesktop(desktop: isize) -> i32;
        fn GetThreadDesktop(thread: u32) -> isize;
        fn CloseDesktop(desktop: isize) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    let desktop_name: Vec<_> = format!("LineworkEditorTest{}", std::process::id())
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let old = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
    let desktop = unsafe {
        CreateDesktopW(
            desktop_name.as_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            0x1ff,
            ptr::null(),
        )
    };
    assert_ne!(desktop, 0);
    assert_ne!(unsafe { SetThreadDesktop(desktop) }, 0);
    let base = Path::new(env!("CARGO_MANIFEST_DIR"));
    if std::env::var_os("LINEWORK_RUNTIME").is_none() {
        unsafe {
            std::env::set_var("LINEWORK_RUNTIME", base.join("../../runtime"));
        }
    }
    let class = "STATIC".encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let parent = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            ptr::null(),
            WS_POPUP,
            0,
            0,
            1000,
            600,
            ptr::null_mut(),
            ptr::null_mut(),
            GetModuleHandleW(ptr::null()),
            ptr::null(),
        )
    };
    assert!(!parent.is_null());
    let e = Editor::create(parent as isize, 100).expect("native editor");
    verify_imported_grammar(&e);
    unsafe {
        MoveWindow(e.hwnd() as _, 0, 0, 840, 550, 0);
    }
    for lang in languages() {
        assert!(e.set_language(&lang), "{lang}");
    }
    let source = "/* open\n nested */\nfn main() {\n    let name = r#\"if not code\"#;\n}\n";
    e.set_text(source);
    assert_eq!(e.text(), source);
    assert!(e.set_language("Rust"));
    e.send(SCI_COLOURISE, 0, -1);
    let style = |word: &str| e.send(SCI_GETSTYLEAT, source.find(word).unwrap(), 0);
    assert_ne!(style("nested"), 0);
    assert_eq!(style("open"), style("nested"));
    assert_ne!(style("fn main"), style("if not code"));
    let mut colors = ColorScheme::default();
    colors.set(Theme::Dark, ColorRole::Comment, 0xbada55);
    e.set_appearance(&colors, HighlightOptions::default());
    assert_eq!(
        e.send(SCI_STYLEGETFORE, style("nested") as usize, 0),
        0x55daba
    );
    e.set_appearance(
        &colors,
        HighlightOptions {
            syntax: false,
            current_line: false,
            brace_pairs: false,
        },
    );
    assert_eq!(
        e.send(SCI_STYLEGETFORE, style("nested") as usize, 0),
        e.send(SCI_STYLEGETFORE, 32, 0)
    );
    e.set_appearance(&colors, Default::default());
    let mut terms: [String; 5] = Default::default();
    terms[0] = "nested".into();
    terms[4] = "fn main".into();
    e.set_mark_terms(&terms);
    assert_ne!(
        e.send(
            SCI_INDICATORVALUEAT,
            10,
            source.find("nested").unwrap() as isize
        ),
        0
    );
    assert_ne!(
        e.send(
            SCI_INDICATORVALUEAT,
            14,
            source.find("fn main").unwrap() as isize
        ),
        0
    );
    e.set_mark_terms(&Default::default());
    assert_eq!(
        e.send(
            SCI_INDICATORVALUEAT,
            10,
            source.find("nested").unwrap() as isize
        ),
        0
    );
    assert!(e.send(SCI_GETFOLDLEVEL, 2, 0) & 0x2000 != 0);
    e.set_text("<script>\nconst label = \"example\";\n</script>");
    e.set_language("HTML");
    e.send(SCI_COLOURISE, 0, -1);
    assert!(e.send(SCI_GETSTYLEAT, e.text().find("example").unwrap(), 0) >= 40);
    e.set_theme(Theme::Light);
    e.set_caret(CaretStyle::Block, 2, 0);
    assert_eq!(e.send(SCI_GETCARETSTYLE, 0, 0), 2);
    assert_eq!(e.send(SCI_GETCARETPERIOD, 0, 0), 0);
    e.set_text("straße x straße\nsecond\nthird");
    e.set_selection(0, 7);
    selection::apply(&e, "selnext").unwrap();
    assert_eq!(e.selections().len(), 2);
    selection::apply(&e, "upper").unwrap();
    assert_eq!(e.text(), "STRASSE x STRASSE\nsecond\nthird");
    e.send(SCI_UNDO, 0, 0);
    assert_eq!(e.text(), "straße x straße\nsecond\nthird");
    e.set_text("a\na\na");
    e.set_selection(0, 5);
    selection::apply(&e, "sellines").unwrap();
    selection::number(&e, 10, 2).unwrap();
    assert_eq!(e.text(), "10\n12\n14");
    e.send(SCI_UNDO, 0, 0);
    assert_eq!(e.text(), "a\na\na");
    e.set_bytes(b"abc\r\nx\nDEF\r\nz\xff");
    e.set_selection(6, 6);
    e.send(SCI_ADDSELECTION, 13, 13);
    selection::apply(&e, "moveup").unwrap();
    assert_eq!(e.raw_bytes(0, e.len()), b"x\r\nabc\nz\xff\r\nDEF");
    e.send(SCI_UNDO, 0, 0);
    assert_eq!(e.raw_bytes(0, e.len()), b"abc\r\nx\nDEF\r\nz\xff");
    e.set_text("ru");
    e.set_language("Rust");
    e.set_selection(2, 2);
    e.set_workspace_words(vec!["run_worker".to_string()]);
    assert!(e.complete());
    assert_ne!(e.send(SCI_AUTOCACTIVE, 0, 0), 0);
    e.send(SCI_AUTOCCANCEL, 0, 0);
    e.set_text("");
    assert!(e.insert_snippet("fn · function"));
    assert_eq!(e.selected_text(), "name");
    // Deliver the actual post-edit byte deltas as a WM_NOTIFY host does.
    let mut notice: Notification = unsafe { std::mem::zeroed() };
    notice.nmhdr.hwnd_from = e.hwnd();
    notice.nmhdr.code = SCN_MODIFIED;
    let (a, b) = e.selection();
    e.replace_range(a, b, "run_action");
    notice.position = a as isize;
    notice.length = (b - a) as isize;
    notice.modification_type = 2;
    e.on_notify(&notice);
    notice.length = 10;
    notice.modification_type = 1;
    e.on_notify(&notice);
    e.set_selection(a + 10, a + 10);
    assert!(e.snippet_next());
    assert_eq!(e.selected_text(), "args");
    let snippet = linework_editor::UserSnippet {
        name: "binding".into(),
        trigger: "bind".into(),
        language: "Rust".into(),
        body: "${2:value} = ${1:clé};\nreturn $1;\n$0".into(),
    };
    assert!(snippet.validate().is_ok());
    e.set_user_snippets(&[snippet]);
    e.set_text("");
    e.send(SCI_SETEOLMODE, 2, 0);
    assert!(e.snippet_labels().iter().any(|s| s == "bind · binding"));
    assert!(e.insert_snippet("bind · binding"));
    assert_eq!(e.text(), "value = clé;\nreturn clé;\n");
    assert_eq!(e.selected_text(), "clé");
    assert_eq!(e.selections().len(), 2);
    assert!(e.snippet_next());
    assert_eq!(e.selected_text(), "value");
    assert!(e.snippet_next());
    assert_eq!(e.caret(), e.len());
    e.set_text("    fn main() {\n");
    e.set_selection(e.len(), e.len());
    notice.nmhdr.code = SCN_CHARADDED;
    notice.ch = 10;
    e.on_notify(&notice);
    assert!(e.text().ends_with("{\n        "));
    e.set_text("This sentnce contains a mistkae.");
    e.set_language("Plain Text");
    e.set_spellcheck(true).unwrap();
    e.set_selection(8, 8);
    assert_ne!(e.send(SCI_INDICATORVALUEAT, 8, 8), 0);
    assert!(!e.spelling_suggestions().is_empty());
    assert!(e.replace_spelling("sentence"));
    assert!(e.text().starts_with("This sentence"));
    e.set_text("let foobaar = 1; // sentnce\n");
    e.set_language("Rust");
    e.refresh_spelling();
    assert_eq!(e.send(SCI_INDICATORVALUEAT, 8, 5), 0);
    assert_ne!(e.send(SCI_INDICATORVALUEAT, 8, 22), 0);
    let folder =
        std::env::temp_dir().join(format!("linework-dictionary-test-{}", std::process::id()));
    std::fs::create_dir_all(&folder).unwrap();
    let dic = folder.join("sv.dic");
    std::fs::write(dic.with_extension("aff"), "SET UTF-8\n").unwrap();
    std::fs::write(&dic, "2\nsmörgås\nSverige\n").unwrap();
    e.set_text("smörgås smörgåsx");
    e.set_language("Plain Text");
    e.set_dictionary(Some(&dic)).unwrap();
    assert_eq!(e.send(SCI_INDICATORVALUEAT, 8, 2), 0);
    assert_ne!(e.send(SCI_INDICATORVALUEAT, 8, 13), 0);
    e.set_dictionary(None).unwrap();
    std::fs::remove_file(dic.with_extension("aff")).unwrap();
    std::fs::remove_file(&dic).unwrap();
    std::fs::remove_dir(folder).unwrap();
    e.set_text("before\0after");
    assert_eq!(e.text(), "before\0after");
    let bad = b"before\xFF\0after";
    e.set_bytes(bad);
    assert_eq!(e.raw_bytes(0, e.len()), bad);
    drop(e);
    unsafe {
        DestroyWindow(parent);
        SetThreadDesktop(old);
        CloseDesktop(desktop);
    }
}

fn verify_imported_grammar(e: &Editor) {
    use linework_editor::{import_user_grammar, load_user_grammars};
    let dir = std::env::temp_dir().join(format!("linework-grammar-test-{}", std::process::id()));
    let installed = dir.join("installed");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("Example.sublime-syntax");
    let syntax = r#"%YAML 1.2
---
name: Example
scope: source.example
file_extensions: [example]
contexts:
  main:
    - match: '\bRUN\b'
      scope: keyword.control.example
    - match: '/\*'
      push: block
  block:
    - meta_scope: comment.block.example
    - match: '/\*'
      push: block
    - match: '\*/'
      pop: true
"#;
    std::fs::write(&file, syntax).unwrap();
    let name = import_user_grammar(&installed, &file).unwrap();
    assert_eq!(name, "Example · Sublime");
    assert!(languages().contains(&name));
    assert_eq!(detect_language(Some(Path::new("a.EXAMPLE")), ""), name);
    assert!(load_user_grammars(&installed).unwrap().is_empty());
    let source = "RUN /* outer\n/* inner */ continued\n*/ RUN\n";
    e.set_text(source);
    assert!(e.set_language(name));
    let pos = source.find("continued").unwrap();
    let check = |at, style| {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(6);
        loop {
            e.pump_grammar();
            if e.send(SCI_GETSTYLEAT, at, 0) == style {
                break;
            }
            assert!(
                std::time::Instant::now() < until,
                "native byte {at} did not become style {style}"
            );
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
    };
    check(pos, 1);
    check(0, 3);
    check(source.rfind("RUN").unwrap(), 3);
    e.replace_range(4, 6, "  ");
    check(pos, 0);
    std::fs::write(&file, "name: Broken\ncontexts: [\n").unwrap();
    assert!(import_user_grammar(&installed, &file).is_err());
    assert_eq!(
        std::fs::read_to_string(installed.join("Example.sublime-syntax")).unwrap(),
        syntax
    );
    let package = dir.join("Packages/Project JSON/Syntaxes");
    std::fs::create_dir_all(&package).unwrap();
    let child = package.join("Project JSON.sublime-syntax");
    std::fs::write(&child,"name: Project JSON\nversion: 2\nscope: source.projectjson\nextends: Packages/JSON/JSON.sublime-syntax\nfile_extensions: [linejson]\ncontexts:\n  main:\n    - meta_prepend: true\n    - match: '\\GX'\n      scope: keyword.control\n    - match: '@'\n      scope: entity.name\n").unwrap();
    assert!(load_user_grammars(&installed).unwrap().is_empty());
    let name = detect_language(Some(Path::new("nested.linejson")), "");
    assert_eq!(name, "Project JSON · Sublime");
    e.set_text("@X[true]\n");
    assert!(e.set_language(name));
    check(0, 5);
    check(1, 3);
    check(4, 4);
    std::fs::remove_file(child).unwrap();
    for folder in [
        package,
        dir.join("Packages/Project JSON"),
        dir.join("Packages"),
    ] {
        std::fs::remove_dir(folder).unwrap();
    }
    e.set_language("Plain Text");
    std::fs::remove_file(installed.join("Example.sublime-syntax")).unwrap();
    std::fs::remove_file(file).unwrap();
    std::fs::remove_dir(installed).unwrap();
    std::fs::remove_dir(dir).unwrap();
}
