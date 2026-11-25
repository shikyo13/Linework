//! The help for lw: one text for each command and topic, shown by `lw help`,
//! given to agents as tool descriptions by `lw serve`, and written out as the
//! Markdown reference by `lw help --markdown`.

pub struct Topic {
    pub name: &'static str,
    /// One line, shown in the overview.
    pub summary: &'static str,
    /// How to call it; empty for a topic that is not a command.
    pub usage: &'static str,
    pub body: &'static str,
}

pub const OVERVIEW: &str = r#"lw: search, read and change text files of any size, from the command line.

lw is the command-line side of Linework, a text editor for Windows. It uses
the editor's own file engine: it reads files of many gigabytes in parallel,
opens them in every encoding the editor opens, and saves changes the way the
editor saves them. It is built for people and for programs (scripts and AI
agents) alike: every result can be printed as plain lines, grouped for
reading, or as JSON.

FIRST STEPS

  lw find TODO src                 every line with "TODO" in the folder src
  lw find -i "error|warn" app.log  lines matching a pattern, any letter case
  lw count status=503 app.log      how many times something occurs
  lw lines app.log 1200 1250       lines 1200 to 1250 of a file of any size
  lw lines --since 14:00 --until 14:05 app.log
                                   five minutes of a time-ordered log
  lw defs parse_header src         where a name is defined in code
  lw uses parse_header src         where it is used (not in comments)
  lw replace --dry-run old new src preview a replacement across a folder
  lw replace old new src           do it, then: lw undo
  lw changes src                   what changed in a folder since last time
  lw open src/main.rs:120          show a place in the Linework editor

COMMANDS
{commands}
TOPICS
{topics}
Run lw help NAME for a command or topic, for example lw help find or
lw help patterns. lw help --markdown prints all of it as one document.

Exit code: 0 when something was found or done, 1 when nothing matched, 2 on
an error (with a message on the error stream).
"#;

