use crate::search::{path_key, walker, OpenBuffer};
use crate::text::open_text;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    OnceLock,
};
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub path: PathBuf,
    pub offset: u64,
    pub line: u64,
    pub column: u64,
}
#[derive(Clone, Debug)]
struct Use {
    file: u32,
    line: u32,
    column: u32,
    offset: u32,
}
#[derive(Default, Clone, Debug)]
pub struct Index {
    pub symbols: Vec<Symbol>,
    pub files: Vec<PathBuf>,
    pub all_files: Vec<PathBuf>,
    /// Ranked on the worker so opening a large workspace keeps the UI responsive.
    pub completion_words: Vec<String>,
    pub skipped: usize,
    pub cancelled: bool,
    pub limited: bool,
    pub issues: Vec<String>,
    uses: HashMap<String, Vec<Use>>,
    words: HashMap<String, u32>,
    count: usize,
}
impl Index {
    pub fn vocabulary(&self, limit: usize) -> Vec<String> {
        let mut rows: Vec<_> = self.words.iter().collect();
        rows.sort_unstable_by(|(a, ac), (b, bc)| bc.cmp(ac).then_with(|| a.cmp(b)));
        rows.into_iter()
            .take(limit)
            .map(|(name, _)| name.clone())
            .collect()
    }
    pub fn definitions(&self, name: &str) -> Vec<Symbol> {
        self.symbols
            .iter()
            .filter(|s| s.name == name)
            .cloned()
            .collect()
    }
    pub fn references(&self, name: &str) -> Vec<Symbol> {
        self.uses
            .get(name)
            .into_iter()
            .flatten()
            .map(|u| Symbol {
                name: name.to_owned(),
                kind: "use".into(),
                path: self.files[u.file as usize].clone(),
                line: u.line as u64,
                column: u.column as u64,
                offset: u.offset as u64,
            })
            .collect()
    }
    pub fn complete(&self, prefix: &str, limit: usize) -> Vec<String> {
        if prefix.is_empty() || limit == 0 {
            return Vec::new();
        }
        let low = prefix.to_lowercase();
        let mut rows: Vec<_> = self
            .words
            .iter()
            .filter(|(word, _)| word.as_str() != prefix && word.to_lowercase().starts_with(&low))
            .collect();
        rows.sort_unstable_by(|(a, ac), (b, bc)| {
            b.starts_with(prefix)
                .cmp(&a.starts_with(prefix))
                .then_with(|| bc.cmp(ac))
                .then_with(|| a.cmp(b))
        });
        rows.into_iter()
            .take(limit)
            .map(|(word, _)| word.clone())
            .collect()
    }
}
pub const MAX_SYMBOL_SOURCE: usize = 4 * 1024 * 1024;
const MAX_USES: usize = 1_000_000;

