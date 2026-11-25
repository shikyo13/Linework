//! `--generate`: a man page and shell completions, made from the same option
//! table the parser reads and the same help text `lw help` shows, so they
//! cannot drift from what lw accepts.
use crate::{
    help,
    options::{Group, Takes, LONG_ALIASES, SHORT_ALIASES, SPECS},
};

/// The text for `--generate KIND`; the kind was checked when the options were read.
pub fn generate(kind: &str) -> String {
    match kind {
        "man" => man(),
        "complete-bash" => bash(),
        "complete-zsh" => zsh(),
        "complete-fish" => fish(),
        "complete-powershell" => powershell(),
        _ => unreachable!("--generate is checked when read"),
    }
}

const COMMANDS: &[(&str, &str)] = &[
    ("find", "lines that match a pattern (the default command)"),
    ("count", "matching lines, or matches, per file"),
    ("lines", "lines by number, or a time span of a log"),
    ("info", "size, lines, encoding and line ends of a file"),
    ("replace", "replace text, with a preview and undo"),
    ("undo", "undo a replace"),
    ("changes", "files changed in a folder since the last look"),
    ("files", "the files a search would read"),
    ("defs", "where a name is defined in code"),
    ("uses", "where a name is used in code"),
    ("open", "show a place in the Linework editor"),
    ("serve", "answer AI agents as a tool server"),
    ("help", "the help"),
];

fn group_title(group: Group) -> &'static str {
    match group {
        Group::Patterns => "PATTERNS",
        Group::Files => "WHICH FILES",
        Group::Input => "READING FILES",
        Group::Output => "HOW MATCHES ARE SHOWN",
        Group::Results => "WHAT IS REPORTED",
        Group::Lw => "LW'S OWN",
        Group::Other => "OTHER",
    }
}

const GROUPS: [Group; 7] = [Group::Patterns, Group::Files, Group::Input, Group::Output, Group::Results, Group::Lw, Group::Other];

/// The value name an option takes, for the help: `NUM`, `GLOB`, or nothing.
fn value(takes: Takes) -> Option<&'static str> {
    match takes {
        Takes::Nothing => None,
        Takes::Value(name) | Takes::Optional(name) => Some(name),
    }
}

/// Text for roff: a backslash doubled, and a leading dot or quote guarded.
fn roff(text: &str) -> String {
    let text = text.replace('\\', "\\e").replace('-', "\\-");
    if text.starts_with('.') || text.starts_with('\'') { format!("\\&{text}") } else { text }
}

fn man() -> String {
    let mut out = String::new();
    out.push_str(&format!(".TH LW 1 \"\" \"{}\" \"User Commands\"\n", env!("CARGO_PKG_VERSION")));
    out.push_str(".SH NAME\nlw \\- search, read and change text files of any size\n");
    out.push_str(".SH SYNOPSIS\n.B lw\n[\\fIOPTIONS\\fR] \\fIPATTERN\\fR [\\fIPATH\\fR...]\n.br\n.B lw\n\\fICOMMAND\\fR [\\fIOPTIONS\\fR] ...\n");
    out.push_str(".SH DESCRIPTION\n.nf\n");
    out.push_str(&roff(&help::overview()));
    out.push_str("\n.fi\n");
    for group in GROUPS {
        out.push_str(&format!(".SH {}\n", group_title(group)));
        for spec in SPECS.iter().filter(|s| s.group == group) {
            let short = spec.short.map(|c| format!("\\-{c}, ")).unwrap_or_default();
            let takes = value(spec.takes).map(|v| format!(" \\fI{}\\fR", roff(v))).unwrap_or_default();
            out.push_str(&format!(".TP\n\\fB{short}\\-\\-{}\\fR{takes}\n{}\n", roff(spec.long), roff(spec.help)));
        }
    }
    out.push_str(".SH COMMANDS AND TOPICS\n");
    for topic in help::TOPICS {
        out.push_str(&format!(".SS {}\n.nf\n{}\n.fi\n", roff(topic.name), roff(&help::page(topic))));
    }
    out.push_str(".SH SEE ALSO\nlw help, lw help compared (for grep and ripgrep users)\n");
    out
}

/// Every option as the shell sees it: `--long` names (aliases included), then `-x` letters.
fn words() -> Vec<String> {
    let mut all: Vec<String> = SPECS.iter().map(|s| format!("--{}", s.long)).collect();
    all.extend(LONG_ALIASES.iter().map(|(alias, _)| format!("--{alias}")));
    all.extend(SPECS.iter().filter_map(|s| s.short).chain(SHORT_ALIASES.iter().map(|(c, _)| *c)).map(|c| format!("-{c}")));
    all
}