pub const TOPICS: &[Topic] = &[
    Topic {
        name: "find",
        summary: "Print the lines that match a pattern, in a file or a folder",
        usage: "lw find [-i] [-F] [-w] [-m N] [OUTPUT] [CONDITIONS] [--since TIME] [--until TIME] PATTERN PATH",
        body: r#"Prints every line that matches PATTERN.

PATH may be a file or a folder.
  A file:   each matching line as  LINE:TEXT,  lines counted from 1.
  A folder: each matching line as  PATH/FILE:LINE:TEXT  for every file the
            folder rules leave in (see lw help folders), files in path order.

PATTERN is plain text unless it has one of the characters  \ ^ $ . | ? * + ( ) [ ] { },
in which case it is a regular expression (see lw help patterns).

OPTIONS
  -i            ignore letter case
  -F            take PATTERN as plain text even if it has special characters
  -w            match whole words only
  -m N          at most N lines: in total for a file, from each file for a folder
  --hidden      in a folder, include hidden files and names starting with a dot
  --no-ignore   in a folder, ignore the .gitignore, .ignore and .rgignore files
  -j N          in a folder, use N threads (default: three quarters of the
                processors, at most 12)

OUTPUT (see lw help output)
  --json        one JSON object per line, then a summary
  --heading     each file's name once, then its lines
  --scope       group lines under the function, class or section around them
  --budget N    stop printing after N bytes, at a line break
  --stats       print the totals on the error stream

CONDITIONS (see lw help conditions)
  --near PATTERN N   keep a line only if PATTERN is within N lines of it
  --and PATTERN      keep only files that also contain PATTERN
  --not PATTERN      keep only files that do not contain PATTERN
  --changed          in a folder, search only files changed since lw changes
                     last looked (see lw help changes)

TIME (one log file; see lw help time)
  --since TIME, --until TIME   search only that part of a time-ordered log

EXAMPLES
  lw find TODO src
  lw find -i -w "fixme|todo" src
  lw find --json --scope -m 5 handle_request src
  lw find --near "status=500" 3 "timeout" server.log
  lw find --since "2026-11-21 21:40" --until 21:45 status=503 app.log

A file of any size is read in parallel and its lines are numbered in the same
pass, so a 10 GB log is searched in about half a second when it is in memory."#,
    },
    Topic {
        name: "count",
        summary: "Count matches in a file, or matching lines per file in a folder",
        usage: "lw count [-i] [-F] [-w] [OUTPUT] [CONDITIONS] [--since TIME] [--until TIME] PATTERN PATH",
        body: r#"Counts what matches PATTERN.

  A file:   prints the number of matches (two on one line count as two).
            With conditions or a time span, it counts matching lines instead.
  A folder: prints  PATH/FILE:COUNT  for each file with matching lines, where
            COUNT is the number of matching lines in that file.

Takes the same pattern rules, options, conditions and time span as lw find.
--json prints {"path": ..., "count": ...} per file and a summary.

EXAMPLES
  lw count status=503 app.log
  lw count -w unwrap src
  lw count --json -i error logs"#,
    },
    Topic {
        name: "lines",
        summary: "Print lines by number, or the part of a log between two times",
        usage: "lw lines FILE FROM [TO]\n       lw lines [--since TIME] [--until TIME] FILE",
        body: r#"Prints a run of lines exactly as they are in the file, line breaks included.

  lw lines FILE FROM [TO]   lines FROM to TO, counted from 1; TO defaults to FROM.
                            The first time a large file is read its lines are
                            counted; Linework keeps that count in its profile,
                            so the next time this takes milliseconds.

  lw lines --since TIME --until TIME FILE
                            the part of a time-ordered log between two times,
                            found by bisecting the timestamps: a few dozen small
                            reads, even in a file of many gigabytes. Either end
                            may be left out. See lw help time.

OPTIONS (for a time span)
  --budget N   stop after N bytes, at a line break
  --stats      print where the span starts and ends on the error stream

EXAMPLES
  lw lines app.log 80000000 80000010
  lw lines --since 21:40 --until 21:45 app.log
  lw lines --since 2026-11-21 app.log"#,
    },
    Topic {
        name: "info",
        summary: "Print a file's size, line count, encoding and line ends",
        usage: "lw info FILE",
        body: r#"Prints four lines:

  bytes N        the size of the file's text
  lines N        the number of lines (a final line break starts an empty last line)
  encoding NAME  the encoding Linework detected, such as UTF-8 or UTF-16 LE
  line ends X    CRLF, LF or CR, from the first line break

The first time a large file is read its lines are counted on several threads;
the count is kept in the profile for the next time.

EXAMPLE
  lw info app.log"#,
    },
    Topic {
        name: "replace",
        summary: "Replace text in a file or a folder, with a preview and undo",
        usage: "lw replace [-i] [-F] [-w] [--dry-run] [--json] OLD NEW PATH",
        body: r#"Replaces every match of OLD with NEW and saves the files.

PATH may be a file or a folder; in a folder, exactly the files lw find OLD
would show are changed. Each file is changed and saved by Linework's engine:
it keeps its encoding and line ends, and it is not saved if another program
changed it on disk after it was read.

OLD follows the same rules as a search pattern (see lw help patterns). When
OLD is a regular expression, NEW may insert what a group matched:
  $1 or ${1}        the first group;   ${1}x  when a letter or digit follows
                    ($1x would mean a group named "1x")
  $name, ${name}    a named group (?P<name>...)
  $$                a dollar sign
When OLD is plain text, NEW is inserted exactly as written.

ALWAYS PREVIEW FIRST
  --dry-run    show each changed line before (-) and after (+), save nothing
  --json       with --dry-run: {"path","line","before","after"} per line;
               without: {"path","replacements"} per file; then a summary

UNDO
Before any file is saved, the exact edits are written to a change record in
the profile. lw undo reverses the whole change; see lw help undo.

EXAMPLES
  lw replace --dry-run getUser fetchUser src
  lw replace getUser fetchUser src
  lw replace -w "colou?r" color docs
  lw replace "(\w+)@old\.com" "${1}@new.com" contacts.csv"#,
    },
    Topic {
        name: "undo",
        summary: "Undo a replace, file by file, where nothing changed since",
        usage: "lw undo [--list] [CHANGE]",
        body: r#"Reverses the last lw replace, or the one named CHANGE.

Each file is restored only if every place the replace changed still holds
the text the replace put there. A file edited since is left as it is and
reported, and the change stays open, so it can be undone again once that
file is dealt with. A replace that was stopped partway (a crash or a power
cut) can be undone the same way; files it never saved are left alone.

  lw undo            undo the most recent change not yet undone
  lw undo CHANGE     undo that change; its name is printed by lw replace
  lw undo --list     the 20 most recent changes: name, files, replacements,
                     whether undone, and the command that made them

Change records are in the changes folder of the profile (see lw help profile).
They take about 50 bytes per replacement; deleting old ones is safe once their
changes need no undoing."#,
    },
    Topic {
        name: "changes",
        summary: "List the files added, changed or removed in a folder since the last look",
        usage: "lw changes [--mark NAME] [--json] [--hidden] [--no-ignore] FOLDER",
        body: r#"Compares the folder with the snapshot taken the last time lw changes looked
at it, prints what differs, and takes a new snapshot.

  added    PATH     a file that was not there before
  changed  PATH     a file whose size or last write time changed
  removed  PATH     a file that is gone

The first run on a folder only takes the snapshot. A snapshot holds each
file's path, size and write time, never its contents; an edit that keeps both
the size and the write time is not seen. The same folder rules as lw find
apply (see lw help folders).

  --mark NAME   keep a separate snapshot under NAME (default "last"), for
                example one per task or per agent
  --json        {"change","path"} per file, then a summary

lw find --changed PATTERN FOLDER searches only the files added or changed
since the snapshot, without taking a new one; use it to re-check just what
moved.

EXAMPLES
  lw changes src
  lw find --changed TODO src
  lw changes --mark review --json src"#,
    },
    Topic {
        name: "files",
        summary: "List the files a folder search would read",
        usage: "lw files [--hidden] [--no-ignore] [-j N] FOLDER",
        body: r#"Prints the path of every file a folder search would read, one per line, by
the folder rules (see lw help folders). Useful to check why a file is or is
not searched.

  --hidden      include hidden files and names starting with a dot
  --no-ignore   ignore the .gitignore, .ignore and .rgignore files
  --budget N    stop after N bytes, at a line break
  --account     print where the listing's time went, step by step

The list is in path order.

EXAMPLES
  lw files src
  lw files --hidden --no-ignore src"#,
    },
    Topic {
        name: "defs",
        summary: "Find where a name is defined in code",
        usage: "lw defs [-m N] [OUTPUT] NAME PATH",
        body: r#"Prints the lines where NAME is defined: a function, method, class, struct,
type, enum or module, by the same rules as the editor's outline.

Languages read with a full parser: Rust, Python, JavaScript, TypeScript and
TSX, Go, C, C++, C#, Java. Read by line outline (definitions only, no uses):
PowerShell, shell, Ruby, Lua, SQL and Markdown (where a heading is a definition).

Only files that contain NAME as a whole word are parsed, and files over 4 MiB
are not. Takes the output options of lw find; --scope shows what each
definition sits inside.

EXAMPLES
  lw defs Document crates
  lw defs --json --scope handle_request src"#,
    },
    Topic {
        name: "uses",
        summary: "Find where a name is used in code, not in comments or strings",
        usage: "lw uses [-m N] [OUTPUT] NAME PATH",
        body: r#"Prints the lines where NAME is used as an identifier in code. A mention in
a comment or a string is not a use, and the definition itself is not either
(see lw defs). Same languages, limits and options as lw defs.

A name is matched by spelling, not by what it refers to: two different things
with the same name are both found.

EXAMPLES
  lw uses parse_header src
  lw uses --scope -m 3 useState app"#,
    },
    Topic {
        name: "open",
        summary: "Show a file in the Linework editor, at a line and column",
        usage: "lw open [--dry-run] FILE[:LINE[:COLUMN]]...",
        body: r#"Shows each file in the Linework window that is already open, or starts it.
LINE and COLUMN are counted from 1. With --dry-run the command is printed
instead of run.

EXAMPLES
  lw open src/main.rs:120
  lw open notes.md app.log:80000000:5"#,
    },
    Topic {
        name: "serve",
        summary: "Answer AI agents as a Model Context Protocol tool server",
        usage: "lw serve",
        body: r#"Runs lw as a Model Context Protocol (MCP) server on standard input and
output, so an AI agent can call lw's commands as tools without starting a
program for each one. Each tool takes the options of the command it is named
after as JSON fields, and its description is this help.

To add it to Claude Code:   claude mcp add linework -- lw serve
To add it to Codex, in ~/.codex/config.toml:
  [mcp_servers.linework]
  command = "lw"
  args = ["serve"]

Tool results are capped at 100,000 bytes unless a call asks for another
budget, so a broad search cannot flood an agent's context; the summary says
when output was cut."#,
    },
    Topic {
        name: "patterns",
        summary: "How PATTERN, OLD and NAME are read",
        usage: "",
        body: r#"A pattern is plain text unless it contains one of  \ ^ $ . | ? * + ( ) [ ] { }.
Then it is a regular expression in the syntax of Rust's regex library, which
is close to Perl's:

  .        any character          \d \w \s   digit, word character, space
  a|b      either                 [a-z]      one of a range
  x* x+ x? repeats                x{2,5}     two to five times
  ^ $      start and end of line  \b         a word boundary
  (x)      a group                (?P<name>x) a named group
  (?i)     ignore case from here

Back-references and look-around are not supported. Write -F to take a
pattern with special characters as plain text, for example lw find -F "a.b()".

In a folder, a line matches when the pattern matches inside that line. In a
single file the pattern may also match across lines, and the line where the
match starts is reported.

-i ignores letter case and -w matches whole words only; both work with plain
text and with patterns."#,
    },
    Topic {
        name: "folders",
        summary: "Which files a folder search reads",
        usage: "",
        body: r#"A folder search reads every file under the folder except:

  - files the .gitignore files leave out, inside a Git work tree, and files the
    .ignore and .rgignore files leave out anywhere; files of folders above
    the searched folder count too. --no-ignore turns these rules off.
  - hidden files and names starting with a dot, unless --hidden is given or an
    ignore file brings one back with !NAME.
  - links, which are not followed.
  - binary files: a file with a zero byte anywhere is left out, the same way
    every time. A file starting with a UTF-16 byte-order mark is decoded and
    searched instead; positions in its results are positions in its text as UTF-8.

lw files FOLDER lists exactly the files that are read.

These are ripgrep's rules, so lw and ripgrep read the same files. (The
editor's Find in folder also applies .gitignore files outside Git work trees
and always skips .git, .hg, .svn, node_modules and .venv.)"#,
    },
    Topic {
        name: "output",
        summary: "Plain, grouped and JSON results, and limits on output",
        usage: "",
        body: r#"PLAIN (the default)
  a file:   LINE:TEXT
  a folder: PATH/FILE:LINE:TEXT   (PATH as you wrote it, / between names)

--heading prints each file's path once, then its lines as LINE:TEXT, with a
blank line between files.

--scope groups each file's lines under the definition around them:
  src/server.rs
    function handle_request (lines 40-118)
  52:    let body = read_body(&req)?;
It parses each file with a match, so it costs more on large searches.

--json prints one object per matching line:
  {"path": "src/server.rs", "line": 52, "column": 9, "byte": 1834,
   "text": "    let body = read_body(&req)?;", "matches": [[15, 24]],
   "scope": {"name": "handle_request", "kind": "function", "line": 40, "end_line": 118}}
  line, column  counted from 1; column in characters
  byte          where the first match starts in the file
  text          the line without its line break
  matches       each match as [start, end) byte offsets within text
  scope         only with --scope
and as the last line a summary:
  {"summary": {"files_searched", "files_matched", "lines", "matches",
               "bytes_searched", "output_bytes", "cut_short", "seconds"}}
"lines" is null and "matches" set when matches, not lines, were counted.

--budget N stops printing at the last line break before N bytes, says so on
the error stream (or sets "cut_short" in JSON), and keeps the exit code.

The files of a folder search are always printed in path order, so the same
search gives the same output every time."#,
    },
    Topic {
        name: "conditions",
        summary: "Keep results only when other patterns are near or present",
        usage: "",
        body: r#"lw find and lw count can require more than one pattern. Each condition may
be given several times; all must hold.

  --near PATTERN N   keep a matching line only if PATTERN matches on a line at
                     most N lines before or after it (0: the same line)
  --and PATTERN      keep a file's results only if PATTERN occurs in the file
  --not PATTERN      keep a file's results only if PATTERN does not occur in it

Condition patterns follow the same rules and options (-i, -F, -w) as the main
pattern.

EXAMPLES
  lw find --near "retry" 2 "timeout" server.log
  lw find --and "import React" --not "test" useEffect src"#,
    },
    Topic {
        name: "time",
        summary: "Times for --since and --until, and the logs they work on",
        usage: "",
        body: r#"--since and --until work on one log whose lines start with timestamps in
time order. These formats are read; the first timestamped line decides which:

  ISO            2026-11-21 21:40:15.123   (also with T, a comma, or no seconds)
  web server     [21/Nov/2026:21:40:15 +0000]
  syslog         Nov 21 21:40:15            (no year: compared from the month)

A line without a timestamp, such as part of a stack trace, goes with the line
before it.

Write a time as:
  2026-11-21 21:40:15.5   a date and time; seconds and fraction may be left out
  2026-11-21              a whole day
  21:40                   a time on the date of the log's first line

Each end covers what it names at the precision written: --until 21:45
includes everything up to 21:45:59.999, and --until 2026-11-21 the whole day.

The span is found by bisecting the timestamps, so its cost does not grow with
the size of the log. A log that is not in time order (for example one whose
clock goes back) gives a span that is right around its ends only."#,
    },
    Topic {
        name: "profile",
        summary: "What lw keeps between runs, and where",
        usage: "",
        body: r#"lw shares the Linework editor's profile folder:
%LOCALAPPDATA%\Linework, or the folder named by the LINEWORK_PROFILE variable.

  line-index\     line counts of files of 1 GiB or more, so a second look at a
                  large file need not count its lines again (4 bytes per 64 KiB)
  save-records\   edits being written into large files; a save cut short is
                  set right the next time Linework or lw starts. Leave it alone.
  changes\        lw replace change records, for lw undo
  marks\          lw changes snapshots

Everything but save-records may be deleted at any time; the only loss is speed,
or the ability to undo old changes."#,
    },
];

