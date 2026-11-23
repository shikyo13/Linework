use crate::{frame, surface::Surface, win::*};
use std::{
    mem::zeroed,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::*,
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};
use windows_sys::Win32::{
    Graphics::Gdi::*,
    UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT},
};
pub(crate) const FINISH: u32 = WM_APP + 90;
pub(crate) const CONTROL_EVENT: u32 = WM_APP + 91;
const FILTER_CHANGED: u32 = WM_APP + 92;
unsafe extern "system" fn proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    unsafe {
        match m {
            WM_MEASUREITEM if w == 501 => {
                (*(l as *mut MEASUREITEMSTRUCT)).itemHeight = size(h as H, 33) as _;
                1
            }
            WM_DRAWITEM if w == 501 => {
                frame::draw_list(h as H, &*(l as *const DRAWITEMSTRUCT), true);
                1
            }
            WM_DRAWITEM if w == 1 || w == 2 || (811..=816).contains(&w) => {
                let d = &*(l as *const DRAWITEMSTRUCT);
                buffered(d.hDC, d.rcItem, |dc| {
                    let s = Surface::new(dc, dpi(h as H));
                    let p = colors(LIGHT.get());
                    let width = (d.rcItem.right - d.rcItem.left) * 96 / s.scale;
                    let height = (d.rcItem.bottom - d.rcItem.top) * 96 / s.scale;
                    fill(dc, d.rcItem, p.panel);
                    let primary = w == 1;
                    s.round(
                        0,
                        0,
                        width,
                        height,
                        5,
                        if primary || d.itemState & 1 != 0 {
                            p.active
                        } else {
                            p.raised
                        },
                        Some(if d.itemState & 16 != 0 || primary {
                            p.accent
                        } else {
                            p.line
                        }),
                    );
                    s.text(
                        2,
                        0,
                        width - 4,
                        height,
                        &text(d.hwndItem as H),
                        if d.itemState & 4 != 0 { p.dim } else if primary { p.accent } else { p.text },
                        13,
                        550,
                        DT_CENTER,
                    );
                });
                1
            }
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(h, &mut ps);
                let (w, ht) = client(h as H);
                if GetDlgItem(h, 500).is_null() {
                    fill(dc, ps.rcPaint, colors(LIGHT.get()).panel);
                } else {
                    buffered(dc, ps.rcPaint, |dc| {
                        let s = Surface::new(dc, dpi(h as H));
                        let p = colors(LIGHT.get());
                        let (w, ht) = (w * 96 / s.scale, ht * 96 / s.scale);
                        s.fill(0, 0, w + 1, ht + 1, p.panel);
                        s.round(14, 13, w - 28, 40, 7, p.raised, Some(p.accent));
                        s.glyph(19, 14, 30, 38, 0xE721, p.accent, 15);
                        s.fill(14, ht - 53, w - 28, 1, p.line);
                    });
                }
                EndPaint(h, &ps);
                0
            }
            WM_CTLCOLORLISTBOX if GetDlgCtrlID(hwnd(l)) == 501 => {
                ctlcolor(w as _, LIGHT.get(), false)
            }
            WM_CLOSE => {
                PostMessageW(h, FINISH, 0, 0);
                0
            }
            WM_COMMAND if w & 65535 == 1 || w & 65535 == 2 => {
                PostMessageW(h, FINISH, (w & 65535 == 1) as usize, 0);
                0
            }
            WM_COMMAND if w & 65535 == 501 && w >> 16 == LBN_DBLCLK as usize => {
                PostMessageW(h, FINISH, 1, 0);
                0
            }
            WM_COMMAND if w & 65535 == 500 && w >> 16 == EN_CHANGE as usize => {
                PostMessageW(h, FILTER_CHANGED, 0, 0);
                0
            }
            WM_COMMAND if (800..=899).contains(&(w & 65535)) => {
                PostMessageW(h, CONTROL_EVENT, w, l);
                0
            }
            WM_CTLCOLORBTN if GetWindowLongW(hwnd(l), GWL_STYLE) & 0xf == BS_OWNERDRAW => {
                GetStockObject(NULL_BRUSH) as isize
            }
            WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => ctlcolor(
                w as _,
                LIGHT.get(),
                m == WM_CTLCOLOREDIT || m == WM_CTLCOLORLISTBOX,
            ),
            WM_ERASEBKGND => 1,
            _ => DefWindowProcW(h, m, w, l),
        }
    }
}
pub fn init() -> Result<(), String> {
    register("LineworkDialog", Some(proc))
}
pub struct Field<'a> {
    pub label: &'a str,
    pub value: String,
}
pub(crate) fn dialog(owner: H, title: &str, width: i32, height: i32) -> H {
    unsafe {
        let mut r = zeroed();
        GetWindowRect(hwnd(owner), &mut r);
        let (w, ht) = (size(owner, width), size(owner, height));
        let h = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            wide("LineworkDialog").as_ptr(),
            wide(title).as_ptr(),
            WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN,
            r.left + ((r.right - r.left - w) / 2).max(0),
            r.top + ((r.bottom - r.top - ht) / 3).max(0),
            w,
            ht,
            hwnd(owner),
            null_mut(),
            module() as _,
            null(),
        ) as H;
        dark_chrome(h, !LIGHT.get());
        h
    }
}
pub(crate) fn control(
    h: H,
    id: i32,
    class: &str,
    name: &str,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    ht: i32,
) -> H {
    let style = if class == "BUTTON" && (id == 1 || id == 2 || (811..=816).contains(&id)) {
        (style & !0xf) | BS_OWNERDRAW as u32
    } else {
        style
    };
    let c = child(h, id, class, name, style | WS_VISIBLE);
    if class == "LISTBOX" {
        crate::chrome::theme_child(c, LIGHT.get());
        frame::controls::buffer_list(c, id != 501);
    }
    place(c, size(h, x), size(h, y), size(h, w), size(h, ht));
    c
}
fn run(owner: H, h: H, first: H, list: H, mut update: impl FnMut()) -> bool {
    unsafe {
        let previous_focus = focused();
        EnableWindow(hwnd(owner), 0);
        ShowWindow(hwnd(h), SW_SHOW);
        focus(first);
        msg(first, EM_SETSEL_NUM, 0, -1);
        let mut answer = false;
        loop {
            let mut m: MSG = zeroed();
            let n = GetMessageW(&mut m, null_mut(), 0, 0);
            if n <= 0 {
                if n == 0 {
                    PostQuitMessage(m.wParam as i32);
                }
                break;
            }
            if m.hwnd == hwnd(h) && m.message == FINISH {
                if list != 0 && m.wParam != 0 { update(); }
                answer = m.wParam != 0;
                break;
            }
            if m.hwnd == hwnd(h) && m.message == FILTER_CHANGED {
                update();
                continue;
            }
            if m.message == WM_KEYDOWN && (m.hwnd == hwnd(h) || IsChild(hwnd(h), m.hwnd) != 0) {
                let vk = m.wParam as u16;
                if vk == VK_ESCAPE {
                    break;
                }
                if vk == VK_RETURN {
                    if list != 0 { update(); }
                    if (574..=578).contains(&GetDlgCtrlID(m.hwnd))
                        && GetKeyState(VK_CONTROL as i32) >= 0
                    {
                        msg(m.hwnd as H, 0xc2, 1, wide("\r\n").as_ptr() as isize);
                        continue;
                    }
                    answer = GetDlgCtrlID(m.hwnd) != 2;
                    break;
                }
                // The filter box keeps the keyboard while the list is driven
                // from it: by row with the arrows, by page with Page Up/Down.
                if list != 0
                    && matches!(vk, VK_DOWN | VK_UP | VK_NEXT | VK_PRIOR)
                    && m.hwnd == hwnd(first)
                {
                    update();
                    let i = msg(list, LB_GETCURSEL, 0, 0);
                    let max = msg(list, LB_GETCOUNT, 0, 0) - 1;
                    if max >= 0 {
                        let row = msg(list, LB_GETITEMHEIGHT, 0, 0).max(1);
                        let page = (client(list).1 as isize / row - 1).max(1);
                        let step = match vk {
                            VK_DOWN => 1,
                            VK_UP => -1,
                            VK_NEXT => page,
                            _ => -page,
                        };
                        msg(list, LB_SETCURSEL, (i + step).clamp(0, max) as _, 0);
                    }
                    continue;
                }
            }
            if IsDialogMessageW(hwnd(h), &m) == 0 {
                TranslateMessage(&m);
                DispatchMessageW(&m);
            }
        }
        EnableWindow(hwnd(owner), 1);
        SetActiveWindow(hwnd(owner));
        if IsChild(hwnd(owner), hwnd(previous_focus)) != 0
            && IsWindowVisible(hwnd(previous_focus)) != 0
        {
            focus(previous_focus);
        }
        answer
    }
}
const EM_SETSEL_NUM: u32 = 0xb1;
pub fn form(
    owner: H,
    title: &str,
    fields: &[Field],
    checks: &[(&str, bool)],
    ok: &str,
) -> Option<(Vec<String>, Vec<bool>)> {
    let height = 99 + fields.len() as i32 * 66 + checks.len() as i32 * 26;
    let h = dialog(owner, title, 550, height);
    if h == 0 {
        return None;
    }
    let mut ed = Vec::new();
    let mut cb = Vec::new();
    let mut y = 12;
    for (i, f) in fields.iter().enumerate() {
        control(h, 0, "STATIC", f.label, 0, 18, y, 500, 20);
        let c = control(
            h,
            100 + i as i32,
            "EDIT",
            &f.value,
            WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32,
            18,
            y + 23,
            500,
            31,
        );
        ed.push(c);
        y += 66;
    }
    for (i, (lab, val)) in checks.iter().enumerate() {
        let c = control(
            h,
            210 + i as i32,
            "BUTTON",
            lab,
            BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
            18,
            y,
            500,
            24,
        );
        msg(c, BM_SETCHECK, *val as _, 0);
        cb.push(c);
        y += 26;
    }
    let yes = control(
        h,
        1,
        "BUTTON",
        ok,
        BS_DEFPUSHBUTTON as u32 | WS_TABSTOP,
        305,
        y + 8,
        108,
        30,
    );
    control(h, 2, "BUTTON", "Cancel", WS_TABSTOP, 421, y + 8, 97, 30);
    let done = run(owner, h, ed.first().copied().unwrap_or(yes), 0, || {});
    let result = done.then(|| {
        (
            ed.iter().map(|v| text(*v)).collect(),
            cb.iter().map(|v| msg(*v, BM_GETCHECK, 0, 0) != 0).collect(),
        )
    });
    destroy(h);
    result
}
pub fn input(owner: H, title: &str, caption: &str, value: &str) -> Option<String> {
    form(
        owner,
        title,
        &[Field {
            label: caption,
            value: value.into(),
        }],
        &[],
        "Apply",
    )
    .map(|(mut a, _)| a.remove(0))
}
pub(crate) fn multiline_form(
    owner: H,
    title: &str,
    fields: &[Field],
    checks: &[(&str, bool)],
    ok: &str,
) -> Option<(Vec<String>, Vec<bool>)> {
    let height = 99 + fields.len() as i32 * 91 + checks.len() as i32 * 26;
    let h = dialog(owner, title, 550, height);
    if h == 0 {
        return None;
    }
    let mut ed = Vec::new();
    let mut cb = Vec::new();
    let mut y = 12;
    for (i, f) in fields.iter().enumerate() {
        control(h, 0, "STATIC", f.label, 0, 18, y, 500, 20);
        let value = f.value.replace("\r\n", "\n").replace('\n', "\r\n");
        let c = control(
            h,
            575 + i as i32,
            "EDIT",
            &value,
            WS_BORDER
                | WS_TABSTOP
                | WS_VSCROLL
                | (ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32,
            18,
            y + 23,
            500,
            57,
        );
        msg(c, 0xc5, 8 * 1024 * 1024, 0);
        ed.push(c);
        y += 91;
    }
    for (i, (lab, val)) in checks.iter().enumerate() {
        let b = control(
            h,
            210 + i as i32,
            "BUTTON",
            lab,
            BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
            18,
            y,
            500,
            24,
        );
        msg(b, BM_SETCHECK, *val as _, 0);
        cb.push(b);
        y += 26;
    }
    control(
        h,
        1,
        "BUTTON",
        ok,
        WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
        305,
        y + 8,
        108,
        30,
    );
    control(h, 2, "BUTTON", "Cancel", WS_TABSTOP, 421, y + 8, 97, 30);
    let result = run(owner, h, ed[0], 0, || {}).then(|| {
        let values = ed
            .iter()
            .zip(fields)
            .map(|(&h, f)| {
                let value = text(h);
                if value == f.value.replace("\r\n", "\n").replace('\n', "\r\n") {
                    f.value.clone()
                } else {
                    value
                }
            })
            .collect();
        (
            values,
            cb.iter().map(|v| msg(*v, BM_GETCHECK, 0, 0) != 0).collect(),
        )
    });
    destroy(h);
    result
}
pub fn snippet_form(
    owner: H,
    initial: &linework_editor::UserSnippet,
) -> Option<linework_editor::UserSnippet> {
    let h = dialog(owner, "Edit snippet", 620, 490);
    if h == 0 {
        return None;
    }
    control(h, 0, "STATIC", "Name", 0, 16, 12, 280, 20);
    control(h, 0, "STATIC", "Completion trigger", 0, 313, 12, 270, 20);
    let name = control(
        h,
        571,
        "EDIT",
        &initial.name,
        WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32,
        16,
        35,
        282,
        30,
    );
    let trigger = control(
        h,
        572,
        "EDIT",
        &initial.trigger,
        WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32,
        313,
        35,
        268,
        30,
    );
    control(h, 0, "STATIC", "Language", 0, 16, 77, 290, 20);
    let combo = control(
        h,
        573,
        "COMBOBOX",
        "",
        WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32,
        16,
        100,
        565,
        280,
    );
    let mut languages = vec!["All languages".to_owned()];
    languages.extend(linework_editor::languages());
    for s in &languages {
        msg(combo, CB_ADDSTRING, 0, wide(s).as_ptr() as isize);
    }
    let i = languages
        .iter()
        .position(|s| s.eq_ignore_ascii_case(&initial.language))
        .unwrap_or(0);
    msg(combo, CB_SETCURSEL, i, 0);
    control(
        h,
        0,
        "STATIC",
        "Body     $1 and $2 set tab order. $0 is the final caret.",
        0,
        16,
        146,
        560,
        22,
    );
    let body = control(
        h,
        574,
        "EDIT",
        &initial.body.replace("\r\n", "\n").replace('\n', "\r\n"),
        WS_BORDER
            | WS_TABSTOP
            | WS_VSCROLL
            | WS_HSCROLL
            | (ES_MULTILINE | ES_AUTOVSCROLL | ES_AUTOHSCROLL | ES_WANTRETURN) as u32,
        16,
        170,
        565,
        210,
    );
    msg(body, 0xc5, 65536, 0);
    control(
        h,
        0,
        "STATIC",
        "Use numbered placeholders with defaults, such as ${1:name}.",
        0,
        16,
        387,
        570,
        22,
    );
    control(
        h,
        1,
        "BUTTON",
        "Save",
        WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
        370,
        412,
        100,
        30,
    );
    control(h, 2, "BUTTON", "Cancel", WS_TABSTOP, 481, 412, 100, 30);
    let ok = run(owner, h, name, 0, || {});
    let result = ok.then(|| {
        let i = msg(combo, CB_GETCURSEL, 0, 0).max(0) as usize;
        linework_editor::UserSnippet {
            name: text(name).trim().into(),
            trigger: text(trigger).trim().into(),
            language: if i == 0 {
                "*".into()
            } else {
                languages[i].clone()
            },
            body: text(body).replace("\r\n", "\n"),
        }
    });
    destroy(h);
    result
}
fn score(s: &str, q: &str) -> Option<i32> {
    if q.is_empty() {
        return Some(0);
    }
    if let Some(i) = s.find(q) {
        return Some(i as i32);
    }
    let mut pos = 0;
    let mut cost = 100;
    for c in q.chars() {
        let i = s[pos..].find(c)?;
        cost += i as i32;
        pos += i + c.len_utf8();
    }
    Some(cost)
}
pub fn pick(owner: H, title: &str, choices: &[String], query: &str) -> Option<usize> {
    let h = dialog(owner, title, 624, 460);
    if h == 0 {
        return None;
    }
    let edit = control(
        h,
        500,
        "EDIT",
        query,
        WS_TABSTOP | ES_AUTOHSCROLL as u32,
        51,
        23,
        530,
        22,
    );
    msg(edit, 0x1501, 1, wide("Type to filter…").as_ptr() as _);
    let list = control(
        h,
        501,
        "LISTBOX",
        "",
        WS_VSCROLL
            | WS_TABSTOP
            | (LBS_OWNERDRAWFIXED | LBS_HASSTRINGS | LBS_NOTIFY | LBS_NOINTEGRALHEIGHT) as u32,
        14,
        63,
        580,
        296,
    );
    msg(list, LB_SETITEMHEIGHT, 0, size(h, 33) as isize);
    control(
        h,
        502,
        "STATIC",
        "↑ ↓  Navigate     Enter  Choose     Esc  Close",
        0,
        16,
        377,
        330,
        30,
    );
    control(
        h,
        1,
        "BUTTON",
        "Choose",
        WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
        387,
        377,
        97,
        30,
    );
    control(h, 2, "BUTTON", "Cancel", WS_TABSTOP, 497, 377, 97, 30);
    let mut last = None::<String>;
    let mut map = Vec::new();
    let mut folded = Vec::new();
    let mut filter = || {
        let q = text(edit).trim().to_lowercase();
        if last.as_ref() == Some(&q) {
            return;
        }
        let next: Vec<usize> = if q.is_empty() {
            (0..choices.len().min(300)).collect()
        } else {
            if folded.is_empty() {
                folded = choices.iter().map(|s| s.to_lowercase()).collect::<Vec<_>>();
            }
            let mut hits: Vec<_> = folded.iter().enumerate()
                .filter_map(|(i, s)| score(s, &q).map(|k| (k, i))).collect();
            if hits.len() > 300 {
                hits.select_nth_unstable(300);
                hits.truncate(300);
            }
            hits.sort_unstable(); // Original order breaks ties.
            hits.into_iter().map(|(_, i)| i).collect()
        };
        if next != map {
            list_set(list, next.iter().map(|&i| choices[i].as_str()), Some(0));
            map = next;
        }
        last = Some(q);
    };
    filter();
    let ok = run(owner, h, edit, list, filter);
    let result = if ok {
        selected(list).and_then(|i| map.get(i).copied())
    } else {
        None
    };
    destroy(h);
    result
}
