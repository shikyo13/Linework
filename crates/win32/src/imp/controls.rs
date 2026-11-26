//! The standard controls: labels, buttons, lists and text fields. Each is a
//! window of ours with the window procedure Windows would give it. A text field
//! holds an AppKit text field, so typing, selection and input methods are the
//! Mac's own; the rest are drawn here.
use crate::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use crate::Win32::Graphics::Gdi::*;
use crate::Win32::UI::Controls::*;
use crate::Win32::UI::Input::KeyboardAndMouse::*;
use crate::Win32::UI::WindowsAndMessaging::*;
use crate::imp::gdi::{self, wide_name};
use crate::imp::window::{self, DefWindowProcW, Proc, Window, control_id, find, handle, send};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSAutoresizingMaskOptions, NSControlTextEditingDelegate, NSTextField, NSTextFieldDelegate, NSView};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRange, NSRect, NSSize, NSString};
use std::cell::Cell;
use std::rc::Rc;

// The label styles the front end uses, which windows-sys keeps elsewhere.
const SS_OWNERDRAW: u32 = 0x0d;
const SS_NOPREFIX: u32 = 0x80;
const SS_NOTIFY: u32 = 0x100;
const SS_CENTERIMAGE: u32 = 0x200;

thread_local! {
    /// Whether the program has asked for dark window chrome, which decides the
    /// colors of a control whose owner names none.
    pub(crate) static DARK: Cell<bool> = const { Cell::new(true) };
}

/// (background, raised, line, text, accent) for controls drawn here.
fn palette() -> (u32, u32, u32, u32, u32) {
    if DARK.get() { (0x1B1615, 0x282120, 0x3B3130, 0xF7EDF0, 0xF5B2CC) } else { (0xFAF4F6, 0xFFFFFF, 0xE5D9DD, 0x322326, 0x974C68) }
}

/// The window procedure of a class Windows itself provides.
pub(crate) fn proc_of(class: &str) -> Option<Proc> {
    match class.to_ascii_uppercase().as_str() {
        "STATIC" => Some(label_proc),
        "BUTTON" => Some(button_proc),
        "LISTBOX" => Some(list_proc),
        "EDIT" => Some(edit_proc),
        _ => None,
    }
}

/// The view inside a control that takes its keys, when it is not the control's own.
pub(crate) fn key_view(window: &Window) -> Option<Retained<NSView>> {
    let state = window.control.borrow();
    let edit = state.as_ref()?.downcast_ref::<Edit>()?;
    Some(Retained::into_super(Retained::into_super(edit.field.clone())))
}

fn state<T: 'static, R>(window: &Window, run: impl FnOnce(&mut T) -> R) -> Option<R> {
    let mut slot = window.control.borrow_mut();
    slot.as_mut()?.downcast_mut::<T>().map(run)
}

fn client(hwnd: HWND) -> RECT {
    let mut area = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { window::GetClientRect(hwnd, &mut area) };
    area
}

fn parent(hwnd: HWND) -> HWND {
    unsafe { window::GetParent(hwnd) }
}

/// Tells the owner something happened in a control, as WM_COMMAND.
fn notify(window: &Window, hwnd: HWND, code: u32) {
    let id = control_id(window) as usize & 0xffff;
    send(parent(hwnd), WM_COMMAND, ((code as usize) << 16) | id, hwnd as isize);
}

/// Selects the control's font and asks its owner for colors. The answer is the
/// brush for the background: none when the owner draws that itself.
fn prepare(window: &Window, hwnd: HWND, dc: HDC, ask: u32) -> HBRUSH {
    let (_, raised, _, text, _) = palette();
    unsafe {
        if window.font.get() != 0 {
            SelectObject(dc, window.font.get() as _);
        }
        SetTextColor(dc, text);
        SetBkColor(dc, raised);
        SetDCBrushColor(dc, raised);
        SetBkMode(dc, 1);
    }
    let brush = send(parent(hwnd), ask, dc as usize, hwnd as isize) as HBRUSH;
    if brush.is_null() { unsafe { GetStockObject(DC_BRUSH) as HBRUSH } } else { brush }
}

