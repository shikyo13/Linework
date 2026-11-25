//! Choosing files by name or type, as ripgrep's `-g`, `--iglob`, `-t`, `-T`,
//! `--type-add` and `--type-clear` do, and grep's `--include`, `--exclude`
//! and `--exclude-dir`.
//!
//! A glob is written as a line of a `.gitignore` file, with the meaning turned
//! round: a glob picks the files it matches, and one starting with `!` leaves
//! them out. When any glob picks, only picked files are searched. A glob with
//! no `/` matches a name at any depth; one with a `/` matches a path from the
//! searched folder. `{a,b}` stands for either. The last glob that matches
//! decides, and a type left out with `-T` always leaves out.
//!
//! Folders: a glob that leaves a folder out (`-g '!target'`) keeps the walk
//! out of it; a glob that picks files never stops the walk going into a
//! folder it does not name, as in ripgrep.
use crate::ignore::{Rules, Verdict};
use std::collections::BTreeMap;

/// The file types lw knows before any `--type-add`.
pub const TYPES: &[(&str, &[&str])] = &[
    ("c", &["*.c", "*.h"]),
    ("cpp", &["*.cpp", "*.cc", "*.cxx", "*.c++", "*.hpp", "*.hh", "*.hxx", "*.h++", "*.h", "*.inl"]),
    ("cs", &["*.cs", "*.csx"]),
    ("css", &["*.css", "*.scss", "*.sass", "*.less"]),
    ("go", &["*.go"]),
    ("html", &["*.html", "*.htm", "*.xhtml"]),
    ("java", &["*.java"]),
    ("js", &["*.js", "*.jsx", "*.mjs", "*.cjs", "*.vue"]),
    ("json", &["*.json", "*.jsonc", "*.json5"]),
    ("kotlin", &["*.kt", "*.kts"]),
    ("log", &["*.log"]),
    ("lua", &["*.lua"]),
    ("md", &["*.md", "*.mdx", "*.markdown"]),
    ("php", &["*.php"]),
    ("ps", &["*.ps1", "*.psm1", "*.psd1"]),
    ("py", &["*.py", "*.pyi", "*.pyw"]),
    ("rb", &["*.rb", "*.rake", "Gemfile", "Rakefile"]),
    ("rust", &["*.rs"]),
    ("sh", &["*.sh", "*.bash", "*.zsh"]),
    ("sql", &["*.sql"]),
    ("swift", &["*.swift"]),
    ("toml", &["*.toml"]),
    ("ts", &["*.ts", "*.tsx", "*.mts", "*.cts"]),
    ("txt", &["*.txt"]),
    ("xml", &["*.xml", "*.xsd", "*.xsl", "*.xslt", "*.svg"]),
    ("yaml", &["*.yaml", "*.yml"]),
];

/// File types by name, with the globs that make each.
#[derive(Clone, Debug)]
pub struct Types {
    map: BTreeMap<String, Vec<String>>,
}

impl Default for Types {
    fn default() -> Self {
        Self { map: TYPES.iter().map(|(n, g)| ((*n).to_owned(), g.iter().map(|g| (*g).to_owned()).collect())).collect() }
    }
}

impl Types {
    /// `--type-add`: `name:glob[,glob...]` adds globs to a type (making it if
    /// new); `name:include:a,b` adds the globs of the types a and b.
    pub fn add(&mut self, spec: &str) -> Result<(), String> {
        let (name, rest) = spec.split_once(':').ok_or_else(|| format!("--type-add needs name:glob, not {spec}"))?;
        if name.is_empty() || !name.chars().all(char::is_alphanumeric) {
            return Err("invalid definition (format is type:glob, e.g., html:*.html); a type name is letters and digits".into());
        }
        let globs: Vec<String> = match rest.strip_prefix("include:") {
            Some(others) => {
                let mut globs = Vec::new();
                for other in others.split(',') {
                    globs.extend(self.globs(other)?.iter().cloned());
                }
                globs
            }
            None => vec![rest.to_owned()].into_iter().filter(|g| !g.is_empty()).collect(),
        };
        if globs.is_empty() {
            return Err(format!("--type-add {spec} names no globs"));
        }
        let entry = self.map.entry(name.to_owned()).or_default();
        for glob in globs {
            if !entry.contains(&glob) {
                entry.push(glob);
            }
        }
        Ok(())
    }

    /// `--type-clear`: the type has no globs from here on.
    pub fn clear(&mut self, name: &str) {
        self.map.remove(name);
    }

    pub fn globs(&self, name: &str) -> Result<&[String], String> {
        self.map.get(name).map(Vec::as_slice).ok_or_else(|| format!("unrecognized file type: {name} (lw --type-list lists them)"))
    }

