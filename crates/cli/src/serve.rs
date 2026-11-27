//! `lw serve`: lw's commands as tools for AI agents, over the Model Context
//! Protocol (JSON-RPC 2.0, one message per line on standard input and output).
//!
//! Each tool runs the lw command it is named after, in this process, with its
//! JSON fields turned into that command's options. Its description is the
//! command's help page. Searches answer in lw's JSON form, and every result is
//! capped at 100,000 bytes unless the call asks for another budget.
use crate::help;
use serde_json::{json, Map, Value};
use std::io::{self, BufRead, Write};

/// The protocol versions this server speaks, newest first.
const VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
/// The output a tool returns unless the call sets `budget`.
const BUDGET: u64 = 100_000;

#[derive(Clone, Copy)]
enum Kind {
    /// A word on the command line, in order.
    Place,
    /// Like Place, but an integer.
    PlaceNumber,
    /// A list of words on the command line.
    Places,
    /// A flag set when true.
    Flag,
    /// An option with a text value.
    Text,
    /// An option with an integer value.
    Number,
    /// An option given once for each text in a list.
    Texts,
    /// A list of {pattern, lines}, each given as --near PATTERN LINES.
    Near,
}

struct Field {
    name: &'static str,
    kind: Kind,
    /// The command-line option, for the kinds that have one.
    option: &'static str,
    required: bool,
    about: &'static str,
}

const fn field(name: &'static str, kind: Kind, option: &'static str, required: bool, about: &'static str) -> Field {
    Field { name, kind, option, required, about }
}