fn owner_draw(window: &Window, hwnd: HWND, dc: HDC, kind: u32, item: u32, area: RECT, flags: u32) {
    let id = control_id(window) as u32;
    let drawing = DRAWITEMSTRUCT { CtlType: kind, CtlID: id, itemID: item, itemAction: ODA_DRAWENTIRE, itemState: flags, hwndItem: hwnd, hDC: dc, rcItem: area, itemData: 0 };
    send(parent(hwnd), WM_DRAWITEM, id as usize, &drawing as *const _ as isize);
}

fn text_of(window: &Window) -> Vec<u16> {
    window.text.borrow().encode_utf16().collect()
}

fn focused(hwnd: HWND) -> bool {
    unsafe { window::GetFocus() == hwnd }
}

// --- Labels -------------------------------------------------------------

unsafe extern "system" fn label_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let Some(window) = find(hwnd) else { return 0 };
    let style = window.style.get();
    match message {
        WM_PAINT => unsafe {
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let dc = window::BeginPaint(hwnd, &mut paint);
            let brush = prepare(&window, hwnd, dc, WM_CTLCOLORSTATIC);
            let area = client(hwnd);
            if style & 0x1f == SS_OWNERDRAW {
                owner_draw(&window, hwnd, dc, ODT_STATIC, 0, area, 0);
            } else {
                FillRect(dc, &area, brush);
                let mut format = match style & 0x1f {
                    1 => DT_CENTER,
                    2 => DT_RIGHT,
                    _ => DT_LEFT,
                };
                format |= if style & SS_CENTERIMAGE != 0 { DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS } else { DT_WORDBREAK };
                if style & SS_NOPREFIX != 0 {
                    format |= DT_NOPREFIX;
                }
                let text = text_of(&window);
                let mut area = area;
                DrawTextW(dc, text.as_ptr(), text.len() as i32, &mut area, format);
            }
            window::EndPaint(hwnd, &paint);
            0
        },
        WM_SETTEXT => {
            let result = unsafe { DefWindowProcW(hwnd, message, w, l) };
            window.view.setNeedsDisplay(true);
            result
        }
        WM_LBUTTONDOWN if style & SS_NOTIFY != 0 => {
            notify(&window, hwnd, 0);
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, w, l) },
    }
}

// --- Buttons ------------------------------------------------------------

#[derive(Default)]
struct Button {
    pressed: bool,
    checked: bool,
}

fn click(window: &Window, hwnd: HWND) {
    if window.style.get() & 0xf == BS_AUTOCHECKBOX as u32 {
        state(window, |button: &mut Button| button.checked = !button.checked);
        window.view.setNeedsDisplay(true);
    }
    notify(window, hwnd, BN_CLICKED);
}

