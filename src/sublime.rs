#[path = "sublime_paged.rs"]
mod paged;
use crate::{
    app::App,
    dialogs, forms, state,
    win::{alert, confirm},
};
use linework_editor::sci::{SCI_ADDSELECTION, SCI_SETSELECTION};
use serde::Deserialize;
use serde_json::{Value, json};
use crate::os_windows::process::CommandExt;
use std::{
    cell::RefCell,
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Deserialize, Default)]
#[serde(default)]
struct Reply {
    error: String,
    errors: Vec<String>,
    packages: Vec<String>,
    listeners: Vec<String>,
    commands: Vec<Entry>,
    installed: String,
    removed: String,
    edits: Vec<(usize, usize, String)>,
    regions: Vec<(usize, usize)>,
    messages: Vec<String>,
    dialog: Option<Dialog>,
    reveal: Option<usize>,
    timers: bool,
    delay: Option<u64>,
}
#[derive(Clone, Deserialize)]
struct Entry {
    label: String,
    name: String,
    args: Value,
}
#[derive(Deserialize)]
struct Dialog {
    kind: String,
    title: String,
    initial: String,
    items: Vec<String>,
    token: u64,
}
#[derive(Clone, PartialEq)]
struct Stamp {
    id: u64,
    revision: u64,
    regions: Vec<(usize, usize)>,
    dirty: bool,
    pane: usize,
}
struct Pending {
    operation: String,
    stamp: Option<Stamp>,
}
struct Request {
    value: Value,
    txn: Option<paged::Transaction>,
}
struct WakeEvent(isize);
impl Drop for WakeEvent {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0 as _);
        }
    }
}
struct Host {
    wake: WakeEvent,
    input: Sender<Request>,
    output: Receiver<Result<(Reply, Option<paged::Commit>), String>>,
    cancelled: Arc<AtomicBool>,
    waiting_for_python: Arc<Mutex<Option<Instant>>>,
    child: Arc<Mutex<Child>>,
}
impl Drop for Host {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}
fn runtime() -> PathBuf {
    let executable = std::env::current_exe().unwrap_or_default();
    let beside = executable
        .parent()
        .unwrap_or(Path::new("."))
        .join("runtime");
    if beside.join("sublime/host.py").is_file() {
        return beside;
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime")
}
fn start() -> Result<Host, String> {
    let runtime = runtime();
    let event_name = format!(
        "Local\\Linework-timers-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let event = unsafe {
        windows_sys::Win32::System::Threading::CreateEventW(
            std::ptr::null(),
            0,
            0,
            crate::win::wide(&event_name).as_ptr(),
        )
    };
    if event.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let wake = WakeEvent(event as isize);
    let log =
        fs::File::create(state::profile().join(format!("sublime-host-{}.log", std::process::id())))
            .map_err(|e| e.to_string())?;
    let mut child = Command::new(runtime.join("python/python.exe"))
        .args(["-I", "-B", "-X", "utf8", "-u"])
        .arg(runtime.join("sublime/host.py"))
        .arg(state::profile())
        .arg(event_name)
        .creation_flags(0x08000000)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|e| format!("Starting package host: {e}"))?;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let (tx, rx) = mpsc::channel::<Request>();
    let (results, output) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let waiting_for_python = Arc::new(Mutex::new(None));
    let wait_state = waiting_for_python.clone();
    thread::spawn(move || {
        use windows_sys::Win32::System::Com::{
            COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize,
        };
        let com = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) } >= 0;
        fn send(out: &mut impl Write, value: &Value) -> Result<(), String> {
            serde_json::to_writer(&mut *out, value).map_err(|e| e.to_string())?;
            out.write_all(b"\n")
                .and_then(|_| out.flush())
                .map_err(|e| e.to_string())
        }
        for mut request in rx {
            if let Some(txn) = &request.txn {
                if let Err(e) = txn.prepare(&mut request.value) {
                    let _ = results.send(Ok((
                        Reply {
                            error: e.to_string(),
                            ..Default::default()
                        },
                        None,
                    )));
                    continue;
                }
            }
            let result = (|| {
                *wait_state.lock().unwrap() = Some(Instant::now());
                send(&mut stdin, &request.value)?;
                drop(request.value);
                loop {
                    let mut line = String::new();
                    // Python strings may occupy up to six wire bytes per ASCII control character.
                    let mut limited = std::io::Read::take(&mut stdout, 56 * 1024 * 1024 + 1);
                    if limited.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
                        return Err("The Python package host exited.".into());
                    }
                    if !line.ends_with('\n') {
                        return Err("The package host exceeded the response size budget.".into());
                    }
                    *wait_state.lock().unwrap() = None;
                    let value: Value = serde_json::from_str(&line)
                        .map_err(|e| format!("Invalid package response: {e}"))?;
                    if let Some(op) = value.get("view_request") {
                        let answer = match request.txn.as_mut() {
                            Some(txn) => txn
                                .call(op)
                                .map(|v| json!({"result":v}))
                                .unwrap_or_else(|e| json!({"error":e.to_string()})),
                            None => json!({"error":"No active paged document request"}),
                        };
                        *wait_state.lock().unwrap() = Some(Instant::now());
                        send(&mut stdin, &answer)?;
                        continue;
                    }
                    let mut reply: Reply =
                        serde_json::from_value(value).map_err(|e| e.to_string())?;
                    let mut commit = None;
                    if reply.error.is_empty() {
                        if let Some(txn) = request.txn.take() {
                            match txn.finish(&mut reply) {
                                Ok(tree) => commit = tree,
                                Err(e) => reply.error = e.to_string(),
                            }
                        }
                    }
                    return Ok((reply, commit));
                }
            })();
            let closed = result.is_err();
            *wait_state.lock().unwrap() = None;
            let _ = results.send(result);
            if closed {
                break;
            }
        }
        paged::close_lexer();
        if com {
            unsafe {
                CoUninitialize();
            }
        }
    });
    Ok(Host {
        wake,
        input: tx,
        cancelled,
        waiting_for_python,
        output,
        child: Arc::new(Mutex::new(child)),
    })
}
#[derive(Default)]
struct Session {
    events: std::collections::VecDeque<(Request, Pending)>,
    host: Option<Host>,
    pending: Option<Pending>,
    initialized: bool,
    listeners: HashSet<String>,
    commands: Vec<Entry>,
    last: Option<Stamp>,
    seen: HashSet<u64>,
    timer_check: Option<(Instant, bool)>,
}
thread_local! { static SESSION: RefCell<Session> = RefCell::default(); }
pub(crate) fn palette_rows() -> Vec<String> {
    SESSION.with_borrow(|s| {
        s.commands
            .iter()
            .map(|e| format!("Sublime · {}", e.label))
            .collect()
    })
}

