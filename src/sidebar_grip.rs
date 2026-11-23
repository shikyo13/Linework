//! A captured sidebar grip has its own input queue. An editor presenting a
//! DirectWrite frame may block the main thread for a refresh. The grip can
//! still commit the actual pointer position and post the newest editor layout.
use crate::{events, frame, win::*};
use std::{
    cell::{Cell, RefCell},
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, AtomicIsize, Ordering},
    },
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

#[path = "sidebar_scene.rs"]
mod scene;
#[path = "sidebar_visual.rs"]
mod visual;

const READY: u32 = WM_APP + 171;
const MOVE: u32 = WM_APP + 172;
const END: u32 = WM_APP + 173;
const ACK: u32 = WM_APP + 174;
const LOST: u32 = WM_APP + 175;
const SYNC: u32 = WM_APP + 176;
const BEGIN: u32 = WM_APP + 177;
const RESTORE: u32 = WM_APP + 178;
const PANE: u32 = WM_APP + 179;
const ART: u32 = WM_APP + 180;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Layout {
    visible: bool,
    side: i32,
    width: i32,
    height: i32,
    dpi: i32,
    light: bool,
}
impl Layout {
    fn px(self, logical: i32) -> i32 {
        (logical * self.dpi + 48) / 96
    }
    fn from_pointer(self, x: i32) -> i32 {
        let max = (self.width * 96 / self.dpi - 220).clamp(150, 900);
        (x * 96 / self.dpi).clamp(150, max) * self.dpi / 96
    }
}

