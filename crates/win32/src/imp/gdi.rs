//! Drawing: device contexts, fonts, text, fills, off-screen images and the
//! GDI+ shapes, all through AppKit's drawing calls.
//!
//! One Windows pixel is one point here and the reported scale is always 96,
//! so the front end's own arithmetic is unchanged; off-screen images hold as
//! many device pixels per point as the screen has, so nothing is blurred.
use crate::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use crate::Win32::Graphics::Gdi::*;
use crate::Win32::Graphics::GdiPlus::*;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, msg_send};
use objc2_app_kit::{
    NSBezierPath, NSBitmapImageRep, NSColor, NSDeviceRGBColorSpace, NSFont,
    NSFontAttributeName, NSForegroundColorAttributeName, NSGraphicsContext, NSLineBreakMode, NSLineCapStyle,
    NSLineJoinStyle, NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSRectClip, NSRectFill,
    NSStringDrawing, NSTextAlignment,
};
use objc2_foundation::{NSAffineTransform, NSDictionary, NSPoint, NSRect, NSSize, NSString};
use std::cell::RefCell;
use std::ffi::c_void;

/// Device pixels per point of off-screen images.
const DENSITY: isize = 2;

pub(crate) fn ns_rect(r: &RECT) -> NSRect {
    NSRect::new(
        NSPoint::new(r.left as f64, r.top as f64),
        NSSize::new((r.right - r.left) as f64, (r.bottom - r.top) as f64),
    )
}

pub(crate) fn color(value: COLORREF) -> Retained<NSColor> {
    let part = |shift: u32| ((value >> shift) & 255) as f64 / 255.0;
    NSColor::colorWithSRGBRed_green_blue_alpha(part(0), part(8), part(16), 1.0)
}