fn bash() -> String {
    let takes: Vec<String> = SPECS
        .iter()
        .filter(|s| matches!(s.takes, Takes::Value(_)))
        .flat_map(|s| std::iter::once(format!("--{}", s.long)).chain(s.short.map(|c| format!("-{c}"))))
        .collect();
    format!(
        r#"# lw completion for bash: source this file, or put it in bash-completion's folder.
_lw() {{
    local cur prev
    cur="${{COMP_WORDS[COMP_CWORD]}}"
    prev="${{COMP_WORDS[COMP_CWORD-1]}}"
    case " {takes} " in
        *" $prev "*)
            case "$prev" in
                -t|--type|-T|--type-not|--type-clear)
                    COMPREPLY=( $(compgen -W "$(lw --type-list 2>/dev/null | cut -d: -f1)" -- "$cur") ); return ;;
                --color|--colour) COMPREPLY=( $(compgen -W "never auto always ansi" -- "$cur") ); return ;;
                --sort|--sortr) COMPREPLY=( $(compgen -W "none path modified accessed created" -- "$cur") ); return ;;
                --engine) COMPREPLY=( $(compgen -W "default pcre2 auto" -- "$cur") ); return ;;
                --generate) COMPREPLY=( $(compgen -W "man complete-bash complete-zsh complete-fish complete-powershell" -- "$cur") ); return ;;
                *) COMPREPLY=( $(compgen -f -- "$cur") ); return ;;
            esac ;;
    esac
    if [[ "$cur" == -* ]]; then
        COMPREPLY=( $(compgen -W "{options}" -- "$cur") )
    elif [[ $COMP_CWORD -eq 1 ]]; then
        COMPREPLY=( $(compgen -W "{commands}" -- "$cur") $(compgen -f -- "$cur") )
    else
        COMPREPLY=( $(compgen -f -- "$cur") )
    fi
}}
complete -F _lw -o bashdefault -o default lw
"#,
        takes = takes.join(" "),
        options = words().join(" "),
        commands = COMMANDS.iter().map(|(c, _)| *c).collect::<Vec<_>>().join(" "),
    )
}

/// Text inside single quotes for zsh, fish and PowerShell: a single quote doubled or escaped as each needs.
fn quoted(text: &str, quote_in: &str) -> String {
    text.replace('\'', quote_in)
}

fn zsh() -> String {
    let mut out = String::from("#compdef lw\n# lw completion for zsh: put this file in a folder on $fpath as _lw.\n_lw() {\n  local -a args\n  args=(\n");
    for spec in SPECS {
        let help = quoted(spec.help, "'\\''").replace('[', "\\[").replace(']', "\\]").replace(':', "\\:");
        let takes = value(spec.takes).map(|v| format!(":{}:_files", v.to_lowercase())).unwrap_or_default();
        match spec.short {
            Some(c) => out.push_str(&format!("    '(-{c} --{long})'{{-{c},--{long}}}'[{help}]{takes}'\n", long = spec.long)),
            None => out.push_str(&format!("    '--{long}[{help}]{takes}'\n", long = spec.long)),
        }
    }
    out.push_str("    '1: :->first'\n    '*:path:_files'\n  )\n  _arguments -s $args\n  case $state in\n    first)\n      local -a commands\n      commands=(\n");
    for (name, about) in COMMANDS {
        out.push_str(&format!("        '{name}:{}'\n", quoted(about, "'\\''")));
    }
    out.push_str("      )\n      _describe 'command' commands\n      _files\n      ;;\n  esac\n}\n_lw \"$@\"\n");
    out
}

fn fish() -> String {
    let mut out = String::from("# lw completion for fish: put this file in ~/.config/fish/completions/lw.fish\n");
    for (name, about) in COMMANDS {
        out.push_str(&format!("complete -c lw -n '__fish_use_subcommand' -a '{name}' -d '{}'\n", quoted(about, "\\'")));
    }
    for spec in SPECS {
        let short = spec.short.map(|c| format!(" -s {c}")).unwrap_or_default();
        let takes = if value(spec.takes).is_some() { " -r" } else { "" };
        out.push_str(&format!("complete -c lw -l {}{short}{takes} -d '{}'\n", spec.long, quoted(spec.help, "\\'")));
    }
    out
}

fn powershell() -> String {
    let mut entries = String::new();
    for (name, about) in COMMANDS {
        entries.push_str(&format!("        @('{name}', '{}', 'command')\n", quoted(about, "''")));
    }
    for spec in SPECS {
        entries.push_str(&format!("        @('--{}', '{}', 'option')\n", spec.long, quoted(spec.help, "''")));
        if let Some(c) = spec.short {
            entries.push_str(&format!("        @('-{c}', '{}', 'option')\n", quoted(spec.help, "''")));
        }
    }
    format!(
        r#"# lw completion for PowerShell: dot-source this file, or add it to $PROFILE.
Register-ArgumentCompleter -Native -CommandName lw, lw.exe -ScriptBlock {{
    param($wordToComplete, $commandAst, $cursorPosition)
    $choices = @(
{entries}    )
    $first = $commandAst.CommandElements.Count -le 2
    foreach ($choice in $choices) {{
        $word, $about, $kind = $choice
        if ($kind -eq 'command' -and -not $first) {{ continue }}
        if ($word -like "$wordToComplete*") {{
            [System.Management.Automation.CompletionResult]::new($word, $word, 'ParameterName', $about)
        }}
    }}
}}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_names_every_option() {
        for kind in ["man", "complete-bash", "complete-zsh", "complete-fish", "complete-powershell"] {
            let text = generate(kind);
            for spec in SPECS {
                let name = if kind == "man" { roff(spec.long) } else { spec.long.to_owned() };
                assert!(text.contains(&name), "{kind} lacks --{}", spec.long);
            }
        }
        assert!(generate("man").starts_with(".TH LW 1"));
    }
}
