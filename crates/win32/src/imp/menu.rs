//! Popup menus. Linework draws every row of its menus itself, so a menu here is
//! a borderless window whose rows its owner measures and paints through the
//! same two messages Windows sends, and `TrackPopupMenu` runs until a row is
//! chosen or the menu is dismissed.
use crate::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use crate::Win32::Graphics::Gdi::*;
use crate::Win32::UI::Controls::*;
use crate::Win32::UI::Input::KeyboardAndMouse::*;
use crate::Win32::UI::WindowsAndMessaging::*;
use crate::imp::gdi::{self, wide_name};
use crate::imp::window::{self, DefWindowProcW, app, find, send};
use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSEventMask, NSEventType};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode};
use std::cell::Cell;

struct Item {
    id: u32,
    kind: u32,
    state: u32,
    text: String,
    data: usize,
    top: i32,
    height: i32,
}

pub(crate) struct Menu {
    items: Vec<Item>,
    back: Option<u32>,
    lit: Option<usize>,
}

/// What the menu's window keeps: the menu, its owner, and the outcome.
struct Showing {
    menu: HMENU,
    owner: HWND,
}

thread_local! {
    /// None while a menu is open and nothing is chosen; then the row's
    /// identifier, or 0 for none.
    static CHOICE: Cell<Option<u32>> = const { Cell::new(None) };
}

fn menu<'a>(handle: HMENU) -> Option<&'a mut Menu> {
    unsafe { (handle as *mut Menu).as_mut() }
}

pub unsafe fn CreatePopupMenu() -> HMENU {
    Box::into_raw(Box::new(Menu { items: Vec::new(), back: None, lit: None })) as HMENU
}

pub unsafe fn CreateMenu() -> HMENU {
    unsafe { CreatePopupMenu() }
}

pub unsafe fn DestroyMenu(hmenu: HMENU) -> i32 {
    if hmenu.is_null() {
        return 0;
    }
    drop(unsafe { Box::from_raw(hmenu as *mut Menu) });
    1
}

pub unsafe fn SetMenuInfo(param0: HMENU, param1: *const MENUINFO) -> i32 {
    let (Some(menu), Some(info)) = (menu(param0), unsafe { param1.as_ref() }) else { return 0 };
    if info.fMask & MIM_BACKGROUND != 0 {
        menu.back = gdi::brush_color(info.hbrBack);
    }
    1
}

pub unsafe fn InsertMenuItemW(hmenu: HMENU, item: u32, _: i32, lpmi: *const MENUITEMINFOW) -> i32 {
    let (Some(menu), Some(info)) = (menu(hmenu), unsafe { lpmi.as_ref() }) else { return 0 };
    let has = |bit: u32| info.fMask & bit != 0;
    let text = if has(MIIM_STRING) && !info.dwTypeData.is_null() { unsafe { wide_name(info.dwTypeData) } } else { String::new() };
    let entry = Item {
        id: if has(MIIM_ID) { info.wID } else { 0 },
        kind: if has(MIIM_FTYPE) { info.fType } else { 0 },
        state: if has(MIIM_STATE) { info.fState } else { 0 },
        text,
        data: if has(MIIM_DATA) { info.dwItemData } else { 0 },
        top: 0,
        height: 0,
    };
    menu.items.insert((item as usize).min(menu.items.len()), entry);
    1
}

pub unsafe fn AppendMenuW(hmenu: HMENU, uflags: MENU_ITEM_FLAGS, uidnewitem: usize, lpnewitem: *const u16) -> i32 {
    let Some(menu) = menu(hmenu) else { return 0 };
    let separator = uflags & MF_SEPARATOR != 0;
    let text = if separator || lpnewitem.is_null() { String::new() } else { unsafe { wide_name(lpnewitem) } };
    menu.items.push(Item { id: uidnewitem as u32, kind: if separator { MFT_SEPARATOR } else { 0 }, state: uflags & (MFS_CHECKED | MFS_GRAYED), text, data: 0, top: 0, height: 0 });
    1
}

pub unsafe fn GetMenuItemInfoW(hmenu: HMENU, item: u32, fbyposition: i32, lpmii: *mut MENUITEMINFOW) -> i32 {
    let (Some(menu), Some(info)) = (menu(hmenu), unsafe { lpmii.as_mut() }) else { return 0 };
    let found = if fbyposition != 0 { menu.items.get(item as usize) } else { menu.items.iter().find(|entry| entry.id == item) };
    let Some(entry) = found else { return 0 };
    (info.wID, info.fType, info.fState, info.dwItemData) = (entry.id, entry.kind, entry.state, entry.data);
    1
}