impl App {
    pub(crate) fn sublime_palette_run(&mut self, index: usize) {
        if let Some(entry) = SESSION.with_borrow(|s| s.commands.get(index).cloned()) {
            self.sublime_request(
                json!({"op":"run","name":entry.name,"args":entry.args}),
                true,
            );
        }
    }
    fn sublime_stamp(&self) -> Option<Stamp> {
        let b = &self.buffers[self.current()?];
        Some(Stamp {
            id: b.id,
            revision: b.rev,
            dirty: b.dirty,
            regions: if b.large {
                self.extension_selections(self.active)
                    .ok()?
                    .into_iter()
                    .map(|(a, b)| (a as usize, b as usize))
                    .collect()
            } else {
                self.edit().selections()
            },
            pane: self.active,
        })
    }
    fn sublime_request(&mut self, mut request: Value, with_view: bool) {
        let op = request["op"].as_str().unwrap_or_default().to_owned();
        if SESSION.with_borrow(|s| s.pending.is_some()) {
            self.notice("A Sublime package request is already running.");
            return;
        }
        self.drain_edits();
        let mut stamp = None;
        let mut paged_doc = None;
        if with_view {
            let Some(i) = self.current() else {
                self.notice("Open a document to run this command.");
                return;
            };
            stamp = self.sublime_stamp();
            let Some(target) = &stamp else {
                self.notice("The current selection could not be read for this package.");
                return;
            };
            let use_paged = self.buffers[i].large || self.edit().len() > 2 * 1024 * 1024;
            if use_paged && let Err(e) = self.sync_buffer(i) {
                self.notice(e);
                return;
            }
            let b = &self.buffers[i];
            request["doc"] = json!({ "id":b.id,"revision":b.rev,"name":b.name,
                "path":b.path,"dirty":b.dirty,"readonly":b.saving||b.follow,"regions":target.regions,
                "settings":{"syntax": format!("Packages/{0}/{0}.sublime-syntax",b.lang),"tab_size":self.edit().send(crate::sci::GETTABWIDTH,0,0)}});
            if use_paged {
                request["doc"]["paged"] = json!(true);
                paged_doc = Some((b.doc.clone(), b.lang.clone()));
            } else {
                let ed = self.edit();
                match String::from_utf8(ed.raw_bytes(0, ed.len())) {
                    Ok(text) => request["doc"]["text"] = json!(text),
                    Err(_) => {
                        self.notice(
                            "The package requires valid UTF-8. Reopen with the original encoding.",
                        );
                        return;
                    }
                }
            }
            request["folders"] = json!(self.prefs.root.iter().collect::<Vec<_>>());
            let ed = self.edit();
            let first = ed.send(crate::sci::GETFIRSTVISIBLELINE, 0, 0).max(0) as usize;
            let last = first + ed.send(crate::sci::LINESONSCREEN, 0, 0).max(1) as usize + 1;
            let position = |display_line| {
                let line = ed.send(crate::sci::DOCLINEFROMVISIBLE, display_line, 0);
                if line < 0 {
                    return ed.len();
                }
                let p = ed.send(crate::sci::POSITIONFROMLINE, line as usize, 0);
                if p < 0 { ed.len() } else { p as usize }
            };
            let (a, z) = (position(first), position(last));
            request["doc"]["visible"] = if b.large {
                json!([
                    crate::events::point(self.active, a as u64),
                    crate::events::end_point(self.active, z as u64)
                ])
            } else {
                json!([a, z])
            };
            if !b.large && ed.len() <= 2 * 1024 * 1024 {
                request["doc"]["scopes"] = json!(ed.package_scope_runs());
            }
            {
                let lang = b.lang.to_lowercase();
                request["doc"]["root_scope"] = json!(match lang.as_str() {
                    "plain text" => "text.plain".to_owned(),
                    "javascript" => "source.js".into(),
                    "c++" => "source.c++".into(),
                    "html" => "text.html.basic".into(),
                    _ => format!("source.{}", lang.trim_end_matches(" · sublime")),
                });
            }
        }
        let result = SESSION.with_borrow_mut(|s| -> Result<(), String> {
            if s.host.is_none() {
                s.host = Some(start()?);
            }
            s.host
                .as_ref()
                .unwrap()
                .input
                .send(Request {
                    value: request,
                    txn: paged_doc.map(|(doc, lang)| {
                        paged::Transaction::new(
                            doc,
                            lang,
                            s.host.as_ref().unwrap().cancelled.clone(),
                        )
                    }),
                })
                .map_err(|_| "Package host disconnected")?;
            if op == "timers" || op == "poll" {
                s.timer_check = None;
            }
            s.pending = Some(Pending {
                operation: op,
                stamp,
            });
            Ok(())
        });
        if let Err(e) = result {
            SESSION.with_borrow_mut(|s| s.host = None);
            self.notice(e);
        }
    }
    fn apply_package_commit(&mut self, i: usize, tree: paged::Commit) -> Result<(), String> {
        if self.buffers[i].large {
            return self.commit_extension_snapshot(i, tree.doc, &tree.spans);
        }
        let mut delta = 0i64;
        let mut edits = vec![];
        for (start, del, add) in tree.spans {
            let new = start
                .checked_add_signed(delta)
                .ok_or("Invalid package position")?;
            let bytes = tree
                .doc
                .read(new, add as usize)
                .map_err(|e| e.to_string())?;
            edits.push((
                start as usize,
                (start + del) as usize,
                String::from_utf8(bytes).map_err(|e| e.to_string())?,
            ));
            delta += add as i64 - del as i64;
        }
        self.change_open(i, edits)
    }
    fn initialize_sublime_listeners(&mut self) -> bool {
        let first = SESSION.with_borrow_mut(|s| !std::mem::replace(&mut s.initialized, true));
        if first
            && fs::read_dir(state::profile().join("Packages"))
                .ok()
                .is_some_and(|mut d| d.next().is_some())
        {
            self.sublime_request(json!({"op":"listeners"}), false);
        }
        first
    }
    pub(crate) fn sublime_saved(&mut self, i: usize) {
        self.initialize_sublime_listeners();
        let b = &self.buffers[i];
        let pane = (0..2).find(|&p| self.assigned[p] == Some(b.id));
        let regions = pane
            .and_then(|p| self.extension_selections(p).ok())
            .unwrap_or_else(|| {
                let at = b.last.min(b.doc.len());
                vec![(at, at)]
            });
        SESSION.with_borrow_mut(|s|{
            let Some(host)=&s.host else {return};
            let names=["on_post_save","on_post_save_async"];
            if !names.iter().any(|n|s.listeners.contains(*n)) && !s.pending.as_ref().is_some_and(|p|p.operation=="listeners") {return;}
            let stamp=Stamp{id:b.id,revision:b.rev,dirty:false,pane:pane.unwrap_or(self.active),regions:regions.iter().map(|(a,z)|(*a as usize,*z as usize)).collect()};
            let value=json!({"op":"events","transient_view":self.assigned[self.active]!=Some(b.id),"events":names,"folders":self.prefs.root.iter().collect::<Vec<_>>(),
                "doc":{"id":b.id,"revision":b.rev,"name":b.name,"path":b.path,"dirty":false,"readonly":b.follow,
                "paged":true,"regions":regions,"visible":[b.last.min(b.doc.len()),b.last.min(b.doc.len())],"settings":{"syntax":format!("Packages/{0}/{0}.sublime-syntax",b.lang),"tab_size":self.prefs.tab_width}}});
            let txn=paged::Transaction::new(b.doc.clone(),b.lang.clone(),host.cancelled.clone());
            s.events.push_back((Request{value,txn:Some(txn)},Pending{operation:"saved-event".into(),stamp:Some(stamp)}));
        });
    }
    pub(crate) fn sublime_closed(&mut self, i: usize) {
        self.initialize_sublime_listeners();
        let id = self.buffers[i].id;
        let wants_context = SESSION.with_borrow(|s| {
            s.host.is_some()
                && (s.listeners.contains("on_close")
                    || s.pending
                        .as_ref()
                        .is_some_and(|p| p.operation == "listeners"))
        });
        let supply_view = wants_context && self.sync_buffer(i).is_ok();
        let b = &self.buffers[i];
        SESSION.with_borrow_mut(|s| {
            s.seen.remove(&id);
            if s.last.as_ref().is_some_and(|v|v.id==id) {s.last=None;}
            if let Some(host)=&s.host {
                let mut value=json!({"op":"close","id":id});
                let txn=supply_view.then(|| {
                    let pos=b.last.min(b.doc.len());
                    value["doc"]=json!({"id":id,"revision":b.rev,"name":b.name,"path":b.path,"dirty":b.dirty,"readonly":true,"paged":true,"regions":[[pos,pos]],"visible":[pos,pos],"settings":{"syntax":format!("Packages/{0}/{0}.sublime-syntax",b.lang),"tab_size":self.prefs.tab_width}});
                    value["folders"]=json!(self.prefs.root.iter().collect::<Vec<_>>());
                    paged::Transaction::new(b.doc.clone(),b.lang.clone(),host.cancelled.clone())
                });
                s.events.push_back((Request{value,txn},Pending{operation:"close".into(),stamp:None}));
            }
        });
    }
    pub(crate) fn cancel_sublime(&mut self) -> bool {
        let cancelled = SESSION.with_borrow_mut(|s| {
            if s.pending.as_ref().is_none_or(|p| {
                !matches!(
                    p.operation.as_str(),
                    "run" | "events" | "saved-event" | "timers"
                )
            }) {
                return false;
            }
            *s = Session::default();
            true
        });
        if cancelled {
            self.notice("Sublime package request cancelled");
        }
        cancelled
    }
    pub(crate) fn sublime_command(&mut self, id: &str) -> bool {
        match id {
            "sublimeinstall" => {
                let choices = vec![
                    "Install a .sublime-package or .zip file".into(),
                    "Install an unpacked package folder".into(),
                ];
                let path = match forms::pick(self.h, "Install Sublime package", &choices, "") {
                    Some(0) => dialogs::files(self.h)
                        .add_filter("Sublime package", &["sublime-package", "zip"])
                        .pick_file(),
                    Some(_) => dialogs::files(self.h).pick_folder(),
                    None => return true,
                };
                if let Some(path) = path {
                    if confirm(
                        self.h,
                        "Install Sublime package",
                        &format!(
                            "Install {}? Python packages run with your Windows account’s access to files and programs.",
                            path.display()
                        ),
                        false,
                    ) == 6
                    {
                        self.sublime_request(json!({"op":"install","path":path}), false);
                    }
                }
            }
            "sublimecommands" => self.sublime_request(json!({"op":"commands"}), false),
            "sublimepackages" => self.sublime_request(json!({"op":"packages"}), false),
            _ => return false,
        }
        true
    }
    pub(crate) fn sublime_tick(&mut self) {
        let awaiting = SESSION.with_borrow(|s| s.pending.is_some());
        if awaiting {
            let done = SESSION.with_borrow_mut(|s| {
                let host = s.host.as_ref()?;
                let result = match host.output.try_recv() {
                    Ok(r) => r,
                    Err(mpsc::TryRecvError::Empty)
                        if !host
                            .waiting_for_python
                            .lock()
                            .unwrap()
                            .is_some_and(|t| t.elapsed() >= Duration::from_secs(60)) =>
                    {
                        return None;
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        Err("The Python package host did not respond for 60 seconds.".into())
                    }
                    Err(_) => Err("The package host disconnected.".into()),
                };
                if result.is_err() {
                    s.host = None;
                }
                Some((s.pending.take()?, result))
            });
            if let Some((pending, result)) = done {
                match result {
                    Ok((reply, tree)) if reply.error.is_empty() => {
                        self.sublime_result(pending, reply, tree)
                    }
                    Ok((reply, _)) => self.notice(format!("Sublime package · {}", reply.error)),
                    Err(e) => self.notice(e),
                }
            }
            return;
        }
        if self.initialize_sublime_listeners() {
            return;
        }
        if SESSION.with_borrow_mut(|s| {
            let Some((req, pending)) = s.events.pop_front() else {
                return false;
            };
            if let Some(host) = &s.host
                && host.input.send(req).is_ok()
            {
                s.pending = Some(pending);
            }
            true
        }) {
            return;
        }
        if SESSION.with_borrow(|s|s.host.as_ref().is_some_and(|h|unsafe {windows_sys::Win32::System::Threading::WaitForSingleObject(h.wake.0 as _,0)}==0)) {
            self.sublime_request(json!({"op":"poll"}),false); return;
        }
        let timer =
            SESSION.with_borrow(|s| s.timer_check.filter(|t| Instant::now() >= t.0).map(|t| t.1));
        if let Some(due) = timer {
            self.sublime_request(
                json!({"op":if due{"timers"}else{"poll"}}),
                due && self.current().is_some(),
            );
            return;
        }
        if SESSION.with_borrow(|s| s.listeners.is_empty()) {
            return;
        }
        let Some(stamp) = self.sublime_stamp() else {
            return;
        };
        let events = SESSION.with_borrow_mut(|s| {
            let mut names = vec![];
            if s.seen.insert(stamp.id) {
                names.push("on_load");
            }
            match &s.last {
                Some(old) if old.id == stamp.id => {
                    if old.revision != stamp.revision {
                        names.push("on_modified");
                    }
                    if old.regions != stamp.regions {
                        names.push("on_selection_modified");
                    }
                }
                _ => names.push("on_activated"),
            }
            s.last = Some(stamp);
            names
                .into_iter()
                .flat_map(|n| [n.to_owned(), format!("{n}_async")])
                .filter(|n| s.listeners.contains(n))
                .collect::<Vec<_>>()
        });
        if !events.is_empty() {
            self.sublime_request(json!({"op":"events","events":events}), true);
        }
    }
    fn sublime_result(&mut self, pending: Pending, r: Reply, tree: Option<paged::Commit>) {
        let errors = r.errors.join("\n");
        SESSION.with_borrow_mut(|s| {
            s.timer_check = if r.timers {
                r.delay.map(|ms| {
                    (
                        Instant::now() + Duration::from_millis(ms.clamp(1, 86_400_000)),
                        true,
                    )
                })
            } else {
                None
            }
        });
        match pending.operation.as_str() {
            "poll" | "close" => {}
            "saved-event" => {
                let Some(stamp) = pending.stamp else {
                    return;
                };
                if let Some(i) = self.idx(stamp.id)
                    && self.buffers[i].rev == stamp.revision
                    && !self.buffers[i].saving
                {
                    if let Some(commit) = tree
                        && let Err(e) = self.apply_package_commit(i, commit)
                    {
                        self.notice(e);
                        return;
                    }
                    if let Some(m) = r.messages.last() {
                        self.notice(m.clone());
                    }
                    if self.assigned[stamp.pane] == Some(stamp.id)
                        && r.regions != stamp.regions
                        && !r.regions.is_empty()
                    {
                        let pairs = r
                            .regions
                            .into_iter()
                            .map(|(a, z)| (a as u64, z as u64))
                            .collect::<Vec<_>>();
                        let _ = self.set_extension_selections(stamp.pane, &pairs);
                    }
                }
            }
            "install" | "remove" => {
                SESSION.with_borrow_mut(|s| *s = Session::default());
                self.notice(if r.installed.is_empty() {
                    format!("Removed {}", r.removed)
                } else {
                    format!(
                        "Installed {} · Open Sublime Package Commands to run it",
                        r.installed
                    )
                });
                self.load_grammars();
            }
            "listeners" => {
                SESSION.with_borrow_mut(|s| {
                    s.listeners = r.listeners.into_iter().collect();
                    s.commands = r.commands;
                });
                if !errors.is_empty() {
                    self.notice(format!("Sublime package · {errors}"));
                }
            }
            "packages" => {
                let choices = r
                    .packages
                    .into_iter()
                    .filter(|name| name != "User")
                    .collect::<Vec<_>>();
                if !errors.is_empty() {
                    alert(self.h, "Sublime package load details", &errors);
                }
                if choices.is_empty() {
                    self.notice("No Sublime packages installed.");
                    return;
                }
                if let Some(i) =
                    forms::pick(self.h, "Remove an installed Sublime package", &choices, "")
                {
                    if confirm(
                        self.h,
                        "Remove Sublime package",
                        &format!("Remove {} from the active package folder?", choices[i]),
                        false,
                    ) == 6
                    {
                        self.sublime_request(json!({"op":"remove","package":choices[i]}), false);
                    }
                }
            }
            "commands" => {
                if !errors.is_empty() {
                    alert(self.h, "Sublime package load details", &errors);
                }
                if r.commands.is_empty() {
                    self.notice("No Sublime package commands available.");
                    return;
                }
                let labels = r
                    .commands
                    .iter()
                    .map(|e| e.label.clone())
                    .collect::<Vec<_>>();
                if let Some(i) = forms::pick(self.h, "Sublime package commands", &labels, "") {
                    let entry = &r.commands[i];
                    self.sublime_request(
                        json!({"op":"run","name":entry.name,"args":entry.args}),
                        true,
                    );
                }
            }
            _ => {
                self.drain_edits();
                if self.sublime_stamp() != pending.stamp {
                    if pending.operation != "events" {
                        self.notice(
                            "The document or selection changed. Run the package command again.",
                        );
                    }
                    return;
                }
                let Some(i) = self.current() else { return };
                let paged = self.buffers[i].large;
                if paged || tree.is_some() {
                    if let Some(tree) = tree {
                        if let Err(e) = self.apply_package_commit(i, tree) {
                            self.notice(e);
                            return;
                        }
                    }
                    let regions = r
                        .regions
                        .iter()
                        .map(|&(a, b)| (a as u64, b as u64))
                        .collect::<Vec<_>>();
                    if !regions.is_empty()
                        && self.extension_selections(self.active).ok().as_ref() != Some(&regions)
                    {
                        if let Err(e) = self.set_extension_selections(self.active, &regions) {
                            self.notice(e);
                            return;
                        }
                    }
                } else {
                    let text_len = self.edit().len();
                    if !valid_edits(&r.edits, text_len) {
                        self.notice("The package returned invalid edit positions.");
                        return;
                    }
                    if let Err(e) = self.change_open(i, r.edits) {
                        self.notice(e);
                        return;
                    }
                    let ed = self.edit();
                    if r.regions != ed.selections()
                        && r.regions
                            .iter()
                            .all(|&(a, b)| a <= ed.len() && b <= ed.len())
                    {
                        for (n, (a, b)) in r.regions.into_iter().enumerate() {
                            ed.send(
                                if n == 0 {
                                    SCI_SETSELECTION
                                } else {
                                    SCI_ADDSELECTION
                                },
                                b,
                                a as isize,
                            );
                        }
                    }
                }
                if let Some(msg) = r.messages.last() {
                    self.notice(msg.clone())
                }
                if let Some(p) = r.reveal {
                    if paged && p as u64 <= self.total_len() {
                        if let Some(regions) = self.extension_selections(self.active).ok() {
                            self.go(p as u64, 0);
                            if !regions.is_empty() {
                                let _ = self.set_extension_selections(self.active, &regions);
                            }
                        }
                    } else if p <= self.edit().len() {
                        self.edit()
                            .send(linework_editor::sci::SCI_SCROLLRANGE, p, p as isize);
                    }
                }
                if let Some(d) = r.dialog {
                    let value = if d.kind == "input" {
                        json!(forms::input(
                            self.h,
                            "Sublime package",
                            &d.title,
                            &d.initial
                        ))
                    } else {
                        json!(
                            forms::pick(self.h, "Sublime package", &d.items, "")
                                .map_or(-1, |x| x as i64)
                        )
                    };
                    self.sublime_request(
                        json!({"op":"callback","token":d.token,"value":value}),
                        true,
                    );
                }
            }
        }
    }
}
fn valid_edits(edits: &[(usize, usize, String)], len: usize) -> bool {
    let mut prev = 0;
    edits.iter().all(|&(a, b, _)| {
        let valid = prev <= a && a <= b && b <= len;
        prev = b;
        valid
    })
}
