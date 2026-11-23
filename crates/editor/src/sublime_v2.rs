//! Compiles the graph of contexts reachable from one Sublime v2 syntax.
//! Matchers run per line. Speculative branches retain up to 128 lines for replay.
use super::grammar::{check_variables, scope_style};
use onig::{MatchParam, Regex, Region, SearchOptions};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};
use syntect::parsing::{Scope, ScopeStack};
use yaml_rust::{Yaml, YamlLoader, yaml::Hash};

fn key(k: &str) -> Yaml {
    Yaml::String(k.to_owned())
}
fn items(value: &Yaml) -> Vec<&str> {
    if let Some(s) = value.as_str() {
        vec![s]
    } else {
        value
            .as_vec()
            .into_iter()
            .flatten()
            .filter_map(Yaml::as_str)
            .collect()
    }
}
fn merged(parent: &Yaml, child: &Yaml) -> Yaml {
    let mut out = child.as_hash().cloned().unwrap_or_default();
    for field in ["variables", "contexts"] {
        let mut merged = parent[field].as_hash().cloned().unwrap_or_default();
        if let Some(entries) = child[field].as_hash() {
            for (k, v) in entries {
                let mut value = v.clone();
                if field == "contexts" {
                    if let (Some(base), Some(rules)) =
                        (merged.get(k).and_then(Yaml::as_vec), v.as_vec())
                    {
                        let prepend = rules
                            .iter()
                            .any(|r| r["meta_prepend"].as_bool() == Some(true));
                        let append = rules
                            .iter()
                            .any(|r| r["meta_append"].as_bool() == Some(true));
                        if prepend || append {
                            let mut list = if prepend { rules.clone() } else { base.clone() };
                            list.extend(if prepend { base.clone() } else { rules.clone() });
                            value = Yaml::Array(list);
                        }
                    }
                }
                merged.insert(k.clone(), value);
            }
        }
        out.insert(key(field), Yaml::Hash(merged));
    }
    Yaml::Hash(out)
}
struct Raw {
    doc: std::sync::OnceLock<Yaml>,
    source: String,
}
impl Raw {
    fn yaml(&self) -> &Yaml {
        self.doc.get_or_init(|| {
            YamlLoader::load_from_str(&self.source)
                .ok()
                .and_then(|mut d| d.drain(..).next())
                .unwrap_or(Yaml::BadValue)
        })
    }
}
#[derive(serde::Deserialize)]
struct Stock {
    path: String,
    scope: String,
    source: String,
}
fn stock() -> &'static [Stock] {
    static DATA: std::sync::OnceLock<Vec<Stock>> = std::sync::OnceLock::new();
    DATA.get_or_init(|| {
        serde_json::from_str(include_str!("../assets/sublime-syntaxes.json"))
            .expect("bundled grammar dependencies")
    })
}
pub(super) struct Catalog {
    docs: Vec<Raw>,
    paths: Vec<PathBuf>,
    aliases: HashMap<String, usize>,
}
impl Catalog {
    pub fn new(sources: &[(PathBuf, String)]) -> Self {
        let mut result = Self {
            docs: vec![],
            paths: vec![],
            aliases: HashMap::new(),
        };
        let mut stems = HashMap::<String, Option<usize>>::new();
        for (path, source) in sources {
            let Ok(mut defs) = YamlLoader::load_from_str(source) else {
                continue;
            };
            let Some(doc) = defs.drain(..).next() else {
                continue;
            };
            let id = result.docs.len();
            let norm = path.to_string_lossy().replace('\\', "/");
            result.aliases.insert(norm.clone(), id);
            if let Some(pos) = norm.rfind("Packages/") {
                result.aliases.insert(norm[pos..].to_owned(), id);
            }
            if let Some(s) = doc["scope"].as_str() {
                result.aliases.insert(format!("scope:{s}"), id);
            }
            if let Some(file) = path.file_name().and_then(|v| v.to_str()) {
                stems
                    .entry(file.to_owned())
                    .and_modify(|v| *v = None)
                    .or_insert(Some(id));
            }
            result.paths.push(path.clone());
            result.docs.push(Raw {
                doc: std::sync::OnceLock::from(doc),
                source: String::new(),
            });
        }
        result
            .aliases
            .extend(stems.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))));
        for d in stock() {
            // Installed packages and user imports take precedence over stock dependencies.
            let id = result.docs.len();
            result.aliases.entry(d.path.clone()).or_insert(id);
            if d.path.starts_with("Legacy/") {
                result
                    .aliases
                    .insert(format!("legacy:scope:{}", d.scope), id);
            }
            result
                .aliases
                .entry(format!("scope:{}", d.scope))
                .or_insert(id);
            if let Some((_, file)) = d.path.rsplit_once('/') {
                result.aliases.entry(file.into()).or_insert(id);
            }
            result.paths.push(PathBuf::from(&d.path));
            result.docs.push(Raw {
                doc: std::sync::OnceLock::new(),
                source: d.source.clone(),
            });
        }
        if let Some(&html) = result.aliases.get("scope:text.html.basic") {
            result
                .aliases
                .entry("scope:text.html".into())
                .or_insert(html);
            result
                .aliases
                .entry("scope:source.html".into())
                .or_insert(html);
        }
        if let Some(&latex) = result.aliases.get("scope:text.tex.latex") {
            result
                .aliases
                .entry("scope:source.tex.latex".into())
                .or_insert(latex);
        }
        result
    }
    fn lookup(&self, name: &str) -> Result<usize, String> {
        self.aliases
            .get(name)
            .or_else(|| name.rsplit('/').next().and_then(|f| self.aliases.get(f)))
            .copied()
            .ok_or_else(|| format!("Install the grammar dependency {name}"))
    }
    fn relative(&self, owner: usize, name: &str) -> Result<usize, String> {
        if !name.starts_with("scope:") && !name.starts_with("Packages/") {
            if let Some(folder) = self.paths[owner].parent() {
                let mut path = folder.to_owned();
                for part in Path::new(name).components() {
                    match part {
                        std::path::Component::Normal(v) => path.push(v),
                        std::path::Component::ParentDir => {
                            path.pop();
                        }
                        _ => {}
                    }
                }
                if let Some(&id) = self.aliases.get(&path.to_string_lossy().replace('\\', "/")) {
                    return Ok(id);
                }
            }
        }
        self.lookup(name)
    }
    fn resolve(&self, id: usize, active: &mut Vec<usize>) -> Result<Yaml, String> {
        if active.contains(&id) || active.len() > 32 {
            return Err("Cyclic or deeply nested syntax inheritance".into());
        }
        active.push(id);
        let child = self.docs[id].yaml();
        let parents = items(&child["extends"]);
        let mut base = Yaml::Hash(Hash::new());
        for name in parents {
            let parent = self.relative(id, name)?;
            let p = self.resolve(parent, active)?;
            if p["version"].as_i64().unwrap_or(1) != child["version"].as_i64().unwrap_or(1) {
                return Err("Parent and child syntax versions differ".into());
            }
            base = merged(&base, &p);
        }
        active.pop();
        let result = merged(&base, child);
        check_variables(&result)?;
        Ok(result)
    }
    pub fn compile(&self, path: &Path) -> Result<Arc<Grammar>, String> {
        // Native Windows GUI stacks can be only 1 MiB. Embedded syntaxes may
        // link hundreds of nested contexts even when the sample text is short.
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("Sublime syntax compiler".into())
                .stack_size(12 * 1024 * 1024)
                .spawn_scoped(scope, || self.compile_graph(path))
                .map_err(|e| e.to_string())?
                .join()
                .map_err(|_| "Grammar compiler stopped".to_owned())?
        })
    }
    fn compile_graph(&self, path: &Path) -> Result<Arc<Grammar>, String> {
        let owner = self
            .paths
            .iter()
            .position(|p| p == path)
            .ok_or("Invalid grammar YAML")?;
        let mut c = Compiler {
            catalog: self,
            defs: HashMap::new(),
            ids: HashMap::new(),
            patterns: HashMap::new(),
            graph: Grammar {
                root: 0,
                scope: vec![],
                contexts: vec![],
                rules: vec![],
            },
            depth: 0,
        };
        c.graph.scope = styles(self.docs[owner].yaml()["scope"].as_str().unwrap_or(""));
        c.graph.root = c.context(owner, "main")?;
        for id in 0..c.graph.contexts.len() {
            let mut out = vec![];
            c.graph.collect(id, true, &mut HashSet::new(), &mut out);
            c.graph.contexts[id].matches = out;
        }
        Ok(Arc::new(c.graph))
    }
}
fn styles(s: &str) -> Vec<u8> {
    s.split_whitespace()
        .map(|word| {
            let mut stack = ScopeStack::new();
            if let Ok(scope) = Scope::new(word) {
                stack.push(scope)
            }
            scope_style(&stack)
        })
        .collect()
}
type Captures = BTreeMap<usize, Vec<u8>>;
fn captures(y: &Yaml) -> Captures {
    y.as_hash()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| {
            Some((
                k.as_i64()
                    .and_then(|n| usize::try_from(n).ok())
                    .or_else(|| k.as_str()?.parse().ok())?,
                styles(v.as_str()?),
            ))
        })
        .collect()
}
fn expand(s: &str, vars: &Yaml, depth: usize) -> Result<String, String> {
    if depth > 64 {
        return Err("Grammar variables nested too deeply".into());
    }
    let mut out = String::new();
    let mut rest = s;
    while let Some((head, tail)) = rest.split_once("{{") {
        let Some((name, next)) = tail.split_once("}}") else {
            break;
        };
        if name.is_empty() || !name.chars().all(|v| v.is_alphanumeric() || v == '_') {
            out.push_str(head);
            out.push_str("{{");
            rest = tail;
            continue;
        }
        out.push_str(head);
        let value = vars[name]
            .as_str()
            .ok_or_else(|| format!("Undefined grammar variable {name}"))?;
        out.push_str(&expand(value, vars, depth + 1)?);
        rest = next;
        if out.len() > 2 * 1024 * 1024 {
            return Err("Expanded regex exceeds 2 MiB".into());
        }
    }
    out.push_str(rest);
    Ok(out)
}
#[derive(Clone)]
struct Pattern {
    source: String,
    position_sensitive: bool,
    compiled: Option<Arc<Regex>>,
}
impl Pattern {
    fn new(s: String, external_backref: bool) -> Result<Self, String> {
        let bound = s
            .as_bytes()
            .windows(2)
            .any(|p| p[0] == b'\\' && matches!(p[1], b'1'..=b'9'));
        // Ruby-syntax Oniguruma rejects a quantified group containing an anchor.
        // In an optional newline-or-end group, the empty end alternative has
        // the same effect as skipping the entire group.
        let compiled = match Regex::new(&s).or_else(|e| {
            if s.contains(r"(?:\n|$)?") {
                Regex::new(&s.replace(r"(?:\n|$)?", r"\n?"))
            } else {
                Err(e)
            }
        }) {
            Ok(r) if !external_backref || !bound => Some(Arc::new(r)),
            _ if bound => None,
            Err(e) => return Err(format!("Invalid match {s}: {e}")),
            Ok(_) => None,
        };
        Ok(Self {
            position_sensitive: s.contains(r"\G"),
            source: s,
            compiled,
        })
    }
    fn bind(&self, caps: &[String]) -> Option<Arc<Regex>> {
        if let Some(r) = &self.compiled {
            return Some(r.clone());
        }
        let mut out = String::new();
        let mut chars = self.source.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                if let Some(next) = chars.next() {
                    if let Some(n) = next.to_digit(10).filter(|&n| n > 0) {
                        let value = caps.get(n as usize)?;
                        for c in value.chars() {
                            if "\\.*+?()[]{}^$|# ".contains(c) {
                                out.push('\\')
                            }
                            out.push(c);
                        }
                    } else {
                        out.push('\\');
                        out.push(next);
                    }
                }
            } else {
                out.push(ch)
            }
        }
        Regex::new(&out).ok().map(Arc::new)
    }
}
#[derive(Default)]
struct ContextDef {
    rules: Vec<Item>,
    matches: Vec<usize>,
    scope: Vec<u8>,
    content: Vec<u8>,
    clear: usize,
    prototype: Option<usize>,
}
enum Item {
    Include(usize, bool),
    Rule(usize),
}
#[derive(Clone)]
enum Action {
    None,
    Push(Vec<usize>),
    Set(Vec<usize>),
    Embed(usize, Pattern, Captures, Vec<u8>),
    Branch(String, Vec<usize>),
    Fail(String),
}
struct Rule {
    pattern: Pattern,
    scope: Vec<u8>,
    captures: Captures,
    pop: usize,
    action: Action,
    extra: Option<usize>,
}
pub(super) struct Grammar {
    root: usize,
    scope: Vec<u8>,
    contexts: Vec<ContextDef>,
    rules: Vec<Rule>,
}
struct Compiler<'a> {
    patterns: HashMap<(String, bool), Pattern>,
    catalog: &'a Catalog,
    defs: HashMap<usize, Arc<Yaml>>,
    ids: HashMap<(usize, String), usize>,
    graph: Grammar,
    depth: usize,
}