fn argb(value: u32) -> Retained<NSColor> {
    let part = |shift: u32| ((value >> shift) & 255) as f64 / 255.0;
    NSColor::colorWithSRGBRed_green_blue_alpha(part(16), part(8), part(0), part(24))
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Face {
    Text,
    Mono,
    /// Segoe MDL2 Assets on Windows: each character is a small picture.
    Icons,
}

pub(crate) struct Font {
    pub(crate) font: Retained<NSFont>,
    pub(crate) face: Face,
    pub(crate) size: f64,
}

pub(crate) struct Bitmap {
    pub(crate) pixels: Retained<NSBitmapImageRep>,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

pub(crate) enum Object {
    Font(Font),
    Bitmap(Bitmap),
    Brush(COLORREF),
    Stock(i32),
}

#[derive(Clone)]
struct Pen {
    color: u32,
    width: f64,
}

/// A device context. `target` is what it draws on: the view being painted, an
/// off-screen image, or nothing (a context taken only to measure text).
pub(crate) struct Dc {
    target: Option<Retained<NSGraphicsContext>>,
    font: HGDIOBJ,
    bitmap: HGDIOBJ,
    text: COLORREF,
    back: COLORREF,
    brush: COLORREF,
    opaque: bool,
    saves: usize,
    memory: bool,
}

thread_local! {
    /// The image last copied from, written out with the pictures.
    pub(crate) static LAST_SOURCE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static STOCK: RefCell<Vec<(i32, HGDIOBJ)>> = const { RefCell::new(Vec::new()) };
}

fn object<'a>(handle: HGDIOBJ) -> Option<&'a Object> {
    unsafe { (handle as *const Object).as_ref() }
}

fn dc<'a>(handle: HDC) -> Option<&'a mut Dc> {
    unsafe { (handle as *mut Dc).as_mut() }
}

fn new_dc(target: Option<Retained<NSGraphicsContext>>, memory: bool) -> HDC {
    Box::into_raw(Box::new(Dc {
        target,
        font: std::ptr::null_mut(),
        bitmap: std::ptr::null_mut(),
        text: 0,
        back: 0xffffff,
        brush: 0xffffff,
        opaque: true,
        saves: 0,
        memory,
    })) as HDC
}

/// A context drawing on whatever AppKit is drawing on now: a view inside its
/// `drawRect:`.
pub(crate) fn painting_dc() -> HDC {
    new_dc(NSGraphicsContext::currentContext(), false)
}

pub(crate) fn measuring_dc() -> HDC {
    new_dc(None, false)
}

pub(crate) fn free_dc(handle: HDC) {
    if let Some(context) = dc(handle) {
        if let Some(target) = &context.target {
            for _ in 0..context.saves {
                target.restoreGraphicsState();
            }
        }
        drop(unsafe { Box::from_raw(handle as *mut Dc) });
    }
}

/// Runs `draw` with the context's target as AppKit's current one.
fn on(handle: HDC, draw: impl FnOnce(&Dc)) -> bool {
    let Some(context) = dc(handle) else { return false };
    let Some(target) = &context.target else { return false };
    let previous = NSGraphicsContext::currentContext();
    let same = previous.as_ref().is_some_and(|current| Retained::as_ptr(current) == Retained::as_ptr(target));
    if !same {
        NSGraphicsContext::setCurrentContext(Some(target));
    }
    draw(context);
    if !same {
        NSGraphicsContext::setCurrentContext(previous.as_deref());
    }
    true
}

fn default_font() -> Font {
    Font { font: NSFont::systemFontOfSize(13.0), face: Face::Text, size: 13.0 }
}

fn with_font<T>(context: &Dc, run: impl FnOnce(&Font) -> T) -> T {
    match object(context.font) {
        Some(Object::Font(font)) => run(font),
        _ => run(&default_font()),
    }
}

fn weight(windows: i32) -> f64 {
    // AppKit's weights run from -1 to 1 with regular at 0, as Windows' run
    // from 100 to 900 with regular at 400.
    match windows {
        ..=350 => -0.4,
        351..=450 => 0.0,
        451..=550 => 0.23,
        551..=625 => 0.3,
        626..=675 => 0.35,
        676..=750 => 0.4,
        _ => 0.56,
    }
}

fn make_font(height: i32, windows_weight: i32, name: &str) -> HFONT {
    let size = if height == 0 { 13.0 } else { height.unsigned_abs() as f64 };
    let (font, face) = if name.contains("MDL2") || name.contains("Fluent") || name.contains("Symbol") {
        (NSFont::systemFontOfSize(size), Face::Icons)
    } else if name.contains("Mono") || name.contains("Consolas") || name.contains("Courier") {
        let menlo = if windows_weight >= 600 { "Menlo-Bold" } else { "Menlo-Regular" };
        let font = NSFont::fontWithName_size(&NSString::from_str(menlo), size)
            .unwrap_or_else(|| NSFont::monospacedSystemFontOfSize_weight(size, weight(windows_weight)));
        (font, Face::Mono)
    } else {
        (NSFont::systemFontOfSize_weight(size, weight(windows_weight)), Face::Text)
    };
    Box::into_raw(Box::new(Object::Font(Font { font, face, size }))) as HFONT
}

unsafe fn wide_text(text: *const u16, length: i32) -> String {
    if text.is_null() {
        return String::new();
    }
    let length = if length < 0 {
        (0..).take_while(|&at| unsafe { *text.add(at) } != 0).count()
    } else {
        length as usize
    };
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) })
}

pub(crate) unsafe fn wide_name(text: *const u16) -> String {
    unsafe { wide_text(text, -1) }
}

fn attributes(font: &Font, text: COLORREF, style: Option<&NSMutableParagraphStyle>) -> Retained<NSDictionary<NSString, AnyObject>> {
    let ink = color(text);
    let mut keys: Vec<&NSString> = unsafe { vec![NSFontAttributeName, NSForegroundColorAttributeName] };
    let mut values: Vec<&AnyObject> = vec![&font.font, &ink];
    if let Some(style) = style {
        keys.push(unsafe { NSParagraphStyleAttributeName });
        values.push(style);
    }
    NSDictionary::from_slices(&keys, &values)
}

fn line_height(font: &Font) -> f64 {
    (font.font.ascender() - font.font.descender() + font.font.leading()).ceil()
}

fn measure(font: &Font, text: &str) -> (f64, f64) {
    if font.face == Face::Icons {
        return (font.size, font.size);
    }
    let size = unsafe { NSString::from_str(text).sizeWithAttributes(Some(&attributes(font, 0, None))) };
    (size.width.ceil(), line_height(font))
}

/// The text with `&` marks taken out, as Windows shows it unless told not to.
fn without_marks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut marks = text.chars().peekable();
    while let Some(letter) = marks.next() {
        if letter == '&' {
            if marks.peek() == Some(&'&') {
                out.push(marks.next().unwrap());
            }
            continue;
        }
        out.push(letter);
    }
    out
}

