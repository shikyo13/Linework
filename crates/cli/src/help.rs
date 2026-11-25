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

lw is the command-line side of Linework, a text editor for Windows. It
searches the way grep and ripgrep do and takes their options, so what you
know from them works here; it reads files of many gigabytes in parallel with
the editor's own engine, and adds what they lack: code structure, conditions,
time spans in logs, and replacing with a preview and undo. It is built for
people and for programs (scripts and AI agents) alike.

FIRST STEPS

  lw TODO src                      every line with "TODO" in the folder src
  lw -i "error|warn" app.log       lines matching a pattern, any letter case
  lw -C2 -t rust unwrap src        two lines of context, Rust files only
  lw -c status=503 app.log         how many lines match
  lw lines app.log 1200 1250       lines 1200 to 1250 of a file of any size
  lw lines --since 14:00 --until 14:05 app.log
                                   five minutes of a time-ordered log
  lw defs parse_header src         where a name is defined in code
  lw uses parse_header src         where it is used (not in comments)
  lw replace --dry-run old new src preview a replacement across a folder
  lw replace old new src           do it, then: lw undo
  lw changes src                   what changed in a folder since last time
  lw open src/main.rs:120          show a place in the Linework editor

lw PATTERN [PATH...] is lw find PATTERN [PATH...]. With no PATH, lw searches
what is piped to it, or else the current folder.

COMMANDS
{commands}
TOPICS
{topics}
Run lw help NAME for a command or topic, for example lw help find or
lw help options. Coming from grep or ripgrep? lw help compared says what
carries over, what lw adds and where it differs. lw help --markdown prints all
of it as one document.

Exit code: 0 when something was found or done, 1 when nothing matched, 2 on
an error (with a message on the error stream), unless -q found a match.
"#;

