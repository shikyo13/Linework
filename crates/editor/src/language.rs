use serde::Deserialize;
use std::path::Path;
use std::sync::OnceLock;

#[derive(Deserialize)]
pub(crate) struct Language {
    pub name: String,
    pub lexer: String,
    pub extensions: Vec<String>,
    pub filenames: Vec<String>,
    pub keywords: Vec<(usize, String)>,
}

pub(crate) fn registry() -> &'static [Language] {
    static LANGS: OnceLock<Vec<Language>> = OnceLock::new();
    LANGS.get_or_init(|| {
        serde_json::from_str(include_str!("../assets/languages.json"))
            .expect("bundled language definitions")
    })
}

pub fn languages() -> Vec<String> {
    registry()
        .iter()
        .map(|lang| lang.name.clone())
        .chain(super::grammar::names())
        .collect()
}

pub(crate) fn lookup(name: &str) -> Option<&'static Language> {
    let key = name.to_ascii_lowercase();
    let name = match key.as_str() {
        "text" | "plaintext" | "fundamental" => "Plain Text",
        "javascriptreact" | "js" => "JavaScript",
        "typescriptreact" | "ts" => "TypeScript",
        "c++" | "cpp" | "cxx" => "C++",
        "csharp" | "cs" => "C#",
        "sh" | "bash" | "zsh" => "Shell",
        "golang" => "Go",
        "py" => "Python",
        "rb" => "Ruby",
        "ps1" | "pwsh" => "PowerShell",
        "md" | "gfm" => "Markdown",
        "rs" => "Rust",
        "yml" => "YAML",
        _ => name,
    };
    registry()
        .iter()
        .find(|lang| lang.name.eq_ignore_ascii_case(name))
}

fn modeline(line: &str) -> Option<&'static Language> {
    let line = line.to_ascii_lowercase();
    let value = if line.contains("vim:") || line.contains("vi:") || line.contains("ex:") {
        ["filetype=", "ft=", "syntax="]
            .iter()
            .find_map(|key| line.split_once(key).map(|x| x.1))?
    } else if let Some((_, rest)) = line.split_once("-*-") {
        let content = rest.split_once("-*-")?.0;
        content.split_once("mode:").map_or(content, |x| x.1)
    } else {
        return None;
    };
    lookup(
        value
            .trim()
            .split(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '+' | '#' | '-')))
            .next()?,
    )
}

/// Detect from a bounded leading sample. Explicit local modelines and executable
/// shebangs take precedence over file extensions. Ambiguous text stays plain.
pub fn detect_language(path: Option<&Path>, sample: &str) -> String {
    let mut limit = sample.len().min(8192);
    while !sample.is_char_boundary(limit) {
        limit -= 1;
    }
    let sample = sample[..limit].trim_start_matches('\u{feff}');
    if let Some(lang) = sample.lines().take(4).find_map(modeline) {
        return lang.name.clone();
    }
    if let Some(shebang) = sample.lines().next().and_then(|x| x.strip_prefix("#!")) {
        for part in shebang.split_whitespace() {
            let file = part.rsplit('/').next().unwrap_or(part);
            let lang = match file.trim_end_matches(|ch: char| ch.is_ascii_digit() || ch == '.') {
                "python" | "pypy" => "Python",
                "node" | "nodejs" | "bun" => "JavaScript",
                "deno" | "ts-node" => "TypeScript",
                "ruby" => "Ruby",
                "perl" => "Perl",
                "bash" | "sh" | "dash" | "ksh" | "zsh" => "Shell",
                "pwsh" => "PowerShell",
                "lua" => "Lua",
                "Rscript" => "R",
                "julia" => "Julia",
                "php" => "PHP",
                _ => continue,
            };
            return lang.to_owned();
        }
    }
    if let Some(path) = path {
        if let Some(grammar) = super::grammar::detect(path) {
            return grammar;
        }
        let name = path.file_name().and_then(|x| x.to_str()).unwrap_or("");
        if let Some(lang) = registry()
            .iter()
            .find(|l| l.filenames.iter().any(|n| n.eq_ignore_ascii_case(name)))
        {
            return lang.name.clone();
        }
        if name.to_ascii_lowercase().starts_with("dockerfile.") {
            return "Dockerfile".into();
        }
        let ext = path.extension().and_then(|x| x.to_str()).unwrap_or("");
        if let Some(lang) = registry()
            .iter()
            .find(|l| l.extensions.iter().any(|v| v.eq_ignore_ascii_case(ext)))
        {
            if lang.name != "Plain Text" {
                return lang.name.clone();
            }
        }
    }
    if let Some(grammar) = super::grammar::detect_first(sample) {
        return grammar;
    }
    if super::log::looks_like_log(sample) {
        return "Log".into();
    }
    let text = sample.trim_start();
    let lower = text.to_ascii_lowercase();
    if lower.starts_with("<!doctype html") || lower.starts_with("<html") {
        "HTML"
    } else if text.starts_with("<?xml") || lower.starts_with("<svg") {
        "XML"
    } else if lower.starts_with("<?php") {
        "PHP"
    } else if (text.starts_with('{') || text.starts_with('['))
        && serde_json::from_str::<serde_json::Value>(text).is_ok()
    {
        "JSON"
    } else if text.starts_with("diff --git ")
        || (text.starts_with("--- ") && text.contains("\n+++ "))
    {
        "Diff"
    } else if text
        .lines()
        .take(16)
        .any(|l| l.trim_start().starts_with("fn main(") || l.starts_with("use std::"))
    {
        "Rust"
    } else if text.starts_with("package main") && text.contains("func ") {
        "Go"
    } else if text
        .lines()
        .take(16)
        .any(|l| l.starts_with("def ") && l.trim_end().ends_with(':'))
    {
        "Python"
    } else if text.starts_with("#include ") {
        "C++"
    } else if text.starts_with("FROM ") {
        "Dockerfile"
    } else if text.starts_with("# ")
        && text
            .lines()
            .skip(1)
            .any(|l| l.starts_with("## ") || l.starts_with("- "))
    {
        "Markdown"
    } else {
        "Plain Text"
    }
    .to_owned()
}