fn paint_button(window: &Window, hwnd: HWND, dc: HDC) {
    let style = window.style.get() & 0xf;
    let (pressed, checked) = state(window, |button: &mut Button| (button.pressed, button.checked)).unwrap_or_default();
    let area = client(hwnd);
    let keys = focused(hwnd);
    let brush = prepare(window, hwnd, dc, WM_CTLCOLORBTN);
    if style == BS_OWNERDRAW as u32 {
        let flags = if pressed { ODS_SELECTED } else { 0 } | if keys { ODS_FOCUS } else { 0 } | if window.enabled.get() { 0 } else { ODS_DISABLED };
        return owner_draw(window, hwnd, dc, ODT_BUTTON, 0, area, flags);
    }
    let (_, raised, line, ink, accent) = palette();
    let text = text_of(window);
    unsafe {
        FillRect(dc, &area, brush);
        SetTextColor(dc, if window.enabled.get() { ink } else { line });
        if style == BS_AUTOCHECKBOX as u32 {
            let side = 16.min(area.bottom - area.top);
            let top = (area.bottom - area.top - side) / 2;
            let tick = RECT { left: 1, top, right: 1 + side, bottom: top + side };
            gdi::rounded(dc, tick, 3.0, if checked { accent } else { raised }, if keys { accent } else { line });
            if checked {
                gdi::check_mark(dc, tick, raised);
            }
            let mut label = RECT { left: side + 8, ..area };
            DrawTextW(dc, text.as_ptr(), text.len() as i32, &mut label, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
            return;
        }
        let default = style == BS_DEFPUSHBUTTON as u32;
        gdi::rounded(dc, area, 4.0, if pressed { line } else { raised }, if default || keys { accent } else { line });
        let mut label = area;
        DrawTextW(dc, text.as_ptr(), text.len() as i32, &mut label, DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_END_ELLIPSIS);
    }
}

unsafe extern "system" fn button_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let Some(window) = find(hwnd) else { return 0 };
    match message {
        WM_CREATE => {
            window.control.replace(Some(Box::new(Button::default())));
            0
        }
        WM_PAINT => unsafe {
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let dc = window::BeginPaint(hwnd, &mut paint);
            paint_button(&window, hwnd, dc);
            window::EndPaint(hwnd, &paint);
            0
        },
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK if window.enabled.get() => unsafe {
            state(&window, |button: &mut Button| button.pressed = true);
            window::SetCapture(hwnd);
            window::SetFocus(hwnd);
            window.view.setNeedsDisplay(true);
            0
        },
        WM_LBUTTONUP => unsafe {
            let was = state(&window, |button: &mut Button| std::mem::take(&mut button.pressed)).unwrap_or(false);
            if window::GetCapture() == hwnd {
                window::ReleaseCapture();
            }
            window.view.setNeedsDisplay(true);
            let (x, y) = (l as i16 as i32, (l >> 16) as i16 as i32);
            let area = client(hwnd);
            if was && x >= 0 && y >= 0 && x < area.right && y < area.bottom {
                click(&window, hwnd);
            }
            0
        },
        WM_KEYDOWN if w == VK_SPACE as usize || w == VK_RETURN as usize => {
            click(&window, hwnd);
            0
        }
        BM_CLICK => {
            click(&window, hwnd);
            0
        }
        BM_GETCHECK => state(&window, |button: &mut Button| button.checked as isize).unwrap_or(0),
        BM_SETCHECK => {
            state(&window, |button: &mut Button| button.checked = w != 0);
            window.view.setNeedsDisplay(true);
            0
        }
        WM_SETTEXT | WM_SETFOCUS | WM_KILLFOCUS | WM_ENABLE => {
            window.view.setNeedsDisplay(true);
            unsafe { DefWindowProcW(hwnd, message, w, l) }
        }
        _ => unsafe { DefWindowProcW(hwnd, message, w, l) },
    }
}

// --- Lists --------------------------------------------------------------

struct List {
    items: Vec<String>,
    selected: isize,
    top: usize,
    row: i32,
    redraw: bool,
}

impl List {
    fn rows(&self, height: i32) -> usize {
        (height / self.row.max(1)).max(1) as usize
    }

    fn show(&mut self, index: usize, height: i32) {
        let rows = self.rows(height);
        if index < self.top {
            self.top = index;
        } else if index >= self.top + rows {
            self.top = index + 1 - rows;
        }
    }

    fn clamp(&mut self, height: i32) {
        self.top = self.top.min(self.items.len().saturating_sub(self.rows(height)));
    }
}