    /// The globs of a type named on the command line: `all` stands for every type.
    pub fn globs_of(&self, name: &str) -> Result<Vec<String>, String> {
        if name == "all" {
            return Ok(self.map.values().flatten().cloned().collect());
        }
        self.globs(name).map(<[String]>::to_vec)
    }

    /// Every type and its globs, by name.
    pub fn list(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.map.iter().map(|(n, g)| (n.as_str(), g.as_slice()))
    }
}

/// What chooses files, in the order given on the command line.
#[derive(Clone, Debug)]
pub enum Choice {
    /// `-g` (`fold` false) or `--iglob` (`fold` true); a leading `!` leaves out.
    Glob { glob: String, fold: bool },
    /// `-t`
    Type(String),
    /// `-T`
    NotType(String),
    /// grep's `--include`, `--exclude`: a name glob that picks or leaves out.
    Include(String),
    Exclude(String),
    /// grep's `--exclude-dir`: folders not gone into.
    ExcludeDir(String),
}

/// Which files of a folder are searched. Globs come first and outrank every
/// other rule (ignore files, the hidden-file rule), as ripgrep's do; types
/// come after the ignore files and cannot bring back a file they leave out.
#[derive(Debug)]
pub struct Pick {
    globs: Rules,
    /// Whether some glob picks; then a file no glob matches is left out.
    picking_globs: bool,
    types: Rules,
    picking_types: bool,
    /// Folders left out by grep's --exclude-dir.
    folders: Rules,
}

/// What the globs say about a path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Said {
    /// A glob picks it: searched whatever the other rules say.
    Picked,
    /// A glob leaves it out.
    LeftOut,
    /// No glob names it.
    Silent,
}

/// `{a,b}` in a glob, as every glob it stands for. Groups do not nest.
fn spread(glob: &str) -> Vec<String> {
    let Some(open) = glob.find('{') else { return vec![glob.to_owned()] };
    let Some(close) = glob[open..].find('}').map(|c| open + c) else { return vec![glob.to_owned()] };
    let (head, tail) = (&glob[..open], &glob[close + 1..]);
    glob[open + 1..close].split(',').flat_map(|choice| spread(&format!("{head}{choice}{tail}"))).collect()
}

impl Pick {
    /// From the choices in order, with `-g`'s case setting (`--glob-case-insensitive`).
    pub fn new(choices: &[Choice], types: &Types, globs_fold: bool) -> Result<Option<Self>, String> {
        if choices.is_empty() {
            return Ok(None);
        }
        let mut globs: Vec<(String, bool)> = Vec::new();
        let mut kinds: Vec<(String, bool)> = Vec::new();
        let mut kinds_last: Vec<(String, bool)> = Vec::new();
        let mut folders: Vec<(String, bool)> = Vec::new();
        let add = |lines: &mut Vec<(String, bool)>, text: &str, fold: bool| {
            let (bang, bare) = match text.strip_prefix('!') {
                Some(bare) => ("!", bare),
                None => ("", text),
            };
            lines.extend(spread(bare).into_iter().map(|g| (format!("{bang}{g}"), fold)));
        };
        for choice in choices {
            match choice {
                Choice::Glob { glob, fold } => add(&mut globs, glob, *fold || globs_fold),
                Choice::Include(text) => add(&mut globs, text, false),
                Choice::Exclude(text) => add(&mut globs, &format!("!{text}"), false),
                Choice::ExcludeDir(text) => add(&mut folders, text, false),
                Choice::Type(name) => kinds.extend(types.globs_of(name)?.iter().flat_map(|g| spread(g)).map(|g| (g, false))),
                Choice::NotType(name) => kinds_last.extend(types.globs_of(name)?.iter().flat_map(|g| spread(g)).map(|g| (format!("!{g}"), false))),
            }
        }
        kinds.extend(kinds_last);
        // A glob picks where a .gitignore line would ignore, so verdicts read the other way round.
        let picking = |lines: &[(String, bool)]| lines.iter().any(|(l, _)| !l.starts_with('!'));
        let rules = |lines: &[(String, bool)], source: &str| Rules::from_lines(lines.iter().map(|(l, f)| (l.as_bytes(), *f)), source);
        Ok(Some(Self {
            picking_globs: picking(&globs),
            picking_types: picking(&kinds),
            globs: rules(&globs, "-g"),
            types: rules(&kinds, "-t"),
            folders: rules(&folders, "--exclude-dir"),
        }))
    }

    /// What the globs say about a path (relative to the searched folder, `/`
    /// between names). A file no glob names is left out when globs pick; a
    /// folder is never left out for that.
    pub fn globs_say(&self, path: &str, is_folder: bool) -> Said {
        if is_folder && self.folders.verdict(path.as_bytes(), true) == Some(Verdict::Ignored) {
            return Said::LeftOut;
        }
        match self.globs.verdict(path.as_bytes(), is_folder) {
            Some(Verdict::Ignored) => Said::Picked,
            Some(Verdict::Kept) => Said::LeftOut,
            None if self.picking_globs && !is_folder => Said::LeftOut,
            None => Said::Silent,
        }
    }