/// The overview, with the lists of commands and topics filled in.
pub fn overview() -> String {
    let list = |commands: bool| {
        TOPICS
            .iter()
            .filter(|t| t.usage.is_empty() != commands)
            .map(|t| format!("  {:11}{}\n", t.name, t.summary))
            .collect::<String>()
    };
    OVERVIEW.replace("{commands}", &list(true)).replace("{topics}", &list(false))
}

pub fn find(name: &str) -> Option<&'static Topic> {
    TOPICS.iter().find(|t| t.name == name)
}

/// One topic as the terminal shows it.
pub fn page(topic: &Topic) -> String {
    let mut text = format!("lw {}: {}\n\n", topic.name, topic.summary);
    if !topic.usage.is_empty() {
        text.push_str(&format!("usage: {}\n\n", topic.usage));
    }
    text.push_str(topic.body);
    text.push('\n');
    text
}

/// Everything, as one Markdown document.
pub fn markdown() -> String {
    let mut text = String::from("# lw reference\n\nThis file is generated by `lw help --markdown`; edit `crates/cli/src/help.rs` instead.\n\n");
    text.push_str("```text\n");
    text.push_str(&overview());
    text.push_str("\n```\n");
    for topic in TOPICS {
        text.push_str(&format!("\n## {}\n\n{}\n\n", topic.name, topic.summary));
        if !topic.usage.is_empty() {
            text.push_str(&format!("```text\n{}\n```\n\n", topic.usage));
        }
        text.push_str(&format!("```text\n{}\n```\n", topic.body));
    }
    text
}