pub unsafe fn DrawTextW(hdc: HDC, lpchtext: *const u16, cchtext: i32, lprc: *mut RECT, format: u32) -> i32 {
    let Some(context) = dc(hdc) else { return 0 };
    let Some(area) = (unsafe { lprc.as_mut() }) else { return 0 };
    let mut text = unsafe { wide_text(lpchtext, cchtext) };
    if format & DT_NOPREFIX == 0 {
        text = without_marks(&text);
    }
    let single = format & DT_SINGLELINE != 0;
    with_font(context, |font| {
        let (width, height) = measure(font, &text);
        if format & DT_CALCRECT != 0 {
            if single || format & DT_WORDBREAK == 0 {
                area.right = area.left + width as i32;
                area.bottom = area.top + height as i32;
                return height as i32;
            }
            let bounds = NSSize::new((area.right - area.left).max(1) as f64, 100_000.0);
            let wrapped: NSRect = unsafe {
                msg_send![&*NSString::from_str(&text), boundingRectWithSize: bounds, options: 1usize, attributes: &*attributes(font, 0, None)]
            };
            area.bottom = area.top + wrapped.size.height.ceil() as i32;
            return wrapped.size.height.ceil() as i32;
        }
        let bounds = ns_rect(area);
        if font.face == Face::Icons {
            on(hdc, |context| {
                for code in text.encode_utf16() {
                    crate::imp::icons::draw(code, bounds, font.size, context.text);
                }
            });
            return height as i32;
        }
        let style = NSMutableParagraphStyle::new();
        style.setAlignment(if format & DT_CENTER != 0 {
            NSTextAlignment::Center
        } else if format & DT_RIGHT != 0 {
            NSTextAlignment::Right
        } else {
            NSTextAlignment::Left
        });
        let mut frame = bounds;
        if single {
            style.setLineBreakMode(if format & (DT_END_ELLIPSIS | DT_WORD_ELLIPSIS) != 0 {
                NSLineBreakMode::ByTruncatingTail
            } else if format & DT_PATH_ELLIPSIS != 0 {
                NSLineBreakMode::ByTruncatingMiddle
            } else {
                NSLineBreakMode::ByClipping
            });
            frame.origin.y += if format & DT_VCENTER != 0 {
                ((bounds.size.height - height) / 2.0).floor()
            } else if format & DT_BOTTOM != 0 {
                bounds.size.height - height
            } else {
                0.0
            };
            frame.size.height = height;
        } else {
            style.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        }
        on(hdc, |context| {
            let clip = format & DT_NOCLIP == 0;
            if clip {
                NSGraphicsContext::saveGraphicsState_class();
                NSRectClip(bounds);
            }
            unsafe { NSString::from_str(&text).drawInRect_withAttributes(frame, Some(&attributes(font, context.text, Some(&style)))) };
            if clip {
                NSGraphicsContext::restoreGraphicsState_class();
            }
        });
        height as i32
    })
}

pub unsafe fn GetTextExtentPoint32W(hdc: HDC, lpstring: *const u16, c: i32, psizl: *mut SIZE) -> i32 {
    let (Some(context), Some(size)) = (dc(hdc), unsafe { psizl.as_mut() }) else { return 0 };
    let text = unsafe { wide_text(lpstring, c) };
    let (width, height) = with_font(context, |font| measure(font, &text));
    (size.cx, size.cy) = (width as i32, height as i32);
    1
}

pub unsafe fn GetTextMetricsW(hdc: HDC, lptm: *mut TEXTMETRICW) -> i32 {
    let (Some(context), Some(metrics)) = (dc(hdc), unsafe { lptm.as_mut() }) else { return 0 };
    with_font(context, |font| {
        metrics.tmHeight = line_height(font) as i32;
        metrics.tmAscent = font.font.ascender().ceil() as i32;
        metrics.tmDescent = (-font.font.descender()).ceil() as i32;
        metrics.tmAveCharWidth = measure(font, "x").0 as i32;
        metrics.tmMaxCharWidth = measure(font, "W").0 as i32;
    });
    1
}

#[allow(clippy::too_many_arguments)]
pub unsafe fn CreateFontW(cheight: i32, _: i32, _: i32, _: i32, cweight: i32, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32, pszfacename: *const u16) -> HFONT {
    make_font(cheight, cweight, &unsafe { wide_name(pszfacename) })
}

pub unsafe fn CreateFontIndirectW(lplf: *const LOGFONTW) -> HFONT {
    let Some(font) = (unsafe { lplf.as_ref() }) else { return std::ptr::null_mut() };
    let name = String::from_utf16_lossy(&font.lfFaceName);
    make_font(font.lfHeight, font.lfWeight, name.trim_end_matches('\0'))
}