    /// What the types say about a file the globs said nothing about: picked
    /// (which, as in ripgrep, also lifts the hidden-file rule), left out, or
    /// nothing when no type was chosen.
    pub fn types_say(&self, path: &str) -> Said {
        match self.types.verdict(path.as_bytes(), false) {
            Some(Verdict::Ignored) => Said::Picked,
            Some(Verdict::Kept) => Said::LeftOut,
            None if self.picking_types => Said::LeftOut,
            None => Said::Silent,
        }
    }

    /// Whether the types keep a file the globs said nothing about.
    pub fn types_keep(&self, path: &str) -> bool {
        self.types_say(path) != Said::LeftOut
    }

    /// Whether a file is searched, as far as these choices go.
    pub fn keeps(&self, path: &str) -> bool {
        match self.globs_say(path, false) {
            Said::Picked => true,
            Said::LeftOut => false,
            Said::Silent => self.types_keep(path),
        }
    }

    /// Whether the walk goes into a folder, as far as these choices go.
    pub fn enters(&self, path: &str) -> bool {
        self.globs_say(path, true) != Said::LeftOut
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(choices: Vec<Choice>) -> Pick {
        Pick::new(&choices, &Types::default(), false).unwrap().unwrap()
    }
    fn g(text: &str) -> Choice {
        Choice::Glob { glob: text.into(), fold: false }
    }

    #[test]
    fn globs_and_types_choose_files_as_ripgrep_does() {
        let ts = pick(vec![g("*.{ts,tsx}")]);
        assert!(ts.keeps("a/b/c.ts") && ts.keeps("x.tsx") && !ts.keeps("x.js"));
        assert!(ts.enters("src"), "a glob that picks files still lets the walk into folders");
        let not_declarations = pick(vec![Choice::Type("ts".into()), g("!*.d.ts")]);
        assert!(not_declarations.keeps("src/a.ts") && !not_declarations.keeps("src/a.d.ts") && !not_declarations.keeps("a.md"));
        let no_md = pick(vec![g("!*.md")]);
        assert!(no_md.keeps("a.rs") && !no_md.keeps("docs/a.md"));
        let src = pick(vec![g("src/**")]);
        assert!(src.keeps("src/a/b.rs") && !src.keeps("lib/src/b.rs"));
        // A glob outranks a type, as in ripgrep.
        assert!(pick(vec![g("*.ts"), Choice::NotType("ts".into())]).keeps("a.ts"));
        assert!(!pick(vec![Choice::Type("ts".into()), Choice::NotType("ts".into())]).keeps("a.ts"));
        // Leaving a folder out keeps the walk out of it.
        let no_target = pick(vec![g("!target")]);
        assert!(!no_target.enters("a/target") && no_target.enters("src"));
        // --iglob and grep's options.
        let folded = Pick::new(&[Choice::Glob { glob: "*.RS".into(), fold: true }], &Types::default(), false).unwrap().unwrap();
        assert!(folded.keeps("a.rs") && folded.keeps("B.Rs"));
        let grep = pick(vec![Choice::Include("*.c".into()), Choice::Exclude("skip.c".into()), Choice::ExcludeDir("build".into())]);
        assert!(grep.keeps("a/x.c") && !grep.keeps("a/skip.c") && !grep.keeps("x.h") && !grep.enters("a/build"));
        assert!(Pick::new(&[Choice::Type("cobol".into())], &Types::default(), false).is_err());
        assert!(Pick::new(&[], &Types::default(), false).unwrap().is_none());
    }

    #[test]
    fn types_can_be_added_and_cleared() {
        let mut types = Types::default();
        types.add("web:*.{html,css}").unwrap();
        types.add("web:*.htm").unwrap();
        types.add("front:include:web,js").unwrap();
        assert_eq!(types.globs("web").unwrap(), ["*.{html,css}", "*.htm"]);
        assert!(types.globs("front").unwrap().contains(&"*.jsx".to_owned()));
        assert!(types.add("we_b:*.x").is_err() && types.add("x:include:nosuch").is_err());
        types.clear("web");
        assert!(types.globs("web").is_err());
        assert!(types.add("bad").is_err());
        let p = Pick::new(&[Choice::Type("front".into())], &types, false).unwrap().unwrap();
        // front took web's globs when it was made; clearing web later does not change it.
        assert!(p.keeps("x.htm") && p.keeps("a.css") && p.keeps("a.html") && p.keeps("a.mjs") && !p.keeps("a.rs"));
    }
}
