use super::{Page, Signal};
use crate::{events, state::{self, PreviewCues}, win::*};
use std::{
    cell::RefCell,
    collections::VecDeque,
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
    rc::Rc,
};
use webview2_com::{Microsoft::Web::WebView2::Win32::*, *};
use windows::{
    Win32::{
        Foundation::{E_POINTER, HWND as ComHwnd, RECT as ComRect},
        UI::Shell::SHCreateMemStream,
    },
    core::{Interface, PCWSTR, PWSTR, Result as WinResult, w},
};
use windows_sys::Win32::{Foundation::*, Graphics::Gdi::*, UI::WindowsAndMessaging::*};

const SHELL: &str = include_str!("../assets/preview.html");
/// The page engine. It installs `window.__lw` once per loaded page; every
/// later update is one short call into it.
const ENGINE: &str = include_str!("../assets/preview-render.js");
const ORIGIN: &str = "https://linework-preview.invalid/";
const SHELL_URL: &str = "https://linework-preview.invalid/view";
const RETRY: usize = 1;
/// Whether the preview page may run the host's own script with event
/// listeners and send messages back. Click to source, the Show edit button,
/// the cue controls in the preview header and Escape-to-source need this.
///
/// Text from the document never runs as script in either mode. It is rebuilt
/// element by element from an allowlist into an iframe that is sandboxed
/// without `allow-scripts`, under a content policy of `default-src 'none'`.
/// With `false` the whole WebView is script-disabled, as it was before these
/// features existed. Rendering, edit cues, reading position and the native
/// preview commands keep working, because the host evaluates those directly.
const INTERACTIVE: bool = true;
type Shared = Rc<RefCell<State>>;
/// A page that was sent with source positions. A click names the page by
/// `view` and `rev`; the host supplies where that page starts in the file.
struct Shown {
    view: String,
    id: u64,
    rev: u64,
    origin: u64,
    len: usize,
}
struct State {
    h: H,
    controller: Option<ICoreWebView2Controller>,
    ready: bool,
    /// The loaded page has its engine. False again after every navigation.
    installed: bool,
    shown: bool,
    mount: u64,
    busy: bool,
    generation: u64,
    applied: u64,
    script: Option<String>,
    retries: u8,
    status: String,
    root: Option<PathBuf>,
    document: u64,
    light: bool,
    /// The newest two mapped pages: the one on screen and the one replacing it.
    pages: VecDeque<Shown>,
}
pub(super) struct View {
    pub h: H,
    state: Shared,
}
pub(super) fn cue_json(cues: &PreviewCues) -> serde_json::Value {
    let cues = cues.checked();
    serde_json::json!({
        "style": cues.style, "letters": cues.letters, "margin": cues.margin,
        "dim": cues.dim, "calm": cues.calm, "click": cues.click_source,
    })
}
impl View {
    pub fn create(parent: H, light: bool) -> Option<Self> {
        register("LineworkDocumentPreview", Some(proc)).ok()?;
        let h = child(
            parent,
            760,
            "LineworkDocumentPreview",
            "Document preview",
            WS_CLIPCHILDREN,
        );
        if h == 0 {
            return None;
        }
        let state = Rc::new(RefCell::new(State {
            h,
            controller: None,
            ready: false,
            installed: false,
            shown: false,
            mount: 0,
            busy: false,
            generation: 0,
            applied: 0,
            script: None,
            retries: 0,
            status: "Opening preview…".into(),
            root: None,
            document: 0,
            light,
            pages: VecDeque::new(),
        }));
        unsafe {
            SetWindowLongPtrW(
                hwnd(h),
                GWLP_USERDATA,
                Box::into_raw(Box::new(state.clone())) as isize,
            );
        }
        if let Err(e) = start(&state) {
            fail(&state, e);
        }
        Some(Self { h, state })
    }
    pub fn switch_document(&self, id: u64, path: &Option<PathBuf>) {
        let mut s = self.state.borrow_mut();
        s.document = id;
        s.root = path
            .as_deref()
            .and_then(Path::parent)
            .and_then(|p| p.canonicalize().ok());
        s.generation += 1;
        s.mount += 1;
        s.script = None;
        s.pages.clear();
        let controller = if s.shown { s.controller.clone() } else { None };
        s.shown = false;
        drop(s);
        if let Some(c) = controller {
            unsafe {
                let _ = c.SetIsVisible(false);
            }
        }
        invalidate(self.h);
    }
    /// Draw a rendered page. `quiet` keeps the page's edit for "Show edit"
    /// without playing its cue, for an edit the source caret has already left.
    pub fn show(&self, page: Page, cues: &PreviewCues, quiet: bool) {
        let key = page.key;
        let format = key.kind.label();
        let mut s = self.state.borrow_mut();
        let view = format!("{}:{}:{}:{}", key.id, format, key.page, s.mount);
        if let Some(origin) = page.origin {
            s.pages.retain(|p| p.view != view || p.rev != key.rev);
            s.pages.push_back(Shown {
                view: view.clone(),
                id: key.id,
                rev: key.rev,
                origin,
                len: page.len,
            });
            while s.pages.len() > 2 {
                s.pages.pop_front();
            }
        }
        let edit = page.edit.map(|e| {
            serde_json::json!({
                "at": e.at, "del": e.del, "add": e.add, "kind": e.kind.label(),
                "serial": e.serial, "tail": e.tail, "quiet": quiet,
                // The revision this edit changed. If the page still shows it,
                // a deletion can be judged against the text the reader saw.
                "prev": key.rev.saturating_sub(1),
            })
        });
        let payload = serde_json::json!({
            "view": view, "light": key.light, "kind": format,
            "content": page.content, "base": format!("{ORIGIN}{}/",key.id), "notice": page.notice,
            "rev": key.rev, "mapped": page.origin.is_some(), "nonce": page.nonce,
            "edit": edit, "cue": cue_json(cues), "live": INTERACTIVE,
        });
        s.generation += 1;
        let theme_changed = s.light != key.light;
        s.light = key.light;
        s.retries = 0;
        s.script = Some(format!("window.__lw.render({payload})"));
        let controller = if theme_changed { s.controller.clone() } else { None };
        drop(s);
        if let Some(c) = controller {
            background(&c, key.light);
        }
        if self.state.borrow().status == "Opening preview…" {
            set_text(self.h, &format!("{format} document preview"));
        }
        flush(&self.state);
    }
    /// Call one engine function, for example `rest()`. A call made before the
    /// page is ready is dropped: the next render carries the current state.
    pub fn run(&self, call: &str) -> bool {
        let s = self.state.borrow();
        if !s.ready || !s.installed || s.h == 0 {
            return false;
        }
        let Some(c) = s.controller.clone() else {
            return false;
        };
        drop(s);
        let script = wide(&format!("window.__lw&&window.__lw.{call}"));
        let done = ExecuteScriptCompletedHandler::create(Box::new(|_, _| Ok(())));
        unsafe {
            c.CoreWebView2()
                .and_then(|v| v.ExecuteScript(PCWSTR(script.as_ptr()), &done))
                .is_ok()
        }
    }
    /// Bytes from the start of the page on screen to a file position, if a
    /// page of this buffer revision is on screen and holds the position.
    pub fn page_offset(&self, id: u64, rev: u64, at: u64) -> Option<u64> {
        let s = self.state.borrow();
        s.pages
            .iter()
            .rev()
            .find(|p| p.id == id && p.rev == rev && at >= p.origin && at - p.origin <= p.len as u64)
            .map(|p| at - p.origin)
    }
    /// Ask the page for the source position at the top of the reading view.
    /// The answer comes back as a jump, checked like a click.
    pub fn locate(&self) -> bool {
        let s = self.state.borrow();
        if !s.ready || !s.installed || s.h == 0 {
            return false;
        }
        let Some(c) = s.controller.clone() else {
            return false;
        };
        drop(s);
        let weak = Rc::downgrade(&self.state);
        let script = wide("window.__lw?window.__lw.where():null");
        let done = ExecuteScriptCompletedHandler::create(Box::new(move |result, value| {
            if let (Ok(()), Some(s)) = (result, weak.upgrade()) {
                message(&s, &value);
            }
            Ok(())
        }));
        unsafe {
            c.CoreWebView2()
                .and_then(|v| v.ExecuteScript(PCWSTR(script.as_ptr()), &done))
                .is_ok()
        }
    }
}
/// Turn a message from the page into a signal for the application. Nothing
/// here is trusted: the page it names must be one the host sent, and the
/// position must lie inside the text that page was rendered from.
fn message(shared: &Shared, json: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return;
    };
    let signal = match v["t"].as_str() {
        Some("jump") => {
            let (Some(view), Some(rev), Some(at)) =
                (v["view"].as_str(), v["rev"].as_u64(), v["at"].as_u64())
            else {
                return;
            };
            let len = v["len"].as_u64().unwrap_or(0);
            let s = shared.borrow();
            let Some(page) = s.pages.iter().find(|p| p.view == view && p.rev == rev) else {
                return;
            };
            let size = page.len as u64;
            if at > size || len > size - at {
                return;
            }
            Signal::Jump {
                id: page.id,
                rev,
                at: page.origin + at,
                len,
            }
        }
        Some("cue") => Signal::Cues(
            PreviewCues {
                style: v["style"].as_str().unwrap_or("pill").to_owned(),
                letters: v["letters"].as_bool().unwrap_or(false),
                margin: v["margin"].as_bool().unwrap_or(false),
                dim: v["dim"].as_u64().unwrap_or(23).min(PreviewCues::MAX_DIM as u64) as u8,
                calm: v["calm"].as_bool().unwrap_or(false),
                click_source: v["click"].as_bool().unwrap_or(true),
            }
            .checked(),
        ),
        Some("focus") => Signal::Focus,
        _ => return,
    };
    events::push(events::Event::Preview(signal));
}
/// Decide whether a key pressed while the preview has the keyboard is an
/// application shortcut. Reading keys stay with the page: copying, selecting,
/// moving, scrolling and everything its controls need.
fn chord(vk: u32) -> Option<Signal> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    // The page lives in another process, so this thread's own key state is
    // not kept current while it has the keyboard. Ask for the physical state.
    let held = |key: VIRTUAL_KEY| {
        let state = unsafe { GetAsyncKeyState(key as i32) };
        state < 0
    };
    let (ctrl, shift, alt) = (held(VK_CONTROL), held(VK_SHIFT), held(VK_MENU));
    let function = (VK_F1 as u32..=VK_F24 as u32).contains(&vk);
    if !(ctrl || alt || function) {
        return None;
    }
    let reading = matches!(
        vk as u16,
        VK_CONTROL | VK_SHIFT | VK_MENU | VK_LWIN | VK_RWIN | VK_LEFT | VK_RIGHT | VK_UP
            | VK_DOWN | VK_HOME | VK_END | VK_PRIOR | VK_NEXT | VK_INSERT | VK_DELETE
            | VK_SPACE | VK_RETURN | VK_ESCAPE | VK_BACK
    ) || (ctrl && !alt && matches!(vk, 0x41 | 0x43 | 0x56 | 0x58 | 0x59 | 0x5a));
    (!reading).then_some(Signal::Chord {
        vk,
        ctrl,
        shift,
        alt,
    })
}
fn fail(shared: &Shared, err: windows::core::Error) {
    let mut s = shared.borrow_mut();
    s.status = format!(
        "Preview could not start. Check the Microsoft Edge WebView2 Runtime. ({:#x})",
        err.code().0
    );
    if s.h != 0 {
        let (h, message) = (s.h, s.status.clone());
        drop(s);
        set_text(h, &message);
        invalidate(h);
    }
}
fn start(shared: &Shared) -> WinResult<()> {
    let weak = Rc::downgrade(shared);
    let profile = state::profile().join("preview-webview");
    let profile = wide(&profile.to_string_lossy());
    unsafe {
        CreateCoreWebView2EnvironmentWithOptions(
            PCWSTR::null(),
            PCWSTR(profile.as_ptr()),
            None,
            &CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |result, env| {
                let Some(s) = weak.upgrade() else {
                    return Ok(());
                };
                if s.borrow().h == 0 {
                    return Ok(());
                }
                let run = || -> WinResult<()> {
                    result?;
                    let env = env.ok_or(E_POINTER)?;
                    let weak = Rc::downgrade(&s);
                    let resources = env.clone();
                    let parent = ComHwnd(hwnd(s.borrow().h));
                    env.CreateCoreWebView2Controller(
                        parent,
                        &CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
                            move |result, controller| {
                                let Some(s) = weak.upgrade() else {
                                    return Ok(());
                                };
                                if s.borrow().h == 0 {
                                    return Ok(());
                                }
                                if let Err(e) = result.and_then(|_| {
                                    configure(&s, &resources, controller.ok_or(E_POINTER)?)
                                }) {
                                    fail(&s, e);
                                }
                                Ok(())
                            },
                        )),
                    )
                };
                if let Err(e) = run() {
                    fail(&s, e);
                }
                Ok(())
            })),
        )
    }
}
fn background(c: &ICoreWebView2Controller, light: bool) {
    let bg = colors(light).bg;
    if let Ok(c) = c.cast::<ICoreWebView2Controller2>() {
        unsafe {
            let _ = c.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR {
                A: 255,
                R: bg as u8,
                G: (bg >> 8) as u8,
                B: (bg >> 16) as u8,
            });
        }
    }
}
fn bounds(c: &ICoreWebView2Controller, h: H) {
    let (w, ht) = client(h);
    if w > 0 && ht > 0 {
        unsafe {
            let _ = c.SetBounds(ComRect {
                left: 0,
                top: 0,
                right: w,
                bottom: ht,
            });
        }
    }
}
fn configure(
    s: &Shared,
    env: &ICoreWebView2Environment,
    c: ICoreWebView2Controller,
) -> WinResult<()> {
    unsafe {
        c.SetIsVisible(false)?;
        background(&c, s.borrow().light);
        bounds(&c, s.borrow().h);
        let view = c.CoreWebView2()?;
        let settings = view.Settings()?;
        settings.SetIsScriptEnabled(INTERACTIVE)?;
        settings.SetAreHostObjectsAllowed(false)?;
        settings.SetIsWebMessageEnabled(INTERACTIVE)?;
        settings.SetAreDefaultScriptDialogsEnabled(false)?;
        settings.SetAreDevToolsEnabled(false)?;
        settings.SetAreDefaultContextMenusEnabled(false)?;
        settings.SetIsStatusBarEnabled(false)?;
        // F5, Ctrl+R, Ctrl+P and the other browser shortcuts have no meaning
        // in this pane, and a reload used to leave it empty.
        if let Ok(extra) = settings.cast::<ICoreWebView2Settings3>() {
            let _ = extra.SetAreBrowserAcceleratorKeysEnabled(false);
        }
        let mut token = 0;
        view.add_NewWindowRequested(
            &NewWindowRequestedEventHandler::create(Box::new(|_, a| {
                if let Some(a) = a {
                    a.SetHandled(true)?;
                }
                Ok(())
            })),
            &mut token,
        )?;
        view.add_PermissionRequested(
            &PermissionRequestedEventHandler::create(Box::new(|_, a| {
                if let Some(a) = a {
                    a.SetState(COREWEBVIEW2_PERMISSION_STATE_DENY)?;
                }
                Ok(())
            })),
            &mut token,
        )?;
        view.add_NavigationStarting(
            &NavigationStartingEventHandler::create(Box::new(|_, a| {
                if let Some(a) = a {
                    let mut ptr = PWSTR::null();
                    a.Uri(&mut ptr)?;
                    if take_pwstr(ptr) != SHELL_URL {
                        a.SetCancel(true)?;
                    }
                }
                Ok(())
            })),
            &mut token,
        )?;
        // The reading frame holds one `about:srcdoc` document for its whole
        // life. A link inside it must never load anything else into it, or
        // the next render finds no page to draw on.
        view.add_FrameNavigationStarting(
            &NavigationStartingEventHandler::create(Box::new(|_, a| {
                if let Some(a) = a {
                    let mut ptr = PWSTR::null();
                    a.Uri(&mut ptr)?;
                    if !take_pwstr(ptr).starts_with("about:srcdoc") {
                        a.SetCancel(true)?;
                    }
                }
                Ok(())
            })),
            &mut token,
        )?;
        if INTERACTIVE {
            let weak = Rc::downgrade(s);
            view.add_WebMessageReceived(
                &WebMessageReceivedEventHandler::create(Box::new(move |_, a| {
                    let (Some(s), Some(a)) = (weak.upgrade(), a) else {
                        return Ok(());
                    };
                    // Only the shell page itself may speak to the host.
                    let mut source = PWSTR::null();
                    a.Source(&mut source)?;
                    if take_pwstr(source) != SHELL_URL {
                        return Ok(());
                    }
                    let mut json = PWSTR::null();
                    a.WebMessageAsJson(&mut json)?;
                    message(&s, &take_pwstr(json));
                    Ok(())
                })),
                &mut token,
            )?;
        }
        // Application shortcuts keep working while the preview has the keyboard.
        c.add_AcceleratorKeyPressed(
            &AcceleratorKeyPressedEventHandler::create(Box::new(|_, a| {
                let Some(a) = a else {
                    return Ok(());
                };
                let mut kind = COREWEBVIEW2_KEY_EVENT_KIND_KEY_UP;
                a.KeyEventKind(&mut kind)?;
                if kind.0 != COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN.0
                    && kind.0 != COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN.0
                {
                    return Ok(());
                }
                let mut vk = 0u32;
                a.VirtualKey(&mut vk)?;
                if let Some(signal) = chord(vk) {
                    // Alt combinations also belong to Windows (Alt+F4), so
                    // those are passed on as well as reported.
                    if let Signal::Chord { alt: false, .. } = signal {
                        a.SetHandled(true)?;
                    }
                    events::push(events::Event::Preview(signal));
                }
                Ok(())
            })),
            &mut token,
        )?;
        let weak = Rc::downgrade(s);
        let env = env.clone();
        view.AddWebResourceRequestedFilter(w!("*"), COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL)?;
        view.add_WebResourceRequested(&WebResourceRequestedEventHandler::create(Box::new(move |_,a| {
            let Some(a)=a else {return Ok(());};
            let request=a.Request()?;
            let mut ptr=PWSTR::null();request.Uri(&mut ptr)?;
            let uri=take_pwstr(ptr);
            let resource=weak.upgrade().and_then(|s|{
                if uri==SHELL_URL {
                    let shell=if s.borrow().light {SHELL.replace("<html lang=\"en\">","<html lang=\"en\" class=\"light\">")} else {SHELL.to_owned()};
                    return Some((shell.into_bytes(),"text/html; charset=utf-8"));
                }
                let s=s.borrow();let root=s.root.as_ref()?;
                let path=uri.strip_prefix(&format!("{ORIGIN}{}/",s.document))?;
                image_bytes(root,path)
            });
            let (body,code,mime)=if let Some((data,mime))=resource {
                (SHCreateMemStream(Some(&data)),200,mime)
            } else {(None,403,"text/plain")};
            let headers=wide(&format!("Content-Type: {mime}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff"));
            a.SetResponse(&env.CreateWebResourceResponse(body.as_ref(),code,
                if code==200 {w!("OK")} else {w!("Blocked")}, PCWSTR(headers.as_ptr()))?)
        })),&mut token)?;
        let weak = Rc::downgrade(s);
        view.add_NavigationCompleted(
            &NavigationCompletedEventHandler::create(Box::new(move |_, a| {
                let Some(s) = weak.upgrade() else {
                    return Ok(());
                };
                let mut ok = windows::core::BOOL(0);
                if let Some(a) = a {
                    a.IsSuccess(&mut ok)?;
                }
                {
                    // A freshly loaded page has no engine and no content, even
                    // if the newest render was applied to the page before it.
                    let mut state = s.borrow_mut();
                    state.ready = ok.as_bool();
                    state.installed = false;
                    if state.script.is_some() {
                        state.applied = state.generation.wrapping_sub(1);
                        state.retries = 0;
                    }
                }
                flush(&s);
                Ok(())
            })),
            &mut token,
        )?;
        s.borrow_mut().controller = Some(c);
        view.Navigate(PCWSTR(wide(SHELL_URL).as_ptr()))
    }
}
fn image_bytes(root: &Path, relative: &str) -> Option<(Vec<u8>, &'static str)> {
    let relative = relative.split(['?', '#']).next()?;
    let name = percent_encoding::percent_decode_str(relative)
        .decode_utf8()
        .ok()?;
    if name.contains([':', '\\', '\0']) || name.starts_with('/') {
        return None;
    }
    let rel = Path::new(name.as_ref());
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    let path = root.join(rel).canonicalize().ok()?;
    if !path.starts_with(root) {
        return None;
    }
    let mime = match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        _ => return None,
    };
    const MAX: u64 = 8 * 1024 * 1024;
    let file = File::open(&path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX {
        return None;
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(MAX + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= MAX).then_some((bytes, mime))
}
fn flush(shared: &Shared) {
    let mut s = shared.borrow_mut();
    if !s.ready || s.busy || s.h == 0 || s.applied == s.generation {
        return;
    }
    let Some(c) = s.controller.clone() else {
        return;
    };
    let Some(render) = s.script.as_deref() else {
        return;
    };
    // The first update after a page load brings the engine with it, in one
    // script, so a render can never run ahead of its own engine.
    let script = if s.installed {
        wide(render)
    } else {
        wide(&format!("{ENGINE}\n{render}"))
    };
    let generation = s.generation;
    s.busy = true;
    drop(s);
    let weak = Rc::downgrade(shared);
    let complete = ExecuteScriptCompletedHandler::create(Box::new(move |result, value| {
        let Some(s) = weak.upgrade() else {
            return Ok(());
        };
        let mut state = s.borrow_mut();
        state.busy = false;
        if state.h == 0 {
            return Ok(());
        }
        let drawn = result.is_ok() && value == "true";
        if drawn {
            state.installed = true;
        }
        if generation != state.generation {
            drop(state);
            flush(&s);
            return Ok(());
        }
        if drawn {
            state.applied = generation;
            let c = if state.shown { None } else { state.controller.clone() };
            drop(state);
            if let Some(c) = c {
                unsafe {
                    if c.SetIsVisible(true).is_ok() {
                        s.borrow_mut().shown = true;
                    }
                }
            }
        } else if state.retries < 3 {
            state.retries += 1;
            unsafe {
                SetTimer(hwnd(state.h), RETRY, 60, None);
            }
        } else {
            state.status = "The document preview could not be drawn.".into();
            invalidate(state.h);
        }
        Ok(())
    }));
    if let Err(err) = unsafe {
        c.CoreWebView2()
            .and_then(|v| v.ExecuteScript(PCWSTR(script.as_ptr()), &complete))
    } {
        shared.borrow_mut().busy = false;
        fail(shared, err);
    }
}
unsafe extern "system" fn proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    unsafe {
        let p = GetWindowLongPtrW(h, GWLP_USERDATA) as *const Shared;
        if p.is_null() {
            return DefWindowProcW(h, m, w, l);
        }
        let shared = (*p).clone();
        match m {
            WM_SIZE => {
                let c = shared.borrow().controller.clone();
                if let Some(c) = c {
                    bounds(&c, h as H);
                }
                0
            }
            WM_TIMER if w == RETRY => {
                KillTimer(h, RETRY);
                flush(&shared);
                0
            }
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                let mut ps = std::mem::zeroed();
                let dc = BeginPaint(h, &mut ps);
                let s = shared.borrow();
                let colors = colors(s.light);
                let (w, ht) = client(h as H);
                fill(dc, ps.rcPaint, colors.bg);
                let pad = size(h as H, 16);
                label(
                    dc,
                    rect(pad, pad, (w - 2 * pad).max(0), ht.min(size(h as H, 88))),
                    &s.status,
                    colors.dim,
                    size(h as H, 13),
                    400,
                    false,
                    DT_WORDBREAK,
                );
                EndPaint(h, &ps);
                0
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(h, GWLP_USERDATA, 0);
                drop(Box::from_raw(p as *mut Shared));
                let mut s = shared.borrow_mut();
                s.h = 0;
                let c = s.controller.take();
                drop(s);
                if let Some(c) = c {
                    let _ = c.Close();
                }
                DefWindowProcW(h, m, w, l)
            }
            _ => DefWindowProcW(h, m, w, l),
        }
    }
}