struct Shared {
    host: AtomicIsize,
    grip: AtomicIsize,
    layout: Mutex<Option<Layout>>,
    dragging: AtomicBool,
    pending_move: AtomicBool,
    pending_art: AtomicBool,
    latest_x: AtomicI32,
    content: Mutex<Option<scene::Content>>,
    images: Mutex<Vec<(H, scene::Pixels)>>,
}
impl Shared {
    fn notify(&self, id: u32, w: usize, l: isize) -> bool {
        let host = self.host.load(Ordering::Acquire);
        host != 0 && unsafe { PostMessageW(hwnd(host), id, w, l) != 0 }
    }
}
thread_local! {
    static LINK: RefCell<Option<Arc<Shared>>> = const { RefCell::new(None) };
    static OWNS_EDGE: Cell<bool> = const { Cell::new(false) };
}
pub(crate) fn owns_edge() -> bool {
    OWNS_EDGE.get()
}
pub(crate) fn captured() -> bool {
    LINK.with_borrow(|link| {
        link.as_ref()
            .is_some_and(|s| s.dragging.load(Ordering::Acquire))
    })
}
pub(crate) fn start(host: H) {
    let shared = Arc::new(Shared {
        host: AtomicIsize::new(host),
        grip: AtomicIsize::new(0),
        layout: Mutex::new(None),
        dragging: AtomicBool::new(false),
        pending_move: AtomicBool::new(false),
        pending_art: AtomicBool::new(false),
        latest_x: AtomicI32::new(0),
        content: Mutex::new(None),
        images: Mutex::new(Vec::new()),
    });
    let worker = Arc::clone(&shared);
    if std::thread::Builder::new()
        .name("Sidebar input".into())
        .spawn(move || run(host, worker))
        .is_ok()
    {
        LINK.replace(Some(shared));
    }
}
pub(crate) fn layout(host: H, visible: bool, side: i32, light: bool) {
    LINK.with_borrow(|link| {
        let Some(shared) = link else {
            return;
        };
        let (width, height) = client(host);
        let cfg = Layout {
            visible,
            side,
            width,
            height,
            dpi: dpi(host),
            light,
        };
        let mut previous = shared.layout.lock().unwrap();
        if *previous == Some(cfg) {
            return;
        }
        *previous = Some(cfg);
        drop(previous);
        let grip = shared.grip.load(Ordering::Acquire);
        if grip != 0 {
            unsafe {
                PostMessageW(hwnd(grip), SYNC, 0, 0);
            }
        }
    });
}
pub(crate) fn host_message(host: H, id: u32, w: usize, l: isize) -> bool {
    if !matches!(id, READY | MOVE | END | ACK | LOST | BEGIN | RESTORE) {
        return false;
    }
    LINK.with_borrow(|link| {
        let Some(shared) = link else {
            return;
        };
        match id {
            READY | LOST => {
                if id == LOST {
                    scene::restore_sources();
                }
                OWNS_EDGE.set(id == READY);
                frame::rendering::reset();
                invalidate(host);
            }
            BEGIN => {
                shared.images.lock().unwrap().clear();
                let content = scene::capture(host, l as i32);
                *shared.content.lock().unwrap() = Some(content);
            }
            RESTORE => {
                if !shared.dragging.load(Ordering::Acquire) {
                    scene::restore_sources();
                }
            }
            MOVE => {
                shared.pending_move.store(false, Ordering::Release);
                events::push(events::Event::SideWidth(
                    shared.latest_x.load(Ordering::Acquire),
                    false,
                ));
            }
            END => {
                events::push(events::Event::SideWidth(l as i32, true));
                for ctrl in [700, 701] {
                    unsafe {
                        let ed = GetDlgItem(hwnd(host), ctrl);
                        if !ed.is_null() {
                            SendMessageW(ed, crate::editor_resize::FLUSH, 0, 0);
                        }
                    }
                }
                // App::dispatch_pending applies the final geometry before ACK.
                unsafe {
                    PostMessageW(hwnd(host), ACK, w, 0);
                }
            }
            ACK => {
                if !shared.dragging.load(Ordering::Acquire) {
                    scene::restore_sources();
                }
                unsafe {
                    RedrawWindow(
                        hwnd(host),
                        null(),
                        null_mut(),
                        RDW_UPDATENOW | RDW_ALLCHILDREN | RDW_NOERASE,
                    );
                }
                let grip = shared.grip.load(Ordering::Acquire);
                if grip != 0 {
                    unsafe {
                        PostMessageW(hwnd(grip), ACK, w, 0);
                    }
                }
            }
            _ => {}
        }
    });
    true
}
pub(crate) fn shutdown() {
    OWNS_EDGE.set(false);
    if let Some(shared) = LINK.take() {
        scene::restore_sources();
        shared.host.store(0, Ordering::Release);
        let grip = shared.grip.load(Ordering::Acquire);
        if grip != 0 {
            unsafe {
                PostMessageW(hwnd(grip), WM_CLOSE, 0, 0);
            }
        }
    }
}
pub(crate) fn editor_changed(h: H) {
    scene::document_changed(h);
}
pub(crate) fn artwork_changed() {
    LINK.with_borrow(|link| {
        if let Some(s) = link {
            if s.dragging.load(Ordering::Acquire) && !s.pending_art.swap(true, Ordering::AcqRel) {
                let grip = s.grip.load(Ordering::Acquire);
                if grip == 0 || unsafe { PostMessageW(hwnd(grip), ART, 0, 0) } == 0 {
                    s.pending_art.store(false, Ordering::Release);
                }
            }
        }
    });
}
pub(crate) fn editor_painted(h: H) {
    if !captured() {
        return;
    }
    let Some(image) = scene::updated(h) else {
        return;
    };
    LINK.with_borrow(|link| {
        if let Some(shared) = link {
            let mut pending = shared.images.lock().unwrap();
            let post = pending.is_empty();
            pending.retain(|(old, _)| *old != h);
            pending.push((h, image));
            drop(pending);
            if post {
                let grip = shared.grip.load(Ordering::Acquire);
                if grip != 0 {
                    unsafe {
                        PostMessageW(hwnd(grip), PANE, 0, 0);
                    }
                }
            }
        }
    });
}

