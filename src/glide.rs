//! The gliding caret and the gliding current-line band.
//!
//! Scintilla draws its caret and its current-line highlight where they are,
//! never in between. While one of them travels, a small click-through window
//! drawn over the editor stands in for it and the editor's own is switched
//! off; when the travel ends the editor's own comes back and the stand-in is
//! hidden. The editor's caret position is never touched, so typing, the input
//! method window and accessibility see exactly what they always did.
//!
//! Nothing here can leave the caret missing: every way out of a glide (its
//! end, a cancel, the watchdog) restores the editor's own caret first.
use crate::win::*;
use linework_editor::sci as s;
use std::{
    cell::{Cell, RefCell},
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicIsize, Ordering},
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::DwmFlush, Gdi::*},
    UI::{Input::KeyboardAndMouse::GetFocus, WindowsAndMessaging::*},
};

/// Posted to the main window once per display refresh while something glides.
pub(crate) const TICK: u32 = WM_APP + 190;
/// The caret takes this long for a short hop and up to `CARET_MAX` for a jump.
const CARET_MIN: f32 = 90.0;
const CARET_MAX: f32 = 150.0;
const BAND: Duration = Duration::from_millis(160);
/// A glide that has not ended by now lost its ticks: end it.
const STUCK: Duration = Duration::from_millis(600);
/// How strongly the travelling band tints what is under it, of 255.
const BAND_ALPHA: u32 = 84;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Config {
    pub caret: bool,
    pub line: bool,
}

#[derive(Clone, Copy, PartialEq, Default, Debug)]
struct Area {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}
impl Area {
    fn between(self, to: Area, t: f32) -> Area {
        let mix = |a: f32, b: f32| a + (b - a) * t;
        Area { x: mix(self.x, to.x), y: mix(self.y, to.y), w: mix(self.w, to.w), h: mix(self.h, to.h) }
    }
}

/// One stand-in window and where it is going.
#[derive(Default)]
struct Slide {
    window: H,
    color: u32,
    alpha: u32,
    from: Area,
    to: Area,
    now: Area,
    start: Option<Instant>,
    time: Duration,
}
impl Slide {
    fn running(&self) -> bool {
        self.start.is_some()
    }
    fn place(&self, show: bool) {
        let a = self.now;
        unsafe {
            SetWindowPos(
                hwnd(self.window),
                std::ptr::null_mut(),
                a.x.round() as i32,
                a.y.round() as i32,
                (a.w.round() as i32).max(1),
                (a.h.round() as i32).max(1),
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER | if show { SWP_SHOWWINDOW } else { 0 },
            );
        }
    }
    /// Move to where the clock says. True when the travel is over.
    fn advance(&mut self) -> bool {
        let Some(start) = self.start else { return false };
        let t = start.elapsed().as_secs_f32() / self.time.as_secs_f32().max(0.001);
        if t >= 1.0 {
            return true;
        }
        // Decelerating, no overshoot.
        let eased = 1.0 - (1.0 - t).powi(3);
        self.now = self.from.between(self.to, eased);
        self.place(false);
        false
    }
}

/// What the focused editor showed when it last reported.
#[derive(Clone, Copy, PartialEq)]
struct Seen {
    editor: H,
    document: isize,
    first_line: isize,
    x_offset: isize,
    origin: (i32, i32),
    /// None when the caret is a block or is out of view.
    caret: Option<Area>,
    band: Option<Area>,
    line: isize,
}

#[derive(Default)]
struct State {
    host: H,
    config: Config,
    caret: Slide,
    band: Slide,
    seen: Option<Seen>,
    /// The editor whose caret is switched off, and the style to give back.
    caret_off: Option<(H, isize)>,
    /// The editor whose line highlight is switched off, and its color.
    line_off: Option<(H, isize)>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
    /// Set while this module is talking to an editor, whose answers can
    /// arrive as another notification.
    static BUSY: Cell<bool> = const { Cell::new(false) };
}
static ACTIVE: AtomicBool = AtomicBool::new(false);
static PENDING: AtomicBool = AtomicBool::new(false);
static HOST: AtomicIsize = AtomicIsize::new(0);
static CLOCK: OnceLock<std::thread::Thread> = OnceLock::new();

