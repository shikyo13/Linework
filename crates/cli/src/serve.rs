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
    /// An option whose value is optional, given as OPTION=VALUE.
    Joined,
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
/// The file tools take GNU's options, which have no --budget: their output is
/// cut by the server instead (see `call`).
const FILE_BUDGET: Field = field("budget", Kind::Number, "", false, "Stop after this many bytes of output (default 100000); for more, write to a file with output");
const THREADS: Field = field("threads", Kind::Number, "-j", false, "Threads to use (default: every processor)");
const ZERO: Field = field("zero_terminated", Kind::Flag, "-z", false, "Lines end with a zero byte, not a line feed");
const OUTPUT: Field = field("output", Kind::Text, "-o", false, "Write the result to this file instead of returning it (for results of any size)");
/// find and count write to a file through the server (see `call`): the
/// lines as the command line prints them, with no budget.
const SINK: Field = field("output", Kind::Text, "", false, "Write the results to this file instead of returning them, as the command line prints them (plain lines, not JSON, no budget); the reply says only how much was written");
const DECODE_HEX: Field = field("decode_hex", Kind::Flag, "--decode-hex", false, "Print $HEX[...], hashcat's form for a password it cannot print, as the bytes it stands for (find: with output only)");
const LINE_NUMBERS: Field = field("line_numbers", Kind::Flag, "-n", false, "With output: put each line's number before it");
const NO_NAMES: Field = field("no_filename", Kind::Flag, "-I", false, "With output: leave out file names, which a folder search otherwise puts before each line");
/// The commands that take GNU's options. None may read standard input here,
/// which is the conversation with the client.
const FILE_TOOLS: [&str; 4] = ["split", "cut", "sort", "look"];

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
            DEPTH, MAX_SIZE, ENCODING, ZIP, SORT, WHOLE_LINES, BUDGET_FIELD, SINK, LINE_NUMBERS, NO_NAMES, DECODE_HEX,
        ],
    },
    Tool {
        name: "count",
        fixed: &["--json"],
        fields: &[
            PATTERN, PATH, PATTERNS, IGNORE_CASE, SMART, FIXED, WORD, LINE, INVERT, MULTILINE, PCRE2, COUNT_MATCHES, GLOB, TYPE, TYPE_NOT, NEAR, NEAR_BEFORE,
            NEAR_AFTER, AND, NOT, CHANGED, MARK, SINCE, UNTIL, HIDDEN, NO_IGNORE, FOLLOW, DEPTH, MAX_SIZE, ENCODING, ZIP, BUDGET_FIELD, SINK, NO_NAMES,
        ],
    },
    Tool {
        name: "lines",
        fixed: &[],
        fields: &[
            field("path", Kind::Place, "", true, "A file"),
            field("from", Kind::PlaceNumber, "", false, "The first line, counted from 1 (not with since or until)"),
            field("to", Kind::PlaceNumber, "", false, "The last line; defaults to from"),
            SINCE,
            UNTIL,
            DECODE_HEX,
            BUDGET_FIELD,
        ],
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
    Tool {
        name: "split",
        fixed: &[],
        fields: &[
            field("file", Kind::Place, "", true, "The file to cut into pieces"),
            field("prefix", Kind::Place, "", false, "The start of each piece's name, a path (default x, in the folder lw serve was started in)"),
            field("lines", Kind::Number, "-l", false, "This many lines a piece (the default, 1000, when no way of cutting is given)"),
            field("bytes", Kind::Text, "-b", false, "This many bytes a piece: 10K, 5M, 2G (powers of 1024) or KB, MB, GB (powers of 1000)"),
            field("line_bytes", Kind::Text, "-C", false, "At most this many bytes of whole lines a piece; a longer line is cut"),
            field("number", Kind::Text, "-n", false, "N pieces of about equal size; l/N without cutting lines; r/N lines dealt in turn; K/N, l/K/N or r/K/N returns only piece K"),
            field("numeric_suffixes", Kind::Flag, "-d", false, "Suffixes 00, 01, ... instead of aa, ab, ..."),
            field("numeric_from", Kind::Joined, "--numeric-suffixes", false, "Numeric suffixes starting from this number"),
            field("hex_suffixes", Kind::Flag, "-x", false, "Suffixes 00 ... 0f, 10, ..."),
            field("hex_from", Kind::Joined, "--hex-suffixes", false, "Hexadecimal suffixes starting from this number"),
            field("suffix_length", Kind::Number, "-a", false, "Suffixes of this many characters"),
            field("additional_suffix", Kind::Text, "--additional-suffix", false, "Text after each suffix, such as .txt"),
            field("elide_empty", Kind::Flag, "-e", false, "No empty pieces with number"),
            field("separator", Kind::Text, "-t", false, "Lines end with this byte instead of a line feed (\\0 for a zero byte)"),
            THREADS,
            FILE_BUDGET,
        ],
    },
    Tool {
        name: "cut",
        fixed: &[],
        fields: &[
            field("files", Kind::Places, "", true, "The files whose lines to cut, in order"),
            field("fields", Kind::Text, "-f", false, "Fields to print, separated by delimiter: 3, 1-4, 7- (to the end), -2 (from the start), comma-separated"),
            field("delimiter", Kind::Text, "-d", false, "The field separator, one byte (default a tab)"),
            field("only_delimited", Kind::Flag, "-s", false, "Leave out lines without the delimiter"),
            field("bytes", Kind::Text, "-b", false, "Bytes to print, as a list like fields"),
            field("characters", Kind::Text, "-c", false, "Characters to print (bytes, as GNU cut counts them), as a list like fields"),
            field("complement", Kind::Flag, "--complement", false, "Print everything but the list"),
            field("output_delimiter", Kind::Text, "--output-delimiter", false, "Text between the fields or byte ranges printed"),
            ZERO,
            OUTPUT,
            THREADS,
            field("decode_hex", Kind::Flag, "--decode-hex", false, "Print $HEX[...], hashcat's form for a password it cannot print, as the bytes it stands for"),
            FILE_BUDGET,
        ],
    },
    Tool {
        name: "sort",
        fixed: &[],
        fields: &[
            field("files", Kind::Places, "", true, "The files whose lines to sort together"),
            field("ignore_leading_blanks", Kind::Flag, "-b", false, "Ignore blanks at the start of each key"),
            field("dictionary_order", Kind::Flag, "-d", false, "Only letters, digits and blanks count"),
            field("ignore_case", Kind::Flag, "-f", false, "Letter case does not count"),
            field("general_numeric", Kind::Flag, "-g", false, "Numbers as C's strtod reads them (1e3, 0x10, inf)"),
            field("numeric", Kind::Flag, "-n", false, "Plain decimal numbers (quicker than general_numeric)"),
            field("human_numeric", Kind::Flag, "-h", false, "Sizes such as 2K and 1G"),
            field("month", Kind::Flag, "-M", false, "Months: JAN < FEB < ... < DEC"),
            field("version", Kind::Flag, "-V", false, "Version numbers within text: file2 < file10"),
            field("random", Kind::Flag, "-R", false, "A random order that keeps equal keys together"),
            field("random_source", Kind::Text, "--random-source", false, "A file whose bytes seed the random order"),
            field("reverse", Kind::Flag, "-r", false, "Reverse the order"),
            field("keys", Kind::Texts, "-k", false, "Keys to sort by, in order of importance: F[.C][OPTS][,F[.C][OPTS]], fields F and characters C from 1, OPTS any of bdfghiMnRrV; 2,2 is the second field alone"),
            field("separator", Kind::Text, "-t", false, "Fields are separated by this character (default: blanks begin a field)"),
            field("stable", Kind::Flag, "-s", false, "Lines with equal keys keep their order"),
            field("unique", Kind::Flag, "-u", false, "Only the first of lines that are equal (by their keys)"),
            field("merge", Kind::Flag, "-m", false, "Merge files that are each sorted already"),
            field("check", Kind::Flag, "-c", false, "Check that the file is sorted and name the first line out of order"),
            field("check_quiet", Kind::Flag, "-C", false, "Check that the file is sorted, answering only sorted or not"),
            field("output", Kind::Text, "-o", false, "Write the sorted lines to this file, which may be one of the inputs (replaced only when the sort succeeds)"),
            field("buffer_size", Kind::Text, "-S", false, "Memory to use: KiB, or with b, K, M, G, T or % (default half the memory available)"),
            field("temporary_directory", Kind::Texts, "-T", false, "Folders for temporary files (default the Windows temporary folder)"),
            field("decode_hex", Kind::Flag, "--decode-hex", false, "Compare keys with $HEX[...] (hashcat's form for a password it cannot print) decoded to its bytes; lines are written as they are"),
            ZERO,
            THREADS,
            FILE_BUDGET,
        ],
    },
    Tool {
        name: "look",
        fixed: &[],
        fields: &[
            field("string", Kind::Place, "", false, "Print the lines that begin with this (or give keys)"),
            field("file", Kind::Place, "", true, "A sorted file (sorted with dictionary_order and ignore_case to match those options)"),
            field("keys", Kind::Text, "--keys", false, "A file of keys, one a line: print every line of the file that begins with any of them, once, in the file's order (the file need not be sorted when there are many keys)"),
            field("dictionary_order", Kind::Flag, "-d", false, "Only letters, digits and blanks count, in the string and the lines"),
            field("ignore_case", Kind::Flag, "-f", false, "Letter case does not count"),
            field("terminate", Kind::Text, "-t", false, "Compare the string only up to and including its first occurrence of this character"),
            THREADS,
            FILE_BUDGET,
        ],
    },
];

