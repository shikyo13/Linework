use crate::{
    app::App,
    events::{self, Event},
    win::*,
};
use linework_editor::{Editor, sci::*};
use linework_storage::Document;
use std::{
    cell::{Cell, RefCell},
    mem::size_of,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    UI::{
        Input::KeyboardAndMouse::{ReleaseCapture, SetCapture},
        WindowsAndMessaging::*,
    },
};

const COLUMNS: usize = 80;
const ROWS: usize = 384;
const SCALE: u32 = 1_000_000;
const READY: u32 = WM_APP + 163;
type Overview = Vec<[u32; COLUMNS]>;

#[derive(Default)]
struct Raster {
    normal: Vec<u32>,
    selected: Vec<u32>,
}
impl Raster {
    fn from_rows(rows: Overview, light: bool) -> Self {
        let colors = colors(light);
        let count = rows.len() * COLUMNS;
        let mut normal = Vec::with_capacity(count);
        let mut selected = Vec::with_capacity(count);
        for ink in rows.into_iter().flatten() {
            for (out, background) in [(&mut normal, colors.panel), (&mut selected, colors.active)] {
                out.push(to_dib(if ink == 0 { background } else { mix(ink, background, 7, 10) }));
            }
        }
        Self { normal, selected }
    }
    fn rows(&self) -> usize { self.normal.len() / COLUMNS }
}