const PATTERN: Field = field("pattern", Kind::Place, "", true, "Plain text, or a regular expression if it has one of \\ ^ $ . | ? * + ( ) [ ] { } (see the patterns help)");
const PATH: Field = field("path", Kind::Place, "", true, "A file or a folder; relative paths are from the folder lw serve was started in");
const IGNORE_CASE: Field = field("ignore_case", Kind::Flag, "-i", false, "Ignore letter case");
const FIXED: Field = field("fixed", Kind::Flag, "-F", false, "Take the pattern as plain text even with special characters");
const WORD: Field = field("word", Kind::Flag, "-w", false, "Match whole words only");
const HIDDEN: Field = field("hidden", Kind::Flag, "--hidden", false, "In a folder, include hidden files and names starting with a dot");
const NO_IGNORE: Field = field("no_ignore", Kind::Flag, "--no-ignore", false, "In a folder, ignore .gitignore, .ignore and .rgignore files");
const MAX_LINES: Field = field("max_lines", Kind::Number, "-m", false, "At most this many matching lines from each file");
const SCOPE: Field = field("scope", Kind::Flag, "--scope", false, "Add the function, class or section around each line (costs a parse of each matching file)");
const BUDGET_FIELD: Field = field("budget", Kind::Number, "--budget", false, "Stop after this many bytes of output (default 100000); the summary says if output was cut");
const NEAR: Field = field("near", Kind::Near, "--near", false, "Keep a line only if each {pattern, lines} matches within that many lines of it");
const AND: Field = field("and", Kind::Texts, "--and", false, "Keep only files that also contain each of these patterns");
const NOT: Field = field("not", Kind::Texts, "--not", false, "Keep only files that contain none of these patterns");
const CHANGED: Field = field("changed", Kind::Flag, "--changed", false, "In a folder, search only files added or changed since the changes tool last looked");
const MARK: Field = field("mark", Kind::Text, "--mark", false, "The name of the folder snapshot to use (default \"last\")");
const SINCE: Field = field("since", Kind::Text, "--since", false, "In a time-ordered log, start at this time: 2026-11-21 21:40, 2026-11-21 or 21:40");
const UNTIL: Field = field("until", Kind::Text, "--until", false, "In a time-ordered log, end at this time, including all of it at the precision written");
const GLOB: Field = field("glob", Kind::Texts, "-g", false, "In a folder, only files matching these globs, as ripgrep's -g: *.rs, src/**, *.{ts,tsx}; one starting with ! leaves files out");
const TYPE: Field = field("type", Kind::Texts, "-t", false, "In a folder, only files of these types: c cpp cs css go html java js json kotlin log lua md php ps py rb rust sh sql swift toml ts txt xml yaml");
const TYPE_NOT: Field = field("type_not", Kind::Texts, "-T", false, "In a folder, leave out files of these types");
const NAMES_ONLY: Field = field("names_only", Kind::Flag, "-l", false, "Only the names of the files with matching lines, once each");
const CONTEXT: Field = field("context", Kind::Number, "-C", false, "Show this many lines before and after each matching line");
const BEFORE: Field = field("before", Kind::Number, "-B", false, "Show this many lines before each matching line");
const AFTER: Field = field("after", Kind::Number, "-A", false, "Show this many lines after each matching line");
const INVERT: Field = field("invert", Kind::Flag, "-v", false, "Show the lines that do not match");
const ONLY: Field = field("only_matching", Kind::Flag, "-o", false, "Show only the matched text, each match on its own line");
const SMART: Field = field("smart_case", Kind::Flag, "-S", false, "Ignore case unless the pattern has a capital letter");
const LINE: Field = field("whole_line", Kind::Flag, "-x", false, "Match whole lines only");
const MULTILINE: Field = field("multiline", Kind::Flag, "-U", false, "Let the pattern match across lines");
const PCRE2: Field = field("pcre2", Kind::Flag, "-P", false, "Use PCRE2, for look-around and back-references");
const PATTERNS: Field = field("patterns", Kind::Texts, "-e", false, "More patterns; a line matches when any of them matches");
const WITHOUT: Field = field("files_without_match", Kind::Flag, "--files-without-match", false, "Only the names of the files with no matching line");
const COUNT_MATCHES: Field = field("count_matches", Kind::Flag, "--count-matches", false, "Count matches, not matching lines");
const FOLLOW: Field = field("follow", Kind::Flag, "-L", false, "In a folder, follow links");
const DEPTH: Field = field("max_depth", Kind::Number, "-d", false, "In a folder, go at most this many folders deep");
const MAX_SIZE: Field = field("max_filesize", Kind::Text, "--max-filesize", false, "In a folder, leave out files larger than this, such as 2M");
const ENCODING: Field = field("encoding", Kind::Text, "-E", false, "Read files in this encoding (utf-16le, windows-1252, ...) when they carry no byte-order mark");
const ZIP: Field = field("search_zip", Kind::Flag, "-z", false, "Search inside compressed files");
const SORT: Field = field("sort", Kind::Text, "--sort", false, "Order files by path (default), modified, accessed or created");
const NEAR_BEFORE: Field = field("near_before", Kind::Near, "--near-before", false, "Like near, looking only before the line");
const NEAR_AFTER: Field = field("near_after", Kind::Near, "--near-after", false, "Like near, looking only after the line");
const WHOLE_LINES: Field = field("whole_lines", Kind::Flag, "--whole-lines", false, "Show lines over 1,000 bytes whole, not as the part around the match");