struct Grip {
    shared: Arc<Shared>,
    edge: RefCell<visual::Edge>,
    config: Cell<Option<Layout>>,
    ready: Cell<bool>,
    epoch: Cell<usize>,
    awaiting: Cell<bool>,
    // While captured, the input HWND stays put. Every queued client coordinate
    // therefore has the same origin, including when the pointer reverses.
    origin: Cell<i32>,
    last_raw: Cell<i32>,
}
impl Grip {
    fn prepare_scene(&self) {
        if !self.shared.dragging.load(Ordering::Acquire) {
            return;
        }
        let Some(cfg) = self.config.get() else {
            return;
        };
        let host = self.shared.host.load(Ordering::Acquire);
        let mut result = 0;
        if host != 0
            && unsafe {
                SendMessageTimeoutW(
                    hwnd(host),
                    BEGIN,
                    0,
                    cfg.side as _,
                    SMTO_ABORTIFHUNG,
                    250,
                    &mut result,
                )
            } != 0
        {
            if let Some(content) = self.shared.content.lock().unwrap().take() {
                let _ = self.edge.borrow_mut().begin_drag(
                    cfg,
                    content,
                    cfg.from_pointer(self.last_raw.get()),
                );
            }
        }
    }
    fn paint_edge(&self, side: i32, cfg: Layout) -> bool {
        let top = cfg.px(frame::HEAD);
        let height = (cfg.height - cfg.px(frame::FOOT) - top).max(1);
        let thickness = cfg.px(1).max(1);
        self.edge
            .borrow_mut()
            .place(
                side - thickness,
                top,
                thickness,
                height,
                colors(cfg.light).line,
            )
            .is_ok()
    }
    fn sync(&self, h: HWND) -> bool {
        let Some(cfg) = *self.shared.layout.lock().unwrap() else {
            return true;
        };
        let old = self.config.replace(Some(cfg));
        if self.shared.dragging.load(Ordering::Acquire)
            && old.is_some_and(|v| {
                (v.width, v.height, v.dpi, v.light) != (cfg.width, cfg.height, cfg.dpi, cfg.light)
            })
        {
            unsafe {
                ReleaseCapture();
            }
        }
        if !cfg.visible || cfg.height <= cfg.px(frame::HEAD + frame::FOOT) {
            if self.shared.dragging.load(Ordering::Acquire) {
                unsafe {
                    ReleaseCapture();
                }
            }
            unsafe {
                ShowWindow(h, SW_HIDE);
            }
            return self.edge.borrow_mut().hide().is_ok();
        }
        if self.shared.dragging.load(Ordering::Acquire) || self.awaiting.get() {
            return self.paint_edge(cfg.from_pointer(self.last_raw.get()), cfg);
        }
        let pad = cfg.px(5).max(1);
        let x = cfg.side - pad;
        self.origin.set(x);
        unsafe {
            SetWindowPos(
                h,
                HWND_TOP,
                x,
                cfg.px(frame::HEAD),
                2 * pad + 1,
                (cfg.height - cfg.px(frame::HEAD + frame::FOOT)).max(1),
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
        if !self.paint_edge(cfg.side, cfg) {
            return false;
        }
        if !self.ready.replace(true) {
            self.shared.notify(READY, 0, 0);
        }
        true
    }
    fn movement(&self, raw: i32) -> bool {
        let Some(cfg) = self.config.get() else {
            return false;
        };
        self.last_raw.set(raw);
        self.shared.latest_x.store(raw, Ordering::Release);
        if !self.shared.pending_move.swap(true, Ordering::AcqRel) && !self.shared.notify(MOVE, 0, 0)
        {
            self.shared.pending_move.store(false, Ordering::Release);
        }
        self.paint_edge(cfg.from_pointer(raw), cfg)
    }
    fn finish(&self, x: i32) {
        if self.shared.dragging.swap(false, Ordering::AcqRel) {
            self.last_raw.set(x);
            self.awaiting.set(true);
            self.shared.latest_x.store(x, Ordering::Release);
            self.shared.notify(END, self.epoch.get(), x as _);
        }
    }
}

fn run(host: H, shared: Arc<Shared>) {
    unsafe {
        let co = CoInitializeEx(null(), COINIT_APARTMENTTHREADED as _);
        if co < 0 {
            return;
        }
        let run = || {
            let Ok(edge) = visual::Edge::new(host) else {
                return;
            };
            if shared.host.load(Ordering::Acquire) == 0
                || register("LineworkSidebarGrip", Some(proc)).is_err()
            {
                return;
            }
            let grip = Grip {
                shared: Arc::clone(&shared),
                edge: RefCell::new(edge),
                config: Cell::new(None),
                ready: Cell::new(false),
                epoch: Cell::new(0),
                awaiting: Cell::new(false),
                origin: Cell::new(0),
                last_raw: Cell::new(0),
            };
            let h = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_NOPARENTNOTIFY,
                wide("LineworkSidebarGrip").as_ptr(),
                wide("Resize sidebar").as_ptr(),
                WS_CHILD,
                0,
                0,
                0,
                0,
                hwnd(host),
                765usize as _,
                module() as _,
                (&grip as *const Grip).cast(),
            );
            if h.is_null() {
                return;
            }
            // One alpha step supplies a hit area without a second visible rule.
            if SetLayeredWindowAttributes(h, 0, 1, LWA_ALPHA) == 0 {
                DestroyWindow(h);
                return;
            }
            shared.grip.store(h as H, Ordering::Release);
            if shared.host.load(Ordering::Acquire) == 0 || !grip.sync(h) {
                DestroyWindow(h);
            }
            let mut m: MSG = std::mem::zeroed();
            while GetMessageW(&mut m, null_mut(), 0, 0) > 0 {
                TranslateMessage(&m);
                DispatchMessageW(&m);
            }
            if IsWindow(h) != 0 {
                DestroyWindow(h);
            }
            shared.grip.store(0, Ordering::Release);
            if grip.ready.get() {
                shared.notify(LOST, 0, 0);
            }
        };
        run();
        CoUninitialize();
    }
}

unsafe extern "system" fn proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    unsafe {
        if m == WM_NCCREATE {
            SetWindowLongPtrW(
                h,
                GWLP_USERDATA,
                (*(l as *const CREATESTRUCTW)).lpCreateParams as isize,
            );
        }
        let ptr = GetWindowLongPtrW(h, GWLP_USERDATA) as *const Grip;
        let Some(grip) = ptr.as_ref() else {
            return DefWindowProcW(h, m, w, l);
        };
        let raw = || grip.origin.get() + l as i16 as i32;
        match m {
            ART => {
                grip.shared.pending_art.store(false, Ordering::Release);
                grip.prepare_scene();
            }
            PANE => {
                let pending = std::mem::take(&mut *grip.shared.images.lock().unwrap());
                for (pane, pixels) in pending {
                    if grip.edge.borrow().update_pane(pane, &pixels).is_err() {
                        DestroyWindow(h);
                        break;
                    }
                }
            }
            SYNC => {
                if !grip.sync(h) {
                    DestroyWindow(h);
                }
            }
            ACK => {
                if grip.epoch.get() == w {
                    let _ = grip.edge.borrow_mut().end_drag();
                    grip.shared.notify(RESTORE, 0, 0);
                    grip.awaiting.set(false);
                    if !grip.sync(h) {
                        DestroyWindow(h);
                    }
                }
            }
            WM_SETCURSOR => {
                SetCursor(LoadCursorW(null_mut(), IDC_SIZEWE));
                return 1;
            }
            WM_LBUTTONDOWN => {
                grip.epoch.set(grip.epoch.get().wrapping_add(1));
                grip.awaiting.set(false);
                grip.last_raw
                    .set(grip.config.get().map_or(raw(), |c| c.side));
                SetCapture(h);
                grip.shared
                    .dragging
                    .store(GetCapture() == h, Ordering::Release);
                grip.prepare_scene();
            }
            WM_MOUSEMOVE if grip.shared.dragging.load(Ordering::Acquire) => {
                if !grip.movement(raw()) {
                    DestroyWindow(h);
                }
            }
            WM_LBUTTONUP if grip.shared.dragging.load(Ordering::Acquire) => {
                let x = raw();
                if let Some(cfg) = grip.config.get() {
                    grip.paint_edge(cfg.from_pointer(x), cfg);
                }
                grip.finish(x);
                ReleaseCapture();
            }
            WM_CAPTURECHANGED => grip.finish(grip.last_raw.get()),
            WM_CANCELMODE => {
                if GetCapture() == h {
                    ReleaseCapture();
                }
            }
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let dc = BeginPaint(h, &mut ps);
                FillRect(dc, &ps.rcPaint, GetStockObject(BLACK_BRUSH) as _);
                EndPaint(h, &ps);
            }
            WM_ERASEBKGND => return 1,
            WM_NCDESTROY => {
                grip.finish(grip.last_raw.get());
                SetWindowLongPtrW(h, GWLP_USERDATA, 0);
                PostQuitMessage(0);
                return DefWindowProcW(h, m, w, l);
            }
            _ => return DefWindowProcW(h, m, w, l),
        }
        0
    }
}
