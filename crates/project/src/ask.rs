//! A folder search as the editor asks for it: the search panel's fields
//! turned into lw's words. The same words given to lw find the same lines.

/// The search panel's fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Panel {
    pub text: String,
    pub match_case: bool,
    pub whole_word: bool,
    pub regex: bool,
    /// Globs of files to search, separated by commas or semicolons (`-g GLOB`).
    pub files: String,
    /// Globs of files to leave out (`-g !GLOB`).
    pub skip: String,
    /// File types to search, by lw's names (`-t TYPE`).
    pub types: String,
    /// Lines shown before and after each match (`-C NUM`).
    pub context: u32,
    /// Keep a line only when this pattern is within `near_lines` lines (`--near`).
    pub near: String,
    pub near_lines: u32,
    /// Keep only files that also contain this (`--and`), or that lack this (`--not`).
    pub also: String,
    pub lacks: String,
    /// Name the definitions each line is in (`--scope`).
    pub scope: bool,
    /// Search hidden files too (`--hidden`) and files the ignore files leave out (`--no-ignore`).
    pub hidden: bool,
    pub ignored: bool,
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            text: String::new(),
            match_case: false,
            whole_word: false,
            regex: false,
            files: String::new(),
            skip: String::new(),
            types: String::new(),
            context: 0,
            near: String::new(),
            near_lines: 3,
            also: String::new(),
            lacks: String::new(),
            scope: false,
            hidden: false,
            ignored: false,
        }
    }
}

/// The items of a list typed with commas, semicolons or spaces between them.
fn items(list: &str) -> impl Iterator<Item = &str> {
    list.split([',', ';', ' ']).map(str::trim).filter(|s| !s.is_empty())
}

impl Panel {
    /// The panel as lw's words, pattern last.
    pub fn words(&self) -> Vec<String> {
        let mut w: Vec<String> = Vec::new();
        let mut add = |words: &[&str]| w.extend(words.iter().map(|s| (*s).to_owned()));
        add(&[if self.match_case { "-s" } else { "-i" }]);
        if self.whole_word {
            add(&["-w"]);
        }
        if !self.regex {
            add(&["-F"]);
        }
        for glob in items(&self.files) {
            add(&["-g", glob]);
        }
        for glob in items(&self.skip) {
            let glob = format!("!{}", glob.trim_start_matches('!'));
            add(&["-g", &glob]);
        }
        for kind in items(&self.types) {
            add(&["-t", kind]);
        }
        if self.hidden {
            add(&["--hidden"]);
        }
        if self.ignored {
            add(&["--no-ignore"]);
        }
        if self.context > 0 {
            add(&["-C", &self.context.to_string()]);
        }
        if !self.near.is_empty() {
            add(&["--near", &self.near, &self.near_lines.to_string()]);
        }
        if !self.also.is_empty() {
            add(&["--and", &self.also]);
        }
        if !self.lacks.is_empty() {
            add(&["--not", &self.lacks]);
        }
        if self.scope {
            add(&["--scope"]);
        }
        // -e, so a pattern that starts with a dash is still the pattern.
        add(&["-e", &self.text]);
        w
    }
}

/// Words joined into a command line, quoted where a shell would split them.
pub fn join(words: &[String]) -> String {
    words
        .iter()
        .map(|w| {
            if w.is_empty() {
                "\"\"".to_owned()
            } else if w.contains(char::is_whitespace) || w.contains('"') || w.contains('\'') {
                if w.contains('"') { format!("'{w}'") } else { format!("\"{w}\"") }
            } else {
                w.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use linework_search::settings::Settings;

    fn w(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_panel_is_lw_words() {
        let panel = Panel {
            text: "-x".into(),
            whole_word: true,
            files: "*.rs; src/**".into(),
            skip: "tests/**".into(),
            types: "rust".into(),
            context: 2,
            near: "unsafe".into(),
            near_lines: 4,
            also: "fn".into(),
            ..Panel::default()
        };
        assert_eq!(
            panel.words(),
            w(&["-i", "-w", "-F", "-g", "*.rs", "-g", "src/**", "-g", "!tests/**", "-t", "rust", "-C", "2", "--near", "unsafe", "4", "--and", "fn", "-e", "-x"])
        );
        let s = Settings::from_words(panel.words()).unwrap();
        assert_eq!(s.patterns, ["-x"]);
        assert_eq!(s.context_lines(), (2, 2));
    }

    #[test]
    fn words_join_into_a_command_line() {
        assert_eq!(join(&w(&["-g", "src/my files/**", "-e", "", "say \"hi\""])), r#"-g "src/my files/**" -e "" 'say "hi"'"#);
    }
}