pub unsafe fn CreateSolidBrush(color: COLORREF) -> HBRUSH {
    Box::into_raw(Box::new(Object::Brush(color))) as HBRUSH
}

pub unsafe fn GetStockObject(i: GET_STOCK_OBJECT_FLAGS) -> HGDIOBJ {
    STOCK.with_borrow_mut(|stock| {
        if let Some(found) = stock.iter().find(|entry| entry.0 == i) {
            return found.1;
        }
        let made = Box::into_raw(Box::new(Object::Stock(i))) as HGDIOBJ;
        stock.push((i, made));
        made
    })
}

pub unsafe fn DeleteObject(ho: HGDIOBJ) -> i32 {
    match object(ho) {
        None | Some(Object::Stock(_)) => 0,
        Some(_) => {
            drop(unsafe { Box::from_raw(ho as *mut Object) });
            1
        }
    }
}

pub unsafe fn SelectObject(hdc: HDC, h: HGDIOBJ) -> HGDIOBJ {
    let Some(context) = dc(hdc) else { return std::ptr::null_mut() };
    match object(h) {
        Some(Object::Font(_)) => std::mem::replace(&mut context.font, h),
        Some(Object::Bitmap(bitmap)) if context.memory => {
            context.target = bitmap_context(bitmap);
            context.saves = 0;
            let previous = std::mem::replace(&mut context.bitmap, h);
            // Windows hands back a one-pixel stand-in the first time.
            if previous.is_null() { unsafe { GetStockObject(-1) } } else { previous }
        }
        Some(Object::Stock(-1)) if context.memory => {
            context.target = None;
            context.saves = 0;
            std::mem::replace(&mut context.bitmap, std::ptr::null_mut())
        }
        Some(Object::Brush(color)) => {
            context.brush = *color;
            h
        }
        _ => h,
    }
}

/// The image as it is now. The bitmap object keeps the first picture it is
/// asked for, so the picture is taken from its drawing context each time.
fn snapshot(bitmap: &Bitmap) -> Option<objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>> {
    let raw = NSGraphicsContext::graphicsContextWithBitmapImageRep(&bitmap.pixels)?;
    objc2_core_graphics::CGBitmapContextCreateImage(Some(&raw.CGContext()))
}

/// A drawing context on an image, turned so that its origin is at the top left
/// and one unit is one point.
fn bitmap_context(bitmap: &Bitmap) -> Option<Retained<NSGraphicsContext>> {
    let raw = NSGraphicsContext::graphicsContextWithBitmapImageRep(&bitmap.pixels)?;
    let context = NSGraphicsContext::graphicsContextWithCGContext_flipped(&raw.CGContext(), true);
    let previous = NSGraphicsContext::currentContext();
    NSGraphicsContext::setCurrentContext(Some(&context));
    let turn = NSAffineTransform::transform();
    turn.translateXBy_yBy(0.0, bitmap.height as f64);
    turn.scaleXBy_yBy(1.0, -1.0);
    let _: () = unsafe { msg_send![&*turn, concat] };
    NSGraphicsContext::setCurrentContext(previous.as_deref());
    Some(context)
}

pub unsafe fn CreateCompatibleDC(_: HDC) -> HDC {
    new_dc(None, true)
}

pub unsafe fn CreateCompatibleBitmap(_: HDC, cx: i32, cy: i32) -> HBITMAP {
    let (width, height) = (cx.max(1), cy.max(1));
    let pixels = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            width as isize * DENSITY,
            height as isize * DENSITY,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            0,
            0,
        )
    };
    let Some(pixels) = pixels else { return std::ptr::null_mut() };
    pixels.setSize(NSSize::new(width as f64, height as f64));
    Box::into_raw(Box::new(Object::Bitmap(Bitmap { pixels, width, height }))) as HBITMAP
}

pub unsafe fn DeleteDC(hdc: HDC) -> i32 {
    free_dc(hdc);
    1
}

pub unsafe fn ReleaseDC(_: HWND, hdc: HDC) -> i32 {
    free_dc(hdc);
    1
}

pub unsafe fn GetDC(_: HWND) -> HDC {
    measuring_dc()
}

