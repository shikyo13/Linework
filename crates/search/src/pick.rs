//! Choosing files by name or type, as ripgrep's `-g`, `-t` and `-T` do.
//!
//! A glob is written as a line of a `.gitignore` file, with the meaning turned
//! round: a glob picks the files it matches, and one starting with `!` leaves
//! them out. When any glob picks, only picked files are searched. A glob with
//! no `/` matches a file's name at any depth; one with a `/` matches its path
//! from the searched folder. `{a,b}` stands for either. The last glob that
//! matches a file decides, and a type left out with `-T` always leaves out.
use crate::ignore::{Rules, Verdict};

/// File types and the globs that make them, for `-t` and `-T`.
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

/// Which files of a folder are searched.
#[derive(Debug)]
pub struct Pick {
    rules: Rules,
    /// Whether some glob picks files; then a file no glob matches is left out.
    picking: bool,
}

/// `{a,b}` in a glob, as every glob it stands for. Groups do not nest.
fn spread(glob: &str) -> Vec<String> {
    let (Some(open), Some(close)) = (glob.find('{'), glob.find('}')) else { return vec![glob.to_owned()] };
    if close < open {
        return vec![glob.to_owned()];
    }
    let (head, tail) = (&glob[..open], &glob[close + 1..]);
    glob[open + 1..close].split(',').flat_map(|choice| spread(&format!("{head}{choice}{tail}"))).collect()
}

impl Pick {
    /// From the globs (`-g`), the types to search (`-t`) and the types to leave out (`-T`), in that order.
    pub fn new(globs: &[String], types: &[String], not_types: &[String]) -> Result<Option<Self>, String> {
        if globs.is_empty() && types.is_empty() && not_types.is_empty() {
            return Ok(None);
        }
        let named = |name: &String| {
            TYPES.iter().find(|(t, _)| t == name).map(|(_, globs)| *globs).ok_or_else(|| {
                format!("there is no file type {name}; lw help folders lists them")
            })
        };
        let mut lines: Vec<String> = Vec::new();
        for name in types {
            lines.extend(named(name)?.iter().map(|g| (*g).to_owned()));
        }
        for glob in globs {
            let (bang, bare) = match glob.strip_prefix('!') {
                Some(bare) => ("!", bare),
                None => ("", glob.as_str()),
            };
            lines.extend(spread(bare).into_iter().map(|g| format!("{bang}{g}")));
        }
        for name in not_types {
            lines.extend(named(name)?.iter().map(|g| format!("!{g}")));
        }
        // A glob picks where a .gitignore line would ignore, so the verdicts read the other way round.
        let picking = lines.iter().any(|l| !l.starts_with('!'));
        Ok(Some(Self { rules: Rules::parse(lines.join("\n").as_bytes()), picking }))
    }

    /// Whether the file at `path` (relative to the searched folder, `/` between names) is searched.
    pub fn keeps(&self, path: &str) -> bool {
        match self.rules.verdict(path.as_bytes(), false) {
            Some(Verdict::Ignored) => true,
            Some(Verdict::Kept) => false,
            None => !self.picking,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(globs: &[&str], types: &[&str], not_types: &[&str]) -> Pick {
        let own = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        Pick::new(&own(globs), &own(types), &own(not_types)).unwrap().unwrap()
    }

    #[test]
    fn globs_and_types_choose_files_as_ripgrep_does() {
        let ts = pick(&["*.{ts,tsx}"], &[], &[]);
        assert!(ts.keeps("a/b/c.ts") && ts.keeps("x.tsx") && !ts.keeps("x.js"));
        let not_declarations = pick(&["!*.d.ts"], &["ts"], &[]);
        assert!(not_declarations.keeps("src/a.ts") && !not_declarations.keeps("src/a.d.ts") && !not_declarations.keeps("a.md"));
        // Only leaving out keeps everything else.
        let no_md = pick(&["!*.md"], &[], &[]);
        assert!(no_md.keeps("a.rs") && !no_md.keeps("docs/a.md"));
        // A slash ties a glob to the searched folder.
        let src = pick(&["src/**"], &[], &[]);
        assert!(src.keeps("src/a/b.rs") && !src.keeps("lib/src/b.rs"));
        // -T wins over a glob that picks.
        assert!(!pick(&["*.ts"], &[], &["ts"]).keeps("a.ts"));
        assert!(Pick::new(&[], &["cobol".into()], &[]).is_err());
        assert!(Pick::new(&[], &[], &[]).unwrap().is_none());
    }
}