#[derive(PartialEq, Eq)]
struct Key {
    id: u64,
    revision: u64,
    light: bool,
    language: String,
}
#[derive(Default)]
struct State {
    window: H,
    shown: bool,
    key: Option<Key>,
    last: Option<Instant>,
    raster: Raster,
    view: (u32, u32),
    light: bool,
    bytes: bool,
    task: Option<(Arc<AtomicBool>, Receiver<Overview>)>,
    drag_offset: Option<i32>,
}
thread_local! {
    static MAP: RefCell<State> = RefCell::new(State::default());
    static REGISTERED: Cell<bool> = const { Cell::new(false) };
}
fn divide(n: u64, d: u64) -> u32 {
    (u128::from(n.min(d)) * u128::from(SCALE) / u128::from(d.max(1))) as u32
}
fn fraction(total: u64, f: u32) -> u64 {
    (u128::from(total) * u128::from(f) / u128::from(SCALE)) as u64
}
fn stop(s: &mut State) {
    if let Some((cancel, _)) = s.task.take() {
        cancel.store(true, Ordering::Relaxed);
    }
}
fn height(s: &State, h: i32) -> i32 {
    if s.bytes && s.raster.normal.is_empty() {
        return (h - 6).max(1);
    }
    (s.raster.rows().max(1) as i32 * size(s.window, 3).max(2)).min((h - 6).max(1))
}
fn aim(y: i32, s: &State, h: i32) -> i32 {
    ((i64::from(y - 3) * i64::from(SCALE) / i64::from(height(s, h))).clamp(0, i64::from(SCALE)))
        as i32
}
fn move_map(y: i32, begin: bool) {
    let position = MAP.with_borrow_mut(|s| {
        if !s.shown {
            return None;
        }
        let (_, h) = client(s.window);
        let at = aim(y, s, h);
        if begin {
            let (first, last) = (s.view.0 as i32, s.view.1 as i32);
            // Grabbing anywhere inside the locator preserves the grab point.
            s.drag_offset = Some(if at >= first && at <= last {
                (first + last) / 2 - at
            } else {
                0
            });
        }
        s.drag_offset
            .map(|grab| (at + grab).clamp(0, SCALE as i32) as u32)
    });
    if let Some(f) = position {
        // A fast drag can produce many mouse messages between paints.
        events::EVENTS.with_borrow_mut(|q| {
            if let Some(Event::MapJump(previous)) = q.back_mut() {
                *previous = f;
            } else {
                q.push_back(Event::MapJump(f));
            }
        });
    }
}
unsafe extern "system" fn proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    unsafe {
        match m {
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let dc = BeginPaint(h, &mut ps);
                buffered(dc, ps.rcPaint, |memory| {
                    MAP.with_borrow(|s| paint(s, memory));
                });
                EndPaint(h, &ps);
                0
            }
            WM_ERASEBKGND => 1,
            WM_PRINTCLIENT => {
                MAP.with_borrow(|s| paint(s, w as HDC));
                0
            }
            WM_LBUTTONDOWN => {
                SetCapture(h);
                move_map((l >> 16) as i16 as i32, true);
                0
            }
            WM_MOUSEMOVE if w & 1 != 0 => {
                move_map((l >> 16) as i16 as i32, false);
                0
            }
            WM_LBUTTONUP => {
                move_map((l >> 16) as i16 as i32, false);
                MAP.with_borrow_mut(|s| s.drag_offset = None);
                ReleaseCapture();
                0
            }
            WM_CAPTURECHANGED => {
                MAP.with_borrow_mut(|s| s.drag_offset = None);
                0
            }
            WM_MOUSEWHEEL => {
                // The map shows the active editor, so the wheel over the map
                // scrolls that editor exactly as it does over the text.
                events::push(Event::MapWheel(w, l));
                0
            }
            READY => {
                events::push(Event::MapReady);
                0
            }
            _ => DefWindowProcW(h, m, w, l),
        }
    }
}
fn to_dib(c: u32) -> u32 {
    (c & 255) << 16 | c & 0xff00 | (c >> 16) & 255
}
fn mix(a: u32, b: u32, a_parts: u32, total: u32) -> u32 {
    let component = |shift: u32| {
        (((a >> shift) & 255) * a_parts + ((b >> shift) & 255) * (total - a_parts)) / total
    };
    rgb(component(0), component(8), component(16))
}
fn paint(s: &State, dc: HDC) {
    if s.window == 0 {
        return;
    }
    let (w, h) = client(s.window);
    let c = colors(s.light);
    fill(dc, rect(0, 0, w, h), c.panel);
    fill(dc, rect(0, 0, 1, h), c.line);
    let drawn = height(s, h);
    let n = s.raster.rows();
    let (lo, hi) = band(s, h, s.view);
    if n > 0 {
        unsafe {
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader = BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as _,
                biWidth: COLUMNS as i32,
                biHeight: -(n as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            };
            SetStretchBltMode(dc, COLORONCOLOR);
            let draw = |pixels: &[u32]| { StretchDIBits(
                dc,
                5,
                3,
                (w - 8).max(1),
                drawn,
                0,
                0,
                COLUMNS as i32,
                n as i32,
                pixels.as_ptr().cast(),
                &bmi,
                DIB_RGB_COLORS,
                SRCCOPY,
            ); };
            draw(&s.raster.normal);
            let saved = SaveDC(dc);
            if saved != 0 {
                IntersectClipRect(dc, 0, lo, w, hi);
                draw(&s.raster.selected);
                RestoreDC(dc, saved);
            }
        }
    }
    let edge = mix(c.accent, c.panel, 2, 5);
    fill(dc, rect(1, lo, w - 1, 1), edge);
    fill(dc, rect(1, hi - 1, w - 1, 1), edge);
    fill(dc, rect(1, lo, size(s.window, 2).max(1), hi - lo), c.accent);
}
fn band(s: &State, h: i32, view: (u32, u32)) -> (i32, i32) {
    let drawn = i64::from(height(s, h));
    let y = |f| 3 + (drawn * i64::from(f) / i64::from(SCALE)) as i32;
    let lo = y(view.0);
    (lo, y(view.1).max(lo + size(s.window, 4)).min(h.max(lo + 1)))
}
// Maps spaces, tabs, UTF-8 scalars and native lexer styles onto tiny ink runs.
fn row(
    bytes: &[u8],
    styles: Option<&[u8]>,
    palette: &[u32; 256],
    plain: u32,
    punctuation: u32,
) -> [u32; COLUMNS] {
    let mut ink = [0; COLUMNS];
    let mut col = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if col == COLUMNS || matches!(b, b'\n' | b'\r') {
            break;
        }
        if b & 0xc0 == 0x80 {
            continue;
        }
        if b == b'\t' {
            col = ((col / 4 + 1) * 4).min(COLUMNS);
            continue;
        }
        if b != b' ' {
            let styled = styles.map_or(0, |s| palette[s[i] as usize]);
            ink[col] = if styled != 0 {
                styled
            } else if b.is_ascii_punctuation() {
                punctuation
            } else {
                plain
            };
        }
        col += 1;
    }
    ink
}
#[repr(C)]
struct TextRange {
    start: isize,
    end: isize,
    output: *mut u8,
}
fn editor_rows(editor: &Editor, lines: usize, bytes_mode: bool, light: bool) -> Overview {
    let c = colors(light);
    let len = editor.len();
    let rows = if bytes_mode {
        len.div_ceil(COLUMNS).clamp(1, ROWS)
    } else {
        lines.clamp(1, ROWS)
    };
    let mut palette = [0u32; 256];
    for (style, ink) in palette.iter_mut().enumerate() {
        *ink = editor.send(SCI_STYLEGETFORE, style, 0) as u32;
    }
    let mut output = Vec::with_capacity(rows);
    let mut pairs = [0_u8; 2 * 256 + 2];
    let mut bytes = [0; 256];
    let mut styles = [0; 256];
    for i in 0..rows {
        let (start, limit) = if bytes_mode {
            (i * len / rows, len)
        } else {
            let line = i * lines / rows;
            (
                editor.send(SCI_POSITIONFROMLINE, line, 0).max(0) as usize,
                editor.send(SCI_GETLINEENDPOSITION, line, 0).max(0) as usize,
            )
        };
        let count = limit.saturating_sub(start).min(bytes.len());
        if count > 0 {
            let mut range = TextRange {
                start: start as _,
                end: (start + count) as _,
                output: pairs.as_mut_ptr(),
            };
            // Native full-width byte offsets and 2*n+2 output capacity, synchronous call.
            unsafe {
                editor.send_ptr(SCI_GETSTYLEDTEXTFULL, 0, &mut range as *mut _ as isize);
            }
            for j in 0..count {
                bytes[j] = pairs[2 * j];
                styles[j] = pairs[2 * j + 1];
            }
        }
        output.push(row(
            &bytes[..count],
            Some(&styles[..count]),
            &palette,
            c.dim,
            c.accent,
        ));
    }
    output
}
fn sample_large(doc: Document, cancel: &AtomicBool, light: bool) -> Overview {
    let len = doc.len();
    let c = colors(light);
    let palette = [0; 256];
    let count = len.div_ceil(256).clamp(1, ROWS as u64);
    let mut rows = Vec::with_capacity(count as usize);
    for i in 0..count {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let at = u128::from(i) * u128::from(len) / u128::from(count);
        let Ok(data) = doc.read(at as u64, 512) else {
            rows.push([0; COLUMNS]);
            continue;
        };
        let newline = data.iter().position(|b| *b == 10 || *b == 13);
        let start = if i > 0 {
            newline
                .and_then(|p| {
                    let next = p + 1 + usize::from(data[p] == 13 && data.get(p + 1) == Some(&10));
                    (next + 32 < data.len()).then_some(next)
                })
                .unwrap_or(0)
        } else {
            0
        };
        rows.push(row(&data[start..], None, &palette, c.dim, c.accent));
    }
    rows
}
impl App {
    pub(crate) fn map_layout(&mut self, layout: &mut WindowLayout, w: i32, side: i32, top: i32, bottom: i32) -> i32 {
        let visible =
            self.prefs.document_map && self.current().is_some() && w - side >= size(self.h, 280);
        let mut map = MAP.with_borrow(|s| s.window);
        if !visible {
            if map != 0 {
                layout.set(map, None);
                MAP.with_borrow_mut(|s| {
                    s.shown = false;
                    s.key = None;
                    stop(s);
                    s.raster = Raster::default();
                });
            }
            return 0;
        }
        if map == 0 {
            if !REGISTERED.get() {
                if register("LineworkDocumentMap", Some(proc)).is_err() {
                    return 0;
                }
                REGISTERED.set(true);
            }
            map = child(self.h, 750, "LineworkDocumentMap", "Document map", 0);
            if map == 0 {
                return 0;
            }
            MAP.with_borrow_mut(|s| s.window = map);
        }
        let width = size(self.h, 92);
        MAP.with_borrow_mut(|s| s.shown = true);
        layout.set(map, Some(rect(w - width, top, width, bottom - top)));
        width
    }
    pub(crate) fn map_refresh(&mut self) {
        if !self.prefs.document_map || !MAP.with_borrow(|s| s.shown) {
            return;
        }
        let Some(i) = self.current() else { return };
        let buffer = &self.buffers[i];
        let editor = self.edit();
        let light = self.prefs.light;
        let lines = editor.send(SCI_GETLINECOUNT, 0, 0).max(1) as usize;
        let bytes_mode = buffer.large || (lines == 1 && editor.len() > 4096);
        let len = if buffer.large {
            buffer.doc.len()
        } else {
            editor.len() as u64
        };
        let first = editor.send(SCI_GETFIRSTVISIBLELINE, 0, 0).max(0) as usize;
        let visible = editor.send(SCI_LINESONSCREEN, 0, 0).max(1) as usize;
        let top = editor.send(SCI_DOCLINEFROMVISIBLE, first, 0).max(0) as usize;
        let last = editor.send(SCI_DOCLINEFROMVISIBLE, first + visible, 0);
        let bottom = if last < 0 { lines } else { last as usize }
            .max(top + 1)
            .min(lines);
        let view = if bytes_mode {

            let (a, b) = if lines == 1 {
                let (w, _) = client(editor.hwnd());
                let a = editor.send(SCI_POSITIONFROMPOINT, 0, 1).max(0) as usize;
                let b = editor
                    .send(SCI_POSITIONFROMPOINT, (w - 2).max(0) as usize, 1)
                    .max(a as isize) as usize;
                (a, b)
            } else {
                let a = editor.send(SCI_POSITIONFROMLINE, top, 0).max(0) as usize;
                let b = if bottom < lines {
                    editor.send(SCI_POSITIONFROMLINE, bottom, 0).max(0) as usize
                } else {
                    editor.len()
                };
                (a, b)
            };
            (divide(crate::events::point(self.active,a as u64),len),divide(crate::events::point(self.active,b as u64),len))
        } else {
            (
                divide(top as u64, lines as u64),
                divide(bottom as u64, lines as u64),
            )
        };
        let key = Key {
            id: buffer.id,
            revision: buffer.rev,
            light,
            language: buffer.lang.clone(),
        };
        let (map, rebuild, bitmap_changed, old_view) = MAP.with_borrow_mut(|s| {
            let old_view = s.view;
            let mut changed = s.bytes != bytes_mode;
            s.view = view;
            s.bytes = bytes_mode;
            s.light = light;
            if let Some((_, rx)) = &s.task
                && let Ok(rows) = rx.try_recv()
            {
                s.raster = Raster::from_rows(rows, light);
                s.task = None;
                changed = true;
            }
            let rebuild = s.key.as_ref() != Some(&key)
                && (s.key.as_ref().is_none_or(|k| {
                    k.id != key.id || k.light != key.light || k.language != key.language
                }) || s
                    .last
                    .is_none_or(|at| at.elapsed() > Duration::from_millis(160)));
            if rebuild {
                if s.key.as_ref().is_none_or(|k| k.id != key.id || k.light != key.light) {
                    s.raster = Raster::default();
                }
                stop(s);
                s.last = Some(Instant::now());
                s.key = Some(key);
            }
            (s.window, rebuild, changed, old_view)
        });
        if rebuild {
            if buffer.large {
                let doc = buffer.doc.clone();
                let cancel = Arc::new(AtomicBool::new(false));
                let (tx, rx) = mpsc::channel();
                MAP.with_borrow_mut(|s| s.task = Some((cancel.clone(), rx)));
                thread::spawn(move || {
                    let rows = sample_large(doc, &cancel, light);
                    if !cancel.load(Ordering::Relaxed) && tx.send(rows).is_ok() {
                        post(map, READY, 0, 0);
                    }
                });
            } else {
                let rows = editor_rows(editor, lines, bytes_mode, light);
                MAP.with_borrow_mut(|s| s.raster = Raster::from_rows(rows, light));
            }
        }
        if bitmap_changed || rebuild {
            invalidate(map);
        } else if old_view != view {
            let (w, h) = client(map);
            MAP.with_borrow(|s| {
                for location in [old_view, view] {
                    let (lo, hi) = band(s, h, location);
                    unsafe { InvalidateRect(hwnd(map), &rect(0, lo, w, hi - lo), 0); }
                }
            });
        }
    }
    pub(crate) fn map_jump(&mut self, f: u32) {
        let Some(i) = self.current() else { return };
        if !self.prefs.document_map {
            return;
        }
        let e = self.edit();
        if MAP.with_borrow(|s| s.bytes) {
            let mut pos = fraction(self.total_len(), f);
            if self.buffers[i].large
                && let Ok(bytes) = self.buffers[i].doc.read(pos, 4)
            {
                pos += bytes.iter().take_while(|b| **b & 0xc0 == 0x80).count() as u64;
            } else if !self.buffers[i].large {
                pos = e
                    .send(
                        SCI_POSITIONAFTER,
                        e.send(SCI_POSITIONBEFORE, pos as _, 0).max(0) as usize,
                        0,
                    )
                    .max(0) as u64;
                if f == 0 {
                    pos = 0;
                }
            }
            self.go(pos, 0);
        } else {
            let lines = e.send(SCI_GETLINECOUNT, 0, 0).max(1) as u64;
            let at = fraction(lines, f).min(lines - 1);
            let visible = e.send(SCI_VISIBLEFROMDOCLINE, at as usize, 0).max(0);
            let page = e.send(SCI_LINESONSCREEN, 0, 0).max(1);
            e.send(
                SCI_SETFIRSTVISIBLELINE,
                (visible - page / 2).max(0) as usize,
                0,
            );
        }
        self.map_refresh();
    }
}