pub unsafe fn GetMenuState(hmenu: HMENU, uid: u32, uflags: MENU_ITEM_FLAGS) -> u32 {
    let Some(menu) = menu(hmenu) else { return u32::MAX };
    let at = if uflags & MF_BYPOSITION != 0 { Some(uid as usize) } else { menu.items.iter().position(|entry| entry.id == uid) };
    match at.and_then(|at| menu.items.get(at).map(|entry| (at, entry))) {
        Some((at, entry)) => entry.state | entry.kind | if menu.lit == Some(at) { MF_HILITE } else { 0 },
        None => u32::MAX,
    }
}

fn chosen(entry: &Item) -> bool {
    entry.kind & MFT_SEPARATOR == 0 && entry.state & MFS_GRAYED == 0
}

/// Asks the owner how large each row is and stacks them. Returns the menu's size.
fn measure(menu: &mut Menu, owner: HWND) -> (i32, i32) {
    let (mut width, mut top) = (120, 4);
    for entry in &mut menu.items {
        let (wide, high) = if entry.kind & MFT_OWNERDRAW != 0 {
            let mut asked = MEASUREITEMSTRUCT { CtlType: ODT_MENU, CtlID: 0, itemID: entry.id, itemWidth: 0, itemHeight: 0, itemData: entry.data };
            send(owner, WM_MEASUREITEM, 0, &mut asked as *mut _ as isize);
            (asked.itemWidth as i32, asked.itemHeight as i32)
        } else if entry.kind & MFT_SEPARATOR != 0 {
            (0, 9)
        } else {
            (entry.text.chars().count() as i32 * 8 + 48, 26)
        };
        (entry.top, entry.height) = (top, high.max(1));
        top += entry.height;
        width = width.max(wide);
    }
    (width, top + 4)
}

fn row_at(menu: &Menu, y: i32) -> Option<usize> {
    menu.items.iter().position(|entry| y >= entry.top && y < entry.top + entry.height)
}

unsafe extern "system" fn menu_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let Some(window) = find(hwnd) else { return 0 };
    let showing = window.control.borrow().as_ref().and_then(|state| state.downcast_ref::<Showing>().map(|showing| (showing.menu, showing.owner)));
    let Some((handle, owner)) = showing else { return unsafe { DefWindowProcW(hwnd, message, w, l) } };
    let Some(menu) = menu(handle) else { return 0 };
    let y = (l >> 16) as i16 as i32;
    match message {
        WM_PAINT => unsafe {
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let dc = window::BeginPaint(hwnd, &mut paint);
            let mut area = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            window::GetClientRect(hwnd, &mut area);
            let back = menu.back.unwrap_or(0x282120);
            gdi::rounded(dc, area, 6.0, back, 0x3B3130);
            for (at, entry) in menu.items.iter().enumerate() {
                let place = RECT { left: 1, top: entry.top, right: area.right - 1, bottom: entry.top + entry.height };
                let flags = if menu.lit == Some(at) { ODS_SELECTED } else { 0 } | if entry.state & MFS_CHECKED != 0 { ODS_CHECKED } else { 0 } | if entry.state & MFS_GRAYED != 0 { ODS_GRAYED } else { 0 };
                if entry.kind & MFT_OWNERDRAW != 0 {
                    let drawing = DRAWITEMSTRUCT { CtlType: ODT_MENU, CtlID: 0, itemID: entry.id, itemAction: ODA_DRAWENTIRE, itemState: flags, hwndItem: handle as HWND, hDC: dc, rcItem: place, itemData: entry.data };
                    send(owner, WM_DRAWITEM, 0, &drawing as *const _ as isize);
                } else if entry.kind & MFT_SEPARATOR != 0 {
                    SetDCBrushColor(dc, 0x3B3130);
                    FillRect(dc, &RECT { left: 8, top: entry.top + 4, right: area.right - 8, bottom: entry.top + 5 }, GetStockObject(DC_BRUSH) as HBRUSH);
                } else {
                    if menu.lit == Some(at) {
                        SetDCBrushColor(dc, 0x3B3130);
                        FillRect(dc, &place, GetStockObject(DC_BRUSH) as HBRUSH);
                    }
                    let text: Vec<u16> = entry.text.encode_utf16().collect();
                    SetTextColor(dc, 0xF7EDF0);
                    SetBkMode(dc, 1);
                    let mut label = RECT { left: 28, ..place };
                    DrawTextW(dc, text.as_ptr(), text.len() as i32, &mut label, DT_SINGLELINE | DT_VCENTER);
                }
            }
            window::EndPaint(hwnd, &paint);
            0
        },
        WM_MOUSEMOVE => {
            let over = row_at(menu, y).filter(|at| chosen(&menu.items[*at]));
            if menu.lit != over {
                menu.lit = over;
                window.view.setNeedsDisplay(true);
            }
            0
        }
        WM_MOUSELEAVE => {
            menu.lit = None;
            window.view.setNeedsDisplay(true);
            0
        }
        WM_LBUTTONUP | WM_RBUTTONUP => {
            if let Some(at) = row_at(menu, y).filter(|at| chosen(&menu.items[*at])) {
                CHOICE.set(Some(menu.items[at].id));
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, w, l) },
    }
}

