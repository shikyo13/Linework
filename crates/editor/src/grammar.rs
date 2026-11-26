use super::sublime_v2 as v2;
use super::{Editor, sci::*};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxDefinition, SyntaxSet};
use yaml_rust::{Yaml, YamlLoader};

#[derive(Clone)]
struct Grammar {
    name: String,
    path: PathBuf,
    syntax: usize,
    extensions: Vec<String>,
    v2: Option<Arc<v2::Grammar>>,
    first: Option<Arc<onig::Regex>>,
    hidden: bool,
}
#[derive(Default)]
struct Registry {
    set: Option<Arc<SyntaxSet>>,
    grammars: Vec<Grammar>,
}
thread_local! {
    static GRAMMARS:RefCell<Registry>=RefCell::new(Registry::default());
    static ATTACHED:RefCell<HashMap<(isize,usize,usize),std::rc::Weak<RefCell<Session>>>>=RefCell::new(HashMap::new());
}
const MAX_FILE: u64 = 2 * 1024 * 1024;
const MAX_LINE: usize = 16 * 1024;
const BATCH_LINES: usize = 128;
const BATCH_BYTES: usize = 64 * 1024;

type Jobs = VecDeque<(isize, Box<dyn FnOnce() + Send>)>;
fn submit(editor: isize, job: impl FnOnce() + Send + 'static) {
    static WORK: OnceLock<Arc<(Mutex<Jobs>, Condvar)>> = OnceLock::new();
    let queue = WORK.get_or_init(|| {
        let queue = Arc::new((Mutex::new(Jobs::new()), Condvar::new()));
        // Both visible panes may request work. Paging or typing only replaces
        // the queued job for that view and never creates additional workers.
        for _ in 0..2 {
            let shared = queue.clone();
            thread::spawn(move || {
                loop {
                    let work = {
                        let jobs = shared.0.lock().unwrap();
                        let mut jobs = shared.1.wait_while(jobs, |q| q.is_empty()).unwrap();
                        jobs.pop_front().unwrap().1
                    };
                    work();
                }
            });
        }
        queue
    });
    let mut jobs = queue.0.lock().unwrap();
    jobs.retain(|(owner, _)| *owner != editor);
    jobs.push_back((editor, Box::new(job)));
    drop(jobs);
    queue.1.notify_one();
}

