//! The document preview's web view on macOS: WKWebView, where Windows has
//! WebView2. Everything that does not touch the web view itself (what a page is
//! sent, what its messages mean, which pictures it may load) is the same text
//! as in `preview_web.rs` and must be changed together with it.
use super::{Page, Signal};
use crate::{events, state::PreviewCues, win::*};
use linework_win32::web::{Hooks, WebView};
use std::{
    cell::RefCell,
    collections::VecDeque,
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
    rc::Rc,
};
use windows_sys::Win32::{Foundation::*, Graphics::Gdi::*, UI::WindowsAndMessaging::*};

const SHELL: &str = include_str!("../../assets/preview.html");
/// The page engine. It installs `window.__lw` once per loaded page; every
/// later update is one short call into it.
const ENGINE: &str = include_str!("../../assets/preview-render.js");
// WebKit serves a program's own pages under a scheme of the program's, where
// WebView2 lets it answer for any address.
const SCHEME: &str = "linework-preview";
const ORIGIN: &str = "linework-preview://doc/";
const SHELL_URL: &str = "linework-preview://doc/view";
/// Where the page finds the code font's files, upright and italic.
const FONTS: &str = "linework-preview://doc/-font/";
const FONT_FILES: [&str; 2] = ["CascadiaMono.ttf", "CascadiaMonoItalic.ttf"];