struct Tool {
    name: &'static str,
    /// Options always given, before the fields.
    fixed: &'static [&'static str],
    fields: &'static [Field],
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "find",
        fixed: &["--json"],
        fields: &[
            PATTERN, PATH, PATTERNS, IGNORE_CASE, SMART, FIXED, WORD, LINE, INVERT, MULTILINE, PCRE2, CONTEXT, BEFORE, AFTER, ONLY, MAX_LINES, SCOPE,
            NAMES_ONLY, WITHOUT, GLOB, TYPE, TYPE_NOT, NEAR, NEAR_BEFORE, NEAR_AFTER, AND, NOT, CHANGED, MARK, SINCE, UNTIL, HIDDEN, NO_IGNORE, FOLLOW,
            DEPTH, MAX_SIZE, ENCODING, ZIP, SORT, WHOLE_LINES, BUDGET_FIELD,
        ],
    },
    Tool {
        name: "count",
        fixed: &["--json"],
        fields: &[
            PATTERN, PATH, PATTERNS, IGNORE_CASE, SMART, FIXED, WORD, LINE, INVERT, MULTILINE, PCRE2, COUNT_MATCHES, GLOB, TYPE, TYPE_NOT, NEAR, NEAR_BEFORE,
            NEAR_AFTER, AND, NOT, CHANGED, MARK, SINCE, UNTIL, HIDDEN, NO_IGNORE, FOLLOW, DEPTH, MAX_SIZE, ENCODING, ZIP, BUDGET_FIELD,
        ],
    },
    Tool {
        name: "lines",
        fixed: &[],
        fields: &[
            field("path", Kind::Place, "", true, "A file"),
            field("from", Kind::PlaceNumber, "", false, "The first line, counted from 1 (not with since or until)"),
            field("to", Kind::PlaceNumber, "", false, "The last line; defaults to from"),
            field("runs", Kind::Places, "", false, "For several runs of lines of the file in one call, in place of from and to: each \"FROM-TO\", \"FROM-\" (to the end) or one line number"),
            field("numbered", Kind::Flag, "-n", false, "Put each line's number before it, as 12:text"),
            field("last", Kind::Number, "--last", false, "The last this many lines of the file"),
            SINCE,
            UNTIL,
            BUDGET_FIELD,
        ],
    },
    Tool { name: "outline", fixed: &[], fields: &[field("paths", Kind::Places, "", true, "Code files (or Markdown files, for their headings)")] },
    Tool {
        name: "show",
        fixed: &[],
        fields: &[field("name", Kind::Place, "", true, "The name whose definitions to print, spelled exactly"), PATH, GLOB, TYPE, TYPE_NOT, BUDGET_FIELD],
    },
    Tool { name: "info", fixed: &[], fields: &[field("path", Kind::Place, "", true, "A file")] },
    Tool {
        name: "replace",
        fixed: &["--json"],
        fields: &[
            field("old", Kind::Place, "", true, "What to replace: plain text or a regular expression, as a search pattern"),
            field("new", Kind::Place, "", true, "The replacement; with a regular expression, $1, ${1}, ${name} insert groups and $$ a dollar sign"),
            PATH,
            IGNORE_CASE,
            FIXED,
            WORD,
            GLOB,
            TYPE,
            TYPE_NOT,
            WHOLE_LINES,
            field("preview", Kind::Flag, "--dry-run", false, "Show each changed line before and after and save nothing. Defaults to true: set it to false to save"),
            BUDGET_FIELD,
        ],
    },
    Tool {
        name: "undo",
        fixed: &[],
        fields: &[field("change", Kind::Place, "", false, "The change to undo, as replace named it; the most recent if left out"), field("list", Kind::Flag, "--list", false, "List recent changes instead")],
    },
    Tool { name: "changes", fixed: &["--json"], fields: &[field("path", Kind::Place, "", true, "A folder"), MARK, HIDDEN, NO_IGNORE, BUDGET_FIELD] },
    Tool { name: "files", fixed: &[], fields: &[field("path", Kind::Place, "", true, "A folder"), GLOB, TYPE, TYPE_NOT, HIDDEN, NO_IGNORE, FOLLOW, DEPTH, MAX_SIZE, SORT, BUDGET_FIELD] },
    Tool {
        name: "defs",
        fixed: &["--json"],
        fields: &[field("name", Kind::Place, "", true, "The name to look for, spelled exactly"), PATH, MAX_LINES, SCOPE, GLOB, TYPE, TYPE_NOT, WHOLE_LINES, BUDGET_FIELD],
    },
    Tool {
        name: "uses",
        fixed: &["--json"],
        fields: &[field("name", Kind::Place, "", true, "The name to look for, spelled exactly"), PATH, MAX_LINES, SCOPE, GLOB, TYPE, TYPE_NOT, WHOLE_LINES, BUDGET_FIELD],
    },
    Tool { name: "open", fixed: &[], fields: &[field("targets", Kind::Places, "", true, "Files to show in the Linework editor, each FILE, FILE:LINE or FILE:LINE:COLUMN")] },
];