unsafe extern "system" fn overlay(h: HWND, m: u32, w: usize, l: isize) -> isize {
    unsafe {
        match m {
            WM_ERASEBKGND => {
                let mut r: RECT = std::mem::zeroed();
                GetClientRect(h, &mut r);
                fill(w as HDC, r, GetWindowLongPtrW(h, GWLP_USERDATA) as u32);
                1
            }
            WM_NCHITTEST => HTTRANSPARENT as isize,
            _ => DefWindowProcW(h, m, w, l),
        }
    }
}

/// Call once, with the main window. Starts the refresh clock.
pub(crate) fn start(host: H) {
    if register("LineworkGlide", Some(overlay)).is_err() {
        return;
    }
    STATE.with_borrow_mut(|st| st.host = host);
    HOST.store(host, Ordering::Relaxed);
    let clock = std::thread::Builder::new().name("glide clock".into()).spawn(|| {
        loop {
            while !ACTIVE.load(Ordering::Acquire) {
                std::thread::park();
            }
            // Returns at the next composition, which is once per refresh.
            if unsafe { DwmFlush() } != 0 {
                std::thread::sleep(Duration::from_millis(4));
            }
            let host = HOST.load(Ordering::Relaxed);
            if host == 0 {
                return;
            }
            if !PENDING.swap(true, Ordering::AcqRel)
                && unsafe { PostMessageW(hwnd(host), TICK, 0, 0) } == 0
            {
                PENDING.store(false, Ordering::Release);
            }
        }
    });
    if let Ok(clock) = clock {
        let _ = CLOCK.set(clock.thread().clone());
    }
}

pub(crate) fn shutdown() {
    cancel();
    HOST.store(0, Ordering::Relaxed);
    ACTIVE.store(true, Ordering::Release);
    if let Some(clock) = CLOCK.get() {
        clock.unpark();
    }
}

pub(crate) fn configure(config: Config) {
    let changed = STATE.with_borrow_mut(|st| std::mem::replace(&mut st.config, config) != config);
    if changed {
        cancel();
    }
}

fn window(host: H) -> H {
    unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            wide("LineworkGlide").as_ptr(),
            std::ptr::null(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            hwnd(host),
            std::ptr::null_mut(),
            module() as _,
            std::ptr::null(),
        ) as H
    }
}

/// Give a stand-in its color and strength, making its window on first use.
fn dress(slide: &mut Slide, host: H, color: u32, alpha: u32) -> bool {
    if slide.window == 0 {
        slide.window = window(host);
        if slide.window == 0 {
            return false;
        }
        slide.color = !color;
    }
    if slide.color != color || slide.alpha != alpha {
        slide.color = color;
        slide.alpha = alpha;
        unsafe {
            SetWindowLongPtrW(hwnd(slide.window), GWLP_USERDATA, color as isize);
            SetLayeredWindowAttributes(hwnd(slide.window), 0, alpha as u8, LWA_ALPHA);
            InvalidateRect(hwnd(slide.window), std::ptr::null(), 1);
        }
    }
    true
}