pub const TOPICS: &[Topic] = &[
    Topic {
        name: "find",
        summary: "Print the lines that match a pattern, in files, folders or what is piped in",
        usage: "lw [find] [OPTIONS] PATTERN [PATH...]\n       lw [find] [OPTIONS] -e PATTERN... [PATH...]",
        body: r#"Prints every line that matches PATTERN, in each PATH: a file, a folder
(searched through, by the rules in lw help folders), or - for what is piped
in. With no PATH, lw searches what is piped in, or else the current folder.

  One file:            LINE:TEXT
  Several, or folders: PATH:LINE:TEXT, the files of a folder in path order
  In a console:        each file's path once above its lines, in colour

The options are ripgrep's, and grep's where ripgrep has none of that name;
lw help options lists every one. The ones used most:

PATTERNS (lw help patterns)
  -i, -s, -S      ignore case; match case; ignore case unless PATTERN has a capital
  -w, -x          whole words; whole lines
  -F              plain text, even with special characters
  -e PATTERN      a pattern; may be given many times (then each other word is a path)
  -f FILE         patterns from a file, one per line
  -v              the lines that do not match
  -U              let a pattern match across lines
  -P              PCRE2: look-around and back-references

WHICH FILES (lw help folders)
  -g GLOB         only files matching GLOB (*.rs, src/**, !*.min.js leaves out)
  -t TYPE, -T TYPE  only, or not, files of a type (lw --type-list)
  --hidden, -u    include hidden files; -u ignores the ignore files, -uu both
  -L              follow links
  -d NUM          at most NUM folders deep

WHAT IS SHOWN (lw help output)
  -A N, -B N, -C N  lines of context after, before, around each match
  -o              only the matched text      -r TEXT   show each match replaced
  -n, -N          line numbers on (the default) or off
  --column, -b    the column, the byte offset
  -c, --count-matches   matching lines, or matches, per file
  -l, --files-without-match   only the names of files with, or without, a match
  -m N            at most N matching lines from each file
  -q              nothing; the exit code says whether something matched
  --json          one JSON object per line, then a summary
  --heading, --color, --vimgrep, -0, --stats   see lw help output

LW'S OWN
  --scope         the functions, classes or sections each line sits in
  --near PATTERN N, --and PATTERN, --not PATTERN   (lw help conditions)
  --since TIME, --until TIME   a time span of a log (lw help time)
  --changed       only files changed since lw changes last looked
  --budget N      stop printing after N bytes, at a line break

EXAMPLES
  lw TODO src
  lw -i -w "fixme|todo" src
  lw -t ts -g "!*.d.ts" -C1 useState src
  lw -l "react-dom/server" src
  lw -c -t rust unwrap
  lw --json --scope -m 5 handle_request src
  lw --near "status=500" 3 timeout server.log
  lw --since "2026-11-21 21:40" --until 21:45 status=503 app.log
  git log | lw -i fix

A file over 4 MiB is searched in parallel with its lines numbered in the same
pass, so a 10 GB log is searched in about half a second when it is in memory."#,
    },
    Topic {
        name: "count",
        summary: "Count the matching lines, or the matches, in each file",
        usage: "lw count [OPTIONS] PATTERN [PATH...]",
        body: r#"lw count is lw find -c: for each file with a match, the number of matching
lines, as COUNT for one file or PATH:COUNT for several. --count-matches counts
matches instead (two on one line count as two); with -U and a pattern that can
match a line end, -c counts matches too, as ripgrep does.

--include-zero lists files without a match as well. With --json, each file is
{"path": ..., "count": ...}, then a summary.

Takes every option of lw find (lw help options); -m N stops at N lines a file.

EXAMPLES
  lw count status=503 app.log
  lw count -w unwrap src
  lw count --count-matches -i error logs
  lw count --json -t ts deprecated src"#,
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
        usage: "lw replace [-i] [-F] [-w] [FILES] [--dry-run] [--json] OLD NEW PATH",
        body: r#"Replaces every match of OLD with NEW and saves the files.

PATH may be a file or a folder; in a folder, exactly the files lw find OLD
would show are changed, and -g, -t and -T choose among them as in lw find
(lw replace -t ts -g "!*.d.ts" old new src). Each file is changed and saved by Linework's engine:
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
  --dry-run    show each changed line before (-) and after (+), save nothing;
               a line over 1,000 bytes shows the same part of both, around
               its first change (--whole-lines shows all of it)
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
apply (see lw help folders), except -g, -t and -T: a snapshot is always of the
whole folder, and lw find --changed -t rust narrows the search instead.

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
        summary: "List the files a search would read",
        usage: "lw files [OPTIONS] [PATH...]",
        body: r#"lw files is lw --files: the path of every file a search of PATH (the current
folder by default) would read, one per line, by the rules in lw help folders.
It takes every option that chooses files (-g, -t, -T, --hidden, -u, -L, -d,
--max-filesize, the --no-ignore family), and --sort, -0 and --path-separator.
Useful to check why a file is or is not searched, and to list files by name
or type.

The list is in path order (ignoring letter case); --sort path orders names as
bytes, folder by folder, as ripgrep does, and --sort modified, accessed or
created by those times.

EXAMPLES
  lw files src
  lw files -g "use*.md*" docs
  lw files -t rust -g "!tests/**" crates
  lw files -uu --sort modified ."#,
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
are not. Takes the file choices (-g, -t, -T) and output options of lw find;
--scope shows what each definition sits inside.

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
        name: "compared",
        summary: "lw for grep and ripgrep users: what carries over, what lw adds, where it differs",
        usage: "",
        body: r#"lw takes ripgrep's options, and grep's where ripgrep has none of that name,
and prints what they print: lw -C2 -w -t rust foo src gives the same lines,
separators, colours and exit code as rg -n -C2 -w -t rust foo src. (Checked
byte for byte on more than 200 cases against ripgrep 15 and GNU grep 3.)
Where the two use one letter differently, ripgrep's meaning wins and grep's
long name still works:

  -L  follow links           (grep's -L is --files-without-match)
  -r  show matches replaced  (lw always searches folders through)
  -s  match case             (grep's -s is --no-messages)
  -h  help                   (grep's -h is -I, no file names)
  -U  match across lines     -z  search compressed files
  -E  name an encoding       (grep's -E is --extended-regexp)
  -T  leave out a file type  (grep's -T is --initial-tab)
  -u  ignore ignore files    -d  folder depth   -I  no file names
  -0  zero byte after paths  (grep's -0 is no context)

WHAT lw ADDS

  Code structure  lw defs NAME finds where a name is defined; lw uses NAME
                  finds where it is used and skips comments and strings;
                  --scope names the functions, classes or sections each
                  matching line sits in, outermost first.
  Conditions      --near PATTERN N keeps a line only if PATTERN is within N
                  lines of it (--near-before, --near-after for one side);
                  --and and --not keep only files that also contain, or do not
                  contain, other patterns. One call instead of a pipeline.
  Big files       lw lines prints lines by number from a file of any size;
                  --since and --until find a time span in a time-ordered log
                  by reading only the part they need (milliseconds in a 10 GB
                  log, where grep reads all of it).
  Safe changes    lw replace --dry-run shows every changed line before and
                  after; lw replace records what it changed, and lw undo puts
                  it back file by file, leaving alone any file edited since.
  What changed    lw changes lists the files added, changed or removed in a
                  folder since it last looked; --changed searches only those.
  Bounded output  --budget stops output at a size, and the JSON summary says
                  how many files were searched and matched and whether the
                  output was cut, so a broad search never silently floods a
                  terminal or an agent's context. A line over 1,000 bytes
                  (minified code, a source map) shows only the part around
                  its match, with how much was left out.
  JSON for all    --json gives objects for lines, context, counts (-c) and
                  names (-l), with a summary.
  Speed           Measured 2026-11-25 on one Windows 11 PC, both programs
                  excluded from antivirus scanning, output captured: printing
                  the matching lines of a 29,498-file source tree was 1.8 to
                  2.2 times as fast as ripgrep, and finding a rare line in a
                  10 GB log 7.7 times as fast (0.58 s against 4.45 s).

WHERE lw DIFFERS ON PURPOSE

  - Line numbers are on by default, also in a pipe (-N turns them off).
  - Paths use / between names; a folder's files come in path order, the
    same every run, not in the order threads happen to finish.
  - A line over 1,000 bytes shows the part around its match, unless -M or
    --whole-lines is given.
  - --json is lw's own format (lw help output), not ripgrep's messages, and
    with -c or -l it gives counts and names as JSON instead of overriding.
  - Binary files are judged by the first 64 KiB and then the exact zero byte,
    the same every time; ripgrep's answer depends on how it read the file.
  - grep's --include and --exclude follow -g's rule (the last glob that
    matches decides), not grep 3's first-option rule.

WHEN ripgrep OR grep IS STILL THE TOOL

  - systems other than Windows
  - a program that reads ripgrep's own --json messages
  - a pipe that never ends with no PATH given: lw, like both, waits on it"#,
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
  (?i)     ignore case from here  [[:alpha:]] a POSIX class

How patterns are read:
  -F               every pattern is plain text
  -G               grep's basic syntax: \( \) \{ \} \| \+ \? are the operators,
                   and ( ) { } | + ? stand for themselves
  --extended-regexp  every pattern is a regular expression
  -P, --engine pcre2  PCRE2, the library ripgrep uses: adds look-around
                   ((?=x) (?!x) (?<=x) (?<!x)) and back-references (\1);
                   --engine auto uses it only when a pattern needs it
  -e PATTERN       one pattern; give -e many times for several, and a line
                   matches when any of them matches
  -f FILE          patterns from a file, one per line (an empty line matches
                   every line); - reads them from standard input

How they match:
  -i, -s, -S       ignore case; match case (the default); ignore case unless a
                   pattern has a capital letter of its own (not \S or \W)
  -w               whole words: no word character just before or after
  -x               whole lines
  -U               let a match run across lines; every line it touches is
                   printed. --multiline-dotall makes . match a line end too.
                   Without -U a match never crosses a line, and a pattern that
                   names a line end (\n) is refused.
  --crlf           treat CRLF as a line end, so $ matches before a CR
  --null-data      lines end at a zero byte instead of a line feed
  --no-unicode     \w, \d, \s, . and case folding cover ASCII only

Write -F, -e or -- to search for a pattern that starts with -:
  lw -F -- "-x" src      lw -e "-x" src"#,
    },
    Topic {
        name: "folders",
        summary: "Which files a folder search reads, and in what order",
        usage: "",
        body: r#"A folder is searched through. Its files are read unless something leaves
them out, decided in this order (ripgrep's):

  1. -g and --iglob globs, which outrank everything after them.
  2. Ignore files: in each folder .rgignore, then .ignore, then .gitignore and
     .git/info/exclude (Git's only inside a Git work tree), nearest folder
     first, folders above the searched one too; then Git's global ignore file
     (core.excludesFile, or .config/git/ignore in the home folder); then the
     files given with --ignore-file.
  3. File types (-t, -T), which cannot bring back an ignored file.
  4. Hidden names: starting with a dot or carrying Windows' hidden attribute,
     unless --hidden, an ignore file's !NAME, or a type that picks them.
  5. Links are not followed without -L; with it a link back to a folder above
     is reported as a loop.

Files named on the command line are always searched, whatever these say.

CHOOSING FILES BY NAME OR TYPE
  -g GLOB    only files matching GLOB. A glob without / matches a name at any
             depth (*.rs, use*.md); one with / matches the path from the
             current folder (src/**, docs/*.md), as in ripgrep. * and ? stay
             within a name, ** crosses folders, [a-z] is a set, {ts,tsx} is
             either. A folder whose name a glob leaves out is not gone into.
  -g !GLOB   leave out what GLOB matches (quote it in a shell: "!*.min.js")
  --iglob GLOB, --glob-case-insensitive   the same, ignoring letter case
  -t TYPE    only files of TYPE (all: every known type);  -T TYPE  leave out
  --type-add name:GLOB     add a glob to a type (name:include:a,b copies types)
  --type-clear TYPE        forget a type;  --type-list  list them all
  grep's --include GLOB, --exclude GLOB, --exclude-dir GLOB, --exclude-from FILE
When any glob picks files, only picked files are searched; the last glob that
matches decides. The types:
{types}
IGNORE FILES AND LIMITS
  --no-ignore              no ignore files (-u); -uu also --hidden; -uuu also --binary
  --no-ignore-dot, --no-ignore-vcs, --no-ignore-exclude, --no-ignore-global,
  --no-ignore-parent, --no-ignore-files   leave out one kind
  --no-require-git         .gitignore applies outside Git work trees too
  --ignore-file PATH       one more ignore file, matched from the current folder
  --ignore-file-case-insensitive   ignore files ignore letter case
  -d NUM, --max-depth NUM  at most NUM folders deep (0: only named files)
  --max-filesize NUM[K|M|G]  leave out larger files
  -L                       follow links;  --one-file-system  not onto other volumes

ORDER
Files are printed in path order ignoring letter case, the same every run.
--sort path orders names as bytes, folder by folder (ripgrep's order);
--sort modified, accessed or created by those times; --sortr reverses.

lw files FOLDER lists exactly the files that are read. (The editor's Find in
folder also applies .gitignore files outside Git work trees and always skips
.git, .hg, .svn, node_modules and .venv.)"#,
    },
    Topic {
        name: "output",
        summary: "How results are shown: lines, context, colours, JSON, limits",
        usage: "",
        body: r#"LINES
Each line is printed as its fields and its text:  PATH:LINE:COLUMN:OFFSET:TEXT
  PATH     when several paths or a folder are searched (-H always, -I never)
  LINE     on by default (-N off)       COLUMN with --column
  OFFSET   with -b: where the line (with -o, the match) starts, from 0
A context line uses - instead of : (--field-match-separator and
--field-context-separator change them), and groups of lines that are not next
to each other are parted by a line of -- (--context-separator, or
--no-context-separator). With context and several files, -- parts the files too.

  --heading       each file's path once above its lines, an empty line between
                  files (the default in a console; --no-heading turns it off)
  -o              each match on its own line (an empty match as an empty line)
  -r TEXT         each match replaced in what is shown ($1, ${name}, $$); files
                  are not changed (lw replace changes them)
  --vimgrep       PATH:LINE:COLUMN:TEXT, once per match
  --passthru      every line, the others as context
  --trim          leave out the spaces a line starts with
  -0              a zero byte after each path, instead of its separator
  --color WHEN    never, auto (in a console), always; --colors path:fg:cyan,
                  match:style:bold, line:bg:0x20,0x20,0x20, highlight:...; -p is
                  --color always --heading -n
  --hyperlink-format default|file|vscode|...  paths as terminal links, with colour
  --stats         the totals after the results

LONG LINES
A line over 1,000 bytes, such as minified code or a source map, is shown as
about 800 bytes around its first match:
  12:[48211 bytes omitted] ...text around the match... [1902 bytes omitted]
--whole-lines prints it whole. -M NUM instead replaces lines over NUM bytes
with [Omitted long matching line] (as ripgrep), and with --max-columns-preview
shows their first NUM characters.

JSON
--json prints one object per line:
  {"path": "src/server.rs", "line": 52, "column": 9, "byte": 1834,
   "text": "    let body = read_body(&req)?;", "matches": [[15, 24]]}
  line, column  counted from 1; column in characters
  byte          where the first match starts in the file
  text          the line without its line end
  matches       each match as [start, end) byte offsets within text
  replaced      with -r, the line with its matches replaced
  context       true on a context line (which has no column or matches)
  scope         with --scope: {"name", "kind", "line", "end_line", "within": [...]},
                the innermost definition and every one around it, outermost first
  omitted_before, omitted_after   bytes left out of a long line
With -c it gives {"path", "count"} per file, with -l or
--files-without-match {"path"}; and as the last line a summary:
  {"summary": {"files_searched", "files_matched", "lines", "matches",
               "bytes_searched", "output_bytes", "cut_short", "seconds"}}

LIMITS
--budget N stops printing at the last line break before N bytes, says so on
the error stream (or sets "cut_short" in JSON), and keeps the exit code.
-m N stops each file after N matching lines (the context after the last one
still shows). -q prints nothing at all."#,
    },
    Topic {
        name: "conditions",
        summary: "Keep results only when other patterns are near or present",
        usage: "",
        body: r#"lw find and lw count can require more than one pattern. Each condition may
be given several times; all must hold.

  --near PATTERN N         keep a matching line only if PATTERN matches on a
                           line at most N lines before or after it (0: the
                           same line)
  --near-before PATTERN N  only before it (or on it)
  --near-after PATTERN N   only after it (or on it)
  --and PATTERN            keep a file's results only if PATTERN occurs in it
  --not PATTERN            keep a file's results only if PATTERN does not

Condition patterns follow the same rules and options (-i, -F, -w) as the main
pattern. Context (-A, -B, -C) is shown around the lines that pass.

EXAMPLES
  lw --near retry 2 timeout server.log
  lw --near-before "BEGIN" 10 error app.log
  lw --and "import React" --not "test" useEffect src"#,
    },
    Topic {
        name: "binary",
        summary: "What happens to binary files",
        usage: "",
        body: r#"A file with a zero byte is binary. lw treats it as ripgrep does:

  A file found in a folder: if its first 64 KiB hold a zero byte it is passed
  over without a word (--debug says why); a later zero byte stops the search
  there, and if lines were printed before it:
    PATH: WARNING: stopped searching binary file after match (found "\0" byte around offset N)
  Its count is left out.

  A file named on the command line, or standard input: it is searched to the
  end, but no line after the zero byte (none at all, when it is in the first
  64 KiB) is printed; if something matches there:
    PATH: binary file matches (found "\0" byte around offset N)
  Counts count every match.

  --binary         treat files in folders as named ones (-uuu does too)
  -a, --text       no binary detection: zero bytes are printed as they are
  --binary-files TYPE   grep's: binary (as --binary), text (as -a), or
                   without-match (binary files never match)
  --null-data      lines end at zero bytes, so nothing is binary

A file that starts with a UTF-16 byte-order mark is not binary: it is decoded
(lw help input)."#,
    },
    Topic {
        name: "input",
        summary: "What is read: piped text, encodings, compressed files, preprocessors",
        usage: "",
        body: r#"STANDARD INPUT
With no PATH, lw searches what is piped or redirected to it; - names it among
other paths. It is shown as <stdin>, or as --label NAME.
  git log | lw -i fix        lw -c TODO - < notes.txt

ENCODINGS
A file that starts with a byte-order mark is read in its encoding: UTF-16 is
decoded to UTF-8, and a UTF-8 mark is dropped. Any other file is searched as
the bytes it is. -E ENCODING reads files without a mark in that encoding
(utf-16le, windows-1252, latin1, shift_jis, ... the WHATWG names); -E none
turns the marks off too. Columns and offsets count in the UTF-8 text.

COMPRESSED FILES
-z searches inside gzip (.gz .tgz), bzip2 (.bz2), xz (.xz), lzma (.lzma),
lz4 (.lz4), zstd (.zst), brotli (.br) and compress (.Z) files, known by their
names or, for those that have one, by the mark their first bytes carry. They
are decompressed as they are searched, never whole in memory.

PREPROCESSORS
--pre COMMAND runs COMMAND PATH for each file (with the file as its input) and
searches what it prints: lw --pre pdftotext invoice report.pdf. --pre-glob GLOB
chooses the files it is used for (!GLOB leaves out). A COMMAND that is not
found stops the search before it starts; one that fails is reported. -z and
--pre undo each other: the later one counts."#,
    },
    Topic {
        name: "options",
        summary: "Every option, grouped",
        usage: "",
        body: r#"Every option lw takes. Short options combine (-iwC2), a value may follow
attached or as the next word (-m5, -m 5, --max-count=5), and a later option
undoes an earlier one where they decide the same thing (-i then -s). Words
after -- are never options. Options in the file named by LW_CONFIG_PATH (one
per line, # for comments) come first; --no-config leaves them out.
{options}"#,
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

/// Every option from the parser's own table, grouped, for `lw help options`.
fn options_list() -> String {
    use crate::options::{Group, Takes, LONG_ALIASES, SHORT_ALIASES, SPECS};
    let groups = [
        (Group::Patterns, "PATTERNS"),
        (Group::Files, "WHICH FILES"),
        (Group::Input, "READING FILES"),
        (Group::Output, "HOW MATCHES ARE SHOWN"),
        (Group::Results, "WHAT IS REPORTED"),
        (Group::Lw, "LW'S OWN"),
        (Group::Other, "OTHER"),
    ];
    let mut out = String::new();
    for (group, title) in groups {
        out.push_str(&format!("\n{title}\n"));
        for spec in SPECS.iter().filter(|s| s.group == group) {
            let short = spec.short.map(|c| format!("-{c}, ")).unwrap_or_default();
            let value = match spec.takes {
                Takes::Nothing => String::new(),
                Takes::Value(v) => format!(" {v}"),
                Takes::Optional(v) => format!("[={v}]"),
            };
            let name = format!("  {short}--{}{value}", spec.long);
            if name.len() < 30 {
                out.push_str(&format!("{name:30}{}\n", spec.help));
            } else {
                out.push_str(&format!("{name}\n{:30}{}\n", "", spec.help));
            }
        }
    }
    out.push_str("\nOTHER NAMES\n");
    for (alias, canonical) in LONG_ALIASES {
        out.push_str(&format!("  --{alias:28}the same as --{canonical}\n"));
    }
    for (letter, canonical) in SHORT_ALIASES {
        out.push_str(&format!("  -{letter:29}the same as --{canonical}\n"));
    }
    out
}

/// A topic's text, with the lists of file types and options filled in where it asks for them.
pub fn body(topic: &Topic) -> String {
    let mut text = topic.body.to_owned();
    if text.contains("{types}") {
        let types: String = linework_search::pick::TYPES.iter().map(|(name, globs)| format!("  {name:8}{}\n", globs.join(" "))).collect();
        text = text.replace("{types}", &types);
    }
    if text.contains("{options}") {
        text = text.replace("{options}", &options_list());
    }
    text
}

/// One topic as the terminal shows it.
pub fn page(topic: &Topic) -> String {
    let mut text = format!("lw {}: {}\n\n", topic.name, topic.summary);
    if !topic.usage.is_empty() {
        text.push_str(&format!("usage: {}\n\n", topic.usage));
    }
    text.push_str(&body(topic));
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
        text.push_str(&format!("```text\n{}\n```\n", body(topic)));
    }
    text
}
