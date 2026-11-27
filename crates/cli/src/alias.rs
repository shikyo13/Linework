//! lw under the names of the tools it stands in for.
//!
//! A link named `rg`, `grep`, `egrep` or `fgrep` that leads to lw makes lw
//! answer as that tool: the same options with that tool's meanings, the same
//! defaults, the same output. Programs and people that call `rg` or `grep`
//! then get lw's speed without knowing of it.
//!
//! lw's own name keeps lw's own ways (line numbers in a pipe, long lines
//! shown around their match, its own JSON). Under another name none of that
//! applies: a script that pipes `rg -o x | sort` must get what ripgrep gives.
//!
//! What lw cannot give exactly as the tool would, it hands over to the real
//! tool: ripgrep kept as `rg.real` beside the link or found further along
//! PATH, grep in the system's place.
//! So a stand-in is never worse than what it stands in for.
use std::{
    env,
    ffi::OsString,
    io::IsTerminal,
    path::{Path, PathBuf},
    sync::OnceLock,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Ripgrep,
    /// grep, with the syntax its name implies when no option names one.
    Grep(&'static str),
}

static NAME: OnceLock<String> = OnceLock::new();
/// The stand-in has reported an error of its own, which makes the exit code 2 as it would the tool's.
static FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The exit code, given what the stand-in itself had to report.
pub fn exit_code(code: i32) -> i32 {
    if FAILED.load(std::sync::atomic::Ordering::Relaxed) { 2 } else { code }
}

/// A message as the tool lw stands in for would word it.
pub fn worded(text: String) -> String {
    let Some(name) = name() else { return text };
    let Some(rest) = text.strip_prefix("lw: ") else { return text };
    // grep names the fault without the system's number for it.
    let rest = match (name.ends_with("grep"), rest.rfind(" (os error ")) {
        (true, Some(at)) if rest.ends_with(')') => &rest[..at],
        _ => rest,
    };
    format!("{name}: {rest}")
}

/// The name lw was called by when it stands in for another tool, for the
/// start of its messages (`grep: file: No such file or directory`).
pub fn name() -> Option<&'static str> {
    NAME.get().map(String::as_str)
}

/// The tool lw was called as, from the name of the program.
pub fn called_as() -> Option<Tool> {
    let program = env::args_os().next()?;
    let stem = Path::new(&program).file_stem()?.to_str()?.to_ascii_lowercase();
    let tool = match stem.as_str() {
        "rg" => Tool::Ripgrep,
        "grep" => Tool::Grep("--basic-regexp"),
        "egrep" => Tool::Grep("--extended-regexp"),
        "fgrep" => Tool::Grep("--fixed-strings"),
        _ => return None,
    };
    let _ = NAME.set(stem);
    Some(tool)
}

/// lw cannot answer this exactly as the tool would.
pub struct NotExact;

/// The tool's command line as lw's own, or `NotExact`.
pub fn translate(tool: Tool, words: &[String]) -> Result<Vec<String>, NotExact> {
    match tool {
        Tool::Ripgrep => ripgrep(words),
        Tool::Grep(syntax) => grep(syntax, words),
    }
}

fn options(words: &[String]) -> impl Iterator<Item = &String> {
    words.iter().take_while(|word| *word != "--")
}

fn ripgrep(words: &[String]) -> Result<Vec<String>, NotExact> {
    // ripgrep's JSON messages are another format than lw's, and its help
    // and version are its own.
    if options(words).any(|word| matches!(word.as_str(), "--json" | "--help" | "-h" | "--version" | "-V" | "--pcre2-version")) {
        return Err(NotExact);
    }
    let mut own = vec!["find".to_owned(), "--whole-lines".to_owned()];
    // ripgrep numbers lines for a person at a console and not for a pipe,
    // except where an option that shows columns brings the numbers with it.
    let numbered = options(words).any(|word| matches!(word.as_str(), "--column" | "--vimgrep" | "--pretty" | "-p"));
    if !numbered && !std::io::stdout().is_terminal() {
        own.push("--no-line-number".to_owned());
    }
    own.extend(words.iter().cloned());
    Ok(own)
}

/// grep's letters, where they mean in lw what they mean in grep or have a
/// long name there. A letter that is not here is handed to grep itself.
fn letter(letter: char) -> Option<&'static str> {
    Some(match letter {
        'a' => "--text",
        'b' => "--byte-offset",
        'c' => "--count",
        'E' => "--extended-regexp",
        'F' => "--fixed-strings",
        'G' => "--basic-regexp",
        'H' => "--with-filename",
        'h' => "--no-filename",
        'I' => "--binary-files=without-match",
        'i' | 'y' => "--ignore-case",
        'L' => "--files-without-match",
        'l' => "--files-with-matches",
        'n' => "--line-number",
        'o' => "--only-matching",
        'q' => "--quiet",
        'R' => "--dereference-recursive",
        'r' => "--recursive",
        's' => "--no-messages",
        'v' => "--invert-match",
        'w' => "--word-regexp",
        'x' => "--line-regexp",
        _ => return None,
    })
}

/// grep's letters that take a value, attached or as the next word.
fn valued(letter: char) -> Option<&'static str> {
    Some(match letter {
        'A' => "--after-context",
        'B' => "--before-context",
        'C' => "--context",
        'e' => "--regexp",
        'f' => "--file",
        'm' => "--max-count",
        _ => return None,
    })
}

