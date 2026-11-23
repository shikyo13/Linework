//! Native buttons supply MSAA/UI Automation names, focus and invocation. Their
//! owner-draw surface is the exact same frame used for mouse interaction.
use super::*;
use windows_sys::Win32::UI::{
    Controls::{ODS_FOCUS, ODS_NOFOCUSRECT, ODS_SELECTED, WM_MOUSELEAVE},
    Input::KeyboardAndMouse::*,
    Shell::{DefSubclassProc, GetWindowSubclass, RemoveWindowSubclass, SetWindowSubclass},
};
const FIRST: usize = 4000;
const FRAME_BUTTON: usize = 1;

// LISTBOX can clear its entire client before asking the owner to draw each
// row. Compose the backdrop, active pill and filename before exposing them.
pub(crate) fn buffer_list(h: H, raised: bool) {
    unsafe { SetWindowSubclass(hwnd(h), Some(list_proc), 1, raised as usize); }
}
unsafe fn paint_list(h: HWND, dc: HDC, area: RECT, raised: bool) {
    unsafe {
        buffered(dc, area, |back| {
            let palette = colors(LIGHT.get());
            fill(back, area, if raised { palette.raised } else { palette.panel });
            DefSubclassProc(h, WM_PRINTCLIENT, back as usize, PRF_CLIENT as isize);
        });
    }
}
unsafe extern "system" fn list_proc(h: HWND, m: u32, w: usize, l: isize, id: usize, raised: usize) -> isize {
    unsafe {
        match m {
            WM_ERASEBKGND => return 1,
            WM_PAINT if w == 0 => {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let dc = BeginPaint(h, &mut ps);
                if ps.rcPaint.right > ps.rcPaint.left && ps.rcPaint.bottom > ps.rcPaint.top {
                    paint_list(h, dc, ps.rcPaint, raised != 0);
                }
                EndPaint(h, &ps);
                return 0;
            }
            WM_PRINTCLIENT if w != 0 => {
                let mut area = std::mem::zeroed();
                GetClientRect(h, &mut area);
                paint_list(h, w as HDC, area, raised != 0);
                return 0;
            }
            WM_NCDESTROY => { RemoveWindowSubclass(h, Some(list_proc), id); }
            _ => {}
        }
        DefSubclassProc(h, m, w, l)
    }
}
pub(crate) fn is_face(h: HWND) -> bool {
    let mut data = 0;
    unsafe { GetWindowSubclass(h, Some(button_proc), FRAME_BUTTON, &mut data) != 0 }
}
unsafe extern "system" fn button_proc(
    h: HWND,
    m: u32,
    w: usize,
    l: isize,
    id: usize,
    _: usize,
) -> isize {
    unsafe {
        match m {
            WM_ERASEBKGND => return 1, // WM_DRAWITEM covers the complete button from the frame.
            WM_MOUSEMOVE => {
                let parent = GetParent(h);
                let mut pt = POINT {
                    x: l as i16 as i32,
                    y: (l >> 16) as i16 as i32,
                };
                MapWindowPoints(h, parent, &mut pt, 1);
                hover(parent as H, hit(pt.x, pt.y));
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: h,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut track);
            }
            WM_MOUSELEAVE => hover_at_pointer(GetParent(h) as H),
            WM_MBUTTONDOWN | WM_MBUTTONUP => {
                // A middle click closes the tab it was pressed and released
                // on, in the tab strip and in the list of open editors.
                let parent = GetParent(h);
                let mut pt = POINT {
                    x: l as i16 as i32,
                    y: (l >> 16) as i16 as i32,
                };
                MapWindowPoints(h, parent, &mut pt, 1);
                let tab = match hit(pt.x, pt.y) {
                    Some(Hit::Tab(i) | Hit::Close(i)) => Some(i),
                    _ => None,
                };
                if m == WM_MBUTTONDOWN {
                    MIDDLE.set(tab);
                } else if let Some(i) = MIDDLE.take().filter(|pressed| Some(*pressed) == tab) {
                    crate::events::push(crate::events::Event::Hit(Hit::Close(i), pt.x, pt.y));
                }
                return 0;
            }
            WM_NCDESTROY => {
                RemoveWindowSubclass(h, Some(button_proc), id);
            }
            _ => {}
        }
        DefSubclassProc(h, m, w, l)
    }
}
thread_local! {
    /// The tab under the middle button when it went down.
    static MIDDLE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}
