use crate::{app::App, commands, forms, macros, sci, state::Step, win::*};
use std::collections::BTreeSet;
use windows_sys::Win32::UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*};

const PAGE: usize = 250;
const LIST: usize = 800;
const EDIT: usize = 801;
const ADD: usize = 802;
const UP: usize = 803;
const DOWN: usize = 804;
const REMOVE: usize = 805;
const PREVIOUS: usize = 806;
const NEXT: usize = 807;
const DELETE: usize = 808;
const SAVE: usize = 809;

fn preview(s: &str) -> String {
    let mut chars = s.chars();
    let mut result: String = chars.by_ref().take(90).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
        .replace('\r', "␍")
        .replace('\n', "␊")
        .replace('\t', "⇥")
        .replace('\0', "␀")
}
fn step_label(s: &Step) -> String {
    match s {
        Step::Command(id) => commands::label(id).to_owned(),
        Step::Find {
            query,
            replace,
            regex,
            back,
            selection,
            ..
        } => {
            let action = if replace.is_some() {
                "Replace"
            } else if *back {
                "Find previous"
            } else {
                "Find next"
            };
            let scope = if *selection { " in selection" } else { "" };
            let mode = if *regex { " regex" } else { "" };
            let after = replace
                .as_ref()
                .map(|s| format!(" → {}", preview(s)))
                .unwrap_or_default();
            format!("{action}{mode}{scope}    {}{after}", preview(query))
        }
        Step::Native { msg, arg, text } => {
            let name = native_name(*msg);
            if let Some(text) = text {
                format!("{name}    {}", preview(text))
            } else if matches!(*msg, sci::GOTOLINE | sci::GOTOPOS | sci::SETSELECTIONMODE) {
                format!("{name}    {arg}")
            } else {
                name.to_owned()
            }
        }
    }
}
fn edit_step(owner: H, step: &Step) -> Option<Step> {
    if let Step::Find {
        query,
        replace,
        regex,
        case,
        whole,
        back,
        wrap,
        selection,
    } = step
    {
        let (v, f) = forms::multiline_form(
            owner,
            "Edit search step",
            &[
                forms::Field {
                    label: "Find",
                    value: query.clone(),
                },
                forms::Field {
                    label: "Replace with",
                    value: replace.clone().unwrap_or_default(),
                },
            ],
            &[
                ("Replace the match", replace.is_some()),
                ("Regular expression", *regex),
                ("Match case", *case),
                ("Whole word", *whole),
                ("Search backwards", *back),
                ("Wrap at the end", *wrap),
                ("Limit to selection", *selection),
            ],
            "Apply",
        )?;
        let value = Step::Find {
            query: v[0].clone(),
            replace: f[0].then(|| v[1].clone()),
            regex: f[1],
            case: f[2],
            whole: f[3],
            back: f[4],
            wrap: f[5],
            selection: f[6],
        };
        if let Err(e) = macros::validate(&value) {
            alert(owner, "Edit search step", &e);
            return None;
        }
        if f[1] {
            if let Err(e) = regex::bytes::RegexBuilder::new(&v[0])
                .size_limit(16 * 1024 * 1024)
                .build()
            {
                alert(owner, "Invalid regular expression", &e.to_string());
                return None;
            }
        }
        return Some(value);
    }
    if let Step::Native {
        msg,
        arg,
        text: Some(text),
    } = step
    {
        if text.contains('\0') {
            alert(
                owner,
                "Edit macro step",
                "This recorded step contains a NUL byte. You can move or remove the step.",
            );
            return None;
        }
        let (v, _) = forms::multiline_form(
            owner,
            "Edit step text",
            &[forms::Field {
                label: "Recorded text",
                value: text.clone(),
            }],
            &[],
            "Apply",
        )?;
        let value = Step::Native {
            msg: *msg,
            arg: if matches!(*msg, sci::ADDTEXT | sci::APPENDTEXT) {
                v[0].len()
            } else {
                *arg
            },
            text: Some(v[0].clone()),
        };
        if let Err(e) = macros::validate(&value) {
            alert(owner, "Edit macro step", &e);
            return None;
        }
        return Some(value);
    }
    None
}
fn blank_search() -> Step {
    Step::Find {
        query: String::new(),
        replace: None,
        regex: false,
        case: false,
        whole: false,
        back: false,
        wrap: true,
        selection: false,
    }
}
enum Outcome {
    Cancel,
    Delete,
    Save(String, Vec<Step>),
}
struct Panel {
    window: H,
    name: H,
    list: H,
    page: H,
    edit: H,
    up: H,
    down: H,
    remove: H,
    prev: H,
    next: H,
}
impl Panel {
    fn new(owner: H, name: &str, saved: bool) -> Option<Self> {
        let h = forms::dialog(owner, "Manage macro", 724, 497);
        if h == 0 {
            return None;
        }
        let ctl = |id: usize, class: &str, label: &str, style, x, y, w, ht| {
            forms::control(h, id as i32, class, label, style, x, y, w, ht)
        };
        ctl(0, "STATIC", "Macro name", 0, 16, 12, 660, 20);
        let name = ctl(
            711,
            "EDIT",
            name,
            WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32,
            16,
            36,
            674,
            30,
        );
        let page = ctl(0, "STATIC", "Steps", 0, 16, 83, 380, 24);
        let button = |id, caption, x, y, w| ctl(id, "BUTTON", caption, WS_TABSTOP, x, y, w, 29);
        let prev = button(PREVIOUS, "Previous page", 445, 77, 119);
        let next = button(NEXT, "Next page", 574, 77, 116);
        let list = ctl(
            LIST,
            "LISTBOX",
            "",
            WS_BORDER | WS_VSCROLL | WS_TABSTOP | LBS_NOTIFY as u32 | LBS_NOINTEGRALHEIGHT as u32,
            16,
            115,
            674,
            213,
        );
        let edit = button(EDIT, "Edit step…", 16, 339, 106);
        button(ADD, "Add search…", 130, 339, 119);
        let up = button(UP, "Move up", 257, 339, 94);
        let down = button(DOWN, "Move down", 359, 339, 109);
        let remove = button(REMOVE, "Remove step", 476, 339, 123);
        if saved {
            button(DELETE, "Delete macro…", 16, 410, 145);
        }
        ctl(
            SAVE,
            "BUTTON",
            "Save",
            WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
            475,
            410,
            102,
            30,
        );
        button(2, "Cancel", 588, 410, 102);
        Some(Self {
            window: h,
            name,
            list,
            page,
            edit,
            up,
            down,
            remove,
            prev,
            next,
        })
    }
    fn update(&self, steps: &[Step], selected: usize) {
        let selected = selected.min(steps.len().saturating_sub(1));
        let base = selected / PAGE * PAGE;
        let until = (base + PAGE).min(steps.len());
        let labels: Vec<_> = steps[base..until]
            .iter()
            .enumerate()
            .map(|(j, s)| format!("{:>3}    {}", base + j + 1, step_label(s)))
            .collect();
        list_set(self.list, labels.iter().map(String::as_str),
            (!steps.is_empty()).then_some(selected - base));
        let caption = if steps.len() > PAGE {
            format!("Steps {}–{} of {}", base + 1, until, steps.len())
        } else {
            format!(
                "{} {}",
                steps.len(),
                if steps.len() == 1 { "step" } else { "steps" }
            )
        };
        set_text(self.page, &caption);
        show(self.prev, steps.len() > PAGE);
        show(self.next, steps.len() > PAGE);
        unsafe {
            EnableWindow(hwnd(self.prev), (base > 0) as i32);
            EnableWindow(hwnd(self.next), (until < steps.len()) as i32);
        }
        self.buttons(steps, selected);
    }
    fn buttons(&self, steps: &[Step], sel: usize) {
        let editable = matches!(
            steps.get(sel),
            Some(Step::Find { .. } | Step::Native { text: Some(_), .. })
        );
        for (h, on) in [
            (self.edit, editable),
            (self.up, sel > 0),
            (self.down, sel + 1 < steps.len()),
            (self.remove, !steps.is_empty()),
        ] {
            unsafe {
                EnableWindow(hwnd(h), on as i32);
            }
        }
    }
}
fn inspect(
    owner: H,
    orig: Option<&str>,
    mut steps: Vec<Step>,
    names: &BTreeSet<String>,
) -> Outcome {
    let Some(panel) = Panel::new(owner, orig.unwrap_or(""), orig.is_some()) else {
        return Outcome::Cancel;
    };
    let h = panel.window;
    let mut selected_index = 0;
    panel.update(&steps, 0);
    unsafe {
        crate::win::show_popup(h, owner);
        EnableWindow(hwnd(owner), 0);
    }
    focus(if steps.is_empty() {
        panel.name
    } else {
        panel.list
    });
    let outcome = loop {
        let mut m: MSG = unsafe { std::mem::zeroed() };
        let result = unsafe { GetMessageW(&mut m, std::ptr::null_mut(), 0, 0) };
        if result <= 0 {
            if result == 0 {
                unsafe {
                    PostQuitMessage(m.wParam as i32);
                }
            }
            break Outcome::Cancel;
        }
        let within = m.hwnd == hwnd(h) || unsafe { IsChild(hwnd(h), m.hwnd) } != 0;
        let mut action = 0;
        if m.hwnd == hwnd(h) && m.message == forms::FINISH {
            break Outcome::Cancel;
        }
        if m.hwnd == hwnd(h) && m.message == forms::CONTROL_EVENT {
            let id = m.wParam & 65535;
            if id == LIST {
                selected_index =
                    selected_index / PAGE * PAGE + crate::win::selected(panel.list).unwrap_or(0);
                panel.buttons(&steps, selected_index);
                if m.wParam >> 16 == LBN_DBLCLK as usize {
                    action = EDIT;
                }
            } else {
                action = id;
            }
        } else if within && m.message == WM_KEYDOWN {
            let vk = m.wParam as u16;
            if vk == VK_ESCAPE {
                break Outcome::Cancel;
            }
            if vk == 0x53 && unsafe { GetKeyState(VK_CONTROL as i32) } < 0 {
                action = SAVE;
            }
            if vk == VK_RETURN && m.hwnd != hwnd(panel.list) {
                let id = unsafe { GetDlgCtrlID(m.hwnd) } as usize;
                if id == 2 {
                    break Outcome::Cancel;
                }
                action = if (EDIT..=SAVE).contains(&id) {
                    id
                } else {
                    SAVE
                };
            } else if m.hwnd == hwnd(panel.list) {
                if vk == VK_RETURN {
                    action = EDIT;
                }
                if vk == VK_DELETE {
                    action = REMOVE;
                }
            }
        }
        if action == 0 {
            if unsafe { IsDialogMessageW(hwnd(h), &m) } == 0 {
                unsafe {
                    TranslateMessage(&m);
                    DispatchMessageW(&m);
                }
            }
            continue;
        }
        match action {
            EDIT => {
                if let Some(step) = steps.get(selected_index).and_then(|s| edit_step(h, s)) {
                    steps[selected_index] = step;
                }
            }
            ADD => {
                if steps.len() >= 100_000 {
                    alert(
                        h,
                        "Manage macro",
                        "A macro can contain up to 100,000 steps.",
                    );
                    continue;
                }
                if let Some(s) = edit_step(h, &blank_search()) {
                    selected_index = (selected_index + 1).min(steps.len());
                    steps.insert(selected_index, s);
                }
            }
            UP if selected_index > 0 => {
                steps.swap(selected_index, selected_index - 1);
                selected_index -= 1;
            }
            DOWN if selected_index + 1 < steps.len() => {
                steps.swap(selected_index, selected_index + 1);
                selected_index += 1;
            }
            REMOVE if !steps.is_empty() => {
                steps.remove(selected_index);
                selected_index = selected_index.min(steps.len().saturating_sub(1));
            }
            PREVIOUS => {
                selected_index = selected_index.saturating_sub(PAGE);
            }
            NEXT => {
                selected_index = (selected_index + PAGE).min(steps.len().saturating_sub(1));
            }
            SAVE => {
                let name = text(panel.name).trim().to_owned();
                if name.len() > 160 || name.is_empty() {
                    alert(h, "Manage macro", "Enter a name of 1–160 bytes.");
                    continue;
                }
                if steps.is_empty() {
                    alert(h, "Manage macro", "Add at least one step before saving.");
                    continue;
                }
                if let Err(err) = steps.iter().try_for_each(macros::validate) {
                    alert(h, "Manage macro", &err);
                    continue;
                }
                if orig != Some(name.as_str())
                    && names.contains(&name)
                    && confirm(
                        h,
                        "Replace macro?",
                        &format!("Replace the saved macro “{name}”?"),
                        false,
                    ) != IDYES
                {
                    continue;
                }
                break Outcome::Save(name, steps);
            }
            DELETE if orig.is_some() => {
                if confirm(
                    h,
                    "Delete macro?",
                    &format!("Delete “{}” from the saved macro library?", orig.unwrap()),
                    false,
                ) == IDYES
                {
                    break Outcome::Delete;
                }
            }
            _ => {}
        }
        panel.update(&steps, selected_index);
        focus(panel.list);
    };
    unsafe {
        EnableWindow(hwnd(owner), 1);
        // Back to the owner only when the popup had the keyboard.
        if GetActiveWindow() == hwnd(h) {
            SetActiveWindow(hwnd(owner));
        }
    }
    destroy(h);
    outcome
}
impl App {
    pub(crate) fn manage_macros(&mut self) {
        if self.recording || self.playing {
            self.notice("Stop recording or replaying before editing a macro");
            return;
        }
        let names: BTreeSet<_> = self.prefs.macros.keys().cloned().collect();
        let mut choices = vec![if self.macro_steps.is_empty() {
            "Create a macro…".into()
        } else {
            "Edit current recording…".into()
        }];
        choices.extend(
            names
                .iter()
                .map(|s| format!("{}    {} steps", s, self.prefs.macros[s].len())),
        );
        let Some(i) = forms::pick(self.h, "Macro library", &choices, "") else {
            return;
        };
        let orig = i.checked_sub(1).and_then(|n| names.iter().nth(n).cloned());
        let steps = orig
            .as_ref()
            .map(|name| self.prefs.macros[name].clone())
            .unwrap_or_else(|| self.macro_steps.clone());
        match inspect(self.h, orig.as_deref(), steps, &names) {
            Outcome::Cancel => {}
            Outcome::Delete => {
                if let Some(name) = orig {
                    self.prefs.macros.remove(&name);
                    self.save_prefs();
                    self.notice(format!("Deleted saved macro “{name}”"));
                }
            }
            Outcome::Save(name, steps) => {
                if let Some(old) = orig.filter(|s| s != &name) {
                    self.prefs.macros.remove(&old);
                }
                self.prefs.macros.insert(name.clone(), steps.clone());
                self.prefs.last_macro = steps.clone();
                self.macro_steps = steps;
                self.save_prefs();
                self.notice(format!("Saved “{name}”"));
            }
        }
    }
}

