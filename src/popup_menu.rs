//! Native menu tracking and accessibility, with the same palette as the frame.
use super::{Colors, H, LIGHT, colors, dpi, fill, font, high_contrast, hwnd, wide};
use std::{cell::RefCell, mem, ptr, rc::Rc};
use windows_sys::Win32::{
    Foundation::{POINT, RECT, SIZE},
    Graphics::Gdi::*,
    UI::{
        Accessibility::{MSAA_MENU_SIG, MSAAMENUINFO},
        Controls::{
            DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_CHECKED, ODS_DISABLED, ODS_GRAYED, ODS_SELECTED,
            ODT_MENU,
        },
        HiDpi::SystemParametersInfoForDpi,
        WindowsAndMessaging::*,
    },
};

pub(crate) struct PopupItem<'a> {
    pub id: u16,
    pub label: &'a str,
    pub shortcut: String,
    pub checked: bool,
}

struct Row {
    accessible: MSAAMENUINFO,
    text: Vec<u16>,
    label_len: usize,
    key: Option<char>,
    id: u16,
    checked: bool,
}

impl Row {
    fn new(item: &PopupItem<'_>) -> Self {
        let mut text = wide(item.label);
        let label_len = text.len() - 1;
        if !item.shortcut.is_empty() {
            text.pop();
            text.push(b'\t' as u16);
            text.extend(wide(&item.shortcut));
        }
        Self {
            accessible: MSAAMENUINFO {
                dwMSAASignature: MSAA_MENU_SIG as u32,
                cchWText: (text.len() - 1) as u32,
                pszWText: text.as_mut_ptr(),
            },
            text,
            label_len,
            key: item.label.chars().next().map(|c| c.to_ascii_lowercase()),
            id: item.id,
            checked: item.checked,
        }
    }

    fn data(&self) -> usize {
        // Windows reads this documented MSAA header directly. Its text and the
        // header's address stay valid until DestroyMenu, including modal callbacks.
        ptr::from_ref(&self.accessible) as usize
    }

    fn label(&self) -> &[u16] {
        &self.text[..self.label_len]
    }
    fn shortcut(&self) -> &[u16] {
        self.text
            .get(self.label_len + 1..self.text.len() - 1)
            .unwrap_or_default()
    }
}

struct Popup {
    handle: HMENU,
    rows: Vec<Row>,
    palette: Colors,
    brush: HBRUSH,
    owned_font: HFONT,
    face: HFONT,
    dpi: i32,
    item_width: i32,
    item_height: i32,
    shortcut_width: i32,
}

impl Drop for Popup {
    fn drop(&mut self) {
        unsafe {
            DestroyMenu(self.handle);
            if !self.brush.is_null() {
                DeleteObject(self.brush);
            }
            if !self.owned_font.is_null() {
                DeleteObject(self.owned_font);
            }
        }
    }
}

thread_local! {
    static ACTIVE: RefCell<Option<Rc<Popup>>> = const { RefCell::new(None) };
}

impl Popup {
    fn px(&self, dip: i32) -> i32 {
        (dip * self.dpi + 48) / 96
    }