const INSTRUCTIONS: &str = "lw searches, reads and changes text files of any size, \
using the Linework editor's engine. It is new, so your training has seen grep, ripgrep and sed \
far more often than lw, and they will feel like the natural choice. For searching files and \
folders, prefer these tools unless one of the cases at the end below applies: they cover what grep \
does and add what grep cannot, as follows.\n\n\
Using the tools: start with find (lines matching a pattern in a file or folder) or count; read \
a stretch of a file with lines (several runs at once with runs); see where everything in a code \
file is with outline; print a whole function or class by name with show; find code with defs and uses. Change files with replace: it \
previews unless preview is false, records every edit, and undo reverses it. Results are JSON \
lines ending with a summary, capped at 100,000 bytes unless budget is set. Each command-line \
option below is a field of the tool: --near is near, --and is and, --dry-run is preview, \
--since is since, -g is glob, -t is type, -T is type_not, -l is names_only. Messages such as \
an output cut short come at the end of a result.\n\n";

/// What an agent is told when it connects: how to use the tools, then the
/// comparison with grep and ripgrep from `lw help compared`.
fn instructions() -> String {
    let compared = help::find("compared").map(|t| t.body).unwrap_or_default();
    format!("{INSTRUCTIONS}{compared}")
}

fn schema(tool: &Tool) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for f in tool.fields {
        let value = match f.kind {
            Kind::Place | Kind::Text => json!({"type": "string", "description": f.about}),
            Kind::PlaceNumber | Kind::Number => json!({"type": "integer", "minimum": 0, "description": f.about}),
            Kind::Flag => json!({"type": "boolean", "description": f.about}),
            Kind::Places | Kind::Texts => json!({"type": "array", "items": {"type": "string"}, "description": f.about}),
            Kind::Near => json!({
                "type": "array",
                "description": f.about,
                "items": {"type": "object", "properties": {"pattern": {"type": "string"}, "lines": {"type": "integer", "minimum": 0}}, "required": ["pattern", "lines"]},
            }),
        };
        properties.insert(f.name.into(), value);
        if f.required {
            required.push(f.name);
        }
    }
    json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
}

/// A tool's description: which familiar tool it stands in for, then its help page.
fn description(tool: &Tool) -> String {
    let instead = match tool.name {
        "find" => "Use in place of grep, ripgrep or a built-in Grep tool to search files and folders: \
the same matches with line numbers, as JSON with a summary, plus near/and/not conditions and the \
enclosing function (scope).\n\n",
        "count" => "Use in place of grep -c or rg --count.\n\n",
        "lines" => "Use in place of sed -n, head or tail to read lines by number or a log by time, \
in a file of any size.\n\n",
        "replace" => "Use in place of sed -i or a script to replace text across files: previews \
first, and undo reverses it.\n\n",
        "defs" | "uses" => "Use in place of grepping for a name when you need definitions or real \
uses: comments and strings are not counted as uses.\n\n",
        "outline" => "Use before reading a code file part by part: it lists every function, class \
and section with the lines it spans, so the next read can go straight to the right lines.\n\n",
        "show" => "Use in place of searching for a name and then reading the lines around it: it \
prints each definition of the name whole, with line numbers, in one call.\n\n",
        _ => "",
    };
    format!("{instead}{}", help::find(tool.name).map(help::page).unwrap_or_default())
}