/// Moves the lit row to the next one that can be chosen, forwards or back.
fn step(menu: &mut Menu, forward: bool) {
    let count = menu.items.len();
    if count == 0 {
        return;
    }
    let mut at = menu.lit.unwrap_or(if forward { count - 1 } else { 0 });
    for _ in 0..count {
        at = if forward { (at + 1) % count } else { (at + count - 1) % count };
        if chosen(&menu.items[at]) {
            menu.lit = Some(at);
            return;
        }
    }
}

pub unsafe fn TrackPopupMenu(hmenu: HMENU, _: TRACK_POPUP_MENU_FLAGS, x: i32, y: i32, _: i32, hwnd: HWND, _: *const RECT) -> i32 {
    let Some(shown) = menu(hmenu) else { return 0 };
    let (width, height) = measure(shown, hwnd);
    shown.lit = None;
    let class: Vec<u16> = "LwMenu".encode_utf16().chain(Some(0)).collect();
    let popup = unsafe {
        window::CreateWindowExW(WS_EX_TOPMOST | WS_EX_NOACTIVATE, class.as_ptr(), std::ptr::null(), WS_POPUP, x, y, width, height, hwnd, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null())
    };
    let Some(window) = find(popup) else { return 0 };
    window.control.replace(Some(Box::new(Showing { menu: hmenu, owner: hwnd })));
    let frame = window.frame.borrow().clone();
    unsafe { window::ShowWindow(popup, SW_SHOWNOACTIVATE) };
    CHOICE.set(None);
    // The menu's own loop: every event is looked at before it goes anywhere.
    while CHOICE.get().is_none() {
        crate::imp::script::run();
        let soon = NSDate::dateWithTimeIntervalSinceNow(0.05);
        let event = autoreleasepool(|_| app().nextEventMatchingMask_untilDate_inMode_dequeue(NSEventMask::Any, Some(&soon), unsafe { NSDefaultRunLoopMode }, true));
        let Some(event) = event else { continue };
        let kind = event.r#type();
        let inside = frame.as_ref().is_some_and(|frame| event.windowNumber() == frame.windowNumber());
        if matches!(kind, NSEventType::LeftMouseDown | NSEventType::RightMouseDown | NSEventType::OtherMouseDown) && !inside {
            // A press anywhere else puts the menu away and is not passed on.
            CHOICE.set(Some(0));
        } else if kind == NSEventType::KeyDown {
            match crate::imp::message::virtual_key(event.keyCode()) {
                VK_ESCAPE => CHOICE.set(Some(0)),
                VK_DOWN => step(shown, true),
                VK_UP => step(shown, false),
                VK_RETURN => CHOICE.set(Some(shown.lit.map_or(0, |at| shown.items[at].id))),
                _ => {
                    // A letter is offered to the owner, which knows its rows' names.
                    let letter = event.characters().and_then(|text| text.to_string().encode_utf16().next()).unwrap_or(0);
                    let answer = send(hwnd, WM_MENUCHAR, letter as usize, hmenu as isize);
                    if (answer >> 16) & 0xffff == 2 {
                        if let Some(entry) = shown.items.get((answer & 0xffff) as usize) {
                            CHOICE.set(Some(entry.id));
                        }
                    }
                }
            }
            window.view.setNeedsDisplay(true);
        } else if kind != NSEventType::KeyUp {
            autoreleasepool(|_| app().sendEvent(&event));
        }
    }
    let choice = CHOICE.take().unwrap_or(0);
    CHOICE.set(None);
    unsafe { window::DestroyWindow(popup) };
    choice as i32
}

pub(crate) fn proc() -> crate::imp::window::Proc {
    menu_proc
}
