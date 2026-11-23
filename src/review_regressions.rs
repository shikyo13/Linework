use super::*;
use linework_project::{ToolMode, UserTool};
use linework_storage::Fold;

#[link(name = "user32")]
unsafe extern "system" {
    fn CreateDesktopW(
        name: *const u16,
        device: *const u16,
        mode: *const std::ffi::c_void,
        flags: u32,
        access: u32,
        sa: *const std::ffi::c_void,
    ) -> isize;
    fn SetThreadDesktop(handle: isize) -> i32;
    fn GetThreadDesktop(thread: u32) -> isize;
    fn CloseDesktop(handle: isize) -> i32;
}
struct Desktop {
    before: isize,
    handle: isize,
}
impl Desktop {
    fn create() -> Self {
        unsafe {
            let before =
                GetThreadDesktop(windows_sys::Win32::System::Threading::GetCurrentThreadId());
            let handle = CreateDesktopW(
                wide(&format!("LineworkReview{}", std::process::id())).as_ptr(),
                null(),
                null(),
                0,
                0x1ff,
                null(),
            );
            assert_ne!(handle, 0);
            assert_ne!(SetThreadDesktop(handle), 0);
            Self { before, handle }
        }
    }
}
impl Drop for Desktop {
    fn drop(&mut self) {
        unsafe {
            SetThreadDesktop(self.before);
            CloseDesktop(self.handle);
        }
    }
}
fn wait_worker(app: &mut App, mut accept: impl FnMut(&Worker) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let w = app
            .rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("expected completed operation");
        let done = accept(&w);
        app.worker(w);
        if done {
            return;
        }
    }
}
fn wait_plugin(app: &mut App, path: &Path, line: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        app.sublime_tick();
        let log = fs::read_to_string(path).unwrap_or_default();
        if log.lines().any(|s| s == line) {
            return log;
        }
        assert!(
            Instant::now() < deadline,
            "waiting for {line}, got {log:?}, status {}, host log {}",
            app.status,
            fs::read_to_string(
                path.with_file_name(format!("sublime-host-{}.log", std::process::id()))
            )
            .unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(4));
    }
}
fn fixture(app: &mut App, file: &Path, text: &str) -> usize {
    fs::write(file, text).unwrap();
    assert!(app.npp_open(file.into(), true));
    app.current().unwrap()
}
fn append(app: &mut App, i: usize, text: &str) {
    app.sync_buffer(i).unwrap();
    let at = app.buffers[i].doc.len() as usize;
    app.change_open(i, vec![(at, at, text.into())]).unwrap();
    app.drain_edits();
}
fn checkpoints(app: &App) -> Vec<FileState> {
    state::read(&app.recovery_dir.join("recovery.json")).unwrap()
}
fn native_file_and_package_events(app: &mut App, dir: &Path) {
    let plugin = dir.join("Packages/Probe");
    fs::create_dir_all(&plugin).unwrap();
    fs::write(
        plugin.join("events.py"),
        r#"import sublime, sublime_plugin
from pathlib import Path
def record(event, view):
    with (Path(sublime.packages_path()).parent / 'probe.log').open('a', encoding='utf-8') as log:
        print(event, view.id(), file=log)
class Probe(sublime_plugin.ViewEventListener):
    def __init__(self, view):
        super().__init__(view)
        record('constructed',view)
    def on_activated(self):
        record('active',self.view)
    def on_post_save(self):
        record('saved',self.view)
    def on_close(self):
        record('released',self.view)
class Global(sublime_plugin.EventListener):
    def on_close(self,view):
        record('closed',view)
"#,
    )
    .unwrap();
    let log = dir.join("probe.log");
    let first = fixture(app, &dir.join("alpha.txt"), "alpha");
    let id = app.buffers[first].id;
    wait_plugin(app, &log, &format!("active {id}"));
    let second = fixture(app, &dir.join("Makefile"), "rule:");
    let other = app.buffers[second].id;
    assert_eq!(app.edit().send(sn::SCI_GETUSETABS, 0, 0), 1);
    wait_plugin(app, &log, &format!("active {other}"));
    append(app, first, "+1");
    append(app, second, "+2");
    app.auto_at = Instant::now() - Duration::from_secs(6);
    app.changed_at = Instant::now();
    app.event(Event::Timer);
    assert!(app.checkpoint_running);
    append(app, first, "+newer");
    wait_worker(app, |w| matches!(w, Worker::Checkpoint(..)));
    let states = checkpoints(app);
    assert_eq!(states.len(), 2);
    for (s, expected) in states.into_iter().zip(["alpha+1", "rule:+2"]) {
        let d = Document::recover(Path::new(&s.recovery.unwrap())).unwrap();
        assert_eq!(d.read(0, d.len() as usize).unwrap(), expected.as_bytes());
    }
    assert!(!app.saved_state);
    app.command("saveall");
    while app.buffers.iter().any(|b| b.saving) {
        wait_worker(app, |w| matches!(w, Worker::Saved(..)));
    }
    wait_plugin(app, &log, &format!("saved {id}"));
    wait_plugin(app, &log, &format!("saved {other}"));
    append(app, second, "temporary");
    app.apply_history(second, 0, false);
    app.drain_edits();
    assert!(!app.buffers[second].dirty);
    assert!(!app.npp_save(other, Some(dir.into()), true));
    app.remove(first); // the background view is closed while the Makefile is active
    let log_text = wait_plugin(app, &log, &format!("released {id}"));
    for event in [
        format!("constructed {id}"),
        format!("constructed {other}"),
        format!("saved {id}"),
        format!("saved {other}"),
        format!("closed {id}"),
    ] {
        assert_eq!(
            log_text.lines().filter(|s| *s == event).count(),
            1,
            "{event} in {log_text}"
        );
    }
    app.schedule_checkpoint();
    wait_worker(app, |w| matches!(w, Worker::Checkpoint(..)));
    assert!(checkpoints(app).iter().all(|s| s.recovery.is_none()));
    assert!(!app.recovery_dir.read_dir().unwrap().flatten().any(|e| {
        e.path()
            .extension()
            .is_some_and(|s| s == "lwr" || s == "lwbase")
    }));
    app.remove(0);
    wait_plugin(app, &log, &format!("released {other}"));
}
fn bookmarks_and_preferences(app: &mut App, dir: &Path) {
    let i = fixture(app, &dir.join("sort.txt"), "zeta\nalpha\nbeta\n");
    app.buffers[i].marks.insert(7); // alpha, column 3
    app.edit().set_selection(12, 12); // beta, column 2
    app.command("sortlines");
    assert_eq!(app.edit().text(), "alpha\nbeta\nzeta\n");
    assert_eq!(
        app.buffers[i].marks.iter().copied().collect::<Vec<_>>(),
        [2]
    );
    assert_eq!(app.edit().caret(), 7);
    app.go(0, 0);
    app.command("bookmark");
    assert!(app.buffers[i].marks.is_empty());
    let base = Prefs::default();
    let mut a = base.clone();
    a.tab_width = 7;
    let mut b = base.clone();
    b.shortcuts.insert("play".into(), "F11".into());
    state::save_settings(&a, &base, false, None).unwrap();
    state::save_settings(&b, &base, false, None).unwrap();
    let stored = state::load_prefs().0;
    assert_eq!(stored.tab_width, 7);
    assert_eq!(stored.shortcuts["play"], "F11");
    let damaged = serde_json::json!({"tab_width":"damaged","shortcuts":{"play":"F11"},"zoom":3});
    fs::write(
        dir.join("settings.json"),
        serde_json::to_vec(&damaged).unwrap(),
    )
    .unwrap();
    let (salvaged, warning) = state::load_prefs();
    assert!(warning.is_some());
    assert_eq!(salvaged.tab_width, 7);
    assert_eq!(salvaged.zoom, 3);
    state::save_settings(&salvaged, &salvaged, false, None).unwrap();
    assert!(dir.read_dir().unwrap().flatten().any(|p| {
        p.file_name()
            .to_string_lossy()
            .starts_with("settings-damaged-")
    }));
    assert_eq!(
        commands::normalize_chord(" shift + ctrl + f11 ").as_deref(),
        Some("Ctrl+Shift+F11")
    );
    assert!(commands::normalize_chord("Ctrl+Ctrl+T").is_none());
    assert!(!commands::COMMANDS.iter().any(|c| {
        c.key
            .strip_prefix("Ctrl+Alt+")
            .is_some_and(|key| key.len() == 1 && key.as_bytes()[0].is_ascii_alphabetic())
    }));
    let id = app.buffers[i].id;
    app.macro_steps = vec![
        Step::Native {
            msg: sn::SCI_CHARLEFT,
            arg: 0,
            text: None
        };
        90_000
    ];
    app.macro_command("play");
    assert!(app.playing);
    app.new_file();
    assert!(!app.playing);
    append(app, i, "1");
    append(app, i, "2");
    app.apply_history(i, 0, false);
    app.sync_buffer(i).unwrap();
    assert!(app.buffers[i].doc.read(0, 64).unwrap().ends_with(b"1"));
    fs::write(dir.join("sort.txt"), "changed on disk").unwrap();
    app.reload_disk(i);
    wait_worker(app, |w| matches!(w, Worker::Recoded(..)));
    assert_eq!(app.buffers[i].id, id);
    assert_eq!(app.buffers[i].doc.read(0, 64).unwrap(), b"changed on disk");
    assert_ne!(app.assigned[app.active], Some(id));
    app.remove(1);
    app.remove(i);
}
fn optional_real_plugin(app: &mut App, dir: &Path, i: usize, target: u64) {
    let Some(dll) = std::env::var_os("LINEWORK_TEST_MIME_DLL").map(PathBuf::from) else {
        return;
    };
    let disabled = crate::npp::home().join(dll.file_stem().unwrap());
    fs::create_dir_all(&disabled).unwrap();
    fs::write(disabled.join(".linework-disabled"), "").unwrap();
    fs::write(disabled.join("previous-user-data"), "keep").unwrap();
    app.npp_install(&dll).unwrap();
    assert!(
        dir.join("Removed Plugins")
            .read_dir()
            .unwrap()
            .flatten()
            .any(|old| old
                .path()
                .join(dll.file_stem().unwrap())
                .join("previous-user-data")
                .is_file())
    );
    let (cmd, _) = crate::npp::rows()
        .into_iter()
        .find(|(_, name)| {
            name.rsplit('/')
                .next()
                .is_some_and(|s| s.trim().eq_ignore_ascii_case("Base64 Encode"))
        })
        .expect("bundled MIME Tools has Base64 Encode");
    app.set_extension_selections(0, &[(target, target + 6)])
        .unwrap();
    let ed = app.edit().hwnd();
    let host = app.h;
    let raw = unsafe { GetDlgItem(hwnd(host), 720) } as H;
    let expected_color = msg(ed, sn::SCI_STYLEGETFORE, 32, 0);
    crate::npp::pump(app);
    crate::npp::dispatch(app, || {
        assert_ne!(raw, 0);
        let mut copy = [0xa5u8; 9];
        msg(raw, sn::SCI_GETTEXT, 6, copy.as_mut_ptr() as _);
        assert_eq!(&copy[..], b"header\0\xa5\xa5");
        assert_eq!(msg(raw, sn::SCI_GETCOLUMN, (target + 3) as usize, 0), 3);
        assert_eq!(
            msg(raw, sn::SCI_WORDSTARTPOSITION, (target + 3) as usize, 1),
            target as isize
        );
        assert_eq!(
            msg(raw, sn::SCI_WORDENDPOSITION, (target + 3) as usize, 1),
            (target + 6) as isize
        );
        assert_eq!(msg(host, WM_USER + 1000 + 90, 0, 0), expected_color);
        assert_eq!(msg(raw, sn::SCI_GETSTATUS, 0, 0), 0);
    });
    assert!(crate::npp::invoke(app, cmd));
    assert_eq!(app.buffers[i].doc.read(target, 8).unwrap(), b"VGFyZ2V0");
    app.apply_history(i, 0, false);
    assert_eq!(app.buffers[i].doc.read(target, 6).unwrap(), b"Target");
}
fn file_wide_edits_and_active_writer(app: &mut App, dir: &Path, i: usize, source: &Path) {
    app.select(i, 0, Some(0));
    let id = app.buffers[i].id;
    app.apply_folds(
        i,
        vec![Fold {
            header: 0,
            start: 7,
            end: 23,
        }],
        0,
    );
    let mark = 200_000;
    app.buffers[i].marks.insert(mark);
    let cursors = (0..1100)
        .map(|k| {
            let at = 1200 + 17 * k;
            (at, at)
        })
        .collect::<Vec<_>>();
    app.set_extension_selections(0, &cursors).unwrap();
    let view = app.edit().hwnd();
    assert!(app.global_char(view, 'Q' as u16));
    assert!(global_edit::pending(id));
    assert!(app.global_char(view, 'R' as u16));
    app.save(i, false, false);
    assert!(!app.buffers[i].saving);
    app.new_file();
    let fresh = app.current().unwrap();
    wait_worker(app, |w| matches!(w, Worker::CursorEdit(_)));
    assert_eq!(app.edit().text(), "");
    assert_eq!(app.buffers[i].doc.read(1200, 1).unwrap(), b"Q");
    assert!(app.buffers[i].marks.contains(&(mark + 1100)));
    assert_eq!(app.closed(id, app.buffers[i].rev).len(), 1);
    app.remove(fresh);
    app.remove(i);
    let path = dir.join("being-written.txt");
    fs::copy(source, &path).unwrap();
    let writer = OpenOptions::new().write(true).open(&path).unwrap();
    assert!(app.npp_open(path.clone(), true));
    let i = app.current().unwrap();
    app.edit().replace_range(2, 2, "x");
    app.dispatch_pending();
    assert!(app.buffers[i].preparing.is_some());
    assert_eq!(app.buffers[i].doc.read(0, 6).unwrap(), b"header");
    wait_worker(app, |w| matches!(w, Worker::Prepared(..)));
    assert_eq!(app.buffers[i].doc.read(0, 7).unwrap(), b"hexader");
    assert_eq!(&fs::read(&path).unwrap()[..6], b"header");
    drop(writer);
}
fn paged_filter_and_native_controls(app: &mut App, dir: &Path) {
    let path = dir.join("paged.txt");
    let prefix = b"header\nhidden sentinel\nTarget after fold\n";
    let mut bytes = prefix.to_vec();
    bytes.extend(b"0123456789abcdef\n".repeat((LARGE as usize + 1) / 17));
    fs::write(&path, &bytes).unwrap();
    assert!(app.npp_open(path.clone(), true));
    let i = app.current().unwrap();
    let id = app.buffers[i].id;
    assert!(app.buffers[i].large);
    let first = 7u64;
    let target = 23u64;
    app.select(i, 1, None);
    app.select(i, 0, None);
    app.apply_folds(
        i,
        vec![Fold {
            header: 0,
            start: first,
            end: target,
        }],
        0,
    );
    assert_eq!(events::range(0, 0, first), 0..first);
    assert_eq!(events::range(0, first, first + 6), target..target + 6);
    for p in 0..2 {
        assert!(
            app.editors[p]
                .text_range(0, 18)
                .starts_with("header\nTarget")
        );
    }
    app.edit().set_selection(first as usize, first as usize + 6);
    let tool = UserTool {
        name: "Uppercase".into(),
        program: Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("runtime/python/python.exe")
            .display()
            .to_string(),
        args: [
            "-I",
            "-B",
            "-c",
            "import sys; sys.stdout.buffer.write(sys.stdin.buffer.read().upper())",
        ]
        .map(str::to_owned)
        .to_vec(),
        mode: ToolMode::Filter,
        shortcut: None,
    };
    app.run_project_tool(tool.clone());
    wait_worker(app, |w| matches!(w, Worker::Tool(..)));
    assert_eq!(app.status, "Selection replaced");
    assert_eq!(app.closed(id, app.buffers[i].rev).len(), 1);
    for p in 0..2 {
        assert!(
            app.editors[p]
                .text_range(0, 18)
                .starts_with("header\nTARGET")
        );
    }
    assert!(app.npp_save(id, None, false));
    bytes[target as usize..target as usize + 6].copy_from_slice(b"TARGET");
    assert_eq!(fs::read(&path).unwrap(), bytes);
    app.select_span(32..(17 * 1024 * 1024 + 32), true);
    app.run_project_tool(tool);
    assert!(app.status.contains("16 MiB"));
    app.apply_history(i, 0, false);
    assert_eq!(app.global_pos(), target);
    assert!(app.npp_save(id, None, false));
    bytes[target as usize..target as usize + 6].copy_from_slice(b"Target");
    assert_eq!(fs::read(&path).unwrap(), bytes);
    optional_real_plugin(app, dir, i, target);
    app.findbar = true;
    app.layout();
    unsafe {
        UpdateWindow(hwnd(app.h));
    }
    let mut labels = Vec::new();
    for id in 4000..4250 {
        let child = unsafe { GetDlgItem(hwnd(app.h), id) };
        if child.is_null() {
            break;
        }
        labels.push(text(child as H));
    }
    assert!(labels.iter().any(|s| s == "File menu"), "{labels:?}");
    assert!(
        labels
            .iter()
            .any(|s| s.contains("paged.txt") && s.contains("selected")),
        "{labels:?}"
    );
    assert!(labels.iter().any(|s| s == "Next match"), "{labels:?}");
    let next = labels.iter().position(|s| s == "Next match").unwrap();
    let child = unsafe { GetDlgItem(hwnd(app.h), 4000 + next as i32) };
    msg(child as H, BM_CLICK, 0, 0);
    assert!(events::EVENTS.with_borrow(|q| {
        q.iter()
            .any(|e| matches!(e,Event::Hit(Hit::Cmd(cmd),_,_) if cmd=="findnext"))
    }));
    events::EVENTS.with_borrow_mut(|q| q.retain(|ev| !matches!(ev, Event::Hit(..))));
    file_wide_edits_and_active_writer(app, dir, i, &path);
}
#[test]
fn real_window_checkpoints_packages_folds_and_accessible_controls() {
    let _desktop = Desktop::create(); // This desktop is never switched to or displayed to the user.
    let dir = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("LINEWORK_PROFILE", dir.path());
        std::env::set_var(
            "LINEWORK_RUNTIME",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime"),
        );
        windows_sys::Win32::System::Com::CoInitializeEx(null(), 2);
    }
    register("LineworkReviewTest", Some(events::proc)).unwrap();
    crate::surface::initialize().unwrap();
    forms::init().unwrap();
    let window = unsafe {
        CreateWindowExW(
            0,
            wide("LineworkReviewTest").as_ptr(),
            wide("Linework test").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            0,
            0,
            1200,
            800,
            null_mut(),
            null_mut(),
            module() as _,
            null(),
        )
    };
    assert!(!window.is_null());
    let mut app = App::new(window as H).unwrap();
    unsafe {
        ShowWindow(window, SW_SHOWNOACTIVATE);
    }
    crate::npp::initialize(&mut app);
    app.layout();
    let (build, flag) = app.new_job("build");
    let (_, find) = app.new_job("find");
    app.new_job("find");
    assert!(
        !flag.load(Ordering::Relaxed)
            && find.load(Ordering::Relaxed)
            && app.current_job("build", build)
    );
    app.cancel_job("build");
    app.cancel_job("find");
    native_file_and_package_events(&mut app, dir.path());
    bookmarks_and_preferences(&mut app, dir.path());
    paged_filter_and_native_controls(&mut app, dir.path());
    crate::npp::shutdown(&mut app);
    destroy(app.h);
    unsafe {
        windows_sys::Win32::System::Com::CoUninitialize();
    }
}