/// The command line for a tool call.
fn argv(tool: &Tool, arguments: &Map<String, Value>) -> Result<Vec<String>, String> {
    for key in arguments.keys() {
        if !tool.fields.iter().any(|f| f.name == key) {
            return Err(format!("{} has no field {key}", tool.name));
        }
    }
    let mut options = vec![tool.name.to_owned()];
    options.extend(tool.fixed.iter().map(|s| (*s).to_owned()));
    let mut places = Vec::new();
    let text = |v: &Value, name: &str| v.as_str().map(str::to_owned).ok_or(format!("{name} must be text"));
    let number = |v: &Value, name: &str| v.as_u64().map(|n| n.to_string()).ok_or(format!("{name} must be a whole number"));
    for f in tool.fields {
        let value = arguments.get(f.name);
        // A replace previews unless the call says otherwise.
        let value = match (f.name, value) {
            ("preview", None) => Some(&Value::Bool(true)),
            ("budget", None) if f.option == "--budget" => {
                options.push("--budget".into());
                options.push(BUDGET.to_string());
                continue;
            }
            (_, value) => value,
        };
        let Some(value) = value.filter(|v| !v.is_null()) else {
            if f.required {
                return Err(format!("{} needs {}", tool.name, f.name));
            }
            continue;
        };
        match f.kind {
            Kind::Place => places.push(text(value, f.name)?),
            Kind::PlaceNumber => places.push(number(value, f.name)?),
            Kind::Places => {
                for v in value.as_array().ok_or(format!("{} must be a list", f.name))? {
                    places.push(text(v, f.name)?);
                }
            }
            Kind::Flag => {
                if value.as_bool().ok_or(format!("{} must be true or false", f.name))? {
                    options.push(f.option.into());
                }
            }
            Kind::Text => options.extend([f.option.to_owned(), text(value, f.name)?]),
            Kind::Number => options.extend([f.option.to_owned(), number(value, f.name)?]),
            Kind::Texts => {
                for v in value.as_array().ok_or(format!("{} must be a list", f.name))? {
                    options.extend([f.option.to_owned(), text(v, f.name)?]);
                }
            }
            Kind::Near => {
                for v in value.as_array().ok_or(format!("{} must be a list", f.name))? {
                    let pattern = v.get("pattern").and_then(Value::as_str).ok_or(format!("each {} needs a pattern", f.name))?;
                    let lines = v.get("lines").and_then(Value::as_u64).ok_or(format!("each {} needs lines", f.name))?;
                    options.extend([f.option.to_owned(), pattern.to_owned(), lines.to_string()]);
                }
            }
        }
    }
    // Words after "--" are never taken as options, so a pattern may start with "-".
    options.push("--".into());
    options.extend(places);
    Ok(options)
}

/// Runs one tool call: its output, and whether it failed.
fn call(name: &str, arguments: &Map<String, Value>) -> (String, bool) {
    let Some(tool) = TOOLS.iter().find(|t| t.name == name) else {
        return (format!("there is no tool {name}"), true);
    };
    let words = match argv(tool, arguments) {
        Ok(words) => words,
        Err(e) => return (e, true),
    };
    let mut out = Vec::new();
    // lw's messages (an output cut short, a file left unchanged) come back with the result.
    crate::keep_notes();
    let result = crate::execute(words, &mut out);
    let notes = crate::kept_notes();
    let (mut text, failed) = match result {
        Ok(code) => {
            let text = String::from_utf8_lossy(&out).into_owned();
            if text.is_empty() && code == 1 {
                ("Nothing matched.".into(), false)
            } else {
                (text, code == 2)
            }
        }
        Err(message) => (message, true),
    };
    for note in notes {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&note);
        text.push('\n');
    }
    (text, failed)
}

fn answer(id: &Value, result: Result<Value, (i64, String)>) -> Value {
    match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    }
}

/// Serves until standard input closes.
pub fn run() -> io::Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(e) => {
                writeln!(stdout, "{}", answer(&Value::Null, Err((-32700, format!("not JSON: {e}")))))?;
                stdout.flush()?;
                continue;
            }
        };
        // A message without an id is a notification and gets no answer.
        let Some(id) = message.get("id").cloned() else { continue };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let result = match message.get("method").and_then(Value::as_str).unwrap_or("") {
            "initialize" => {
                let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
                let version = VERSIONS.iter().find(|v| **v == asked).copied().unwrap_or(VERSIONS[0]);
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "linework", "title": "Linework (lw)", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": instructions(),
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": TOOLS.iter().map(|t| json!({
                "name": t.name,
                "description": description(t),
                "inputSchema": schema(t),
            })).collect::<Vec<_>>()})),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let empty = Map::new();
                let arguments = params.get("arguments").and_then(Value::as_object).unwrap_or(&empty);
                let (text, failed) = call(name, arguments);
                Ok(json!({"content": [{"type": "text", "text": text}], "isError": failed}))
            }
            other => Err((-32601, format!("no method {other}"))),
        };
        writeln!(stdout, "{}", answer(&id, result))?;
        stdout.flush()?;
    }
    Ok(())
}