    fn new(owner: H, items: &[PopupItem<'_>], system: bool) -> Option<Self> {
        unsafe {
            let handle = CreatePopupMenu();
            if handle.is_null() {
                return None;
            }
            let mut menu = Self {
                handle,
                rows: items.iter().map(Row::new).collect(),
                palette: colors(LIGHT.get()),
                brush: ptr::null_mut(),
                owned_font: ptr::null_mut(),
                face: ptr::null_mut(),
                dpi: dpi(owner),
                item_width: 0,
                item_height: 0,
                shortcut_width: 0,
            };
            if !system {
                menu.brush = CreateSolidBrush(menu.palette.raised);
                let info = MENUINFO {
                    cbSize: size_of::<MENUINFO>() as u32,
                    fMask: MIM_BACKGROUND | MIM_STYLE,
                    hbrBack: menu.brush,
                    dwStyle: MNS_NOCHECK, // The painted row owns its check gutter.
                    ..mem::zeroed()
                };
                if menu.brush.is_null() || SetMenuInfo(handle, &info) == 0 {
                    return None;
                }
                let mut metrics: NONCLIENTMETRICSW = mem::zeroed();
                metrics.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
                if SystemParametersInfoForDpi(
                    SPI_GETNONCLIENTMETRICS,
                    metrics.cbSize,
                    ptr::from_mut(&mut metrics).cast(),
                    0,
                    menu.dpi as u32,
                ) != 0
                {
                    menu.owned_font = CreateFontIndirectW(&metrics.lfMenuFont);
                }
                menu.face = if menu.owned_font.is_null() {
                    font(menu.px(14), 400, false) as HFONT
                } else {
                    menu.owned_font
                };
                let dc = GetDC(hwnd(owner));
                if dc.is_null() {
                    return None;
                }
                let old = SelectObject(dc, menu.face);
                let extent = |text: &[u16]| -> i32 {
                    let mut out: SIZE = mem::zeroed();
                    GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32, &mut out);
                    out.cx
                };
                let max_label = menu
                    .rows
                    .iter()
                    .map(|r| extent(r.label()))
                    .max()
                    .unwrap_or(0);
                menu.shortcut_width = menu
                    .rows
                    .iter()
                    .map(|r| extent(r.shortcut()))
                    .max()
                    .unwrap_or(0);
                let mut tm = mem::zeroed();
                GetTextMetricsW(dc, &mut tm);
                SelectObject(dc, old);
                ReleaseDC(hwnd(owner), dc);
                menu.item_height = menu.px(26).max(tm.tmHeight + menu.px(8));
                let gap = if menu.shortcut_width > 0 {
                    menu.px(26)
                } else {
                    0
                };
                menu.item_width = menu.px(31) + max_label + gap + menu.shortcut_width + menu.px(14);
            }
            for (index, row) in menu.rows.iter().enumerate() {
                let separator = if row.id == 0 {
                    MFT_SEPARATOR
                } else {
                    MFT_STRING
                };
                let info = MENUITEMINFOW {
                    cbSize: size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_ID
                        | MIIM_FTYPE
                        | MIIM_STATE
                        | if row.id == 0 { 0 } else { MIIM_STRING }
                        | if system { 0 } else { MIIM_DATA },
                    fType: separator | if system { 0 } else { MFT_OWNERDRAW },
                    fState: if row.checked {
                        MFS_CHECKED
                    } else {
                        MFS_UNCHECKED
                    },
                    wID: u32::from(row.id),
                    dwTypeData: row.text.as_ptr().cast_mut(),
                    dwItemData: if system { 0 } else { row.data() },
                    ..mem::zeroed()
                };
                if InsertMenuItemW(handle, index as u32, 1, &info) == 0 {
                    return None;
                }
            }
            Some(menu)
        }
    }

    fn row(&self, data: usize) -> Option<&Row> {
        self.rows.iter().find(|r| r.data() == data)
    }

    fn paint(&self, row: &Row, item: &DRAWITEMSTRUCT) {
        let rect = item.rcItem;
        let dc = item.hDC;
        let selected = item.itemState & ODS_SELECTED != 0;
        let disabled = item.itemState & (ODS_DISABLED | ODS_GRAYED) != 0;
        let p = self.palette;
        unsafe {
            let saved = SaveDC(dc);
            fill(dc, rect, if selected { p.active } else { p.raised });
            let left = rect.left + self.px(31);
            let right = rect.right - self.px(14);
            if row.id == 0 {
                let mid = (rect.top + rect.bottom) / 2;
                fill(
                    dc,
                    RECT {
                        left,
                        right,
                        top: mid,
                        bottom: mid + self.px(1),
                    },
                    p.line,
                );
            } else {
                if selected && !disabled {
                    fill(
                        dc,
                        RECT {
                            left: rect.left + self.px(2),
                            right: rect.left + self.px(4),
                            top: rect.top + self.px(5),
                            bottom: rect.bottom - self.px(5),
                        },
                        p.accent,
                    );
                }
                SelectObject(dc, self.face);
                SetBkMode(dc, TRANSPARENT as i32);
                let text = |s: &[u16], left, right, color, alignment| {
                    if s.is_empty() || right <= left {
                        return;
                    }
                    SetTextColor(dc, color);
                    let mut bounds = RECT {
                        left,
                        right,
                        ..rect
                    };
                    DrawTextW(
                        dc,
                        s.as_ptr(),
                        s.len() as i32,
                        &mut bounds,
                        DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX | alignment,
                    );
                };
                if item.itemState & ODS_CHECKED != 0 {
                    text(
                        &[0x2713],
                        rect.left,
                        left,
                        if disabled { p.dim } else { p.accent },
                        DT_CENTER,
                    );
                }
                let label_right = if self.shortcut_width > 0 {
                    right - self.shortcut_width - self.px(12)
                } else {
                    right
                };
                text(
                    row.label(),
                    left,
                    label_right,
                    if disabled { p.dim } else { p.text },
                    DT_LEFT,
                );
                text(
                    row.shortcut(),
                    right - self.shortcut_width,
                    right,
                    p.dim,
                    DT_RIGHT,
                );
            }
            RestoreDC(dc, saved);
        }
    }
}