fn measure(editor: H) -> Option<Seen> {
    if msg(editor, s::SCI_GETSELECTIONS, 0, 0) != 1 {
        return None;
    }
    let at = msg(editor, s::SCI_GETCURRENTPOS, 0, 0).max(0) as usize;
    let line = msg(editor, s::SCI_LINEFROMPOSITION, at, 0).max(0);
    let height = msg(editor, s::SCI_TEXTHEIGHT, line as usize, 0).max(1) as i32;
    let (width, bottom) = client(editor);
    let margins: i32 = (0..5).map(|n| msg(editor, s::SCI_GETMARGINWIDTHN, n, 0) as i32).sum();
    let mut origin = POINT { x: 0, y: 0 };
    unsafe { ClientToScreen(hwnd(editor), &mut origin) };
    let screen = |x: i32, y: i32, w: i32, h: i32| Area {
        x: (origin.x + x) as f32,
        y: (origin.y + y) as f32,
        w: w as f32,
        h: h as f32,
    };
    let x = msg(editor, s::SCI_POINTXFROMPOSITION, 0, at as isize) as i32;
    let y = msg(editor, s::SCI_POINTYFROMPOSITION, 0, at as isize) as i32;
    let style = STATE
        .with_borrow(|st| st.caret_off.filter(|off| off.0 == editor).map(|off| off.1))
        .unwrap_or_else(|| msg(editor, s::SCI_GETCARETSTYLE, 0, 0));
    let bar = msg(editor, s::SCI_GETCARETWIDTH, 0, 0).clamp(1, 20) as i32;
    let in_view = x >= margins && x + bar <= width && y >= 0 && y + height <= bottom;
    let caret = (style & 0xf == 1 && in_view).then(|| screen(x, y, bar, height));
    // The highlight covers every wrapped row of the caret's line.
    let line_start = msg(editor, s::SCI_POSITIONFROMLINE, line as usize, 0).max(0);
    let rows = msg(editor, s::SCI_WRAPCOUNT, line as usize, 0).max(1) as i32;
    let top = msg(editor, s::SCI_POINTYFROMPOSITION, 0, line_start) as i32;
    let (a, b) = (top.max(0), (top + rows * height).min(bottom));
    let band = (b > a && width > margins).then(|| screen(margins, a, width - margins, b - a));
    Some(Seen {
        editor,
        document: msg(editor, s::SCI_GETDOCPOINTER, 0, 0),
        first_line: msg(editor, s::SCI_GETFIRSTVISIBLELINE, 0, 0),
        x_offset: msg(editor, s::SCI_GETXOFFSET, 0, 0),
        origin: (origin.x, origin.y),
        caret,
        band,
        line,
    })
}

/// The color that, laid over the page at `BAND_ALPHA`, looks like the
/// editor's own line highlight.
fn band_color(editor: H, highlight: isize) -> u32 {
    let page = msg(editor, s::SCI_STYLEGETBACK, 32, 0) as u32;
    let highlight = highlight as u32;
    (0..3).fold(0, |color, i| {
        let (p, a) = ((page >> (8 * i) & 255) as i32, (highlight >> (8 * i) & 255) as i32);
        let c = (p + (a - p) * 255 / BAND_ALPHA as i32).clamp(0, 255) as u32;
        color | c << (8 * i)
    })
}

/// An editor pane reported a change of caret, selection or scroll position.
/// Called from inside the notification, before the editor paints.
pub(crate) fn editor_updated(editor: H, updated: i32) {
    if BUSY.get() {
        return;
    }
    let (config, host) = STATE.with_borrow(|st| (st.config, st.host));
    if host == 0 || !(config.caret || config.line) {
        return;
    }
    if unsafe { GetFocus() } as H != editor {
        // Another pane's report says nothing about the pane being typed in.
        if STATE.with_borrow(|st| st.seen.is_some_and(|seen| seen.editor == editor)) {
            cancel();
        }
        return;
    }
    BUSY.set(true);
    let now = measure(editor);
    let before = STATE.with_borrow_mut(|st| std::mem::replace(&mut st.seen, now));
    let steady = match (before, now) {
        (Some(a), Some(b)) => {
            a.editor == b.editor
                && a.document == b.document
                && a.first_line == b.first_line
                && a.x_offset == b.x_offset
                && a.origin == b.origin
                && updated & 12 == 0
        }
        _ => false,
    };
    let (Some(before), Some(now), true) = (before, now, steady) else {
        // The view moved under the caret: there is no path to draw.
        BUSY.set(false);
        stop(false);
        return;
    };
    let mut wake = false;
    if config.caret
        && let (Some(from), Some(to)) = (before.caret, now.caret)
        && from != to
    {
        let off = STATE.with_borrow(|st| st.caret_off);
        let style = off.map_or_else(|| msg(editor, s::SCI_GETCARETSTYLE, 0, 0), |off| off.1);
        let color = msg(editor, s::SCI_GETELEMENTCOLOUR, 40, 0) as u32 & 0xff_ffff;
        let started = STATE.with_borrow_mut(|st| {
            if !dress(&mut st.caret, host, color, 255) {
                return false;
            }
            let slide = &mut st.caret;
            slide.from = if slide.running() { slide.now } else { from };
            slide.to = to;
            slide.now = slide.from;
            let far = (to.x - slide.from.x).hypot(to.y - slide.from.y);
            slide.time = Duration::from_secs_f32((CARET_MIN + far * 0.12).min(CARET_MAX) / 1000.0);
            slide.start = Some(Instant::now());
            slide.place(true);
            st.caret_off = Some((editor, style));
            true
        });
        if started {
            if off.is_none() {
                msg(editor, s::SCI_SETCARETSTYLE, 0, 0);
            }
            wake = true;
        }
    }
    if config.line
        && before.line != now.line
        && let (Some(from), Some(to)) = (before.band, now.band)
    {
        let off = STATE.with_borrow(|st| st.line_off);
        let highlight = off.map_or_else(|| msg(editor, s::SCI_GETELEMENTCOLOUR, 50, 0), |off| off.1);
        let color = band_color(editor, highlight);
        let started = STATE.with_borrow_mut(|st| {
            if !dress(&mut st.band, host, color, BAND_ALPHA) {
                return false;
            }
            let slide = &mut st.band;
            slide.from = if slide.running() { slide.now } else { from };
            slide.to = to;
            slide.now = slide.from;
            slide.time = BAND;
            slide.start = Some(Instant::now());
            slide.place(true);
            st.line_off = Some((editor, highlight));
            true
        });
        if started {
            if off.is_none() {
                msg(editor, s::SCI_RESETELEMENTCOLOUR, 50, 0);
            }
            wake = true;
        }
    }
    BUSY.set(false);
    if wake {
        ACTIVE.store(true, Ordering::Release);
        if let Some(clock) = CLOCK.get() {
            clock.unpark();
        }
    }
}