pub(super) fn names() -> Vec<String> {
    GRAMMARS.with_borrow(|r| {
        r.grammars
            .iter()
            .filter(|g| !g.hidden)
            .map(|g| g.name.clone())
            .collect()
    })
}
pub(super) fn detect_first(sample: &str) -> Option<String> {
    let line = sample.lines().next()?;
    GRAMMARS.with_borrow(|r| {
        r.grammars
            .iter()
            .find(|g| {
                g.first.as_ref().is_some_and(|re| {
                    let mut param = onig::MatchParam::default();
                    param.set_retry_limit_in_match(40_000);
                    re.search_with_param(
                        line,
                        0,
                        line.len(),
                        onig::SearchOptions::SEARCH_OPTION_NONE,
                        None,
                        param,
                    )
                    .ok()
                    .flatten()
                    .is_some()
                })
            })
            .map(|g| g.name.clone())
    })
}
pub(super) fn detect(path: &Path) -> Option<String> {
    GRAMMARS.with_borrow(|r| {
        if r.grammars.is_empty() {
            return None;
        }
        let file = path.file_name()?.to_str()?.to_lowercase();
        r.grammars
            .iter()
            .flat_map(|g| g.extensions.iter().map(move |e| (&g.name, e)))
            .filter(|(_, e)| {
                file == **e
                    || file
                        .strip_suffix(e.as_str())
                        .is_some_and(|p| p.ends_with('.'))
            })
            .max_by_key(|(_, e)| e.len())
            .map(|(name, _)| name.clone())
    })
}
fn read_source(path: &Path) -> Result<String, String> {
    let f = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut s = String::new();
    f.take(MAX_FILE + 1)
        .read_to_string(&mut s)
        .map_err(|e| e.to_string())?;
    if s.len() as u64 > MAX_FILE {
        return Err("Grammar files can contain up to 2 MiB of YAML.".into());
    }
    Ok(s)
}
pub(super) fn check_variables(doc: &Yaml) -> Result<(), String> {
    let Some(vars) = doc["variables"].as_hash() else {
        return Ok(());
    };
    fn size(
        name: &str,
        vars: &yaml_rust::yaml::Hash,
        visiting: &mut HashSet<String>,
        sizes: &mut HashMap<String, usize>,
    ) -> Result<usize, String> {
        if let Some(n) = sizes.get(name) {
            return Ok(*n);
        }
        if visiting.len() >= 64 || !visiting.insert(name.into()) {
            return Err(format!("Recursive grammar variable: {name}"));
        }
        let Some(value) = vars.get(&Yaml::String(name.into())).and_then(Yaml::as_str) else {
            return Err(format!("Undefined grammar variable: {name}"));
        };
        let mut length = value.len();
        let mut rest = value;
        while let Some((_, tail)) = rest.split_once("{{") {
            let Some((var, following)) = tail.split_once("}}") else {
                break;
            };
            if !var.is_empty() && var.chars().all(|c| c.is_alphanumeric() || c == '_') {
                length = length.saturating_add(size(var, vars, visiting, sizes)?);
            }
            if length > MAX_FILE as usize {
                return Err("Grammar variables expand beyond 2 MiB.".into());
            }
            rest = following;
        }
        visiting.remove(name);
        sizes.insert(name.into(), length);
        Ok(length)
    }
    let mut sizes = HashMap::new();
    let mut visiting = HashSet::new();
    for k in vars.keys() {
        if let Some(name) = k.as_str() {
            size(name, vars, &mut visiting, &mut sizes)?;
        }
    }
    Ok(())
}
fn definition(
    file: &Path,
    source: &str,
    catalog: &v2::Catalog,
) -> Result<(SyntaxDefinition, Option<Arc<v2::Grammar>>), String> {
    let docs = YamlLoader::load_from_str(source).map_err(|e| e.to_string())?;
    let doc = docs.first().ok_or("Grammar is empty")?;
    let fallback = file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("User syntax");
    let version = doc["version"].as_i64().unwrap_or(1);
    if !matches!(version, 1 | 2) {
        return Err(format!("Unknown grammar version {version}"));
    }
    let advanced = version == 2
        || !doc["extends"].is_badvalue()
        || source
            .lines()
            .any(|line| line.trim_start().starts_with("branch_point:"));
    let (def, compiled) = if advanced {
        let grammar = catalog.compile(file)?;
        // Syntect's v1 set still indexes this name and scope for language selection.
        // Its placeholder never parses a v2 document.
        let scope = doc["scope"].as_str().ok_or("Syntax requires a scope")?;
        let mut def = SyntaxDefinition::load_from_str(
            "name: Temporary\nscope: text.plain\ncontexts:\n  main: []\n",
            true,
            None,
        )
        .map_err(|e| e.to_string())?;
        def.name = doc["name"].as_str().unwrap_or(fallback).to_owned();
        def.scope = Scope::new(scope).map_err(|e| e.to_string())?;
        def.hidden = doc["hidden"].as_bool().unwrap_or(false);
        def.first_line_match = doc["first_line_match"].as_str().map(str::to_owned);
        def.file_extensions = ["file_extensions", "hidden_file_extensions"]
            .iter()
            .flat_map(|k| doc[*k].as_vec().into_iter().flatten())
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        (def, Some(grammar))
    } else {
        check_variables(doc)?;
        (
            SyntaxDefinition::load_from_str(source, true, Some(fallback))
                .map_err(|e| e.to_string())?,
            None,
        )
    };
    if def.name.trim().is_empty() || def.name.len() > 160 {
        return Err("Give the grammar a name of up to 160 bytes.".into());
    }
    Ok((def, compiled))
}
fn assemble(mut sources: Vec<(PathBuf, String)>) -> Result<(Registry, Vec<String>), String> {
    if sources.is_empty() {
        return Ok((Registry::default(), Vec::new()));
    }
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    let catalog = v2::Catalog::new(&sources);
    let mut defs = Vec::new();
    let mut issues = Vec::new();
    let mut seen = HashSet::new();
    for (file, data) in sources {
        match definition(&file, &data, &catalog) {
            Ok((def, compiled)) if seen.insert(def.name.to_lowercase()) => {
                defs.push((file, def, compiled))
            }
            Ok((def, _)) => issues.push(format!(
                "{}: duplicate grammar name {}",
                file.display(),
                def.name
            )),
            Err(e) => issues.push(format!("{}: {e}", file.display())),
        }
    }
    if defs.is_empty() {
        return Ok((Registry::default(), issues));
    }
    let defaults = SyntaxSet::load_defaults_newlines();
    let base = defaults.syntaxes().len();
    let mut builder = defaults.into_builder();
    let mut grammars = Vec::new();
    for (path, def, v2) in defs {
        grammars.push(Grammar {
            name: format!("{} · Sublime", def.name),
            path,
            syntax: base + grammars.len(),
            v2,
            hidden: def.hidden,
            first: def
                .first_line_match
                .as_deref()
                .and_then(|s| onig::Regex::new(s).ok())
                .map(Arc::new),
            extensions: def
                .file_extensions
                .iter()
                .map(|s| s.trim_start_matches('.').to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
        });
        builder.add(def);
    }
    let set = Arc::new(builder.build());
    let unresolved = set.find_unlinked_contexts();
    grammars.retain(|g| {
        let syn = &set.syntaxes()[g.syntax];
        let prefix = format!("Syntax '{}' with scope '{}' ", syn.name, syn.scope);
        if let Some(e) = unresolved.iter().find(|e| e.starts_with(&prefix)) {
            issues.push(e.clone());
            false
        } else {
            true
        }
    });
    Ok((
        Registry {
            set: Some(set),
            grammars,
        },
        issues,
    ))
}
fn sources(folder: &Path) -> Result<(Vec<(PathBuf, String)>, Vec<String>), String> {
    let mut result = vec![];
    let mut issues = vec![];
    let mut dirs = vec![folder.to_path_buf()];
    if let Some(profile) = folder.parent() {
        dirs.push(profile.join("Packages"))
    }
    while let Some(current) = dirs.pop() {
        let dir = match fs::read_dir(&current) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.to_string()),
        };
        for item in dir.flatten() {
            let p = item.path();
            if item.file_type().is_ok_and(|t| t.is_dir())
                && !item.file_name().to_string_lossy().starts_with('.')
            {
                // Away from Windows a link to a folder is not reported as a folder,
                // so there is nothing further to rule out.
                #[cfg(not(windows))]
                dirs.push(p);
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if item
                        .metadata()
                        .is_ok_and(|m| m.file_attributes() & 0x400 == 0)
                    {
                        dirs.push(p);
                    }
                }
                continue;
            }
            if !p
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("sublime-syntax"))
            {
                continue;
            }
            if result.len() >= 256 {
                return Err("Up to 256 local grammars are supported.".into());
            }
            match read_source(&p) {
                Ok(s) => result.push((p, s)),
                Err(e) => issues.push(format!("{}: {e}", p.display())),
            }
        }
    }
    Ok((result, issues))
}
pub fn load_user_grammars(folder: &Path) -> Result<Vec<String>, String> {
    let (sources, mut issues) = sources(folder)?;
    let (registry, extra) = assemble(sources)?;
    issues.extend(extra);
    GRAMMARS.replace(registry);
    Ok(issues)
}
/// Validates and links before publishing the local file and activating the registry.
pub fn import_user_grammar(folder: &Path, input: &Path) -> Result<String, String> {
    let source = read_source(input)?;
    let docs = YamlLoader::load_from_str(&source).map_err(|e| e.to_string())?;
    let expected = docs.first().ok_or("Empty grammar")?["name"]
        .as_str()
        .or_else(|| input.file_stem()?.to_str())
        .ok_or("Missing grammar name")?
        .to_owned();
    let filename = input
        .file_name()
        .filter(|_| {
            input
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("sublime-syntax"))
        })
        .ok_or("Choose a .sublime-syntax file.")?;
    let dest = folder.join(filename);
    let (mut items, _) = sources(folder)?;
    items.retain(|(p, _)| p != &dest);
    if items.len() >= 256 {
        return Err("Up to 256 local grammars are supported.".into());
    }
    items.push((dest.clone(), source.clone()));
    let (registry, issues) = assemble(items)?;
    let name = format!("{expected} · Sublime");
    if !registry
        .grammars
        .iter()
        .any(|g| g.name == name && g.path == dest)
        || issues
            .iter()
            .any(|e| e.ends_with(&format!("duplicate grammar name {expected}")))
    {
        return Err(if issues.is_empty() {
            "The grammar could not be linked.".into()
        } else {
            issues.join("\n")
        });
    }
    fs::create_dir_all(folder).map_err(|e| e.to_string())?;
    // Validate first, then atomically publish in the grammar directory.
    let staged = dest.with_extension(format!("{}.tmp", std::process::id()));
    {
        use std::io::Write;
        let mut f = fs::File::create(&staged).map_err(|e| e.to_string())?;
        f.write_all(source.as_bytes())
            .and_then(|_| f.sync_all())
            .map_err(|e| e.to_string())?;
    }
    if let Err(e) = fs::rename(&staged, &dest) {
        let _ = fs::remove_file(&staged);
        return Err(e.to_string());
    }
    GRAMMARS.replace(registry);
    Ok(name)
}
#[derive(Clone)]
struct Context {
    parse: ParseState,
    scopes: ScopeStack,
    v2: Option<v2::State>,
}
pub(super) fn scope_style(stack: &ScopeStack) -> u8 {
    static PREFIXES: OnceLock<Vec<(Scope, u8)>> = OnceLock::new();
    let prefixes = PREFIXES.get_or_init(|| {
        [
            ("comment", 1),
            ("constant.character.escape", 4),
            ("string", 2),
            ("keyword.operator", 7),
            ("keyword", 3),
            ("storage", 3),
            ("constant", 4),
            ("entity.name.tag", 6),
            ("entity.name", 5),
            ("entity.other", 5),
            ("support", 5),
            ("markup.heading", 5),
            ("variable.language", 3),
            ("invalid", 8),
            ("punctuation", 7),
        ]
        .into_iter()
        .map(|(k, n)| (Scope::new(k).unwrap(), n))
        .collect()
    });
    for scope in stack.as_slice().iter().rev() {
        if let Some((_, id)) = prefixes
            .iter()
            .find(|(prefix, _)| prefix.is_prefix_of(*scope))
        {
            return *id;
        }
    }
    0
}
fn parse_line(cx: &mut Context, text: &str, set: &SyntaxSet) -> Vec<u8> {
    let mut styles = vec![0; text.len()];
    let mut cursor = 0;
    if let Ok(events) = cx.parse.parse_line(text, set) {
        for (at, op) in events {
            let at = at.min(styles.len());
            if at > cursor {
                styles[cursor..at].fill(scope_style(&cx.scopes));
                cursor = at;
            }
            if cx.scopes.apply(&op).is_err() {
                break;
            }
        }
        styles[cursor..].fill(scope_style(&cx.scopes));
    }
    styles
}
struct Line {
    offset: usize,
    text: Vec<u8>,
}
struct WorkResult {
    generation: u64,
    next: usize,
    state: Context,
    patches: Vec<(usize, Vec<u8>)>,
}
pub(super) struct Session {
    pub name: String,
    set: Arc<SyntaxSet>,
    syntax: usize,
    v2: Option<Arc<v2::Grammar>>,
    generation: u64,
    next: usize,
    state: Context,
    checkpoints: BTreeMap<usize, Context>,
    pending: Option<(Arc<AtomicBool>, Receiver<WorkResult>)>,
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = ATTACHED.try_with(|a| {
            if let Ok(mut entries) = a.try_borrow_mut() {
                entries.retain(|_, s| s.strong_count() > 0);
            }
        });
        if let Some((c, _)) = &self.pending {
            c.store(true, Ordering::Relaxed)
        }
    }
}
impl Session {
    fn initial(set: &SyntaxSet, index: usize, v2: Option<Arc<v2::Grammar>>) -> Context {
        Context {
            parse: ParseState::new(&set.syntaxes()[index]),
            scopes: ScopeStack::new(),
            v2: v2.map(v2::State::new),
        }
    }
    fn new(name: &str) -> Option<Self> {
        GRAMMARS.with_borrow(|r| {
            let g = r
                .grammars
                .iter()
                .find(|g| g.name.eq_ignore_ascii_case(name))?;
            let set = r.set.clone()?;
            let state = Self::initial(&set, g.syntax, g.v2.clone());
            Some(Self {
                name: g.name.clone(),
                set,
                syntax: g.syntax,
                v2: g.v2.clone(),
                generation: 0,
                next: 0,
                state,
                checkpoints: BTreeMap::new(),
                pending: None,
            })
        })
    }
    fn invalidate(&mut self, line: usize) {
        let line = if self.v2.is_some() {
            line.saturating_sub(128)
        } else {
            line
        };
        let _ = self.checkpoints.split_off(&line.saturating_add(1));
        self.generation = self.generation.wrapping_add(1);
        if let Some((c, _)) = &self.pending {
            c.store(true, Ordering::Relaxed)
        }
        if line < self.next {
            if let Some((at, state)) = self.checkpoints.last_key_value() {
                self.next = *at;
                self.state = state.clone();
            } else {
                self.next = 0;
                self.state = Self::initial(&self.set, self.syntax, self.v2.clone());
            }
        }
    }
}
impl Editor {
    pub(super) fn use_grammar(&self, name: &str) -> bool {
        let Some(session) = Session::new(name) else {
            return false;
        };
        let tag = (
            self.send(SCI_GETDOCPOINTER, 0, 0),
            Arc::as_ptr(&session.set) as usize,
            session.syntax,
        );
        let old = ATTACHED.with_borrow(|v| v.get(&tag).and_then(|s| s.upgrade()));
        let installed = old.is_some();
        let shared = old.unwrap_or_else(|| {
            let s = std::rc::Rc::new(RefCell::new(session));
            ATTACHED.with_borrow_mut(|v| {
                v.retain(|_, session| session.strong_count() != 0);
                v.insert(tag, std::rc::Rc::downgrade(&s));
            });
            s
        });
        self.language.set(&super::language::registry()[0]);
        self.grammar.replace(Some(shared));
        self.note_log();
        // Style bytes belong to the shared document. A second split pane
        // attaching to it must preserve styling already visible in the first.
        if !installed || self.send(SCI_GETLEXER, 0, 0) != 0 {
            unsafe {
                self.send_ptr(SCI_SETILEXER, 0, 0);
            }
            self.send(SCI_CLEARDOCUMENTSTYLE, 0, 0);
        }
        self.apply_lexer_styles();
        self.spelling.borrow_mut().invalidate();
        self.mark_view.set(None);
        self.pump_grammar();
        true
    }
    pub fn is_custom_grammar(&self) -> bool {
        self.grammar.borrow().is_some()
    }
    pub fn invalidate_grammar(&self, offset: usize) {
        if let Some(s) = self.grammar.borrow_mut().as_mut() {
            let line = self
                .send(SCI_LINEFROMPOSITION, offset.min(self.len()), 0)
                .max(0) as usize;
            s.borrow_mut().invalidate(line);
        }
    }
    pub fn pump_grammar(&self) {
        let guard = self.grammar.borrow();
        let Some(session) = guard.as_ref() else {
            return;
        };
        let mut state = session.borrow_mut();
        let s = &mut *state;
        if let Some((_, rx)) = &s.pending {
            match rx.try_recv() {
                Ok(done) => {
                    if done.generation == s.generation {
                        for (at, styles) in done.patches {
                            self.send(SCI_STARTSTYLING, at, 0);
                            unsafe {
                                self.send_ptr(
                                    SCI_SETSTYLINGEX,
                                    styles.len(),
                                    styles.as_ptr() as isize,
                                );
                            }
                        }
                        s.next = done.next;
                        s.state = done.state;
                        // A bounded number of checkpoints still covers long normal documents.
                        if s.checkpoints.len() >= 256 {
                            s.checkpoints.pop_first();
                        }
                        s.checkpoints.insert(s.next, s.state.clone());
                    }
                    s.pending = None;
                }
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    s.pending = None;
                }
            }
        }
        let first = self.send(SCI_GETFIRSTVISIBLELINE, 0, 0).max(0) as usize;
        let shown = self.send(SCI_LINESONSCREEN, 0, 0).clamp(12, 400) as usize;
        let total = self.send(SCI_GETLINECOUNT, 0, 0).max(1) as usize;
        let doc_bottom = self.send(SCI_DOCLINEFROMVISIBLE, first + shown + 12, 0);
        let target = if doc_bottom < 0 {
            total
        } else {
            (doc_bottom as usize + 2).min(total)
        };
        let target = if s.v2.is_some() {
            (target + 128).min(total)
        } else {
            target
        };
        if s.next >= target {
            return;
        }
        let len = self.len();
        let mut lines = Vec::new();
        let mut bytes = 0;
        for n in s.next..target.min(s.next + BATCH_LINES) {
            let at = self.send(SCI_POSITIONFROMLINE, n, 0).max(0) as usize;
            let end = if n + 1 == total {
                len
            } else {
                self.send(SCI_POSITIONFROMLINE, n + 1, 0).max(0) as usize
            };
            let size = end.saturating_sub(at);
            if !lines.is_empty() && bytes + size.min(MAX_LINE) > BATCH_BYTES {
                break;
            }
            if size > MAX_LINE {
                // A single pathological record must not monopolize the parser.
                // Clear its stale native styles in place without copying the line.
                self.send(SCI_STARTSTYLING, at, 0);
                self.send(SCI_SETSTYLING, size.min(MAX_LINE), 0);
                lines.push(Line {
                    offset: at,
                    text: Vec::new(),
                });
            } else {
                bytes += size;
                lines.push(Line {
                    offset: at,
                    text: self.raw_bytes(at, end),
                });
            }
        }
        let next = s.next + lines.len();
        let mut state = s.state.clone();
        let generation = s.generation;
        let set = s.set.clone();
        let (cancel, flag) = {
            let c = Arc::new(AtomicBool::new(false));
            (c.clone(), c)
        };
        let (tx, rx) = mpsc::channel();
        s.pending = Some((cancel, rx));
        #[cfg(windows)]
        let host =
            unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetParent(self.hwnd() as _) }
                as isize;
        submit(self.hwnd(), move || {
            let mut patches = Vec::new();
            for line in lines {
                if flag.load(Ordering::Relaxed) {
                    break;
                }
                if line.text.is_empty() {
                    continue;
                }
                if let Ok(text) = std::str::from_utf8(&line.text) {
                    if let Some(v2) = &mut state.v2 {
                        patches.extend(v2.feed(line.offset, text));
                    } else {
                        patches.push((line.offset, parse_line(&mut state, text, &set)));
                    }
                }
            }
            if tx
                .send(WorkResult {
                    generation,
                    next,
                    state,
                    patches,
                })
                .is_ok()
            {
                #[cfg(target_os = "macos")]
                crate::mac::wake();
                #[cfg(windows)]
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                        host as _,
                        windows_sys::Win32::UI::WindowsAndMessaging::WM_TIMER,
                        1,
                        0,
                    );
                }
            }
        });
    }
}