#[allow(clippy::too_many_arguments)]
pub unsafe fn BitBlt(hdc: HDC, x: i32, y: i32, cx: i32, cy: i32, hdcsrc: HDC, x1: i32, y1: i32, _: ROP_CODE) -> i32 {
    let Some(source) = dc(hdcsrc) else { return 0 };
    let Some(Object::Bitmap(bitmap)) = object(source.bitmap) else { return 0 };
    LAST_SOURCE.set(hdcsrc as usize);
    // Anything still waiting in the image's context must reach its pixels first.
    if let Some(target) = &source.target {
        target.flushGraphics();
    }
    // The part wanted, in the image's own pixels, which count down from its top.
    let part = NSRect::new(
        NSPoint::new((x1 as isize * DENSITY) as f64, (y1 as isize * DENSITY) as f64),
        NSSize::new((cx as isize * DENSITY) as f64, (cy as isize * DENSITY) as f64),
    );
    if std::env::var_os("LINEWORK_TRACE").is_some() {
        eprintln!("BitBlt to {x},{y} {cx}x{cy} from {x1},{y1} target memory={}", dc(hdc).is_some_and(|context| context.memory));
    }
    let Some(whole) = snapshot(bitmap) else { return 0 };
    let Some(image) = objc2_core_graphics::CGImage::with_image_in_rect(Some(&whole), part) else { return 0 };
    on(hdc, |context| {
        let Some(target) = &context.target else { return };
        let port = target.CGContext();
        // Every target counts down from the top, and Core Graphics draws an
        // image upwards, so the image is turned over about its own place.
        objc2_core_graphics::CGContext::save_g_state(Some(&port));
        objc2_core_graphics::CGContext::translate_ctm(Some(&port), x as f64, (y + cy) as f64);
        objc2_core_graphics::CGContext::scale_ctm(Some(&port), 1.0, -1.0);
        let place = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(cx as f64, cy as f64));
        objc2_core_graphics::CGContext::draw_image(Some(&port), place, Some(&image));
        objc2_core_graphics::CGContext::restore_g_state(Some(&port));
    }) as i32
}

pub unsafe fn FillRect(hdc: HDC, lprc: *const RECT, hbr: HBRUSH) -> i32 {
    let Some(area) = (unsafe { lprc.as_ref() }) else { return 0 };
    on(hdc, |context| {
        let ink = match object(hbr) {
            Some(Object::Brush(color)) => *color,
            Some(Object::Stock(NULL_BRUSH)) => return,
            Some(Object::Stock(WHITE_BRUSH)) => 0xffffff,
            Some(Object::Stock(BLACK_BRUSH)) => 0,
            _ => context.brush,
        };
        color(ink).set();
        NSRectFill(ns_rect(area));
    }) as i32
}

pub unsafe fn SetDCBrushColor(hdc: HDC, color: COLORREF) -> COLORREF {
    dc(hdc).map_or(0, |context| std::mem::replace(&mut context.brush, color))
}

pub unsafe fn SetTextColor(hdc: HDC, color: COLORREF) -> COLORREF {
    dc(hdc).map_or(0, |context| std::mem::replace(&mut context.text, color))
}

pub unsafe fn SetBkColor(hdc: HDC, color: COLORREF) -> COLORREF {
    dc(hdc).map_or(0, |context| std::mem::replace(&mut context.back, color))
}

pub unsafe fn SetBkMode(hdc: HDC, mode: i32) -> i32 {
    dc(hdc).map_or(0, |context| if std::mem::replace(&mut context.opaque, mode != 1) { 2 } else { 1 })
}

pub unsafe fn SaveDC(hdc: HDC) -> i32 {
    let Some(context) = dc(hdc) else { return 0 };
    if let Some(target) = &context.target {
        target.saveGraphicsState();
    }
    context.saves += 1;
    context.saves as i32
}

pub unsafe fn RestoreDC(hdc: HDC, _: i32) -> i32 {
    let Some(context) = dc(hdc) else { return 0 };
    if context.saves == 0 {
        return 0;
    }
    if let Some(target) = &context.target {
        target.restoreGraphicsState();
    }
    context.saves -= 1;
    1
}

pub unsafe fn IntersectClipRect(hdc: HDC, left: i32, top: i32, right: i32, bottom: i32) -> GDI_REGION_TYPE {
    on(hdc, |_| NSRectClip(ns_rect(&RECT { left, top, right, bottom })));
    SIMPLEREGION
}

pub unsafe fn GdiFlush() -> i32 {
    1
}