fn paint_list(window: &Rc<Window>, hwnd: HWND, dc: HDC) {
    let area = client(hwnd);
    let brush = prepare(window, hwnd, dc, WM_CTLCOLORLISTBOX);
    let Some((top, row, selected, count)) = state(window, |list: &mut List| {
        list.clamp(area.bottom);
        (list.top, list.row.max(1), list.selected, list.items.len())
    }) else {
        return;
    };
    let drawn = window.style.get() & LBS_OWNERDRAWFIXED as u32 != 0;
    let keys = focused(hwnd);
    let (_, _, line, _, accent) = palette();
    unsafe { FillRect(dc, &area, brush) };
    for index in top..count.min(top + (area.bottom / row) as usize + 1) {
        let place = RECT { left: 0, top: (index - top) as i32 * row, right: area.right, bottom: (index - top + 1) as i32 * row };
        let chosen = index as isize == selected;
        if drawn {
            let flags = if chosen { ODS_SELECTED } else { 0 } | if chosen && keys { ODS_FOCUS } else { 0 };
            owner_draw(window, hwnd, dc, ODT_LISTBOX, index as u32, place, flags);
            continue;
        }
        let text: Vec<u16> = state(window, |list: &mut List| list.items[index].encode_utf16().collect()).unwrap_or_default();
        unsafe {
            if chosen {
                SetDCBrushColor(dc, line);
                FillRect(dc, &place, GetStockObject(DC_BRUSH) as HBRUSH);
            }
            let mut label = RECT { left: place.left + 8, ..place };
            DrawTextW(dc, text.as_ptr(), text.len() as i32, &mut label, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX);
        }
    }
    // More rows than fit: a thin mark at the right shows where the view is.
    let shown = (area.bottom / row) as usize;
    if count > shown && shown > 0 {
        let length = (area.bottom as usize * shown / count).max(18) as i32;
        let travel = area.bottom - length;
        let offset = travel * top as i32 / (count - shown).max(1) as i32;
        gdi::rounded(dc, RECT { left: area.right - 7, top: offset + 2, right: area.right - 2, bottom: offset + length - 2 }, 2.5, accent & 0x7f7f7f | line, line);
    }
}

fn select(window: &Rc<Window>, hwnd: HWND, index: isize, tell: bool) {
    let height = client(hwnd).bottom;
    let changed = state(window, |list: &mut List| {
        let index = index.clamp(-1, list.items.len() as isize - 1);
        let changed = list.selected != index;
        list.selected = index;
        if index >= 0 {
            list.show(index as usize, height);
        }
        changed
    })
    .unwrap_or(false);
    window.view.setNeedsDisplay(true);
    if tell && changed {
        notify(window, hwnd, LBN_SELCHANGE);
    }
}

