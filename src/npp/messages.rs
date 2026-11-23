use super::*;
use linework_editor::sci::*;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

pub(super) const BASE: u32 = WM_USER + 1000;
const LANGS: &[&str] = &[
    "Plain Text",
    "PHP",
    "C",
    "C++",
    "C#",
    "Objective-C",
    "Java",
    "RC",
    "HTML",
    "XML",
    "Makefile",
    "Pascal",
    "Batch",
    "INI",
    "ASCII",
    "User",
    "ASP",
    "SQL",
    "Visual Basic",
    "JavaScript (embedded)",
    "CSS",
    "Perl",
    "Python",
    "Lua",
    "TeX",
    "Fortran",
    "Shell",
    "Flash",
    "NSIS",
    "Tcl",
    "Lisp",
    "Scheme",
    "Assembly",
    "Diff",
    "Properties",
    "PostScript",
    "Ruby",
    "Smalltalk",
    "VHDL",
    "KiXtart",
    "AutoIt",
    "OCaml",
    "Ada",
    "Verilog",
    "MATLAB",
    "Haskell",
    "Inno Setup",
    "Search result",
    "CMake",
    "YAML",
    "COBOL",
    "Gui4Cli",
    "D",
    "PowerShell",
    "R",
    "JSP",
    "CoffeeScript",
    "JSON",
    "JavaScript",
    "Fortran 77",
    "BaanC",
    "SREC",
    "Intel HEX",
    "Tektronix HEX",
    "Swift",
    "ASN.1",
    "AviSynth",
    "BlitzBasic",
    "PureBasic",
    "FreeBasic",
    "Csound",
    "Erlang",
    "EScript",
    "Forth",
    "LaTeX",
    "MMIXAL",
    "Nim",
    "nnCron",
    "OScript",
    "REBOL",
    "Registry",
    "Rust",
    "SPICE",
    "txt2tags",
    "Visual Prolog",
    "TypeScript",
    "JSON5",
    "MSSQL",
    "GDScript",
    "Hollywood",
    "Go",
    "Raku",
    "TOML",
    "SAS",
    "Error list",
    "Escape sequence",
    "Structured Text",
];
fn lang_id(s: &str) -> isize {
    LANGS
        .iter()
        .position(|n| n.eq_ignore_ascii_case(s))
        .unwrap_or(0) as isize
}
fn current() -> Option<Doc> {
    doc(active_id())
}
fn doc(id: usize) -> Option<Doc> {
    HOST.with_borrow(|h| h.docs.iter().find(|d| d.id == id as u64).cloned())
}
fn doc_at(h: &Host, index: usize, pane: isize) -> Option<&Doc> {
    match pane {
        0 => h.docs.get(index),
        1 if h.split && index == 0 => h.docs.iter().find(|d| Some(d.id) == h.assigned[1]),
        _ => None,
    }
}
fn position(h: &Host, id: usize, pane: isize) -> isize {
    let Some(i) = h.docs.iter().position(|d| d.id == id as u64) else {
        return -1;
    };
    if pane == 1 && h.split && h.assigned[1] == Some(id as u64) {
        1 << 30
    } else {
        i as isize
    }
}
fn app_edit(f: impl FnOnce(&mut App) -> bool) -> isize {
    with_app(|a| {
        let ok = f(a);
        if ok {
            let changes = snapshot(a);
            for (code, id) in changes {
                announce(code, id)
            }
        }
        ok as isize
    })
    .unwrap_or(0)
}
fn str_in(ptr: isize) -> String {
    unsafe { abi::read_string(ptr as *const u16) }
}
fn output_i32(l: isize, v: i32) -> isize {
    if l == 0 {
        return 0;
    }
    unsafe { *(l as *mut i32) = v };
    1
}
fn write(s: &str, l: isize, cap: Option<usize>) -> isize {
    unsafe { abi::write_string(s, l, cap) }
}
fn save(id: usize, p: Option<PathBuf>, copy: bool) -> isize {
    app_edit(|a| a.npp_save(id as u64, p, copy))
}
fn edit_hwnd() -> H {
    HOST.with_borrow(|h| {
        if h.active == 0 {
            h.data.main
        } else {
            h.data.second
        }
    })
}
fn variable(m: u32, w: usize, l: isize) -> isize {
    // Legacy plugins pass zero for the original MAX_PATH-sized output buffers.
    let w = if w == 0 { 260 } else { w };
    let ed = edit_hwnd();
    let pos = msg(ed, SCI_GETCURRENTPOS, 0, 0) as usize;
    if m == 8 {
        return msg(ed, SCI_LINEFROMPOSITION, pos, 0);
    }
    if m == 9 {
        return msg(ed, SCI_GETCOLUMN, pos, 0);
    }
    let path = current().map(|d| d.path).unwrap_or_default();
    let exe = std::env::current_exe().unwrap_or_default();
    let text = match m {
        1 => path.to_string_lossy().into_owned(),
        2 => path
            .parent()
            .unwrap_or(Path::new(""))
            .to_string_lossy()
            .into_owned(),
        3 => path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        4 => path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        5 => path
            .extension()
            .map(|s| format!(".{}", s.to_string_lossy()))
            .unwrap_or_default(),
        6 | 12 => {
            let (a, b) = if m == 6 {
                (
                    msg(ed, SCI_WORDSTARTPOSITION, pos, 1),
                    msg(ed, SCI_WORDENDPOSITION, pos, 1),
                )
            } else {
                let line = msg(ed, SCI_LINEFROMPOSITION, pos, 0) as usize;
                (
                    msg(ed, SCI_POSITIONFROMLINE, line, 0),
                    msg(ed, SCI_GETLINEENDPOSITION, line, 0),
                )
            };
            if l == 0 || b < a || usize::try_from(b - a).unwrap_or(usize::MAX) > w.saturating_mul(4)
            {
                return 0;
            }
            let mut dst = vec![0u8; (b - a) as usize + 1];
            #[repr(C)]
            struct Range {
                start: isize,
                end: isize,
                text: *mut u8,
            }
            let mut r = Range {
                start: a,
                end: b,
                text: dst.as_mut_ptr(),
            };
            msg(ed, SCI_GETTEXTRANGEFULL, 0, (&mut r as *mut Range) as isize);
            String::from_utf8_lossy(&dst[..dst.len() - 1]).into_owned()
        }
        7 => exe
            .parent()
            .unwrap_or(Path::new(""))
            .to_string_lossy()
            .into_owned(),
        10 => exe.to_string_lossy().into_owned(),
        _ => return 0,
    };
    if l == 0 { 0 } else { write(&text, l, Some(w)) }
}
fn alloc(counter: &mut i32, max: i32, n: usize, l: isize) -> isize {
    let start = *counter;
    if l != 0
        && n > 0
        && i32::try_from(n)
            .ok()
            .and_then(|n| start.checked_add(n))
            .is_some_and(|end| end <= max)
    {
        *counter += n as i32;
        output_i32(l, start)
    } else {
        output_i32(l, 0);
        0
    }
}
pub(super) fn handle(m: u32, w: usize, l: isize) -> isize {
    if m > WM_USER + 3000 {
        return variable(m - WM_USER - 3000, w, l);
    }
    let code = m - BASE;
    match code {
        4 => output_i32(l, HOST.with_borrow(|h| h.active as i32)),
        5 => output_i32(l, current().map_or(0, |d| lang_id(&d.language)) as i32),
        6 | 65 => {
            let id = if code == 6 { active_id() } else { w };
            LANGS
                .get(l as usize)
                .map_or(0, |lang| app_edit(|a| a.npp_language(id as u64, lang)))
        }
        7 => HOST.with_borrow(|h| match l {
            0 => h.docs.len() + h.split as usize,
            1 => h.docs.len(),
            2 => h.split as usize,
            _ => 0,
        } as isize),
        8 | 17 | 18 if w != 0 && l > 0 => HOST.with_borrow(|h| {
            let mut names = Vec::new();
            if code != 18 {
                names.extend(h.docs.iter())
            }
            if code != 17 && h.split {
                names.extend(doc_at(h, 0, 1))
            }
            let mut copied = 0;
            for (i, d) in names.into_iter().take(l as usize).enumerate() {
                let dest = unsafe { *(w as *const *mut u16).add(i) };
                if dest.is_null() {
                    break;
                }
                // The deprecated message contract allocates MAX_PATH per element.
                if write(&d.path.to_string_lossy(), dest as isize, Some(260)) == 0 {
                    break;
                }
                copied += 1;
            }
            copied
        }),
        12 if l != 0 => HOST.with_borrow_mut(|h| {
            h.dialogs.retain(|d| *d != l);
            if w == 0 {
                h.dialogs.push(l)
            } else if w != 1 {
                return 0;
            }
            l
        }),
        20 => {
            let host = HOST.with_borrow(|h| h.data.host);
            let ed = child(
                if l == 0 { host } else { l },
                0,
                "Scintilla",
                "",
                WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            );
            if ed != 0 {
                if super::scintilla::install(ed).is_err() {
                    unsafe {
                        DestroyWindow(hwnd(ed));
                    }
                    return 0;
                }
                msg(ed, SCI_SETCODEPAGE, 65001, 0);
            }
            ed
        }
        21 => 1,
        22 => {
            output_i32(l, 0);
            0
        }
        23 => HOST.with_borrow(|h| match l {
            0 => h.assigned[0]
                .and_then(|id| h.docs.iter().position(|d| d.id == id))
                .map_or(-1, |i| i as isize),
            1 if h.split => 0,
            _ => -1,
        }),
        24 if l != 0 => app_edit(|a| {
            a.notice(str_in(l));
            true
        }),
        25 => HOST.with_borrow(|h| match w {
            0 => h.menu,
            1 => h.root,
            _ => 0,
        }),
        28 => {
            let id = HOST.with_borrow(|h| doc_at(h, l as usize, w as isize).map(|d| d.id));
            id.map_or(0, |id| {
                app_edit(|a| {
                    let Some(i) = a.buffers.iter().position(|b| b.id == id) else {
                        return false;
                    };
                    a.select(i, w, None);
                    true
                })
            })
        }
        30 | 31 | 32 | 33 | 35 | 43 => super::docking::message(code, w, l),
        37 | 77 if l != 0 => app_edit(|a| a.npp_open(str_in(l).into(), code == 77)),
        38 => save(active_id(), None, false),
        39 => app_edit(|a| {
            let ids = a
                .buffers
                .iter()
                .filter(|b| b.dirty)
                .map(|b| b.id)
                .collect::<Vec<_>>();
            ids.into_iter().all(|id| a.npp_save(id, None, false))
        }),
        40 => {
            let root = HOST.with_borrow(|h| h.menu);
            let updates = unsafe {
                CheckMenuItem(
                    root as _,
                    w as u32,
                    MF_BYCOMMAND | if l != 0 { MF_CHECKED } else { MF_UNCHECKED },
                )
            };
            (updates != u32::MAX) as isize
        }
        44 => app_edit(|a| {
            let Some(i) = a.current() else { return false };
            a.buffers[i].dirty = true;
            a.buffers[i].forced_dirty = true;
            true
        }),
        46 | 97 | 119 => {
            let dir = match code {
                46 => home().join("Config"),
                97 => home(),
                _ => state::profile(),
            };
            let value = dir.to_string_lossy();
            let copied = write(&value, l, Some(w));
            if code != 46 && l != 0 && copied != 0 {
                value.encode_utf16().count() as isize
            } else {
                copied
            }
        }
        47 if w != 0 => {
            let name = str_in(w as isize);
            let f = HOST.with_borrow(|h| {
                h.plugins
                    .iter()
                    .find(|p| p.file.eq_ignore_ascii_case(&name))
                    .map(|p| p.message)
            });
            f.map_or(0, |f| {
                unsafe { f(m, w, l) };
                1
            })
        }
        48 => {
            if find_command(l as i32).is_some() {
                return app_edit(|a| invoke(a, l as i32));
            }
            let command = match l {
                41001 => "new",
                41002 => "open",
                41003 => "close",
                41006 => "save",
                41007 => "saveall",
                41008 => "saveas",
                42001 => "cut",
                42002 => "copy",
                42003 => "undo",
                42004 => "redo",
                42005 => "paste",
                42007 => "selall",
                43001 => "find",
                43003 => "replace",
                _ => return 0,
            };
            app_edit(|a| {
                a.command(command);
                true
            })
        }
        // ABI version 8.9.9, encoded as described by NPPM_GETNPPVERSION.
        50 => (8 << 16) | if w != 0 { 990 } else { 99 },
        57 => HOST.with_borrow(|h| position(h, w, l)),
        58 => doc(w).map_or(-1, |d| write(&d.path.to_string_lossy(), l, None)),
        59 => HOST.with_borrow(|h| doc_at(h, w, l).map_or(0, |d| d.id as isize)),
        60 => active_id() as isize,
        64 => doc(w).map_or(-1, |d| lang_id(&d.language)),
        66 => doc(w).map_or(-1, |d| d.encoding as isize),
        68 => doc(w).map_or(-1, |d| d.eol as isize),
        76 if l != 0 => HOST.with_borrow(|h| {
            if let Some(a) = h
                .plugins
                .iter()
                .flat_map(|p| &p.actions)
                .find(|a| a.id == w as i32 && a.shortcut.key != 0)
            {
                unsafe { *(l as *mut Shortcut) = a.shortcut };
                1
            } else {
                0
            }
        }),
        78 if l != 0 => save(active_id(), Some(str_in(l).into()), w != 0),
        79 => 65001,
        80 | 87 => 1,
        81 if l != 0 => {
            let v = reserve_commands(w).unwrap_or(0);
            output_i32(l, v);
            (v != 0) as isize
        }
        82 => HOST.with_borrow_mut(|h| alloc(&mut h.marker, 21, w, l)),
        83 | 84 => LANGS.get(w).map_or(0, |s| write(s, l, None)),
        88 => HOST.with_borrow(|h| h.active as isize),
        90 | 91 => msg(
            edit_hwnd(),
            if code == 90 {
                SCI_STYLEGETFORE
            } else {
                SCI_STYLEGETBACK
            },
            STYLE_DEFAULT as _,
            0,
        ),
        94 if l != 0 => {
            let path: PathBuf = str_in(l).into();
            let id = HOST.with_borrow(|h| {
                h.docs
                    .iter()
                    .find(|d| state::key(&d.path) == state::key(&path))
                    .map(|d| d.id)
            });
            id.map_or(0, |id| save(id as usize, None, false))
        }
        96 => HOST.with_borrow_mut(|h| {
            if let Some(a) = h
                .plugins
                .iter_mut()
                .flat_map(|p| &mut p.actions)
                .find(|a| a.id == w as i32)
            {
                a.shortcut = Shortcut::default();
                1
            } else {
                0
            }
        }),
        105 => with_app(|a| a.prefs.auto_indent as isize).unwrap_or(0),
        106 => HOST.with_borrow(|h| h.macro_status),
        107 => HOST.with_borrow(|h| !h.light) as isize,
        108 if w == 12 * 4 && l != 0 => {
            let c = colors(HOST.with_borrow(|h| h.light));
            let colors = [
                c.bg,
                c.panel,
                c.raised,
                c.bg,
                hx(0x612728),
                c.text,
                c.dim,
                c.dim,
                c.accent,
                c.line,
                c.accent,
                c.line,
            ];
            unsafe { std::ptr::copy_nonoverlapping(colors.as_ptr(), l as *mut u32, 12) };
            1
        }
        110 if l != 0 => unsafe {
            let lex = GetModuleHandleW(wide("Lexilla.dll").as_ptr());
            let Some(f) = GetProcAddress(lex, c"CreateLexer".as_ptr().cast()) else {
                return 0;
            };
            let Ok(name) = std::ffi::CString::new(str_in(l)) else {
                return 0;
            };
            let create = std::mem::transmute::<_, unsafe extern "system" fn(*const i8) -> isize>(f);
            create(name.as_ptr())
        },
        111 => 0,
        113 => HOST.with_borrow_mut(|h| alloc(&mut h.indicator, 32, w, l)),
        114 => -1,
        117 => {
            let data = HOST.with_borrow(|h| h.data);
            for ed in [data.main, data.second] {
                let flags = msg(ed, SCI_GETMODEVENTMASK, 0, 0);
                msg(ed, SCI_SETMODEVENTMASK, (flags | l) as usize, 0);
            }
            1
        }
        _ => 0,
    }
}
