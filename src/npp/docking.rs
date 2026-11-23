use super::*;
#[repr(C)]
struct Widget {
    client: H,
    title: *const u16,
    id: i32,
    mask: u32,
    icon: H,
    info: *const u16,
    rect: RECT,
    container: i32,
    module: *const u16,
}
unsafe extern "system" fn proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    unsafe {
        let child = GetWindowLongPtrW(h, GWLP_USERDATA) as HWND;
        match m {
            WM_SIZE if !child.is_null() => {
                let mut r: RECT = std::mem::zeroed();
                GetClientRect(h, &mut r);
                MoveWindow(child, 0, 0, r.right, r.bottom, 1);
                0
            }
            WM_CLOSE => {
                ShowWindow(h, SW_HIDE);
                0
            }
            WM_SETFOCUS if !child.is_null() => {
                SetFocus(child);
                0
            }
            _ => DefWindowProcW(h, m, w, l),
        }
    }
}
pub(super) fn init() {
    let _ = register("LineworkPluginPanel", Some(proc));
}
fn tool(client: H) -> Option<H> {
    let found = HOST.with_borrow(|h| h.tools.iter().any(|x| x.0 == client));
    if !found {
        return None;
    }
    let parent = unsafe { GetParent(hwnd(client)) } as H;
    (parent != 0).then_some(parent)
}
pub(super) fn message(code: u32, w: usize, l: isize) -> isize {
    match code {
        33 if l != 0 => unsafe {
            let data = &*(l as *const Widget);
            if data.client == 0 || IsWindow(hwnd(data.client)) == 0 {
                return 0;
            }
            if tool(data.client).is_some() {
                return 1;
            }
            let title = abi::read_string(data.title);
            let module_name = abi::read_string(data.module);
            let owner = HOST.with_borrow(|h| h.data.host);
            let mut rect: RECT = std::mem::zeroed();
            GetWindowRect(hwnd(data.client), &mut rect);
            let width = (rect.right - rect.left).max(size(owner, 300));
            let height = (rect.bottom - rect.top).max(size(owner, 210));
            let mut anchor: RECT = std::mem::zeroed();
            GetWindowRect(hwnd(owner), &mut anchor);
            let frame = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                wide("LineworkPluginPanel").as_ptr(),
                wide(&title).as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
                (anchor.right - width - 24).max(anchor.left),
                anchor.top + 54,
                width,
                height,
                hwnd(owner),
                null_mut(),
                module() as _,
                null(),
            );
            if frame.is_null() {
                return 0;
            }
            let child = hwnd(data.client);
            SetWindowLongPtrW(
                child,
                GWL_STYLE,
                ((GetWindowLongPtrW(child, GWL_STYLE) as u32
                    & !(WS_POPUP | WS_CAPTION | WS_THICKFRAME))
                    | WS_CHILD) as isize,
            );
            SetParent(child, frame);
            SetWindowLongPtrW(frame, GWLP_USERDATA, data.client);
            GetClientRect(frame, &mut rect);
            SetWindowPos(
                child,
                null_mut(),
                0,
                0,
                rect.right,
                rect.bottom,
                SWP_NOZORDER | SWP_FRAMECHANGED | SWP_SHOWWINDOW,
            );
            HOST.with_borrow_mut(|h| {
                h.tools.push((data.client, title, module_name));
                if !h.dialogs.contains(&data.client) {
                    h.dialogs.push(data.client)
                }
            });
            ShowWindow(frame, SW_SHOWNOACTIVATE);
            1
        },
        30 | 31 => tool(l).map_or(0, |f| {
            unsafe { ShowWindow(hwnd(f), if code == 30 { SW_SHOW } else { SW_HIDE }) };
            1
        }),
        32 => tool(l).map_or(0, |f| {
            unsafe { windows_sys::Win32::Graphics::Gdi::InvalidateRect(hwnd(f), null(), 1) };
            1
        }),
        35 if l != 0 => {
            let name = unsafe { abi::read_string(l as _) };
            let found = HOST.with_borrow(|h| {
                h.tools
                    .iter()
                    .find(|x| x.1.eq_ignore_ascii_case(&name))
                    .map(|x| x.0)
            });
            found.map_or(0, |c| message(30, 0, c))
        }
        43 if l != 0 => {
            let name = unsafe { abi::read_string(w as _) };
            let module = unsafe { abi::read_string(l as _) };
            HOST.with_borrow(|h| {
                h.tools
                    .iter()
                    .find(|x| {
                        (w == 0 || x.1.eq_ignore_ascii_case(&name))
                            && x.2.eq_ignore_ascii_case(&module)
                    })
                    .map_or(0, |x| x.0)
            })
        }
        _ => 0,
    }
}