pub unsafe fn UnionRect(lprcdst: *mut RECT, lprcsrc1: *const RECT, lprcsrc2: *const RECT) -> i32 {
    let (Some(out), Some(a), Some(b)) = (unsafe { lprcdst.as_mut() }, unsafe { lprcsrc1.as_ref() }, unsafe { lprcsrc2.as_ref() }) else {
        return 0;
    };
    let empty = |r: &RECT| r.right <= r.left || r.bottom <= r.top;
    *out = match (empty(a), empty(b)) {
        (true, true) => RECT { left: 0, top: 0, right: 0, bottom: 0 },
        (true, false) => *b,
        (false, true) => *a,
        _ => RECT { left: a.left.min(b.left), top: a.top.min(b.top), right: a.right.max(b.right), bottom: a.bottom.max(b.bottom) },
    };
    !empty(out) as i32
}

pub unsafe fn InflateRect(lprc: *mut RECT, dx: i32, dy: i32) -> i32 {
    let Some(r) = (unsafe { lprc.as_mut() }) else { return 0 };
    (r.left, r.top, r.right, r.bottom) = (r.left - dx, r.top - dy, r.right + dx, r.bottom + dy);
    1
}

pub unsafe fn DrawFocusRect(hdc: HDC, lprc: *const RECT) -> i32 {
    let Some(area) = (unsafe { lprc.as_ref() }) else { return 0 };
    on(hdc, |context| {
        let mut inner = ns_rect(area);
        (inner.origin.x, inner.origin.y) = (inner.origin.x + 0.5, inner.origin.y + 0.5);
        (inner.size.width, inner.size.height) = (inner.size.width - 1.0, inner.size.height - 1.0);
        let path = NSBezierPath::bezierPathWithRect(inner);
        let pattern = [1.0f64, 1.0];
        unsafe { path.setLineDash_count_phase(pattern.as_ptr(), 2, 0.0) };
        color(context.text).set();
        path.stroke();
    }) as i32
}

// --- GDI+ ---------------------------------------------------------------

struct Graphics {
    dc: HDC,
    clipped: bool,
}

fn graphics<'a>(handle: *mut GpGraphics) -> Option<&'a mut Graphics> {
    unsafe { (handle as *mut Graphics).as_mut() }
}

fn path<'a>(handle: *mut GpPath) -> Option<&'a NSBezierPath> {
    unsafe { (handle as *const NSBezierPath).as_ref() }
}

pub unsafe fn GdiplusStartup(token: *mut usize, _: *const GdiplusStartupInput, _: *mut GdiplusStartupOutput) -> Status {
    if let Some(token) = unsafe { token.as_mut() } {
        *token = 1;
    }
    0
}

pub unsafe fn GdipCreateFromHDC(hdc: HDC, graphics: *mut *mut GpGraphics) -> Status {
    unsafe { *graphics = Box::into_raw(Box::new(Graphics { dc: hdc, clipped: false })) as *mut GpGraphics };
    0
}

pub unsafe fn GdipDeleteGraphics(handle: *mut GpGraphics) -> Status {
    if let Some(state) = graphics(handle) {
        if state.clipped {
            unsafe { RestoreDC(state.dc, -1) };
        }
        drop(unsafe { Box::from_raw(handle as *mut Graphics) });
    }
    0
}

pub unsafe fn GdipSetSmoothingMode(_: *mut GpGraphics, _: SmoothingMode) -> Status {
    0
}

pub unsafe fn GdipSetClipRectI(handle: *mut GpGraphics, x: i32, y: i32, width: i32, height: i32, _: CombineMode) -> Status {
    let Some(state) = graphics(handle) else { return 2 };
    // The clip belongs to this object and ends with it.
    if state.clipped {
        unsafe { RestoreDC(state.dc, -1) };
    }
    unsafe {
        SaveDC(state.dc);
        IntersectClipRect(state.dc, x, y, x + width, y + height);
    }
    state.clipped = true;
    0
}

pub unsafe fn GdipCreatePath(_: FillMode, path: *mut *mut GpPath) -> Status {
    unsafe { *path = Retained::into_raw(NSBezierPath::bezierPath()) as *mut GpPath };
    0
}

pub unsafe fn GdipDeletePath(handle: *mut GpPath) -> Status {
    if !handle.is_null() {
        drop(unsafe { Retained::from_raw(handle as *mut NSBezierPath) });
    }
    0
}