pub fn index(root: &Path, cancel: &AtomicBool) -> Index {
    index_with_buffers(root, &[], cancel)
}
pub fn index_with_buffers(root: &Path, buffers: &[OpenBuffer], cancel: &AtomicBool) -> Index {
    let mut index = Index::default();
    let mut seen = HashSet::new();
    for b in buffers {
        if crate::relative_path(&b.path, root).is_none() || !seen.insert(path_key(&b.path)) {
            continue;
        }
        index.all_files.push(b.path.clone());
        if b.text.len() > MAX_SYMBOL_SOURCE {
            index.skipped += 1;
            continue;
        }
        index_source(&mut index, &b.path, &b.text, cancel);
    }
    for entry in walker(root, false).flatten() {
        if cancel.load(Ordering::Relaxed) {
            index.cancelled = true;
            break;
        }
        if !entry.file_type().is_some_and(|f| f.is_file()) {
            continue;
        }
        let path = entry.path();
        if seen.contains(&path_key(path)) {
            continue;
        }
        index.all_files.push(path.into());
        if index.limited || lang(path).is_none() {
            continue;
        }
        if !entry
            .metadata()
            .is_ok_and(|m| m.len() <= MAX_SYMBOL_SOURCE as u64)
        {
            index.skipped += 1;
            continue;
        }
        let Ok((r, enc, bom)) = open_text(path) else {
            index.skipped += 1;
            continue;
        };
        let mut text = String::new();
        let mut result = r
            .take(MAX_SYMBOL_SOURCE as u64 + 1)
            .read_to_string(&mut text);
        if result
            .as_ref()
            .is_err_and(|e| e.kind() == std::io::ErrorKind::InvalidData)
            && enc == crate::text::Encoding::Utf8
            && bom == 0
        {
            text.clear();
            result = std::fs::File::open(path)
                .and_then(|f| crate::text::reader_as(f, crate::text::Encoding::Windows1252, 0))
                .and_then(|(r, _, _)| {
                    r.take(MAX_SYMBOL_SOURCE as u64 + 1)
                        .read_to_string(&mut text)
                });
        }
        if result.is_err() || text.len() > MAX_SYMBOL_SOURCE {
            index.skipped += 1;
            continue;
        }
        index_source(&mut index, path, &text, cancel);
    }
    index
        .symbols
        .sort_by(|a, b| a.path.cmp(&b.path).then(a.offset.cmp(&b.offset)));
    if !index.cancelled {
        index.completion_words = index.vocabulary(8000);
    }
    index
}
/// Function list for an in-memory editor buffer, including unsaved edits.
pub fn symbols_for_file(path: &Path, text: &str) -> Vec<Symbol> {
    index_file(path, text).symbols
}
pub fn index_file(path: &Path, text: &str) -> Index {
    index_file_as(path, text, lang(path))
}
/// The editor can supply the parser selected by detection or by a manual override.
pub fn index_file_as(path: &Path, text: &str, language: Option<&str>) -> Index {
    if text.len() > MAX_SYMBOL_SOURCE {
        return Index::default();
    }
    let mut index = Index::default();
    if let Some(language) = language {
        index_language(&mut index, path, text, language, &AtomicBool::new(false));
    }
    index
}
pub fn references(root: &Path, name: &str, cancel: &AtomicBool) -> Vec<Symbol> {
    index(root, cancel).references(name)
}

fn lang(path: &Path) -> Option<&'static str> {
    Some(
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "rs" => "rust",
            "py" | "pyi" | "pyw" => "python",
            "js" | "mjs" | "jsx" | "cjs" => "javascript",
            "ts" | "mts" | "cts" => "typescript",
            "tsx" => "tsx",
            "go" => "go",
            "c" => "c",
            "h" | "hpp" | "hxx" | "cpp" | "cc" | "cxx" => "cpp",
            "cs" => "csharp",
            "java" => "java",
            "ps1" | "psm1" => "powershell",
            "sh" | "bash" | "zsh" => "shell",
            "md" | "mdx" | "markdown" => "markdown",
            "sql" => "sql",
            "rb" => "ruby",
            "lua" => "lua",
            _ => return None,
        },
    )
}

