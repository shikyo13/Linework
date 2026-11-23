mod abi;
mod converter;
mod docking;
mod messages;
mod paged;
mod panes;
mod scintilla;
use crate::{app::App, events, state, win::*};
use abi::*;
use linework_editor::Notification;
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::*,
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

#[derive(Clone, Default)]
struct Doc {
    id: u64,
    path: PathBuf,
    language: String,
    encoding: i32,
    eol: i32,
}
#[derive(Default)]
struct Host {
    data: Data,
    plugins: Vec<Plugin>,
    dialogs: Vec<H>,
    menu: H,
    root: H,
    active: usize,
    docs: Vec<Doc>,
    assigned: [Option<u64>; 2],
    split: bool,
    light: bool,
    macro_status: isize,
    cmd: i32,
    indicator: i32,
    marker: i32,
    pending: VecDeque<i32>,
    ready: bool,
    running: Vec<i32>,
    tools: Vec<(H, String, String)>,
}
thread_local! {
    static HOST:RefCell<Host>=RefCell::default();
    static LIVE:Cell<*mut App>=const{Cell::new(null_mut())};
}
struct Restore(*mut App);
impl Drop for Restore {
    fn drop(&mut self) {
        LIVE.set(self.0)
    }
}
// A native call may synchronously re-enter the host. Lend App to one nested
// request at a time. Pure ABI queries use the independent host snapshot.
pub fn dispatch<T>(app: &mut App, f: impl FnOnce() -> T) -> T {
    let _restore = Restore(LIVE.replace(app as *mut App));
    f()
}
fn with_app<T>(f: impl FnOnce(&mut App) -> T) -> Option<T> {
    let ptr = LIVE.replace(null_mut());
    let _restore = Restore(ptr);
    if ptr.is_null() {
        None
    } else {
        Some(f(unsafe { &mut *ptr }))
    }
}
// Native move/size loops run inside DispatchMessage, before the outer event
// queue can drain. Use the same exclusive, reentrancy-aware app borrow.
pub(crate) fn live_resize(host: H) -> bool {
    with_app(|app| {
        if app.h != host { return false; }
        app.resize_client();
        true
    }).unwrap_or(false)
}
pub(crate) fn home() -> PathBuf {
    state::profile().join("plugins")
}
fn reserve_commands(n: usize) -> Option<i32> {
    HOST.with_borrow_mut(|h| {
        let end = h.cmd.checked_add(i32::try_from(n).ok()?)?;
        if n == 0 || end > 40000 {
            return None;
        }
        let start = h.cmd;
        h.cmd = end;
        Some(start)
    })
}
fn directories() -> Vec<PathBuf> {
    let mut files = vec![];
    if let Ok(entries) = home().read_dir() {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir()
                && p.file_name()
                    .is_some_and(|s| !s.eq_ignore_ascii_case("Config"))
            {
                let mut file = p.file_name().unwrap().to_owned();
                file.push(".dll");
                let dll = p.join(file);
                if dll.is_file() && !p.join(".linework-disabled").exists() {
                    files.push(dll)
                }
            }
        }
    }
    files.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    files
}
pub fn initialize(app: &mut App) {
    docking::init();
    let _ = fs::create_dir_all(home().join("Config"));
    let data = Data {
        host: app.h,
        main: app.editors[0].hwnd(),
        second: app.editors[1].hwnd(),
    };
    let (menu, root) = unsafe {
        let menu = CreatePopupMenu();
        let root = CreateMenu();
        AppendMenuW(root, MF_POPUP, menu as usize, wide("Plugins").as_ptr());
        (menu as H, root as H)
    };
    HOST.with_borrow_mut(|h| {
        *h = Host {
            data,
            menu,
            root,
            cmd: 20000,
            indicator: 16,
            marker: 4,
            ..Host::default()
        }
    });
    let paths = directories();
    if !paths.is_empty() {
        if let Err(e) = panes::prepare(app) {
            app.notice(e);
            return;
        }
    }
    for path in paths {
        if let Err(e) = dispatch(app, || attach(&path)) {
            app.notice(format!(
                "{} · {e}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ))
        }
    }
}
fn attach(path: &Path) -> Result<(), String> {
    let (data, menu, existing) = HOST.with_borrow(|h| {
        (
            h.data,
            h.menu,
            h.plugins.iter().any(|p| {
                path.file_name()
                    .is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case(&p.file))
            }),
        )
    });
    if existing {
        return Err("This plugin is already loaded".into());
    }
    scintilla::install(data.main)?;
    scintilla::install(data.second)?;
    let p = abi::load(path, data)?;
    unsafe {
        let sub = CreatePopupMenu();
        for a in &p.actions {
            if a.callback.is_none() {
                AppendMenuW(sub, MF_SEPARATOR, 0, null());
            } else {
                AppendMenuW(
                    sub,
                    MF_STRING | if a.checked { MF_CHECKED } else { 0 },
                    a.id as usize,
                    wide(&a.label).as_ptr(),
                );
            }
        }
        AppendMenuW(menu as _, MF_POPUP, sub as usize, wide(&p.name).as_ptr());
    }
    HOST.with_borrow_mut(|h| h.plugins.push(p));
    Ok(())
}
pub(crate) fn installed() -> bool {
    HOST.with_borrow(|h| !h.plugins.is_empty())
}
fn deliver(n: *mut Notification) {
    let targets = HOST.with_borrow(|h| h.plugins.iter().map(|p| p.notify).collect::<Vec<_>>());
    for f in targets {
        unsafe { f(n) }
    }
}
fn notification(code: u32, id: usize) -> Notification {
    let mut n: Notification = unsafe { std::mem::zeroed() };
    n.nmhdr.hwnd_from = HOST.with_borrow(|h| h.data.host);
    n.nmhdr.code = code;
    n.nmhdr.id_from = id;
    n
}
fn announce(code: u32, id: usize) {
    deliver(&mut notification(code, id))
}
fn snapshot(app: &App) -> Vec<(u32, usize)> {
    HOST.with_borrow_mut(|h| {
        let mut changed = vec![];
        let old = h.assigned[h.active];
        let old_light = h.light;
        h.light = app.prefs.light;
        h.active = app.active;
        h.assigned = app.assigned;
        h.split = app.split;
        h.macro_status = if app.playing {
            3
        } else if app.recording {
            1
        } else if !app.macro_steps.is_empty() {
            2
        } else {
            0
        };
        // The usual event-loop pass keeps tab order and storage intact. Only
        // rebuild the ID map when the user adds, closes or reorders a tab.
        if h.docs.len() != app.buffers.len()
            || h.docs.iter().zip(&app.buffers).any(|(d, b)| d.id != b.id)
        {
            let mut previous: std::collections::HashMap<_, _> =
                h.docs.drain(..).map(|d| (d.id, d)).collect();
            for b in &app.buffers {
                h.docs.push(previous.remove(&b.id).unwrap_or_else(|| {
                    changed.push((1004, b.id as usize));
                    Doc {
                        id: b.id,
                        language: b.lang.clone(),
                        ..Doc::default()
                    }
                }));
            }
            changed.extend(previous.keys().map(|&id| (1005, id as usize)));
        }
        for (d, b) in h.docs.iter_mut().zip(&app.buffers) {
            if d.language != b.lang {
                d.language.clone_from(&b.lang);
                changed.push((1011, b.id as usize));
            }
            let path = b.path.as_deref().unwrap_or_else(|| Path::new(&b.name));
            if d.path != path {
                d.path = path.to_owned();
            }
            d.encoding = match b.doc.encoding_name() {
                "UTF-8 BOM" => 1,
                "UTF-16 BE" => 2,
                "UTF-16 LE" => 3,
                "UTF-8" => 4,
                _ => 0,
            };
            d.eol = match b.doc.newline() {
                "\r\n" => 0,
                "\r" => 1,
                "\n" => 2,
                _ => 3,
            };
        }
        if old != h.assigned[h.active] {
            if let Some(id) = h.assigned[h.active] {
                changed.push((1010, id as usize))
            }
        }
        if old_light != h.light {
            changed.push((1027, 0))
        }
        changed
    })
}
pub fn ready(app: &mut App) {
    snapshot(app);
    HOST.with_borrow_mut(|h| h.ready = true);
    dispatch(app, || {
        announce(1002, 0);
        announce(1001, 0);
        let active = active_id();
        if active != 0 {
            announce(1010, active)
        }
    })
}
pub fn pump(app: &mut App) {
    if !installed() {
        return;
    }
    let changes = snapshot(app);
    dispatch(app, || {
        for (code, id) in changes {
            announce(code, id)
        }
    });
    let queue = HOST.with_borrow_mut(|h| std::mem::take(&mut h.pending));
    for id in queue {
        invoke(app, id);
    }
}
pub fn shutdown(app: &mut App) {
    if HOST.with_borrow_mut(|h| std::mem::take(&mut h.ready)) {
        dispatch(app, || {
            announce(1019, 0);
            announce(1009, 0)
        });
        let root = HOST.with_borrow_mut(|h| {
            h.plugins.clear();
            std::mem::take(&mut h.root)
        });
        unsafe {
            DestroyMenu(root as _);
        }
    }
}
fn active_id() -> usize {
    HOST.with_borrow(|h| h.assigned[h.active].unwrap_or(0) as usize)
}
fn find_command(id: i32) -> Option<Command> {
    HOST.with_borrow(|h| {
        h.plugins
            .iter()
            .flat_map(|p| &p.actions)
            .find(|a| a.id == id)
            .and_then(|a| a.callback)
    })
}
pub(crate) fn invoke(app: &mut App, id: i32) -> bool {
    let Some(f) = find_command(id) else {
        return false;
    };
    if HOST.with_borrow(|h| h.running.contains(&id)) {
        return false;
    }
    snapshot(app);
    HOST.with_borrow_mut(|h| h.running.push(id));
    let adapter = HOST.with_borrow(|h| {
        h.plugins.iter().find_map(|p| {
            let wrap = p.converter_wrap?;
            let action = p.actions.iter().find(|a| a.id == id)?;
            Some((
                wrap,
                action.label == "ASCII -> HEX",
                action.label == "HEX -> ASCII",
            ))
        })
    });
    let pane = app.active;
    paged::command(app, adapter.is_some(), || {
        paged::invoke(pane, adapter, || unsafe { f() })
    });
    HOST.with_borrow_mut(|h| {
        h.running.pop();
    });
    true
}
pub fn pretranslate(app: &mut App, m: &MSG) -> bool {
    let dialogs = HOST.with_borrow_mut(|h| {
        h.dialogs.retain(|d| unsafe { IsWindow(hwnd(*d)) != 0 });
        h.tools
            .retain(|(d, _, _)| unsafe { IsWindow(hwnd(*d)) != 0 });
        h.dialogs.clone()
    });
    if dispatch(app, || {
        dialogs
            .into_iter()
            .any(|d| unsafe { IsWindowVisible(hwnd(d)) != 0 && IsDialogMessageW(hwnd(d), m) != 0 })
    }) {
        return true;
    }
    if !matches!(m.message, WM_KEYDOWN | WM_SYSKEYDOWN) || m.wParam > 255 {
        return false;
    }
    if !app.editors.iter().any(|e| e.hwnd() == m.hwnd as H) {
        return false;
    }
    let found = HOST.with_borrow(|h| {
        h.plugins
            .iter()
            .flat_map(|p| &p.actions)
            .find(|a| {
                let s = a.shortcut;
                s.key != 0
                    && s.key as usize == m.wParam
                    && (s.ctrl != 0) == down(VK_CONTROL as _)
                    && (s.alt != 0) == down(VK_MENU as _)
                    && (s.shift != 0) == down(VK_SHIFT as _)
            })
            .map(|a| a.id)
    });
    found.is_some_and(|id| invoke(app, id))
}
pub struct NotifyAfter(*mut Notification);
impl Drop for NotifyAfter {
    fn drop(&mut self) {
        if !self.0.is_null() {
            deliver(self.0)
        }
    }
}
// Forward synchronously, after the editor has copied the notification text.
// A plugin may send another Scintilla message from inside beNotified.
pub fn after_notification(l: isize, binding: events::Binding) -> NotifyAfter {
    NotifyAfter(if !binding.large && installed() {
        l as *mut _
    } else {
        null_mut()
    })
}
pub fn message(h: HWND, m: u32, w: usize, l: isize) -> Option<isize> {
    let ours = HOST.with_borrow(|s| s.data.host != 0 && s.data.host == h as H);
    if !ours {
        return None;
    }
    if (messages::BASE..messages::BASE + 200).contains(&m)
        || (WM_USER + 3001..=WM_USER + 3012).contains(&m)
    {
        return Some(messages::handle(m, w, l));
    }
    if m == WM_COMMAND && l == 0 && find_command((w & 65535) as i32).is_some() {
        let id = (w & 65535) as i32;
        if with_app(|a| invoke(a, id)).is_none() {
            HOST.with_borrow_mut(|s| s.pending.push_back(id))
        }
        return Some(0);
    }
    None
}
pub(crate) fn rows() -> Vec<(i32, String)> {
    HOST.with_borrow(|h| {
        h.plugins
            .iter()
            .flat_map(|p| {
                p.actions.iter().filter(|a| a.callback.is_some()).map(|a| {
                    (
                        a.id,
                        format!("{}   /   {}", p.name, a.label.replace('&', "")),
                    )
                })
            })
            .collect()
    })
}
pub(crate) fn attach_new(app: &mut App, path: &Path) -> Result<(), String> {
    panes::prepare(app)?;
    snapshot(app);
    dispatch(app, || attach(path))?;
    let notify = HOST.with_borrow(|s| s.plugins.last().unwrap().notify);
    dispatch(app, || {
        for (code, id) in [(1002, 0), (1001, 0), (1010, active_id())] {
            if code != 1010 || id != 0 {
                unsafe { notify(&mut notification(code, id)) }
            }
        }
    });
    Ok(())
}

pub fn file_event(app: &mut App, code: u32, id: u64) {
    if code == 1003 {
        paged::invalidate(id);
    }
    if !installed() {
        return;
    }
    snapshot(app);
    announce(code, id as usize);
    if code == 1007 {
        // A formatter can update the native document while handling BEFORESAVE.
        // Its queued SCN_MODIFIED may still be waiting in the normal event pump.
        if let Some(b) = app.buffers.iter_mut().find(|b| b.id == id && !b.large) {
            b.synced = b.rev.wrapping_sub(1)
        }
    }
}

pub(crate) fn switch_pane(app: &mut App, p: usize, large: bool) {
    panes::switch(app, p, large)
}

pub(crate) use paged::checkpoint as checkpoint_pages;