#[test]
fn tab_paints_during_native_reposition() {
    use windows_sys::Win32::{
        Graphics::Gdi::*,
        UI::{Controls::{DRAWITEMSTRUCT, ODT_BUTTON}, Shell::{DefSubclassProc, SetWindowSubclass}},
    };
    struct Sample { dc: HDC, handled: isize, pixel: u32 }
    unsafe extern "system" fn observe(
        h: HWND, m: u32, w: usize, l: isize, _: usize, data: usize,
    ) -> isize {
        unsafe {
            let res = DefSubclassProc(h, m, w, l);
            if m == WM_WINDOWPOSCHANGED {
                let sample = &mut *(data as *mut Sample);
                let mut item: DRAWITEMSTRUCT = std::mem::zeroed();
                item.CtlType = ODT_BUTTON;
                item.CtlID = GetDlgCtrlID(h) as u32;
                item.hwndItem = h;
                item.hDC = sample.dc;
                GetClientRect(h, &mut item.rcItem);
                SetPixelV(sample.dc, 4, 12, 0x00ff00);
                sample.handled = SendMessageW(GetParent(h), WM_DRAWITEM, item.CtlID as usize, &item as *const _ as isize);
                sample.pixel = GetPixel(sample.dc, 4, 12);
            }
            res
        }
    }
    let _desktop = Desktop::create();
    crate::surface::initialize().unwrap();
    register("LineworkResizeTest", Some(events::proc)).unwrap();
    unsafe {
        let h = CreateWindowExW(0, wide("LineworkResizeTest").as_ptr(), wide("Resize").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN, 0, 0, 960, 620, null_mut(), null_mut(), module() as _, null());
        assert!(!h.is_null());
        frame::FRAME.replace(Frame { sidebar: true, sidebar_width: 230, find: true, replace: true,
            tabs: vec![("example.md".into(), false)], active: Some(0), ..Default::default() });
        ShowWindow(h, SW_SHOWNOACTIVATE);
        invalidate(h as H);
        UpdateWindow(h);
        let tab = (4000..4040).map(|id| GetDlgItem(h, id))
            .find(|b| !b.is_null() && text(*b as H).contains("example.md tab")).unwrap();
        let screen = GetDC(h);
        let dc = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, 256, 96);
        let old = SelectObject(dc, bmp);
        let mut sample = Sample { dc, handled: -1, pixel: 0 };
        assert_ne!(SetWindowSubclass(tab, Some(observe), 1, &mut sample as *mut _ as usize), 0);
        for (light, width) in [(false, 310), (true, 250)] {
            LIGHT.set(light);
            frame::FRAME.with_borrow_mut(|f| { f.light = light; f.sidebar_width = width; });
            sample.handled = -1;
            invalidate(h as H);
            UpdateWindow(h);
            assert_eq!(sample.handled, 1, "Windows requested tab artwork inside SetWindowPos");
            assert_eq!(sample.pixel, colors(light).bg, "the active tab must already have its themed pixels");
            for (label, background) in [
                ("example.md tab, selected", colors(light).bg),
                ("File menu", colors(light).panel),
                ("Replace all", colors(light).active),
            ] {
                let button = (4000..4040).map(|id| GetDlgItem(h, id))
                    .find(|b| !b.is_null() && text(*b as H) == label).unwrap();
                let mut item: DRAWITEMSTRUCT = std::mem::zeroed();
                item.CtlType = ODT_BUTTON;
                item.CtlID = GetDlgCtrlID(button) as u32;
                item.hwndItem = button;
                item.hDC = dc;
                GetClientRect(button, &mut item.rcItem);
                fill(dc, item.rcItem, 0x00ff00);
                assert_eq!(SendMessageW(h, WM_DRAWITEM, item.CtlID as usize, &item as *const _ as isize), 1);
                assert_eq!(GetPixel(dc, 4, 12), background, "{label} background");
                let middle = item.rcItem.bottom / 2;
                assert!((middle-4..=middle+4).any(|y| (7..item.rcItem.right-7).any(|x| {
                    let pixel = GetPixel(dc, x, y);
                    pixel != background && pixel != 0x00ff00
                })), "{label} must contain its visible glyphs");
                assert_eq!(SendMessageW(h, WM_CTLCOLORBTN, dc as usize, button as isize),
                    GetStockObject(NULL_BRUSH) as isize, "{label} erase brush");
            }
        }
        let list = child(h as H, 601, "LISTBOX", "", WS_VISIBLE | LBS_HASSTRINGS as u32 | LBS_OWNERDRAWFIXED as u32);
        frame::controls::buffer_list(list, false);
        msg(list, LB_ADDSTRING, 0, wide("example.md").as_ptr() as _);
        msg(list, LB_ADDSTRING, 0, wide("another.txt").as_ptr() as _);
        for light in [false, true] {
            LIGHT.set(light);
            frame::FRAME.with_borrow_mut(|f| f.light = light);
            let mut previous = None;
            for width in [210, 246, 180] {
                place(list, 48, 125, width, 80);
                fill(dc, rect(0, 0, 250, 80), 0x00ff00);
                assert_eq!(msg(list, WM_ERASEBKGND, dc as usize, 0), 1);
                assert_eq!(GetPixel(dc, 35, 6), 0x00ff00, "erase must preserve the previous complete frame");
                msg(list, WM_PRINTCLIENT, dc as usize, PRF_CLIENT as isize);
                let pixels: Vec<_> = (3..22).flat_map(|y| (31..95).map(move |x| GetPixel(dc, x, y))).collect();
                assert!(pixels.contains(&colors(light).active), "selected row has its highlight");
                assert!(pixels.iter().filter(|&&p| p != colors(light).active && p != 0x00ff00).count() > 20, "filename is visible");
                assert!(!pixels.contains(&0x00ff00));
                if let Some(old) = previous { assert_eq!(old, pixels, "stationary filename changed during a width update"); }
                previous = Some(pixels);
            }
        }
        SelectObject(dc, old);
        DeleteObject(bmp);
        DeleteDC(dc);
        ReleaseDC(h, screen);
        destroy(h as H);
    }
}