impl Grammar {
    fn collect(
        &self,
        id: usize,
        prototype: bool,
        seen: &mut HashSet<(usize, bool)>,
        out: &mut Vec<usize>,
    ) {
        if !seen.insert((id, prototype)) {
            return;
        }
        let ctx = &self.contexts[id];
        if prototype {
            if let Some(p) = ctx.prototype {
                self.collect(p, false, seen, out)
            }
        }
        for rule in &ctx.rules {
            match rule {
                Item::Include(inner, proto) => self.collect(*inner, *proto, seen, out),
                Item::Rule(i) => out.push(*i),
            }
        }
    }
}
#[derive(Clone)]
struct Escape {
    pattern: Arc<Regex>,
    captures: Captures,
    scope: Vec<u8>,
}
#[derive(Clone)]
struct Frame {
    context: usize,
    captures: Arc<Vec<String>>,
    extra: Option<usize>,
    escape: Option<Escape>,
}
impl Frame {
    fn new(context: usize, captures: Arc<Vec<String>>, extra: Option<usize>) -> Self {
        Self {
            context,
            captures,
            extra,
            escape: None,
        }
    }
}
#[derive(Clone)]
struct Branch {
    name: String,
    choices: Vec<usize>,
    next: usize,
    line: u64,
    start: usize,
    end: usize,
    rule: usize,
    stack: Vec<Frame>,
    captured: Arc<Vec<String>>,
    extra: Option<usize>,
}
#[derive(Clone)]
struct SavedLine {
    number: u64,
    offset: usize,
    text: Arc<str>,
    style: Arc<Vec<u8>>,
}
#[derive(Clone)]
pub(super) struct State {
    grammar: Arc<Grammar>,
    stack: Vec<Frame>,
    branches: Vec<Branch>,
    history: VecDeque<SavedLine>,
    number: u64,
}
enum Candidate {
    Rule(usize),
    Escape(usize),
}
type Groups = Vec<Option<(usize, usize)>>;
fn find(rx: &Regex, s: &str, pos: usize) -> Option<Groups> {
    let mut captures = Region::new();
    let mut params = MatchParam::default();
    params.set_retry_limit_in_match(80_000);
    params.set_match_stack_limit(256 * 1024);
    rx.search_with_param(
        s,
        pos,
        s.len(),
        SearchOptions::SEARCH_OPTION_NONE,
        Some(&mut captures),
        params,
    )
    .ok()??;
    Some((0..captures.len()).map(|i| captures.pos(i)).collect())
}
fn color(scopes: &[u8]) -> u8 {
    scopes.iter().rev().copied().find(|&v| v != 0).unwrap_or(0)
}
fn paint(
    out: &mut [u8],
    range: (usize, usize),
    base: &[u8],
    scopes: &[u8],
    caps: &Captures,
    groups: &Groups,
) {
    let (a, b) = range;
    if b <= a {
        return;
    }
    let col = scopes
        .iter()
        .rev()
        .copied()
        .find(|&s| s != 0)
        .unwrap_or_else(|| color(base));
    out[a..b].fill(col);
    for (idx, scopes) in caps {
        if let Some(Some((x, y))) = groups.get(*idx) {
            let (x, y) = ((*x).max(a), (*y).min(b));
            let col = color(scopes);
            if x < y && col != 0 {
                out[x..y].fill(col)
            }
        }
    }
}
impl State {
    pub fn new(grammar: Arc<Grammar>) -> Self {
        Self {
            stack: vec![Frame::new(grammar.root, Arc::new(vec![]), None)],
            grammar,
            branches: vec![],
            history: VecDeque::new(),
            number: 0,
        }
    }
    fn scopes(&self, stack: &[Frame], omit_content: usize) -> Vec<u8> {
        let mut scopes = self.grammar.scope.clone();
        for (i, f) in stack.iter().enumerate() {
            let d = &self.grammar.contexts[f.context];
            scopes.truncate(scopes.len().saturating_sub(d.clear));
            if let Some(e) = &f.escape {
                scopes.extend(&e.scope)
            }
            scopes.extend(&d.scope);
            if i < stack.len().saturating_sub(omit_content) {
                scopes.extend(&d.content)
            }
        }
        scopes
    }
    pub fn feed(&mut self, offset: usize, text: &str) -> Vec<(usize, Vec<u8>)> {
        let last = self.number;
        self.number += 1;
        self.branches.retain(|b| last - b.line <= 128);
        let first = self.branches.first().map_or(last, |b| b.line);
        while self.history.front().is_some_and(|h| h.number < first) {
            self.history.pop_front();
        }
        self.history.push_back(SavedLine {
            number: last,
            offset,
            text: Arc::from(text),
            style: Arc::new(vec![0; text.len()]),
        });
        let mut line = last;
        let mut pos = 0;
        let mut touched = BTreeMap::new();
        let mut ignored: HashSet<usize> = HashSet::new();
        let mut stalled = 0;
        let mut steps = 0;
        let mut cache: HashMap<usize, Option<Groups>> = HashMap::new();
        loop {
            let slot = (line - self.history[0].number) as usize;
            let text = self.history[slot].text.clone();
            let len = text.len();
            let top = self.stack.last().unwrap().clone();
            let mut best: Option<(Candidate, Groups)> = None;
            let mut earliest = usize::MAX;
            // An enclosing embed escape wins ties, even inside a nested string.
            for (i, f) in self.stack.iter().enumerate() {
                if let Some(e) = &f.escape {
                    if let Some(g) = find(&e.pattern, &text, pos) {
                        let at = g[0].unwrap().0;
                        if at < earliest {
                            earliest = at;
                            best = Some((Candidate::Escape(i), g));
                        }
                    }
                }
            }
            let contexts = self
                .stack
                .iter()
                .filter_map(|f| f.extra)
                .chain(std::iter::once(top.context));
            'rules: for c in contexts {
                for &id in &self.grammar.contexts[c].matches {
                    let r = &self.grammar.rules[id];
                    let found = if let Some(rx) = &r.pattern.compiled {
                        if r.pattern.position_sensitive
                            || !cache.contains_key(&id)
                            || cache[&id].as_ref().is_some_and(|g| g[0].unwrap().0 < pos)
                        {
                            cache.insert(id, find(rx, &text, pos));
                        }
                        cache[&id].clone()
                    } else {
                        r.pattern
                            .bind(&top.captures)
                            .and_then(|rx| find(&rx, &text, pos))
                    };
                    if let Some(g) = found {
                        let at = g[0].unwrap().0;
                        if at == pos && ignored.contains(&id) {
                            continue;
                        }
                        if at < earliest {
                            earliest = at;
                            best = Some((Candidate::Rule(id), g));
                        }
                        if earliest == pos {
                            break 'rules;
                        }
                    }
                }
            }
            let base = self.scopes(&self.stack, 0);
            let style = Arc::make_mut(&mut self.history[slot].style);
            let end = earliest.min(len);
            style[pos..end].fill(color(&base));
            touched.insert(line, ());
            let Some((candidate, g)) = best else {
                if line == last {
                    break;
                }
                line += 1;
                pos = 0;
                cache.clear();
                ignored.clear();
                stalled = 0;
                continue;
            };
            let (a, b) = g[0].unwrap();
            let mut retry = false;
            match candidate {
                Candidate::Escape(i) => {
                    let e = self.stack[i].escape.clone().unwrap();
                    self.stack.truncate(i.max(1));
                    let basis = self.scopes(&self.stack, 0);
                    paint(
                        Arc::make_mut(&mut self.history[slot].style).as_mut_slice(),
                        (a, b),
                        &basis,
                        &[],
                        &e.captures,
                        &g,
                    );
                }
                Candidate::Rule(id) => {
                    let r = &self.grammar.rules[id];
                    let saved = if matches!(r.action, Action::Branch(..)) {
                        Some(self.stack.clone())
                    } else {
                        None
                    };
                    let pop = r.pop + usize::from(matches!(r.action, Action::Set(_)));
                    let mut boundary = if pop > 0 {
                        self.scopes(&self.stack, pop)
                    } else {
                        base
                    };
                    self.stack
                        .truncate(self.stack.len().saturating_sub(pop).max(1));
                    let captured = Arc::new(
                        g.iter()
                            .map(|p| p.map(|(x, y)| text[x..y].to_owned()).unwrap_or_default())
                            .collect::<Vec<_>>(),
                    );
                    let mut pushed = 0;
                    match &r.action {
                        Action::Push(targets) | Action::Set(targets) => {
                            for &ctx in targets {
                                self.stack.push(Frame::new(ctx, captured.clone(), r.extra));
                            }
                            pushed = targets.len();
                        }
                        Action::Embed(ctx, rx, sc, scope) => {
                            if let Some(pattern) = rx.bind(&captured) {
                                let mut frame = Frame::new(*ctx, captured.clone(), r.extra);
                                frame.escape = Some(Escape {
                                    pattern,
                                    captures: sc.clone(),
                                    scope: scope.clone(),
                                });
                                self.stack.push(frame);
                            }
                        }
                        Action::Branch(name, choices) => {
                            if self.branches.len() < 128 {
                                self.branches.push(Branch {
                                    name: name.clone(),
                                    choices: choices.clone(),
                                    next: 1,
                                    line,
                                    start: a,
                                    end: b,
                                    rule: id,
                                    stack: saved.unwrap(),
                                    captured: captured.clone(),
                                    extra: r.extra,
                                });
                            }
                            self.stack
                                .push(Frame::new(choices[0], captured.clone(), r.extra));
                            pushed = 1;
                        }
                        Action::Fail(name) => {
                            if let Some(k) = self.branches.iter().rposition(|br| &br.name == name) {
                                let mut br = self.branches[k].clone();
                                self.branches.truncate(k);
                                self.stack = br.stack.clone();
                                line = br.line;
                                cache.clear();
                                ignored.clear();
                                stalled = 0;
                                if let Some(&ctx) = br.choices.get(br.next) {
                                    self.stack
                                        .push(Frame::new(ctx, br.captured.clone(), br.extra));
                                    pos = br.end;
                                    br.next += 1;
                                    self.branches.push(br);
                                } else {
                                    pos = br.start;
                                    ignored.insert(br.rule);
                                }
                                retry = true;
                            }
                        }
                        Action::None => {}
                    }
                    if !retry {
                        if pushed > 0 {
                            boundary = self.scopes(&self.stack, pushed)
                        }
                        paint(
                            Arc::make_mut(&mut self.history[slot].style).as_mut_slice(),
                            (a, b),
                            &boundary,
                            &r.scope,
                            &r.captures,
                            &g,
                        );
                        if a == b && pop == 0 && matches!(&r.action, Action::None | Action::Fail(_))
                        {
                            ignored.insert(id);
                        }
                    }
                }
            }
            if retry {
                steps += 1;
            } else {
                self.branches.retain(|b| self.stack.len() > b.stack.len());
                if b > pos {
                    stalled = 0;
                    ignored.clear()
                } else {
                    stalled += 1
                }
                pos = b;
                steps += 1;
            }
            // Zero-width loops and exponential branch grammars must yield the worker.
            if stalled > 128 || self.stack.len() > 512 || steps > 200_000 {
                self.branches.clear();
                self.stack.truncate(1);
                if line == last {
                    break;
                }
                line = last;
                pos = 0;
                stalled = 0;
                steps = 0;
                cache.clear();
                ignored.clear();
            }
        }
        touched
            .into_keys()
            .map(|n| {
                let line = &self.history[(n - self.history[0].number) as usize];
                (line.offset, (*line.style).clone())
            })
            .collect()
    }
}
impl Compiler<'_> {
    fn pattern(&mut self, source: String, backrefs: bool) -> Result<Pattern, String> {
        use std::collections::hash_map::Entry;
        match self.patterns.entry((source, backrefs)) {
            Entry::Occupied(e) => Ok(e.get().clone()),
            Entry::Vacant(e) => {
                let pattern = Pattern::new(e.key().0.clone(), backrefs)?;
                Ok(e.insert(pattern).clone())
            }
        }
    }
    fn doc(&mut self, owner: usize) -> Result<Arc<Yaml>, String> {
        if let Some(d) = self.defs.get(&owner) {
            return Ok(d.clone());
        }
        let d = Arc::new(self.catalog.resolve(owner, &mut vec![])?);
        self.defs.insert(owner, d.clone());
        Ok(d)
    }
    fn reference(&mut self, owner: usize, s: &str) -> Result<usize, String> {
        if s.starts_with("scope:") || s.contains(".sublime-syntax") {
            let (file, ctx) = s.split_once('#').unwrap_or((s, "main"));
            let id = self.catalog.relative(owner, file)?;
            // Rd and third-party grammars still include named TeX contexts
            // retired from current Sublime packages. Resolve these against the
            // original definitions, including their own matching TeX contexts.
            if let Some(&legacy) = self.catalog.aliases.get(&format!("legacy:{file}")) {
                if self.catalog.paths[owner].starts_with("Legacy")
                    || self.doc(id)?["contexts"][ctx].is_badvalue()
                {
                    return self.context(legacy, ctx);
                }
            }
            return self.context(id, ctx);
        }
        self.context(owner, s)
    }
    fn context(&mut self, owner: usize, name: &str) -> Result<usize, String> {
        if let Some(id) = self.ids.get(&(owner, name.to_owned())) {
            return Ok(*id);
        }
        let doc = self.doc(owner)?;
        let rules = doc["contexts"][name].as_vec().ok_or_else(|| {
            format!(
                "Missing context {name} in {}",
                self.catalog.paths[owner].display()
            )
        })?;
        let id = self.graph.contexts.len();
        self.ids.insert((owner, name.to_owned()), id);
        self.graph.contexts.push(ContextDef::default());
        self.fill(id, owner, rules, name != "prototype")?;
        Ok(id)
    }
    fn anonymous(&mut self, owner: usize, rules: &[Yaml]) -> Result<usize, String> {
        let id = self.graph.contexts.len();
        self.graph.contexts.push(ContextDef::default());
        self.fill(id, owner, rules, true)?;
        Ok(id)
    }
    fn targets(&mut self, owner: usize, y: &Yaml) -> Result<Vec<usize>, String> {
        if let Some(s) = y.as_str() {
            return Ok(vec![self.reference(owner, s)?]);
        }
        let list = y.as_vec().ok_or("Expected a context or context list")?;
        if list.iter().any(|e| e.as_hash().is_some()) {
            return Ok(vec![self.anonymous(owner, list)?]);
        }
        list.iter()
            .map(|v| {
                if let Some(block) = v.as_vec() {
                    self.anonymous(owner, block)
                } else {
                    self.reference(
                        owner,
                        v.as_str()
                            .ok_or_else(|| format!("Invalid context reference {v:?}"))?,
                    )
                }
            })
            .collect()
    }
    fn fill(
        &mut self,
        id: usize,
        owner: usize,
        rules: &[Yaml],
        prototype: bool,
    ) -> Result<(), String> {
        if self.depth > 384 || self.graph.contexts.len() > 65536 {
            return Err(format!(
                "Grammar graph exceeds parser limits at depth {} with {} contexts",
                self.depth,
                self.graph.contexts.len()
            ));
        }
        self.depth += 1;
        let doc = self.doc(owner)?;
        let variables = &doc["variables"];
        let mut ctx = ContextDef::default();
        if prototype
            && !rules
                .iter()
                .any(|r| r["meta_include_prototype"].as_bool() == Some(false))
            && doc["contexts"]["prototype"].as_vec().is_some()
        {
            ctx.prototype = Some(self.context(owner, "prototype")?);
        }
        for row in rules {
            if let Some(s) = row["meta_scope"].as_str() {
                ctx.scope.extend(styles(s))
            }
            if let Some(s) = row["meta_content_scope"].as_str() {
                ctx.content.extend(styles(s))
            }
            if let Some(n) = row["clear_scopes"].as_i64() {
                ctx.clear = n.max(0) as usize
            }
            if row["clear_scopes"].as_bool() == Some(true) {
                ctx.clear = usize::MAX
            }
            if let Some(s) = row["include"].as_str() {
                ctx.rules.push(Item::Include(
                    self.reference(owner, s)?,
                    row["apply_prototype"].as_bool() == Some(true),
                ));
            }
            let Some(s) = row["match"].as_str() else {
                continue;
            };
            let pop = row["pop"]
                .as_i64()
                .map(|n| n.max(0) as usize)
                .unwrap_or_else(|| usize::from(row["pop"].as_bool() == Some(true)));
            let pattern = self.pattern(expand(s, variables, 0)?, pop > 0)?;
            let extra = if let Some(r) = row["with_prototype"].as_vec() {
                Some(self.anonymous(owner, r)?)
            } else {
                None
            };
            let action = if !row["push"].is_badvalue() {
                Action::Push(self.targets(owner, &row["push"])?)
            } else if !row["set"].is_badvalue() {
                Action::Set(self.targets(owner, &row["set"])?)
            } else if let Some(s) = row["embed"].as_str() {
                let dest = self.reference(owner, s)?;
                let escape = row["escape"].as_str().ok_or("Embed requires escape")?;
                let es = if let Some(scope) = row["embed_scope"].as_str() {
                    styles(scope)
                } else if s.starts_with("scope:") {
                    styles(
                        s.trim_start_matches("scope:")
                            .split('#')
                            .next()
                            .unwrap_or(""),
                    )
                } else {
                    vec![]
                };
                Action::Embed(
                    dest,
                    self.pattern(expand(escape, variables, 0)?, true)?,
                    captures(&row["escape_captures"]),
                    es,
                )
            } else if !row["branch"].is_badvalue() {
                let choices = self.targets(owner, &row["branch"])?;
                if choices.is_empty() {
                    return Err("Empty syntax branch".into());
                }
                Action::Branch(
                    row["branch_point"]
                        .as_str()
                        .ok_or("Branch requires a branch_point")?
                        .into(),
                    choices,
                )
            } else if let Some(name) = row["fail"].as_str() {
                Action::Fail(name.into())
            } else {
                Action::None
            };
            let index = self.graph.rules.len();
            self.graph.rules.push(Rule {
                pattern,
                scope: styles(row["scope"].as_str().unwrap_or("")),
                captures: captures(&row["captures"]),
                action,
                pop,
                extra,
            });
            ctx.rules.push(Item::Rule(index));
        }
        self.graph.contexts[id] = ctx;
        self.depth -= 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source(name: &str, body: &str) -> (PathBuf, String) {
        (
            PathBuf::from(format!("Packages/Test/{name}.sublime-syntax")),
            format!("name: {name}\nversion: 2\nscope: source.{name}\n{body}"),
        )
    }
    #[test]
    fn inherited_variables_multiline_branch_replay_and_embedding() {
        let base = source(
            "Base",
            r#"variables:
  word: beta
contexts:
  main:
    - match: '{{word}}'
      scope: keyword
    - match: '(?=\()'
      branch_point: kind
      branch: [first, second]
    - match: '<'
      embed: scope:source.Inner
      escape: '>'
      escape_captures: {0: punctuation.end}
  first:
    - match: '\('
      scope: string
    - match: '!'
      fail: kind
  second:
    - match: '\('
      scope: comment
    - match: '!'
      scope: constant
      pop: true
"#,
        );
        let derived = source(
            "Derived",
            r#"extends: Packages/Test/Base.sublime-syntax
variables:
  word: alpha
contexts:
  main:
    - meta_prepend: true
    - match: '@'
      scope: entity.name
"#,
        );
        let inner = source(
            "Inner",
            "contexts:\n  main:\n    - match: x\n      scope: string\n",
        );
        let items = [base, derived, inner];
        let c = Catalog::new(&items);
        let mut state = State::new(c.compile(&items[1].0).unwrap());
        assert_eq!(state.feed(0, "@alpha(\n")[0].1, [5, 3, 3, 3, 3, 3, 2, 0]);
        let updates = state.feed(8, "!<x>\n");
        assert_eq!(updates[0].1[6], 1);
        assert_eq!(updates[1].1, [4, 0, 2, 7, 0]);
    }
    #[test]
    fn stock_json_and_inherited_sql_attach_from_package_directory() {
        // Unmodified syntax sources from sublimehq/Packages at 91fad583b0afcc82735bb7fa637e20c561a453cf.
        let cases = [
            (
                "Packages/JSON/JSON.sublime-syntax",
                "[true, false, null]\n",
                "true",
                4,
            ),
            (
                "Packages/SQL/SQL.sublime-syntax",
                "SELECT * FROM accounts;\n",
                "SELECT",
                3,
            ),
            (
                "Packages/JavaScript/JavaScript.sublime-syntax",
                "const view = true;\n",
                "const",
                3,
            ),
        ];
        for (path, text, token, expected) in cases {
            let item = stock().iter().find(|g| g.path == path).unwrap();
            let absolute = std::env::temp_dir().join(path);
            let catalog = Catalog::new(&[(absolute.clone(), item.source.clone())]);
            let mut parser = State::new(catalog.compile(&absolute).unwrap());
            let styled = parser.feed(0, text);
            let at = text.find(token).unwrap();
            assert_eq!(
                &styled[0].1[at..at + token.len()],
                vec![expected; token.len()],
                "{path}"
            );
        }
    }
}

#[cfg(test)]
#[test]
#[ignore = "Compiles the full upstream Sublime v2 grammar corpus"]
fn upstream_v2_definitions_compile_and_parse_a_line() {
    let catalog = Catalog::new(&[]);
    let input = "function example() { return 42; }\n";
    let mut count = 0;
    for syntax in stock()
        .iter()
        .filter(|s| s.source.lines().any(|line| line.starts_with("version: 2")))
    {
        let grammar = catalog
            .compile(Path::new(&syntax.path))
            .unwrap_or_else(|e| panic!("{}: {e}", syntax.path));
        let mut parser = State::new(grammar);
        let patches = parser.feed(0, input);
        assert_eq!(
            patches.last().unwrap().1.len(),
            input.len(),
            "{}",
            syntax.path
        );
        count += 1;
    }
    assert_eq!(count, 176);
}