struct Grammar {
    lang: Language,
    tags: Query,
}
static GRAMMARS: OnceLock<HashMap<&'static str, Result<Grammar, String>>> = OnceLock::new();
fn grammars() -> &'static HashMap<&'static str, Result<Grammar, String>> {
    GRAMMARS.get_or_init(|| {
        let mut rows = HashMap::new();
        // TypeScript's distributed query augments JavaScript. Stable common
        // node names are used here across TS/TSX grammar versions.
        let js_compatible = r#"
         (function_declaration name: (identifier) @name) @definition.function
         (method_definition name: (property_identifier) @name) @definition.method
         (class_declaration name: (type_identifier) @name) @definition.class
         (variable_declarator name: (identifier) @name value: (arrow_function)) @definition.function
         (type_alias_declaration name: (type_identifier) @name) @definition.type
         (enum_declaration name: (identifier) @name) @definition.enum
        "#;
        let ts = format!("{}\n{}", tree_sitter_typescript::TAGS_QUERY, js_compatible);
        let langs = [
            (
                "rust",
                tree_sitter_rust::LANGUAGE,
                tree_sitter_rust::TAGS_QUERY,
            ),
            (
                "python",
                tree_sitter_python::LANGUAGE,
                tree_sitter_python::TAGS_QUERY,
            ),
            (
                "javascript",
                tree_sitter_javascript::LANGUAGE,
                tree_sitter_javascript::TAGS_QUERY,
            ),
            (
                "typescript",
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
                ts.as_str(),
            ),
            ("tsx", tree_sitter_typescript::LANGUAGE_TSX, ts.as_str()),
            ("go", tree_sitter_go::LANGUAGE, tree_sitter_go::TAGS_QUERY),
            ("c", tree_sitter_c::LANGUAGE, tree_sitter_c::TAGS_QUERY),
            (
                "cpp",
                tree_sitter_cpp::LANGUAGE,
                tree_sitter_cpp::TAGS_QUERY,
            ),
            (
                "csharp",
                tree_sitter_c_sharp::LANGUAGE,
                tree_sitter_c_sharp::TAGS_QUERY,
            ),
            (
                "java",
                tree_sitter_java::LANGUAGE,
                tree_sitter_java::TAGS_QUERY,
            ),
        ];
        for (name, language, source) in langs {
            let language = language.into();
            let tags = Query::new(&language, source)
                .map(|tags| Grammar {
                    lang: language,
                    tags,
                })
                .map_err(|e| e.to_string());
            rows.insert(name, tags);
        }
        rows
    })
}
fn outline_re(lang: &str) -> Option<&'static Regex> {
    static SIMPLE: OnceLock<HashMap<&str, Regex>> = OnceLock::new();
    SIMPLE.get_or_init(||[
        ("powershell",r"(?i)^\s*(?:function|filter|class|enum)\s+([\w-]+)"),
        ("shell",r"^\s*(?:function\s+([\w-]+)|([\w-]+)\s*\(\s*\)\s*\{)"),
        ("ruby",r"^\s*(?:def|class|module)\s+([\w.:!?]+)"),
        ("lua",r"^\s*(?:local\s+)?function\s+([\w.:]+)"),
        ("markdown",r"^\s{0,3}#{1,6}\s+(.+?)\s*#*\s*$"),
        ("sql",r#"(?i)^\s*create\s+(?:or\s+replace\s+)?(?:table|view|procedure|function|index|trigger)\s+(?:if\s+not\s+exists\s+)?([\w."\[\]]+)"#),
    ].into_iter().map(|(k,p)|(k,Regex::new(p).unwrap())).collect()).get(lang)
}
/// Translate Tree-sitter byte columns while walking names in source order.
/// Counting from line start for each name is quadratic on minified code.
#[derive(Default)]
struct ColumnCursor {
    line_start: usize,
    position: usize,
    column: u32,
}
impl ColumnCursor {
    fn at(&mut self, text: &str, byte: usize, byte_column: usize) -> u32 {
        let line_start = byte - byte_column;
        if line_start != self.line_start || byte < self.position {
            self.line_start = line_start;
            self.position = line_start;
            self.column = 0;
        }
        self.column += text[self.position..byte].chars().count() as u32;
        self.position = byte;
        self.column + 1
    }
}
fn index_source(index: &mut Index, path: &Path, text: &str, cancel: &AtomicBool) {
    let Some(key) = lang(path) else {
        return;
    };
    index_language(index, path, text, key, cancel);
}
fn index_language(index: &mut Index, path: &Path, text: &str, key: &str, cancel: &AtomicBool) {
    let file = index.files.len() as u32;
    index.files.push(path.into());
    if let Some(pattern) = outline_re(key) {
        let mut at = 0;
        for (row, s) in text.split_inclusive(['\n']).enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            if let Some(m) = pattern
                .captures(s.trim_end_matches(['\n', '\r']))
                .and_then(|c| c.iter().skip(1).flatten().next())
            {
                index.symbols.push(Symbol {
                    name: m.as_str().into(),
                    kind: if key == "markdown" {
                        "heading"
                    } else {
                        "definition"
                    }
                    .into(),
                    path: path.into(),
                    offset: (at + m.start()) as u64,
                    line: row as u64 + 1,
                    column: s[..m.start()].chars().count() as u64 + 1,
                });
                *index.words.entry(m.as_str().into()).or_default() += 1;
            }
            at += s.len();
        }
        return;
    }
    let Some(grammar) = grammars().get(key) else {
        return;
    };
    let grammar = match grammar {
        Ok(g) => g,
        Err(reason) => {
            index.issues.push(format!("{key} symbols: {reason}"));
            return;
        }
    };
    let mut parser = Parser::new();
    if parser.set_language(&grammar.lang).is_err() {
        return;
    }
    let mut cancel_parse = |_: &tree_sitter::ParseState| cancel.load(Ordering::Relaxed);
    let source = text.as_bytes();
    let Some(tree) = parser.parse_with_options(
        &mut |i, _| source.get(i..).unwrap_or_default(),
        None,
        Some(tree_sitter::ParseOptions::new().progress_callback(&mut cancel_parse)),
    ) else {
        return;
    };
    let mut query_cursor = QueryCursor::new();
    let mut matches = query_cursor.matches(&grammar.tags, tree.root_node(), source);
    let mut declared = HashSet::new();
    let mut columns = ColumnCursor::default();
    while let Some(m) = matches.next() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let labels = grammar.tags.capture_names();
        let Some(kind) = m
            .captures
            .iter()
            .find_map(|c| labels[c.index as usize].strip_prefix("definition."))
        else {
            continue;
        };
        let Some(c) = m
            .captures
            .iter()
            .find(|c| labels[c.index as usize] == "name")
        else {
            continue;
        };
        if !declared.insert(c.node.start_byte()) {
            continue;
        }
        let Some(name) = text
            .get(c.node.byte_range())
            .filter(|n| !n.is_empty() && n.len() <= 256)
        else {
            continue;
        };
        let point = c.node.start_position();
        let column = columns.at(text, c.node.start_byte(), point.column);
        index.symbols.push(Symbol {
            name: name.into(),
            kind: kind.into(),
            path: path.into(),
            offset: c.node.start_byte() as u64,
            line: point.row as u64 + 1,
            column: column as u64,
        });
    }
    // Collect identifiers from the syntax tree, avoiding strings and comments.
    // A reference is a spelling match within parsed code, not a type-checked binding.
    let mut cursor = tree.walk();
    let mut columns = ColumnCursor::default();
    let mut visited = 0;
    loop {
        visited += 1;
        if visited % 1024 == 0 && cancel.load(Ordering::Relaxed) {
            break;
        }
        let node = cursor.node();
        let kind = node.kind();
        let pos = node.start_byte();
        if (kind == "identifier" || kind.ends_with("_identifier") || kind == "field_name")
            && node.child_count() == 0
        {
            if let Some(name) = text
                .get(node.byte_range())
                .filter(|n| !n.is_empty() && n.len() <= 128)
            {
                let freq = index.words.entry(name.into()).or_default();
                *freq = freq.saturating_add(1);
                if !declared.contains(&pos) {
                    let p = node.start_position();
                    index.uses.entry(name.into()).or_default().push(Use {
                        file,
                        line: p.row as u32 + 1,
                        // Only the public display column needs scalar counting.
                        column: columns.at(text, pos, p.column),
                        offset: pos as u32,
                    });
                    index.count += 1;
                    if index.count >= MAX_USES {
                        index.limited = true;
                        break;
                    }
                }
            }
        }
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return;
            }
        }
    }
}