/// Part of an ellipse inside the box given, from `start` degrees through `sweep`
/// more, both counted the way the hands of a clock turn.
pub unsafe fn GdipAddPathArc(handle: *mut GpPath, x: f32, y: f32, width: f32, height: f32, startangle: f32, sweepangle: f32) -> Status {
    let Some(path) = path(handle) else { return 2 };
    let (rx, ry) = (width as f64 / 2.0, height as f64 / 2.0);
    let (cx, cy) = (x as f64 + rx, y as f64 + ry);
    let at = |degrees: f64| {
        let angle = degrees.to_radians();
        NSPoint::new(cx + rx * angle.cos(), cy + ry * angle.sin())
    };
    let pieces = (sweepangle.abs() / 90.0).ceil().max(1.0) as usize;
    let step = sweepangle as f64 / pieces as f64;
    for piece in 0..pieces {
        let from = startangle as f64 + step * piece as f64;
        let (a, b) = (from.to_radians(), (from + step).to_radians());
        // One cubic curve stands in for up to a quarter turn.
        let k = 4.0 / 3.0 * ((b - a) / 4.0).tan();
        let start = at(from);
        if piece == 0 {
            if path.isEmpty() { path.moveToPoint(start) } else { path.lineToPoint(start) }
        }
        let first = NSPoint::new(start.x - k * rx * a.sin(), start.y + k * ry * a.cos());
        let end = at(from + step);
        let second = NSPoint::new(end.x + k * rx * b.sin(), end.y - k * ry * b.cos());
        path.curveToPoint_controlPoint1_controlPoint2(end, first, second);
    }
    0
}

pub unsafe fn GdipClosePathFigure(handle: *mut GpPath) -> Status {
    if let Some(path) = path(handle) {
        path.closePath();
    }
    0
}

pub unsafe fn GdipCreateSolidFill(color: u32, brush: *mut *mut GpSolidFill) -> Status {
    unsafe { *brush = Box::into_raw(Box::new(color)) as *mut GpSolidFill };
    0
}

pub unsafe fn GdipDeleteBrush(brush: *mut GpBrush) -> Status {
    if !brush.is_null() {
        drop(unsafe { Box::from_raw(brush as *mut u32) });
    }
    0
}

pub unsafe fn GdipCreatePen1(color: u32, width: f32, _: Unit, pen: *mut *mut GpPen) -> Status {
    unsafe { *pen = Box::into_raw(Box::new(Pen { color, width: width as f64 })) as *mut GpPen };
    0
}

pub unsafe fn GdipDeletePen(pen: *mut GpPen) -> Status {
    if !pen.is_null() {
        drop(unsafe { Box::from_raw(pen as *mut Pen) });
    }
    0
}

// Every pen the front end makes is given round ends and joins straight away.
pub unsafe fn GdipSetPenLineJoin(_: *mut GpPen, _: LineJoin) -> Status {
    0
}

pub unsafe fn GdipSetPenStartCap(_: *mut GpPen, _: LineCap) -> Status {
    0
}

pub unsafe fn GdipSetPenEndCap(_: *mut GpPen, _: LineCap) -> Status {
    0
}

fn stroke(path: &NSBezierPath, pen: *mut GpPen) {
    let Some(pen) = (unsafe { (pen as *const Pen).as_ref() }) else { return };
    path.setLineWidth(pen.width);
    path.setLineCapStyle(NSLineCapStyle::Round);
    path.setLineJoinStyle(NSLineJoinStyle::Round);
    argb(pen.color).set();
    path.stroke();
}

pub unsafe fn GdipFillPath(handle: *mut GpGraphics, brush: *mut GpBrush, shape: *mut GpPath) -> Status {
    let (Some(state), Some(shape), Some(ink)) = (graphics(handle), path(shape), unsafe { (brush as *const u32).as_ref() }) else { return 2 };
    on(state.dc, |_| {
        argb(*ink).set();
        shape.fill();
    });
    0
}

pub unsafe fn GdipDrawPath(handle: *mut GpGraphics, pen: *mut GpPen, shape: *mut GpPath) -> Status {
    let (Some(state), Some(shape)) = (graphics(handle), path(shape)) else { return 2 };
    on(state.dc, |_| stroke(shape, pen));
    0
}

fn polygon(points: *const PointF, count: i32) -> Retained<NSBezierPath> {
    let shape = NSBezierPath::bezierPath();
    for (at, point) in unsafe { std::slice::from_raw_parts(points, count.max(0) as usize) }.iter().enumerate() {
        let point = NSPoint::new(point.X as f64, point.Y as f64);
        if at == 0 { shape.moveToPoint(point) } else { shape.lineToPoint(point) }
    }
    shape
}