const INSTRUCTIONS: &str = "lw searches, reads and changes text files of any size, \
using the Linework editor's engine. It is new, so your training has seen grep, ripgrep and sed \
far more often than lw, and they will feel like the natural choice. For searching files and \
folders, prefer these tools unless one of the cases at the end below applies: they cover what grep \
does and add what grep cannot, as follows.\n\n\
Using the tools: start with find (lines matching a pattern in a file or folder) or count; read \
a stretch of a file with lines; find code with defs and uses. Change files with replace: it \
previews unless preview is false, records every edit, and undo reverses it. Results are JSON \
lines ending with a summary, capped at 100,000 bytes unless budget is set; for results of any \
size, give find or count an output file and they write plain lines there instead. Each command-line \
option below is a field of the tool: --near is near, --and is and, --dry-run is preview, \
--since is since, -g is glob, -t is type, -T is type_not, -l is names_only. Messages such as \
an output cut short come at the end of a result.\n\n\
For very large data files (word lists, hash lists, dumps), split, cut, sort and look take GNU \
split's, cut's and sort's and util-linux look's options as named fields, work on every thread on \
files larger than memory, and give byte-identical results to those tools; look finds lines by \
bisection in a sorted file of any size in milliseconds. Give their results an output file when \
they may be large.\n\n";

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
            Kind::Joined => json!({"type": "string", "description": f.about}),
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
        "split" => "Use in place of split to cut a file of any size into pieces by lines, bytes or count.\n\n",
        "cut" => "Use in place of cut or awk -F to take fields or bytes from every line of a file of any size.\n\n",
        "sort" => "Use in place of sort to sort, deduplicate (unique), merge or check files of any size, \
larger than memory.\n\n",
        "look" => "Use in place of searching a sorted file for lines that start with a string: finds them \
by bisection, in milliseconds on any size.\n\n",
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
            // The file tools' budget, and find's and count's output file, are
            // applied by `call`, not given to the command.
            ("budget" | "output", _) if f.option.is_empty() => continue,
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
            Kind::Joined => options.push(format!("{}={}", f.option, text(value, f.name)?)),
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
    let file_tool = FILE_TOOLS.contains(&name);
    if file_tool {
        // "-" would read standard input, which here is the conversation itself.
        let piped = words.iter().skip_while(|w| *w != "--").skip(1).any(|w| w == "-")
            || arguments.get("keys").and_then(Value::as_str) == Some("-");
        if piped {
            return (format!("{name} cannot read piped input here; give a file"), true);
        }
        if name == "look" && arguments.get("string").is_none() && arguments.get("keys").is_none() {
            return ("look needs a string or keys".into(), true);
        }
    }
    if name == "find" && arguments.get("decode_hex") == Some(&Value::Bool(true)) && arguments.get("output").is_none_or(Value::is_null) {
        return ("decode_hex writes plain lines, so find needs an output file with it".into(), true);
    }
    if !file_tool {
        if let Some(path) = arguments.get("output").filter(|v| !v.is_null()) {
            let Some(path) = path.as_str() else {
                return ("output must be text".into(), true);
            };
            return to_file(name, words, path);
        }
    }
    let limit = arguments.get("budget").and_then(Value::as_u64).unwrap_or(BUDGET) as usize;
    let mut out = Capped { kept: Vec::new(), limit: if file_tool { limit } else { usize::MAX }, cut: false };
    // lw's messages (an output cut short, a file left unchanged) come back with the result.
    crate::keep_notes();
    let result = crate::execute(words, &mut out);
    let notes = crate::kept_notes();
    let cut = out.cut;
    let (mut text, failed) = match result {
        Ok(code) => {
            let text = String::from_utf8_lossy(&out.kept).into_owned();
            if text.is_empty() && code == 1 {
                // sort's check answers 1 for a file out of order; the others for no match.
                (if name == "sort" { "Not sorted." } else { "Nothing matched." }.into(), false)
            } else if text.is_empty() && name == "sort" && ["check", "check_quiet"].iter().any(|k| arguments.get(*k) == Some(&Value::Bool(true))) {
                ("Sorted.".into(), false)
            } else {
                (text, code == 2)
            }
        }
        // The cap below ended the command; what it wrote before that is the answer.
        Err(_) if cut => (String::from_utf8_lossy(&out.kept).into_owned(), false),
        Err(message) => (message, true),
    };
    if cut {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("lw: output cut at {limit} bytes (budget); give output a file for all of it, or a larger budget\n"));
    }
    for note in notes {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&note);
        text.push('\n');
    }
    (text, failed)
}