/// Give the editor its own caret or line highlight back, then hide the
/// stand-in. The editor repaints first so there is no frame without either.
fn settle(caret: Option<(H, isize)>, line: Option<(H, isize)>, windows: [H; 2]) {
    BUSY.set(true);
    if let Some((editor, style)) = caret {
        msg(editor, s::SCI_SETCARETSTYLE, style as usize, 0);
    }
    if let Some((editor, color)) = line {
        msg(editor, s::SCI_SETELEMENTCOLOUR, 50, color);
    }
    for (editor, _) in caret.into_iter().chain(line) {
        unsafe { UpdateWindow(hwnd(editor)) };
    }
    BUSY.set(false);
    for window in windows {
        if window != 0 {
            unsafe { ShowWindow(hwnd(window), SW_HIDE) };
        }
    }
}

/// One display refresh has passed.
pub(crate) fn tick() {
    PENDING.store(false, Ordering::Release);
    let (caret, line, windows) = STATE.with_borrow_mut(|st| {
        let mut done = (None, None, [0, 0]);
        if st.caret.advance() {
            st.caret.start = None;
            done.0 = st.caret_off.take();
            done.2[0] = st.caret.window;
        }
        if st.band.advance() {
            st.band.start = None;
            done.1 = st.line_off.take();
            done.2[1] = st.band.window;
        }
        if !st.caret.running() && !st.band.running() {
            ACTIVE.store(false, Ordering::Release);
        }
        done
    });
    settle(caret, line, windows);
}

/// End every glide now, with the editor's own caret and highlight in place.
pub(crate) fn cancel() {
    stop(true);
}

fn stop(forget: bool) {
    let (caret, line, windows) = STATE.with_borrow_mut(|st| {
        if forget {
            st.seen = None;
        }
        ACTIVE.store(false, Ordering::Release);
        let windows = [
            if st.caret.start.take().is_some() { st.caret.window } else { 0 },
            if st.band.start.take().is_some() { st.band.window } else { 0 },
        ];
        (st.caret_off.take(), st.line_off.take(), windows)
    });
    if caret.is_some() || line.is_some() || windows != [0, 0] {
        settle(caret, line, windows);
    }
}

/// The editor was given other text or another place without reporting it:
/// the next report has nothing to glide from.
pub(crate) fn forget() {
    STATE.with_borrow_mut(|st| st.seen = None);
}

/// Called from the maintenance timer. Ends a glide whose clock stopped.
pub(crate) fn watchdog() {
    let stuck = STATE.with_borrow(|st| {
        [&st.caret, &st.band]
            .iter()
            .any(|slide| slide.start.is_some_and(|start| start.elapsed() > STUCK))
    });
    if stuck {
        cancel();
    }
}