unsafe extern "system" fn list_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let Some(window) = find(hwnd) else { return 0 };
    let height = client(hwnd).bottom;
    match message {
        WM_CREATE => {
            let mut row = 18;
            if window.style.get() & LBS_OWNERDRAWFIXED as u32 != 0 {
                let id = control_id(&window) as u32;
                let mut measure = MEASUREITEMSTRUCT { CtlType: ODT_LISTBOX, CtlID: id, itemID: 0, itemWidth: 0, itemHeight: row as u32, itemData: 0 };
                send(parent(hwnd), WM_MEASUREITEM, id as usize, &mut measure as *mut _ as isize);
                row = measure.itemHeight as i32;
            }
            window.control.replace(Some(Box::new(List { items: Vec::new(), selected: -1, top: 0, row, redraw: true })));
            0
        }
        WM_PAINT => unsafe {
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let dc = window::BeginPaint(hwnd, &mut paint);
            paint_list(&window, hwnd, dc);
            window::EndPaint(hwnd, &paint);
            0
        },
        WM_PRINTCLIENT => {
            paint_list(&window, hwnd, w as HDC);
            0
        }
        WM_SETREDRAW => {
            state(&window, |list: &mut List| list.redraw = w != 0);
            if w != 0 {
                window.view.setNeedsDisplay(true);
            }
            0
        }
        LB_RESETCONTENT => {
            state(&window, |list: &mut List| {
                list.items.clear();
                list.selected = -1;
                list.top = 0;
            });
            window.view.setNeedsDisplay(true);
            0
        }
        LB_ADDSTRING => {
            let text = unsafe { wide_name(l as *const u16) };
            let index = state(&window, |list: &mut List| {
                list.items.push(text);
                list.items.len() as isize - 1
            });
            if state(&window, |list: &mut List| list.redraw).unwrap_or(true) {
                window.view.setNeedsDisplay(true);
            }
            index.unwrap_or(-1)
        }
        LB_GETCOUNT => state(&window, |list: &mut List| list.items.len() as isize).unwrap_or(0),
        LB_GETCURSEL => state(&window, |list: &mut List| list.selected).unwrap_or(-1),
        LB_SETCURSEL => {
            select(&window, hwnd, w as isize, false);
            w as isize
        }
        LB_GETTOPINDEX => state(&window, |list: &mut List| list.top as isize).unwrap_or(0),
        LB_GETITEMHEIGHT => state(&window, |list: &mut List| list.row as isize).unwrap_or(0),
        LB_SETITEMHEIGHT => {
            state(&window, |list: &mut List| list.row = (l as i32).max(1));
            window.view.setNeedsDisplay(true);
            0
        }
        LB_GETTEXTLEN => state(&window, |list: &mut List| list.items.get(w).map_or(-1, |item| item.encode_utf16().count() as isize)).unwrap_or(-1),
        LB_GETTEXT => state(&window, |list: &mut List| {
            let Some(item) = list.items.get(w) else { return -1 };
            let text: Vec<u16> = item.encode_utf16().chain(Some(0)).collect();
            unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), l as *mut u16, text.len()) };
            text.len() as isize - 1
        })
        .unwrap_or(-1),
        LB_SETHORIZONTALEXTENT => 0,
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            unsafe { window::SetFocus(hwnd) };
            let y = (l >> 16) as i16 as i32;
            let index = state(&window, |list: &mut List| {
                let index = list.top + (y / list.row.max(1)).max(0) as usize;
                (index < list.items.len()).then_some(index as isize)
            })
            .flatten();
            if let Some(index) = index {
                select(&window, hwnd, index, true);
                if message == WM_LBUTTONDBLCLK {
                    notify(&window, hwnd, LBN_DBLCLK);
                }
            }
            0
        }
        WM_MOUSEWHEEL => {
            let turn = (w >> 16) as i16 as i32;
            state(&window, |list: &mut List| {
                let rows = (turn.abs() * 3 / 120).max(1) as usize;
                list.top = if turn > 0 { list.top.saturating_sub(rows) } else { list.top + rows };
                list.clamp(height);
            });
            window.view.setNeedsDisplay(true);
            0
        }
        WM_KEYDOWN => {
            let (now, count, page) = state(&window, |list: &mut List| (list.selected, list.items.len() as isize, list.rows(height) as isize)).unwrap_or((-1, 0, 1));
            let to = match w as u16 {
                VK_UP => now - 1,
                VK_DOWN => now + 1,
                VK_PRIOR => now - page,
                VK_NEXT => now + page,
                VK_HOME => 0,
                VK_END => count - 1,
                _ => return unsafe { DefWindowProcW(hwnd, message, w, l) },
            };
            if count > 0 {
                select(&window, hwnd, to.clamp(0, count - 1), true);
            }
            0
        }
        WM_SETFOCUS | WM_KILLFOCUS | WM_SIZE => {
            window.view.setNeedsDisplay(true);
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, w, l) },
    }
}

// --- Text fields --------------------------------------------------------