/// Runs find or count with its lines written to `path`: plain, as the
/// command line prints them, without JSON or a budget.
fn to_file(name: &str, words: Vec<String>, path: &str) -> (String, bool) {
    let words: Vec<String> = {
        let mut skip = false;
        words
            .into_iter()
            .filter(|w| {
                if std::mem::take(&mut skip) {
                    return false;
                }
                if w == "--budget" {
                    skip = true;
                    return false;
                }
                w != "--json"
            })
            .collect()
    };
    let file = match std::fs::File::create(path) {
        Ok(f) => f,
        Err(e) => return (format!("could not create {path}: {e}"), true),
    };
    let mut out = Counted { inner: io::BufWriter::with_capacity(8 << 20, file), bytes: 0, lines: 0 };
    crate::keep_notes();
    let result = crate::execute(words, &mut out);
    let flushed = out.flush();
    let notes = crate::kept_notes();
    let (mut text, failed) = match (result, flushed) {
        (Err(message), _) => (message, true),
        (_, Err(e)) => (format!("could not write {path}: {e}"), true),
        (Ok(1), _) if out.bytes == 0 => (format!("Nothing matched; {path} is empty."), false),
        (Ok(code), _) => (
            format!(
                "{name}: wrote {} {}, {} bytes, to {path}.",
                out.lines,
                if out.lines == 1 { "line" } else { "lines" },
                out.bytes
            ),
            code == 2,
        ),
    };
    for note in notes {
        text.push(char::from(10));
        text.push_str(&note);
    }
    (text, failed)
}

/// A writer that counts what passes through it.
struct Counted<W: Write> {
    inner: W,
    bytes: u64,
    lines: u64,
}
impl<W: Write> Write for Counted<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(bytes)?;
        self.bytes += n as u64;
        self.lines += bytes[..n].iter().filter(|b| **b == 10).count() as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The output of a file tool, kept up to `limit` bytes. Past it the command is
/// told its output has closed, as when a pipe's reader stops, and ends.
struct Capped {
    kept: Vec<u8>,
    limit: usize,
    cut: bool,
}
impl Write for Capped {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let room = self.limit - self.kept.len();
        if bytes.len() <= room {
            self.kept.extend_from_slice(bytes);
            return Ok(bytes.len());
        }
        self.kept.extend_from_slice(&bytes[..room]);
        self.cut = true;
        Err(io::ErrorKind::BrokenPipe.into())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
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
                    "serverInfo": {"name": "linework", "title": "Linework (lw)", "version": env!("LINEWORK_VERSION")},
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