pub unsafe fn GdipDrawLines(handle: *mut GpGraphics, pen: *mut GpPen, points: *const PointF, count: i32) -> Status {
    let Some(state) = graphics(handle) else { return 2 };
    on(state.dc, |_| stroke(&polygon(points, count), pen));
    0
}

pub unsafe fn GdipFillPolygon2(handle: *mut GpGraphics, brush: *mut GpBrush, points: *const PointF, count: i32) -> Status {
    let (Some(state), Some(ink)) = (graphics(handle), unsafe { (brush as *const u32).as_ref() }) else { return 2 };
    on(state.dc, |_| {
        let shape = polygon(points, count);
        shape.closePath();
        argb(*ink).set();
        shape.fill();
    });
    0
}

/// The color of a solid brush.
pub(crate) fn brush_color(handle: HBRUSH) -> Option<COLORREF> {
    match object(handle as HGDIOBJ) {
        Some(Object::Brush(color)) => Some(*color),
        _ => None,
    }
}

/// The text and background colors a context holds.
pub(crate) fn colors_of(hdc: HDC) -> (COLORREF, COLORREF) {
    dc(hdc).map_or((0, 0xffffff), |context| (context.text, context.back))
}

/// The AppKit font behind a font handle.
pub(crate) fn font_of(handle: HFONT) -> Option<Retained<NSFont>> {
    match object(handle as HGDIOBJ) {
        Some(Object::Font(font)) => Some(font.font.clone()),
        _ => None,
    }
}

fn rounded_path(area: RECT, radius: f64) -> Retained<NSBezierPath> {
    let mut bounds = ns_rect(&area);
    (bounds.origin.x, bounds.origin.y) = (bounds.origin.x + 0.5, bounds.origin.y + 0.5);
    (bounds.size.width, bounds.size.height) = ((bounds.size.width - 1.0).max(0.0), (bounds.size.height - 1.0).max(0.0));
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(bounds, radius, radius)
}

/// A rectangle with rounded corners, filled and edged, for controls drawn here.
pub(crate) fn rounded(hdc: HDC, area: RECT, radius: f64, inside: COLORREF, edge: COLORREF) {
    on(hdc, |_| {
        let path = rounded_path(area, radius);
        color(inside).set();
        path.fill();
        color(edge).set();
        path.stroke();
    });
}

pub(crate) fn outline(hdc: HDC, area: RECT, radius: f64, edge: COLORREF) {
    on(hdc, |_| {
        color(edge).set();
        rounded_path(area, radius).stroke();
    });
}

/// The tick inside a checked box.
pub(crate) fn check_mark(hdc: HDC, area: RECT, ink: COLORREF) {
    on(hdc, |_| {
        let (x, y, side) = (area.left as f64, area.top as f64, (area.right - area.left) as f64);
        let path = NSBezierPath::bezierPath();
        path.moveToPoint(NSPoint::new(x + side * 0.25, y + side * 0.52));
        path.lineToPoint(NSPoint::new(x + side * 0.43, y + side * 0.70));
        path.lineToPoint(NSPoint::new(x + side * 0.76, y + side * 0.32));
        path.setLineWidth(1.6);
        path.setLineCapStyle(NSLineCapStyle::Round);
        path.setLineJoinStyle(NSLineJoinStyle::Round);
        color(ink).set();
        path.stroke();
    });
}

/// Writes the image a memory context draws on to a PNG file, for looking at
/// what was drawn without a screen.
pub fn save_picture(hdc: HDC, path: &std::path::Path) -> bool {
    let Some(context) = dc(hdc) else { return false };
    let Some(Object::Bitmap(bitmap)) = object(context.bitmap) else { return false };
    if let Some(target) = &context.target {
        target.flushGraphics();
    }
    let empty = NSDictionary::new();
    let Some(image) = snapshot(bitmap) else { return false };
    let copy = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &image);
    let Some(data) = (unsafe { copy.representationUsingType_properties(objc2_app_kit::NSBitmapImageFileType::PNG, &empty) }) else { return false };
    unsafe { msg_send![&*data, writeToFile: &*NSString::from_str(&path.to_string_lossy()), atomically: true] }
}

#[allow(dead_code)]
pub(crate) fn unused(_: *mut c_void, _: POINT) {}