/// grep's long options that lw reads the same way.
const SAME: &[&str] = &[
    "regexp", "file", "fixed-strings", "basic-regexp", "extended-regexp", "ignore-case", "word-regexp", "line-regexp",
    "invert-match", "include", "exclude", "exclude-dir", "recursive", "dereference-recursive", "text", "binary-files",
    "after-context", "before-context", "context", "line-number", "byte-offset", "only-matching", "with-filename",
    "no-filename", "count", "files-with-matches", "files-without-match", "max-count", "quiet", "silent", "no-messages",
    "color", "colour", "line-buffered", "null",
];

fn grep(syntax: &'static str, words: &[String]) -> Result<Vec<String>, NotExact> {
    let mut given: Vec<String> = Vec::new();
    // Where the words that are not options start in `given`, and whether a
    // pattern was given by option, so that the first of them is a file.
    let (mut plain, mut patterned): (Vec<usize>, bool) = (Vec::new(), false);
    let (mut chosen, mut recursive, mut counting) = (false, false, false);
    let mut note = |option: &str| {
        chosen |= matches!(option, "--extended-regexp" | "--fixed-strings" | "--basic-regexp");
        recursive |= matches!(option, "--recursive" | "--dereference-recursive");
        counting |= option == "--count";
    };
    let mut at = 0;
    while at < words.len() {
        let word = &words[at];
        at += 1;
        if word == "--" {
            given.push(word.clone());
            plain.extend(given.len()..given.len() + words.len() - at);
            given.extend(words[at..].iter().cloned());
            break;
        }
        if let Some(long) = word.strip_prefix("--") {
            let name = long.split('=').next().unwrap_or(long);
            if !SAME.contains(&name) {
                return Err(NotExact);
            }
            note(&format!("--{name}"));
            patterned |= matches!(name, "regexp" | "file");
            given.push(word.replacen("--colour", "--color", 1));
            continue;
        }
        let Some(cluster) = word.strip_prefix('-').filter(|rest| !rest.is_empty()) else {
            plain.push(given.len());
            given.push(word.clone());
            continue;
        };
        let letters: Vec<char> = cluster.chars().collect();
        let mut index = 0;
        while index < letters.len() {
            let one = letters[index];
            index += 1;
            if one.is_ascii_digit() {
                // -NUM is NUM lines of context.
                let mut number = one.to_string();
                while index < letters.len() && letters[index].is_ascii_digit() {
                    number.push(letters[index]);
                    index += 1;
                }
                given.push(format!("--context={number}"));
            } else if let Some(option) = valued(one) {
                let attached: String = letters[index..].iter().collect();
                let value = if attached.is_empty() {
                    at += 1;
                    words.get(at - 1).ok_or(NotExact)?.clone()
                } else {
                    attached
                };
                patterned |= matches!(one, 'e' | 'f');
                given.push(format!("{option}={value}"));
                break;
            } else {
                let option = letter(one).ok_or(NotExact)?;
                note(option);
                given.push(option.to_owned());
            }
        }
    }
    // grep's ways, first, so that what was asked for comes after and wins:
    // no line numbers, the file's name on every line, files in the order
    // they were named, every file searched whatever ignore files say, and a
    // binary file only said to match.
    let mut own: Vec<String> = ["find", "--no-line-number", "--no-heading", "--whole-lines", "--sort=none", "--no-ignore", "--hidden", "--binary-files=binary", "--label=(standard input)"]
        .map(str::to_owned)
        .to_vec();
    if !chosen {
        own.push(syntax.to_owned());
    }
    if counting {
        // grep says 0 for a file with no matching line.
        own.push("--include-zero".to_owned());
    }
    if !recursive {
        // Without -r grep does not go into a folder named among the files:
        // it says that it is one, and that counts as an error.
        own.push("--directories=skip".to_owned());
        for &at in plain.iter().skip(usize::from(!patterned)) {
            if Path::new(&given[at]).is_dir() {
                eprintln!("{}: {}: Is a directory", name().unwrap_or("grep"), given[at]);
                FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    own.extend(given);
    Ok(own)
}

/// The real tool, to hand a command over to. grep is in the system's own
/// place. ripgrep is the file `rg.real` beside the link lw was called by,
/// where whoever put lw in ripgrep's place kept the original, or else the
/// first `rg` along PATH that is not this program.
fn real(tool: Tool) -> Option<PathBuf> {
    let name = name()?;
    if matches!(tool, Tool::Grep(_)) {
        return [format!("/usr/bin/{name}"), format!("/bin/{name}")].into_iter().map(PathBuf::from).find(|path| path.is_file());
    }
    let kept = if cfg!(windows) { "rg.real.exe" } else { "rg.real" };
    let called = env::args_os().next().map(PathBuf::from);
    let beside = [called, env::current_exe().ok()]
        .into_iter()
        .flatten()
        .filter_map(|path| Some(path.parent()?.join(kept)))
        .find(|path| path.is_file());
    if beside.is_some() {
        return beside;
    }
    let own = env::current_exe().ok().and_then(|path| path.canonicalize().ok());
    env::split_paths(&env::var_os("PATH")?)
        .map(|folder| folder.join(if cfg!(windows) { "rg.exe" } else { "rg" }))
        .find(|path| path.is_file() && path.canonicalize().ok() != own)
}

/// Runs the real tool with the words lw was given and ends with its exit
/// code. `None` when there is no such tool to run.
pub fn hand_over(tool: Tool, words: &[String]) -> Option<i32> {
    let program = real(tool)?;
    let mut command = std::process::Command::new(program);
    command.args(words.iter().map(OsString::from));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Returns only when the program could not be started.
        let _ = command.exec();
        None
    }
    #[cfg(not(unix))]
    {
        command.status().ok().map(|status| status.code().unwrap_or(2))
    }
}