pub(crate) fn menu_at(owner: H, items: &[PopupItem<'_>], x: i32, y: i32) -> u16 {
    let system = high_contrast();
    let Some(menu) = Popup::new(owner, items, system)
        .or_else(|| {
            if system {
                None
            } else {
                Popup::new(owner, items, true)
            }
        })
        .map(Rc::new)
    else {
        return 0;
    };
    // TrackPopupMenu runs its own nested message loop. Recover any outer popup
    // afterwards, and keep Rust borrows out of the native tracking call.
    let previous = ACTIVE.replace(Some(Rc::clone(&menu)));
    let mut pt = POINT { x, y };
    let command = unsafe {
        ClientToScreen(hwnd(owner), &mut pt);
        TrackPopupMenu(
            menu.handle,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            0,
            hwnd(owner),
            ptr::null(),
        )
    };
    ACTIVE.replace(previous);
    command as u16
}

pub(crate) fn measure(item: &mut MEASUREITEMSTRUCT) -> bool {
    if item.CtlType != ODT_MENU {
        return false;
    }
    ACTIVE.with_borrow(|active| {
        let Some(menu) = active else {
            return false;
        };
        let Some(row) = menu.row(item.itemData) else {
            return false;
        };
        item.itemWidth = menu.item_width as u32;
        item.itemHeight = if row.id == 0 {
            menu.px(9)
        } else {
            menu.item_height
        } as u32;
        true
    })
}

pub(crate) fn draw(item: &DRAWITEMSTRUCT) -> bool {
    if item.CtlType != ODT_MENU {
        return false;
    }
    ACTIVE.with_borrow(|active| {
        let Some(menu) = active else {
            return false;
        };
        if menu.handle != item.hwndItem {
            return false;
        }
        let Some(row) = menu.row(item.itemData) else {
            return false;
        };
        menu.paint(row, item);
        true
    })
}

pub(crate) fn menu_char(handle: HMENU, key: u16) -> Option<isize> {
    let key = char::from_u32(u32::from(key))?.to_ascii_lowercase();
    ACTIVE.with_borrow(|active| {
        let menu = active.as_ref()?;
        if menu.handle != handle || menu.brush.is_null() {
            return None;
        }
        let selected = (0..menu.rows.len())
            .find(|&i| unsafe { GetMenuState(handle, i as u32, MF_BYPOSITION) & MF_HILITE != 0 });
        let start = selected.map_or(0, |s| s + 1);
        let mut matches = (0..menu.rows.len())
            .map(|n| (n + start) % menu.rows.len())
            .filter(|&i| menu.rows[i].id != 0 && menu.rows[i].key == Some(key));
        let first = matches.next()?;
        let action = if matches.next().is_some() {
            MNC_SELECT
        } else {
            MNC_EXECUTE
        };
        Some(((action as isize) << 16) | first as isize)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_menu_metadata_and_row_pixels_follow_the_selected_theme() {
        let items = [
            PopupItem {
                id: 41,
                label: "Word wrap",
                shortcut: "Alt+Z".into(),
                checked: true,
            },
            PopupItem {
                id: 0,
                label: "",
                shortcut: String::new(),
                checked: false,
            },
            PopupItem {
                id: 54,
                label: "Save",
                shortcut: "Ctrl+S".into(),
                checked: false,
            },
            PopupItem {
                id: 78,
                label: "Save as…",
                shortcut: String::new(),
                checked: false,
            },
        ];
        for (light, system) in [(false, false), (true, false), (false, true)] {
            LIGHT.set(light);
            let popup = Rc::new(Popup::new(0, &items, system).unwrap());
            unsafe {
                let mut entry: MENUITEMINFOW = mem::zeroed();
                entry.cbSize = size_of::<MENUITEMINFOW>() as u32;
                entry.fMask = MIIM_ID | MIIM_FTYPE | MIIM_STATE | MIIM_DATA;
                assert_ne!(GetMenuItemInfoW(popup.handle, 0, 1, &mut entry), 0);
                assert_eq!(entry.wID, 41);
                assert_ne!(entry.fState & MFS_CHECKED, 0);
                assert_eq!(entry.fType & MFT_OWNERDRAW != 0, !system);
                if system {
                    assert_eq!(entry.dwItemData, 0);
                    continue;
                }
                let row = &popup.rows[0];
                assert_eq!(entry.dwItemData, row.data());
                let name = &*(entry.dwItemData as *const MSAAMENUINFO);
                let name_text = std::slice::from_raw_parts(name.pszWText, name.cchWText as usize);
                assert_eq!(name.dwMSAASignature, MSAA_MENU_SIG as u32);
                assert_eq!(String::from_utf16(name_text).unwrap(), "Word wrap\tAlt+Z");
                assert_ne!(GetMenuItemInfoW(popup.handle, 1, 1, &mut entry), 0);
                assert_ne!(entry.fType & MFT_SEPARATOR, 0);

                ACTIVE.replace(Some(Rc::clone(&popup)));
                assert_eq!(
                    menu_char(popup.handle, b'w' as u16),
                    Some((MNC_EXECUTE << 16) as isize)
                );
                assert_eq!(
                    menu_char(popup.handle, b'S' as u16),
                    Some(((MNC_SELECT << 16) | 2) as isize)
                );
                let mut measured: MEASUREITEMSTRUCT = mem::zeroed();
                measured.CtlType = ODT_MENU;
                measured.itemData = row.data();
                assert!(measure(&mut measured));
                let (w, h) = (measured.itemWidth as i32, measured.itemHeight as i32);
                let dc = CreateCompatibleDC(ptr::null_mut());
                let bitmap = CreateBitmap(w, h, 1, 32, ptr::null());
                assert!(!dc.is_null() && !bitmap.is_null());
                let old = SelectObject(dc, bitmap);
                let mut drawing: DRAWITEMSTRUCT = mem::zeroed();
                drawing.CtlType = ODT_MENU;
                drawing.itemData = row.data();
                drawing.hwndItem = popup.handle;
                drawing.hDC = dc;
                drawing.rcItem = RECT {
                    left: 0,
                    top: 0,
                    right: w,
                    bottom: h,
                };
                for (state, expected) in [
                    (0, popup.palette.raised),
                    (ODS_SELECTED, popup.palette.active),
                ] {
                    drawing.itemState = state | ODS_CHECKED;
                    assert!(draw(&drawing));
                    assert_eq!(GetPixel(dc, 1, 1), expected);
                    let marker = if state == ODS_SELECTED { popup.palette.accent } else { expected };
                    assert_eq!(GetPixel(dc, popup.px(3), h / 2), marker);
                    for (left, right) in [
                        (popup.px(6), popup.px(31)),
                        (popup.px(31), w - popup.shortcut_width - popup.px(14)),
                        (w - popup.shortcut_width - popup.px(14), w),
                    ] {
                        assert!(
                            (left..right).any(|x| (0..h).any(|y| GetPixel(dc, x, y) != expected))
                        );
                    }
                }
                SelectObject(dc, old);
                DeleteObject(bitmap);
                DeleteDC(dc);
                ACTIVE.replace(None);
            }
        }
        LIGHT.set(false);
    }
}