fn font_rules() -> String {
    FONT_FILES
        .iter()
        .zip(["normal", "italic"])
        .map(|(file, style)| format!("@font-face{{font-family:\"Cascadia Mono\";font-style:{style};font-weight:200 700;src:url(\"{FONTS}{file}\")}}"))
        .collect()
}
const RETRY: usize = 1;
/// As on Windows: the page runs the host's own script and may send messages back.
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
    web: Option<Rc<WebView>>,
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
        "ink": cues.ink, "echo": cues.echo, "ghosts": cues.ghosts, "peek": cues.peek,
        "mirror": cues.mirror, "follow": cues.follow, "tasks": cues.tasks,
    })
}
impl View {
    pub fn create(parent: H, light: bool) -> Option<Self> {
        register("LineworkDocumentPreview", Some(proc)).ok()?;
        let h = child(parent, 760, "LineworkDocumentPreview", "Document preview", WS_CLIPCHILDREN);
        if h == 0 {
            return None;
        }
        let state = Rc::new(RefCell::new(State {
            h,
            web: None,
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
            SetWindowLongPtrW(hwnd(h), GWLP_USERDATA, Box::into_raw(Box::new(state.clone())) as isize);
        }
        start(&state);
        Some(Self { h, state })
    }
    pub fn switch_document(&self, id: u64, path: &Option<PathBuf>) {
        let mut s = self.state.borrow_mut();
        s.document = id;
        s.root = path.as_deref().and_then(Path::parent).and_then(|p| p.canonicalize().ok());
        s.generation += 1;
        s.mount += 1;
        s.script = None;
        s.pages.clear();
        let web = if s.shown { s.web.clone() } else { None };
        s.shown = false;
        drop(s);
        if let Some(web) = web {
            web.set_visible(false);
        }
        invalidate(self.h);
    }
    pub fn show(&self, page: Page, cues: &PreviewCues, quiet: bool, traces: serde_json::Value) {
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
            "edit": edit, "cue": cue_json(cues), "live": INTERACTIVE, "marks": traces,
        });
        s.generation += 1;
        let theme_changed = s.light != key.light;
        s.light = key.light;
        s.retries = 0;
        s.script = Some(format!("window.__lw.render({payload})"));
        let _ = theme_changed;
        drop(s);
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
        let Some(web) = s.web.clone() else {
            return false;
        };
        drop(s);
        web.run(&format!("window.__lw&&window.__lw.{call}"), |_| {});
        true
    }
    pub fn page_offset(&self, id: u64, rev: u64, at: u64) -> Option<u64> {
        let s = self.state.borrow();
        s.pages
            .iter()
            .rev()
            .find(|p| p.id == id && p.rev == rev && at >= p.origin && at - p.origin <= p.len as u64)
            .map(|p| at - p.origin)
    }
    /// A selection and a caret as bytes from the start of the page on screen,
    /// if a page of this buffer revision is on screen. The selection is cut
    /// to the page. A caret outside the page is reported as -1.
    pub fn page_span(&self, id: u64, rev: u64, lo: u64, hi: u64, caret: u64) -> Option<(u64, u64, i64)> {
        let s = self.state.borrow();
        let p = s.pages.iter().rev().find(|p| p.id == id && p.rev == rev)?;
        let end = p.origin + p.len as u64;
        let (a, z) = (lo.clamp(p.origin, end) - p.origin, hi.clamp(p.origin, end) - p.origin);
        let c = if caret >= p.origin && caret <= end { (caret - p.origin) as i64 } else { -1 };
        Some((a, z, c))
    }
    /// Ask the page for the source position at the top of the reading view.
    /// The answer comes back as a jump, checked like a click.
    pub fn locate(&self) -> bool {
        let s = self.state.borrow();
        if !s.ready || !s.installed || s.h == 0 {
            return false;
        }
        let Some(web) = s.web.clone() else {
            return false;
        };
        drop(s);
        let weak = Rc::downgrade(&self.state);
        // The answer comes back as text, so the page writes it out as JSON itself.
        web.run("JSON.stringify(window.__lw?window.__lw.where():null)", move |answer| {
            if let (Some(json), Some(s)) = (answer, weak.upgrade()) {
                message(&s, &json);
            }
        });
        true
    }
}
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
                ink: v["ink"].as_bool().unwrap_or(true),
                echo: v["echo"].as_bool().unwrap_or(true),
                ghosts: v["ghosts"].as_bool().unwrap_or(true),
                peek: v["peek"].as_bool().unwrap_or(true),
                mirror: v["mirror"].as_bool().unwrap_or(true),
                follow: v["follow"].as_bool().unwrap_or(false),
                tasks: v["tasks"].as_bool().unwrap_or(true),
            }
            .checked(),
        ),
        Some("focus") => Signal::Focus,
        Some(kind @ ("task" | "peek")) => {
            // Like a jump: the page must be one the host sent, and the
            // position must lie inside the text it was rendered from.
            let (Some(view), Some(rev), Some(at)) =
                (v["view"].as_str(), v["rev"].as_u64(), v["at"].as_u64())
            else {
                return;
            };
            let s = shared.borrow();
            let Some(page) = s.pages.iter().find(|p| p.view == view && p.rev == rev) else {
                return;
            };
            let size = page.len as u64;
            if at >= size {
                return;
            }
            let (id, at) = (page.id, page.origin + at);
            if kind == "task" {
                Signal::Task { id, rev, at }
            } else {
                Signal::Peek {
                    id,
                    rev,
                    at,
                    len: v["len"].as_u64().unwrap_or(0).min(size - (at - page.origin)),
                    token: v["token"].as_u64().unwrap_or(0),
                }
            }
        }
        _ => return,
    };
    events::push(events::Event::Preview(signal));
}
/// Decide whether a key pressed while the preview has the keyboard is an
/// application shortcut. Reading keys stay with the page: copying, selecting,
/// moving, scrolling and everything its controls need.
fn start(shared: &Shared) {
    let h = shared.borrow().h;
    let (pages, said, arrived) = (Rc::downgrade(shared), Rc::downgrade(shared), Rc::downgrade(shared));
    let hooks = Hooks {
        resource: Box::new(move |address| {
            let s = pages.upgrade()?;
            if address == SHELL_URL {
                // The page may load pictures from the program's own address and nowhere else.
                let shell = SHELL.replace("https://linework-preview.invalid", "linework-preview:");
                // Text in the page's code font: the page's own process cannot see a
                // font this process has made known, so the page is given the files.
                let shell = shell.replace("style-src 'unsafe-inline';", "style-src 'unsafe-inline'; font-src linework-preview:;").replace("<style>", &format!("<style>{}", font_rules()));
                let shell = if s.borrow().light { shell.replace("<html lang=\"en\">", "<html lang=\"en\" class=\"light\">") } else { shell };
                return Some((shell.into_bytes(), "text/html; charset=utf-8"));
            }
            if let Some(name) = address.strip_prefix(FONTS) {
                let file = FONT_FILES.iter().find(|file| **file == name)?;
                let bytes = std::fs::read(linework_win32::carried_fonts()?.join(file)).ok()?;
                return Some((bytes, "font/ttf"));
            }
            let s = s.borrow();
            let root = s.root.as_ref()?;
            let path = address.strip_prefix(&format!("{ORIGIN}{}/", s.document))?;
            image_bytes(root, path)
        }),
        message: Box::new(move |json| {
            if let Some(s) = said.upgrade() {
                message(&s, json);
            }
        }),
        loaded: Box::new(move |ok| {
            let Some(s) = arrived.upgrade() else { return };
            {
                let mut state = s.borrow_mut();
                state.ready = ok;
                state.installed = false;
                if state.script.is_some() {
                    state.applied = state.generation.wrapping_sub(1);
                    state.retries = 0;
                }
            }
            flush(&s);
        }),
        // The page itself stays where it is; its frames hold only what the host wrote into them.
        allow: Box::new(|address, whole| if whole { address == SHELL_URL } else { address.starts_with("about:srcdoc") }),
    };
    match WebView::new(h, SCHEME, hooks) {
        Some(web) => {
            let web = Rc::new(web);
            shared.borrow_mut().web = Some(web.clone());
            web.load(SHELL_URL);
        }
        None => {
            shared.borrow_mut().status = "Preview could not start.".into();
            invalidate(h);
        }
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
    let Some(web) = s.web.clone() else {
        return;
    };
    let Some(render) = s.script.as_deref() else {
        return;
    };
    let script = if s.installed { render.to_owned() } else { format!("{ENGINE}\n{render}") };
    let generation = s.generation;
    s.busy = true;
    drop(s);
    let weak = Rc::downgrade(shared);
    web.run(&script, move |answer| {
        let Some(s) = weak.upgrade() else {
            return;
        };
        let mut state = s.borrow_mut();
        state.busy = false;
        if state.h == 0 {
            return;
        }
        let drawn = answer.as_deref() == Some("true");
        if drawn {
            state.installed = true;
        }
        if generation != state.generation {
            drop(state);
            flush(&s);
            return;
        }
        if drawn {
            state.applied = generation;
            let web = if state.shown { None } else { state.web.clone() };
            drop(state);
            if let Some(web) = web {
                web.set_visible(true);
                s.borrow_mut().shown = true;
            }
            // For looking at the program without the screen (see linework-win32's pictures).
            if let (Some(folder), Some(web)) = (std::env::var_os("LINEWORK_PICTURES"), s.borrow().web.clone()) {
                web.picture(PathBuf::from(folder).join("preview.png"));
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
    });
}
unsafe extern "system" fn proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    unsafe {
        let p = GetWindowLongPtrW(h, GWLP_USERDATA) as *const Shared;
        if p.is_null() {
            return DefWindowProcW(h, m, w, l);
        }
        let shared = (*p).clone();
        match m {
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
                let web = s.web.take();
                drop(s);
                if let Some(web) = web {
                    web.close();
                }
                DefWindowProcW(h, m, w, l)
            }
            _ => DefWindowProcW(h, m, w, l),
        }
    }
}