struct Edit {
    field: Retained<NSTextField>,
    /// Colors last given to the field, so they are set only when they change.
    colors: (u32, u32),
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `Typing` has no `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    struct Typing;

    unsafe impl NSObjectProtocol for Typing {}
    unsafe impl NSTextFieldDelegate for Typing {}

    unsafe impl NSControlTextEditingDelegate for Typing {
        #[unsafe(method(controlTextDidChange:))]
        fn changed(&self, notice: &NSNotification) {
            let field: Option<Retained<NSView>> = unsafe { msg_send![notice, object] };
            let Some(owner) = field.and_then(|field| unsafe { field.superview() }) else { return };
            let hwnd = handle(&owner);
            if let Some(window) = find(hwnd) {
                notify(&window, hwnd, EN_CHANGE);
            }
        }
    }
);

thread_local! {
    static TYPING: std::cell::OnceCell<Retained<Typing>> = const { std::cell::OnceCell::new() };
}

fn field_text(window: &Window) -> String {
    state(window, |edit: &mut Edit| edit.field.stringValue().to_string()).unwrap_or_default()
}

unsafe extern "system" fn edit_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let Some(window) = find(hwnd) else { return 0 };
    match message {
        WM_CREATE => {
            let mtm = window::mtm();
            let field = NSTextField::new(mtm);
            let many = window.style.get() & ES_MULTILINE as u32 != 0;
            field.setBezeled(false);
            field.setBordered(false);
            field.setDrawsBackground(true);
            field.setFocusRingType(objc2_app_kit::NSFocusRingType::None);
            field.setUsesSingleLineMode(!many);
            if let Some(cell) = field.cell() {
                cell.setScrollable(!many);
                cell.setWraps(many);
            }
            field.setStringValue(&NSString::from_str(&window.text.borrow()));
            field.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
            TYPING.with(|cell| {
                let typing = cell.get_or_init(|| unsafe { msg_send![Typing::alloc(mtm), init] });
                unsafe { field.setDelegate(Some(ProtocolObject::from_ref(&**typing))) };
            });
            unsafe { window.view.addSubview(&field) };
            window.native_keys.set(true);
            window.control.replace(Some(Box::new(Edit { field, colors: (u32::MAX, u32::MAX) })));
            0
        }
        WM_SIZE => {
            let area = client(hwnd);
            // The text sits a little inside the field's edge, as on Windows.
            let inset = if area.bottom > 22 { ((area.bottom - 18) / 2).min(6) } else { 1 };
            let place = NSRect::new(NSPoint::new(6.0, inset as f64), NSSize::new((area.right - 12).max(0) as f64, (area.bottom - inset * 2).max(0) as f64));
            state(&window, |edit: &mut Edit| edit.field.setFrame(place));
            0
        }
        WM_PAINT => unsafe {
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let dc = window::BeginPaint(hwnd, &mut paint);
            let brush = prepare(&window, hwnd, dc, WM_CTLCOLOREDIT);
            let area = client(hwnd);
            FillRect(dc, &area, brush);
            let (text, back) = gdi::colors_of(dc);
            state(&window, |edit: &mut Edit| {
                if edit.colors != (text, back) {
                    edit.colors = (text, back);
                    edit.field.setTextColor(Some(&gdi::color(text)));
                    edit.field.setBackgroundColor(Some(&gdi::color(back)));
                }
            });
            if window.style.get() & WS_BORDER != 0 {
                let (_, _, line, _, accent) = palette();
                gdi::outline(dc, area, 3.0, if focused(hwnd) { accent } else { line });
            }
            window::EndPaint(hwnd, &paint);
            0
        },
        WM_SETFONT => {
            window.font.set(w as isize);
            if let Some(font) = gdi::font_of(w as _) {
                state(&window, |edit: &mut Edit| edit.field.setFont(Some(&font)));
            }
            0
        }
        WM_SETTEXT => {
            let text = unsafe { wide_name(l as *const u16) };
            let changed = field_text(&window) != text;
            state(&window, |edit: &mut Edit| edit.field.setStringValue(&NSString::from_str(&text)));
            // A one-line field tells its owner of a change made this way too.
            if changed && window.style.get() & ES_MULTILINE as u32 == 0 {
                notify(&window, hwnd, EN_CHANGE);
            }
            1
        }
        WM_GETTEXTLENGTH => field_text(&window).encode_utf16().count() as isize,
        WM_GETTEXT => {
            let text: Vec<u16> = field_text(&window).encode_utf16().collect();
            let room = w.saturating_sub(1).min(text.len());
            if w > 0 && l != 0 {
                unsafe {
                    std::ptr::copy_nonoverlapping(text.as_ptr(), l as *mut u16, room);
                    *(l as *mut u16).add(room) = 0;
                }
            }
            room as isize
        }
        EM_SETSEL => {
            state(&window, |edit: &mut Edit| {
                let Some(editor) = edit.field.currentEditor() else { return };
                let length = edit.field.stringValue().length();
                let start = (w as isize).clamp(0, length as isize) as usize;
                let end = if l < 0 { length } else { (l as usize).min(length) };
                editor.setSelectedRange(NSRange::new(start.min(end), end.abs_diff(start)));
            });
            0
        }
        WM_ENABLE => {
            state(&window, |edit: &mut Edit| edit.field.setEnabled(w != 0));
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, w, l) },
    }
}