// Command names follow the Scintilla interface used by the recorder.
fn native_name(msg: u32) -> &'static str {
    match msg {
        2001 => "Insert text",
        2003 => "Insert text at position",
        2004 => "Clear all",
        2013 => "Select all",
        2024 => "Go to line",
        2025 => "Go to byte",
        2170 => "Type text",
        2177 => "Cut",
        2178 => "Copy",
        2179 => "Paste",
        2180 => "Clear",
        2282 => "Append text",
        2300 => "Line down",
        2301 => "Line down  (extend selection)",
        2302 => "Line up",
        2303 => "Line up  (extend selection)",
        2304 => "Char left",
        2305 => "Char left  (extend selection)",
        2306 => "Char right",
        2307 => "Char right  (extend selection)",
        2308 => "Word left",
        2309 => "Word left  (extend selection)",
        2310 => "Word right",
        2311 => "Word right  (extend selection)",
        2312 => "Home",
        2313 => "Home  (extend selection)",
        2314 => "Line end",
        2315 => "Line end  (extend selection)",
        2316 => "Document start",
        2317 => "Document start  (extend selection)",
        2318 => "Document end",
        2319 => "Document end  (extend selection)",
        2320 => "Page up",
        2321 => "Page up  (extend selection)",
        2322 => "Page down",
        2323 => "Page down  (extend selection)",
        2324 => "Edit toggle overtype",
        2325 => "Cancel",
        2326 => "Delete back",
        2327 => "Tab",
        2328 => "Back tab",
        2329 => "Insert line break",
        2330 => "Form feed",
        2331 => "First nonblank column",
        2332 => "First nonblank column  (extend selection)",
        2335 => "Del word left",
        2336 => "Del word right",
        2337 => "Line cut",
        2338 => "Line delete",
        2339 => "Line transpose",
        2340 => "Lower case",
        2341 => "Upper case",
        2342 => "Line scroll down",
        2343 => "Line scroll up",
        2344 => "Duplicate line",
        2345 => "Home display",
        2346 => "Home display  (extend selection)",
        2347 => "Line end display",
        2348 => "Line end display  (extend selection)",
        2349 => "Home wrap",
        2354 => "Line reverse",
        2366 => "Search anchor",
        2367 => "Native find next",
        2368 => "Native find previous",
        2390 => "Word part left",
        2391 => "Word part left  (extend selection)",
        2392 => "Word part right",
        2393 => "Word part right  (extend selection)",
        2395 => "Del line left",
        2396 => "Del line right",
        2404 => "Line duplicate",
        2413 => "Para down",
        2414 => "Para down  (extend selection)",
        2415 => "Para up",
        2416 => "Para up  (extend selection)",
        2422 => "Set selection mode",
        2426 => "Line down  (rectangle)",
        2427 => "Line up  (rectangle)",
        2428 => "Char left  (rectangle)",
        2429 => "Char right  (rectangle)",
        2430 => "Home  (rectangle)",
        2431 => "First nonblank column  (rectangle)",
        2432 => "Line end  (rectangle)",
        2433 => "Page up  (rectangle)",
        2434 => "Page down  (rectangle)",
        2435 => "Stuttered page up",
        2436 => "Stuttered page up  (extend selection)",
        2437 => "Stuttered page down",
        2438 => "Stuttered page down  (extend selection)",
        2439 => "Word left end",
        2440 => "Word left end  (extend selection)",
        2441 => "Word right end",
        2442 => "Word right end  (extend selection)",
        2450 => "Home wrap  (extend selection)",
        2451 => "Line end wrap",
        2452 => "Line end wrap  (extend selection)",
        2453 => "First nonblank column wrap",
        2454 => "First nonblank column wrap  (extend selection)",
        2455 => "Line copy",
        2469 => "Selection duplicate",
        2518 => "Del word right end",
        2519 => "Copy allow line",
        2619 => "Vertical centre caret",
        2620 => "Move selected lines up",
        2621 => "Move selected lines down",
        2628 => "Scroll to start",
        2629 => "Scroll to end",
        2652 => "First nonblank column display",
        2653 => "First nonblank column display  (extend selection)",
        2810 => "Cut allow line",
        2813 => "Line indent",
        2814 => "Line dedent",
        _ => "Native editing command",
    }
}