struct Button {
    hwnd: H,
    action: Hit,
    name: String,
    bounds: [i32; 4],
}
thread_local! {
    static BUTTONS:RefCell<Vec<Button>>=const{RefCell::new(Vec::new())};
}
fn label(hit: &Hit, f: &Frame) -> Option<String> {
    Some(match hit {
        Hit::Menu(s) => format!("{s} menu"),
        Hit::Tab(i) => {
            let (name, dirty) = f.tabs.get(*i)?;
            format!(
                "{} tab{}{}",
                name,
                if *dirty { ", unsaved changes" } else { "" },
                if f.active == Some(*i) {
                    ", selected"
                } else {
                    ""
                }
            )
        }
        Hit::Close(i) => format!("Close {}", f.tabs.get(*i)?.0),
        Hit::ScrollTabs(n) => if *n < 0 { "Previous tabs" } else { "Next tabs" }.into(),
        Hit::Cmd(id) => match id.as_str() {
            "docpreview" => if f.document_preview {
                "Close document preview"
            } else {
                "Open document preview"
            }.into(),
            "case" => format!("Match case, {}", on(f.case_)),
            "regex" => format!("Regular expression, {}", on(f.regex)),
            "word" => format!("Whole word, {}", on(f.whole)),
            "scope" => format!(
                "Search scope, {}",
                if f.search_folder {
                    "folder"
                } else if f.selection_scope {
                    "selection"
                } else {
                    "file"
                }
            ),
            "findprev" => "Previous match".into(),
            "findnext" => "Next match".into(),
            "hidefind" => "Close find bar".into(),
            "replaceone" => "Replace next".into(),
            "replaceall" => if f.search_folder {
                "Preview replacements"
            } else {
                "Replace all"
            }
            .into(),
            _ => return None,
        },
        _ => return None,
    })
}
fn on(v: bool) -> &'static str {
    if v { "on" } else { "off" }
}
pub(super) fn stage(parent: H, damage: RECT, layout: &mut WindowLayout) {
    let hits = HITS.with_borrow(|hits| {
        FRAME.with_borrow(|f| {
            hits.iter()
                .filter_map(|(r, hit)| {
                    let bounds = [r.left, r.top, r.right, r.bottom];
                    label(hit, f).map(|name| (hit.clone(), name, bounds))
                })
                .collect::<Vec<_>>()
        })
    });
    // Native creation and window positioning can synchronously request redraw.
    // Keep the item registry borrow out of all SendMessage calls.
    let mut buttons = BUTTONS.take();
    while buttons.len() > hits.len() {
        destroy(buttons.pop().unwrap().hwnd);
    }
    for (i, (action, name, bounds)) in hits.into_iter().enumerate() {
        if i == buttons.len() {
            let h = child(
                parent,
                (FIRST + i) as i32,
                "BUTTON",
                &name,
                WS_TABSTOP | BS_OWNERDRAW as u32,
            );
            unsafe {
                SetWindowSubclass(hwnd(h), Some(button_proc), FRAME_BUTTON, 0);
            }
            buttons.push(Button {
                hwnd: h,
                action: action.clone(),
                name: name.clone(),
                bounds: [0; 4],
            });
        }
        let b = &mut buttons[i];
        b.action = action;
        if b.name != name {
            set_text(b.hwnd, &name);
            b.name = name;
        }
        let moved = b.bounds != bounds;
        if moved {
            b.bounds = bounds;
            let [l, t, r, z] = bounds;
            layout.set(b.hwnd, Some(rect(l, t, r - l, z - t)));
        }
        if moved
            || (bounds[0] < damage.right
                && bounds[2] > damage.left
                && bounds[1] < damage.bottom
                && bounds[3] > damage.top)
        {
            layout.redraw(b.hwnd);
        }
    }
    BUTTONS.replace(buttons);
}
pub(crate) fn release() {
    BUTTONS.take();
    rendering::reset();
}
pub(super) fn paint_pending() {
    // Let editor painting follow Windows' normal queue. A slow editor paint
    // must not hold the next divider input behind a synchronous tree redraw.
    let children =
        BUTTONS.with_borrow(|buttons| buttons.iter().map(|b| b.hwnd).collect::<Vec<_>>());
    for h in children {
        unsafe { UpdateWindow(hwnd(h)); }
    }
}
pub(crate) fn draw(id: usize, d: &DRAWITEMSTRUCT) -> bool {
    if id < FIRST || !is_face(d.hwndItem) {
        return false;
    }
    unsafe {
        let mut from = POINT {
            x: d.rcItem.left,
            y: d.rcItem.top,
        };
        MapWindowPoints(d.hwndItem, GetParent(d.hwndItem), &mut from, 1);
        if !rendering::copy(d.hDC, d.rcItem, from.x, from.y) {
            fill(d.hDC, d.rcItem, colors(LIGHT.get()).panel);
        }
        if d.itemState & ODS_SELECTED != 0 {
            let mut r = d.rcItem;
            r.left += size(d.hwndItem as H, 4);
            r.right -= size(d.hwndItem as H, 4);
            r.bottom -= size(d.hwndItem as H, 2);
            r.top = r.bottom - size(d.hwndItem as H, 2).max(1);
            fill(d.hDC, r, colors(LIGHT.get()).accent);
        }
        if d.itemState & ODS_FOCUS != 0 && d.itemState & ODS_NOFOCUSRECT == 0 {
            let mut r = d.rcItem;
            InflateRect(&mut r, -2, -2);
            DrawFocusRect(d.hDC, &r);
        }
    }
    true
}
pub(crate) fn invoke(id: usize, sender: H) -> bool {
    let Some(i) = id.checked_sub(FIRST) else {
        return false;
    };
    BUTTONS.with_borrow(|buttons| {
        let Some(b) = buttons.get(i) else {
            return false;
        };
        if b.hwnd != sender {
            return false;
        }
        crate::events::push(crate::events::Event::Hit(
            b.action.clone(),
            b.bounds[0],
            b.bounds[3],
        ));
        true
    })
}
pub(crate) fn key(key: u16, shift: bool, editor: H) -> bool {
    if !matches!(key, VK_F10 | VK_TAB | VK_LEFT | VK_RIGHT | VK_RETURN | VK_DOWN | VK_ESCAPE) {
        return false;
    }
    let buttons = BUTTONS.with_borrow(|v| v.iter().map(|b| b.hwnd).collect::<Vec<_>>());
    if buttons.is_empty() {
        return false;
    }
    if key == VK_F10 && !shift {
        focus(buttons[0]);
        return true;
    }
    let focused = focused();
    let Some(i) = buttons.iter().position(|&h| h == focused) else {
        return false;
    };
    match key {
        VK_TAB | VK_LEFT | VK_RIGHT => {
            let back = key == VK_LEFT || key == VK_TAB && shift;
            focus(
                buttons[if back {
                    (i + buttons.len() - 1) % buttons.len()
                } else {
                    (i + 1) % buttons.len()
                }],
            );
            true
        }
        VK_RETURN | VK_DOWN => invoke(FIRST + i, buttons[i]),
        VK_ESCAPE => {
            focus(editor);
            true
        }
        _ => false,
    }
}
