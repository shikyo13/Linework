# Linework user guide

This guide covers Linework 1.8.0 in full. For a quick tour with pictures, see the README in the project repository.

Linework is a Windows x64 text editor for code, notes and multi-gigabyte text files. Run **Linework-Setup-1.8.0-x64.msi** to install it for your Windows account. The installer adds Linework to Start, Windows Default Apps and Explorer’s **Edit with Linework** menu. To change file associations, choose **Choose Linework file defaults** in Start. Files opened from Explorer join your existing Linework window. Use **Settings → Apps → Installed apps** to remove the installation. Your documents and editor preferences are stored separately.

For a portable copy, keep **Linework.exe** beside its **runtime** folder and launch it from there. Both editions accept files and folders dropped onto the window or passed on the command line.

## Start editing

Launch a file at a one-based line and column from a terminal or build tool:

```powershell
.\Linework.exe --reuse-window "C:\My project\sample.rs:27:8"
```

Omit the column to jump to the start of a line. The editor counts Unicode characters and expands tabs as displayed. With `--reuse-window`, it opens the location in an existing window using the same profile. Press **Alt+Left** to return. `--new-window` opens a separate editor and is the default. A tool that launches the executable and waits resumes when that editor window closes. With `--reuse-window --wait`, the launcher waits for the reused window to close.

Open a file with **Ctrl+O**, or a folder with **Ctrl+Shift+O**. **Ctrl+P** finds files by name. **Ctrl+Shift+P** searches the commands. **F1** lets you browse commands and shortcuts. **Tools → Change keyboard shortcut** assigns a different combination.

For Markdown, HTML, SVG, CSV and TSV, choose **Preview** at the right of the tab strip or press **Ctrl+Shift+V**. Drag the divider to give the source or preview more room. The preview follows unsaved edits and remembers your reading position. Markdown and static HTML support local images beneath the document’s folder. The pane uses the Microsoft Edge WebView2 Runtime. Document scripts and remote resources are disabled. For documents over 128 KiB, the pane labels the excerpt around the caret. SVG diagrams must fit in that limit.

In a Markdown preview, choose **Effects** to change how edits are marked. **Pill** outlines the word, **Beam** lights the surrounding area, **Underline** adds a two-pixel line, and **Block** marks the paragraph or other enclosing block. You can mark just the changed characters, adjust dimming, add a margin marker, reduce motion, or switch the cue off. If you’re reading elsewhere when an edit arrives, **Show edit** takes you to it.

Click rendered Markdown text to place the source caret there. Double-click selects the word in the editor and leaves the following space out of the selection. **Esc** returns focus to the source. Hold **Alt** and click a link to go to its source instead of following it. Alt+click also works while click-to-source is switched off under **Effects**. The **View** menu and command palette also include the preview navigation commands, and **View → Preview effects** offers the same settings from the keyboard.

**Effects** also holds seven switches for the Markdown preview. **Fresh ink** shows newly typed text in a warm color that settles in under a second. **Echo** washes and underlines pasted text in the preview and boxes it in the source. **Ghost** floats deleted text, struck through, above the place it left without moving the page. **The source selection** highlights in the preview whatever is selected in the source. **Peek** shows where a link leads when the pointer rests on it, and the Markdown behind a block while **Ctrl** is held over it. **Follow the source caret** scrolls the preview with the caret; it is off by default so the preview keeps its own place, and **View → Preview follows the source caret** switches it from the keyboard. **Task boxes** in the preview can be ticked, which edits the `[ ]` in the source as an ordinary change that Undo takes back. A click that jumps to the source briefly marks the line it landed on.

The source editor has six effects of its own, under **View**, **Editor effects…**. **Gliding cursor** slides the caret to its new place in 90 to 150 milliseconds, and **Active line glide** slides the current-line highlight between lines. **Edit heat** puts an amber mark in the gutter beside each edited line, which cools away over ten seconds and stays with its line. **Pair glow** lights both ends of the Markdown bold, italic, strikethrough, code span or link the caret is in. **Paragraph focus** dims everything outside the caret's paragraph; it also has its own switch in **View**. **Smart Markdown typing** wraps selected text when you type `*`, `_`, `~` or a backtick, makes it a link when you type `[`, continues lists, numbered lists, task lists and quotes on **Enter**, nests a list item with **Tab** at its start, and makes **Ctrl+B** and **Ctrl+I** bold and italic. In a Markdown document Ctrl+B is bold; everywhere else it still shows or hides the sidebar. In ordinary Markdown text the completion list no longer opens by itself with words from the page; it still offers your own snippets there, still completes inside code, and **Ctrl+Space** lists everything. Edit heat and Paragraph focus start switched off. The two glides stop when **Reduce motion** is on or Windows animations are off.

Encodings. Linework reads a byte order mark first, then UTF-16 without one, then UTF-8. A file that is none of these has its encoding guessed from its bytes, as Notepad++ and Firefox do, and the status line says which encoding was used. **File → Reopen with encoding…** and **Save with encoding…** list the encodings Notepad++ and Sublime Text offer, grouped by language and filtered as you type: Unicode, the Windows and ISO code pages, KOI8, the DOS code pages such as 437 and 850, the Mac code pages, Shift JIS, EUC-JP, GB18030, Big5 and EUC-KR. A binary file opens as **Hexadecimal**, sixteen bytes to a line, and saving writes the edited digits back as bytes. **File → Detect encoding of files that are not UTF-8** turns guessing off, **Encoding for files that are not UTF-8…** sets what is used then (Windows 1252 to start), and **Encoding for new files…** sets the encoding of new files (UTF-8 to start).

| To do this | Use |
|---|---|
| Jump to a line and column, logical UTF-8 byte, or percentage | Ctrl+G, then enter e.g. `120:8`, `@12582912`, or `95%` |
| Add the next matching word or selection | Ctrl+D |
| Skip an occurrence or undo a selection change | Ctrl+K or Ctrl+U |
| Add a caret above or below | Ctrl+Alt+Up or Ctrl+Alt+Down |
| Select a rectangle or fill it with a number series | Selection → Rectangular selection, then Edit → Fill selections with a number series |
| Toggle comments on selected lines | Ctrl+/ |
| Search or replace in the active file | Ctrl+F or Ctrl+H |
| Stay inside the original selection while finding or replacing | Select text, then Ctrl+F3 |
| Search a folder or preview folder replacements | Ctrl+Shift+F or Ctrl+Shift+H |
| Search or preview changes across open tabs, including unnamed drafts | Search → Find in open tabs or Preview replacements in open tabs |
| Jump to symbols, definitions and references | Ctrl+R, F12 and Shift+F12 |
| Split a view, focus the other pane, return to one pane | Ctrl+Alt+2, F8 and Ctrl+Alt+1 |
| Toggle a bookmark and jump to the next | Ctrl+F2 and F2 |
| Record or finish a macro, then replay it | Ctrl+Shift+M, then F9 |

The left rail opens Explorer, Find, Symbols, Search Results, Bookmarks and keyboard shortcuts. Explorer keeps the open editors above the current folder. Hover over a rail button to see its name.

**Review replacements** opens after a folder replacement search. Select a result to read the original and proposed text in labeled panes. Press **Space** to include or exclude that match, **Ctrl+Enter** to apply the included changes, or **Esc** to cancel. Results identify files that changed after preview. Edits to an open tab remain in memory until you save it.

Use **File → Compare changes with saved file** to inspect a draft, or **File → Compare with another file** to select the other side. The two panes scroll together and show line numbers, surrounding context and highlighted words. **F7** moves to the next change and **Shift+F7** goes back. Press **Enter**, double-click a change, or choose **Show in editor** to close the comparison with the caret on that change in the revised file. **Alt+Left** returns to where you were.

Each split pane names its file and the focused pane is labeled **Active**. Changed tabs and pane headers say **Unsaved**. If another program changes or removes a file, the save dialog offers **Save As**, **Compare**, **Reload**, and an explicit **Overwrite** or **Recreate**. **Save As** preserves both versions. Press **F10** to focus the menu bar. Tab and the arrow keys move through the named menus, tabs, and find controls.

After **Undo** or **Redo**, the editor briefly outlines the text that came back and marks the place where text was removed. When one step changes several places, the message area says how many. Find says when it wraps past the end or the start of the file, and when the match is the only one. **F2** says when it wraps to the first bookmark. Turn the mouse wheel over the tab strip to reach tabs that don’t fit, or over the document map to scroll the editor. Middle-click a tab to close it. In the command palette and the other pick lists, **Page Up** and **Page Down** move a page at a time.

## Set up your editing environment

- **Syntax and colors.** The 55 built-in profiles use Lexilla. Detection checks filename, extension, shebang, modeline and a sample of the contents. Untitled code gets another detection pass after you pause typing. Set an override in **View → Syntax language** or create an exact filename or extension rule. **Syntax colors** exposes 15 color roles and named palettes. **Highlighting options** controls syntax colors, brace pairs, the current line and matching selections. **Search → Mark phrase with a color** keeps up to five independent phrase marks.
- **Local grammars.** Choose **View → Import Sublime syntax grammar** for a single `.sublime-syntax` file, or install a package to preserve sibling definitions. Imported syntaxes enter the language picker and can detect files by extension or first-line expression. Version 2 supports inheritance, embedded languages and branch replay across up to 128 lines. The compiler resolves standard dependencies from 202 bundled upstream definitions. Highlighting on paged files starts at the loaded window. The custom highlighter skips individual lines longer than 16 KiB. [Sublime syntax definitions](https://www.sublimetext.com/docs/syntax.html)
- **Carets and completion.** Choose line, wide or block cursor shapes, width and blink interval. Set the interval to zero for a steady caret. **Ctrl+Space** requests completion. Under **Edit → Typing and completion preferences**, adjust the minimum prefix length, turn automatic suggestions off, or change automatic indentation. Suggestions include language keywords, nearby identifiers, indexed project vocabulary and snippets.
- **Snippets.** Use **Edit → Manage snippets** to create a language-specific or shared template. In the body, use numbered placeholders such as `$1` or `${1:name}`, repeat a number for linked fields, and place `$0` at the final caret. Insert from the picker or completion list, then press Tab to advance.
- **Spelling.** F6 enables offline checking. Put the caret in an underlined word and press **Ctrl+.** for suggestions, learning and ignoring. US English ships with the app. Select a UTF-8 Hunspell `.dic` with its matching `.aff` to change languages.
- **Macros.** Record typing, movement, selections and explicit find/replace actions. **Macro → Repeat macro** accepts a count or `EOF`. Playback stops when a pass cannot progress toward EOF. Esc cancels. **Manage macros** lets you name, inspect, edit or reorder steps, including the query, replacement and flags of a recorded search.
- **Navigation and tools.** Show a document map, stack two editors vertically, or link their scrolling under View. Project symbol navigation uses Tree-sitter for supported languages, with text outlines for selected other formats. References match identifiers by name. **Tools → Set build command** configures a folder build. F7 runs it and F4 opens the next diagnostic. Configurable external tools can process the current selection and return replacement text.
- **Sessions.** Normal exit checkpoints unsaved drafts and restores tabs and positions on the next launch. After an interrupted run, Linework offers the last completed recovery checkpoint. Linework verifies the original with SHA-256 before using it for recovery. If it changed or disappeared, the draft opens as a separate copy. On NTFS, large-file recovery stores each distinct 1 MiB block once and compresses it when that saves space. Small saved changes can reuse one recovery base even when edits are gigabytes apart. The checkpoint stores references to the unchanged text and the new bytes. New large recovery archives support direct reads, so opening a recovered copy needs no full-size decompression file. On a clone-capable Windows volume, a native UTF-8 file and its recovery directory on the same volume can share disk blocks. The first checkpoint of unique, incompressible data on ordinary NTFS still needs roughly the size of that data. Linework 1.3.3 reads older drafts. A recovery record that combines distant saved changes requires 1.3.3 or newer. Saving or discarding a draft retires its recovery data after the next durable checkpoint. Preferences, learned words, macros, grammars and session data live under `%LOCALAPPDATA%\Linework`.

## Large files

A Rust piece tree references original file ranges and inserted data. Earlier tree roots support undo. A background scanner builds a sparse line index in 64 KiB blocks. Files over 24 MiB use editing windows of roughly 384 KiB. Opening a UTF-8 log and jumping by byte or percentage can start before its entire line index is ready. Legacy encodings and UTF-16 need a conversion pass to temporary UTF-8 storage. Converting back on save preserves the selected encoding or reports an unrepresentable character.

The line scanner, file search and Replace All read a large file on several processor threads. For a file of 1 GiB or more, Linework keeps the finished line index in the `line-index` folder of its profile, so opening the same file again does not count its lines a second time. An entry holds line counts and no text, takes 4 bytes per 64 KiB of file, and is used only while the file's size and modification time are unchanged and a sample of its pages still counts the same. Linework keeps the 64 most recently used entries. Deleting the folder is safe.

A small change to a file over 24 MiB on an NTFS drive of this computer (up to 1 MiB replaced by the same amount, or a changed end of the file) is saved by writing only the changed bytes into the file. Before it touches the file, Linework writes the bytes it is about to replace to the `save-records` folder of its profile and removes that record when the save is complete. If the computer or Linework stops in the middle of such a save, the next start of Linework uses the record to put the file back as it was. Until then another program could see the half-written change. Leave that folder alone while it holds a record.

File search and replace, global navigation, Select All, deletion, typing over a selection, undo and save work across editing windows. Copying a selection over 32 MiB offers to write it directly to a file. Select Next, Select All Occurrences, Split into Lines and Add Caret Above/Below store selections at full-file offsets. Typing, paste, Backspace, Delete, case conversion and number fill apply to the entire selection set. Dense selections spill to temporary storage and apply edits in a background worker. Large-file folding recognizes braces and brackets, indentation for Python and YAML, Markdown headings and simple line-oriented XML/HTML tags. Collapsing a region brings its following text into the same editing view. Some line-oriented selection commands require finishing a file-wide selection with Escape. Syntax that starts earlier than the page can need surrounding context for accurate colors. Folder/open-tab search previews stop at 10,000 hits or a results-memory budget, and their line reader reports lines over 8 MiB. The current-file search engine handles multiline patterns and matches crossing storage boundaries. Its Rust regex dialect supports captures and named replacements. Pattern backreferences and lookaround are outside that dialect.

A Windows end-to-end test replaced two selections more than 3 GiB apart, saved, undid the edit, saved again, and matched the original SHA-256. The test folded the intervening block to show the first and last lines together, edited the visible tail, and checked its physical offset on disk. A separate 20,000-caret test verified rapidly typed characters followed immediately by Save.

### Measured Windows performance

The comparison timings predate version 1.3.1. The **1.1 release benchmark** used two fresh processes per editor. Each opened the same 3,221,225,600-byte UTF-8 log on Windows 11 x64 build 26200, with a Ryzen 7 9800X3D, 96 GiB RAM and NVMe storage. Each run started with a warm OS file cache. Readiness required the exact logical byte length, **25,165,826 lines** including the trailing empty line, and the exact EOF marker. Linework's probe also checked the global caret position after navigation. The reference editors used isolated profiles on private Windows desktops.

| Editor | First run | Second run | Main-process working set at readiness |
|---|---:|---:|---:|
| Linework 1.1 | 0.822 s | 0.822 s | 70.1–70.6 MiB |
| Notepad++ 8.9.9.1 x64 | 3.365 s | 3.177 s | 3,338.7–3,339.0 MiB |
| Sublime Text build 4215 x64 | 17.886 s | 24.594 s | 1,903.4–2,014.4 MiB |

Polling occurred about every 20 ms. Memory excludes helper processes and the OS file cache. Cold storage, sustained typing latency and other file shapes need separate measurements. An independent 1.0 storage-engine run on a 3 GiB CRLF log indexed in 0.473 s, found a marker in the last record in 0.916 s, and saved an edited copy in 2.026 s. SHA-256 verified the complete output.

A dense CSV transformation replaced **45 million Unicode regex matches** in **24.03 seconds** in the 1.0 storage benchmark. Input grew from 855,000,000 bytes to 3,375,000,000 bytes, with an 8.92 MiB peak engine-process working set. Small replacements stay in memory. A plain-text Replace All with up to a million matches is assembled from pieces of the original file around one copy of the replacement, without copying the file. Denser replacements and regular-expression replacements stream through temporary storage. Each commits as one undoable edit. Full-file hashes verified the output, undo, redo and original input. In the Windows app, a separate 220,000-match replacement grew a normal tab into a paged document, saved the result and restored the original bytes with one undo after save.

## Command line: lw

The installer puts `lw.exe` next to `Linework.exe` and adds that folder to your user `PATH`, so `lw` runs in any terminal. Run `lw` or `lw help` for a short introduction and the list of commands, and `lw help COMMAND` (or `lw COMMAND --help`) for one command in full, with examples. The complete reference is in [docs/lw.md](docs/lw.md), generated from the same text.

`lw serve` makes the same commands available to AI agents as Model Context Protocol tools, each described by its help page: add it to Claude Code with `claude mcp add linework -- lw serve`, or to Codex with a `[mcp_servers.linework]` entry whose command is `lw` and whose arguments are `["serve"]`. A replace through the server only previews unless the call sets `preview` to false, and every tool's output is capped at 100,000 bytes unless the call sets `budget`. It uses the same file engine as the editor and the same profile, so a line index saved by either is used by the other.

| Command | What it does |
|---|---|
| `lw find [-i] [-F] [-w] [-m N] [output options] PATTERN PATH` | Prints each matching line as `line:text` for a file, or `path:line:text` for a folder |
| `lw count [-i] [-F] [-w] [output options] PATTERN PATH` | Prints the number of matches in a file, or `path:count` per file of a folder |
| `lw lines FILE FROM [TO]` | Prints lines FROM to TO, counted from 1 |
| `lw lines --since TIME [--until TIME] FILE` | Prints the part of a time-ordered log between two times |
| `lw info FILE` | Prints the size, line count, encoding and line ends |
| `lw replace [-i] [-F] [-w] [--dry-run] [--json] OLD NEW PATH` | Replaces every match in a file or a folder and saves, or shows each changed line before and after with `--dry-run` |
| `lw changes [--mark NAME] [--json] FOLDER` | Lists the files added, changed or removed since the last time it looked at the folder |
| `lw undo [--list] [CHANGE]` | Undoes the last replace, or the one named; `--list` shows the recent ones |
| `lw files [--hidden] [--no-ignore] FOLDER` | Lists the files a folder search would read |
| `lw defs [output options] NAME PATH` | Prints where NAME is defined in code |
| `lw uses [output options] NAME PATH` | Prints where NAME is used in code, leaving out comments and strings |
| `lw open [--dry-run] FILE[:LINE[:COLUMN]]...` | Shows files in the Linework window, at a line and column when given |

`find` and `count` take output options:

- `--json` prints one JSON object per matching line, with `path`, `line`, `column` (counted in characters from 1), `byte` (the first match's position in the file), `text` (without its line break) and `matches` (each match as a byte range within `text`); `count --json` prints `path` and `count` per file. The last object is a `summary` with the files searched and matched, the lines found, the bytes searched and printed, whether the output was cut short, and the seconds taken.
- `--heading` prints each file's path once, then its lines, with a blank line between files.
- `--scope` groups lines under the function, method, class or type around them, found with the same Tree-sitter grammars as the editor's outline, or under the Markdown heading of their section; with `--json` it adds a `scope` object with `name`, `kind`, `line` and `end_line`. It parses every file with a match, so it costs more: 1.5 s instead of 0.11 s for 52,558 lines in 9,848 files.
- `--budget BYTES` stops printing at a line break once that much output is written, and says so.
- `--stats` prints the totals on the error stream.

The files of a folder search are printed in path order, so the same search gives the same output each time.

`find` and `count` also take conditions, each of which may repeat: `--near PATTERN N` keeps a line only when PATTERN matches within N lines before or after it, `--and PATTERN` keeps only files that also contain PATTERN, and `--not PATTERN` keeps only files that do not. With conditions, `count` on a single file counts the matching lines that pass them.

`lw replace` on a folder changes the files a search for OLD would show. Each file is changed and saved by the editor's engine, in its own encoding, and is not saved if it changed on disk since it was read. In a pattern replacement, `$1` or `${1}` inserts a captured group; write `${1}x` when a letter or digit follows, since `$1x` names a group called `1x`. Before any file is saved, the edits that will be made in it (each place, the new text and the text it replaces) are written to a change record in the `changes` folder of the profile. `lw undo` reads that record and restores each file only if every changed place still holds the text the replace put there; a file edited since is left as it is and reported, and the change can be undone again once it is dealt with. A replace stopped partway through, by a crash or power cut, can be undone the same way: files that were never saved are left alone. On the machine described above, replacing in 2,000 small files took 2.9 s, and undoing it 2.9 s. A change record takes about 50 bytes per replacement; deleting old records is safe once their changes need no undoing.

`lw defs` and `lw uses` use the same Tree-sitter grammars and rules as the editor's outline and references: a definition comes from the language's tags, a use is an identifier in parsed code, so a name in a comment or a string is neither. Markdown headings and the other line outlines count as definitions. Only files that contain NAME as a whole word are parsed, and files over 4 MiB are not. Finding the 1,733 uses of `useState` in a 29,498-file tree took 0.93 s.

`--since` and `--until` work on a log whose lines carry timestamps in time order: ISO (`2026-11-21 21:40:15.123`, also with `T`), web-server (`[21/Nov/2026:21:40:15 +0000]`) or syslog (`Nov 21 21:40:15`). The format is taken from the first timestamped line, and a line without a timestamp, such as part of a stack trace, goes with the line before it. Write a time as `2026-11-21 21:40`, `2026-11-21` or `21:40:15`; a time without a date takes the date of the log's first line. Each end covers what it names at the precision written: `--until 21:45` includes all of 21:45, and `--until 2026-11-21` all of that day. The span is found by bisecting the timestamps, so it costs a few dozen small reads: 31 to 135 ms in a 10 GB log. `lw find` and `lw count` on a log take `--since` and `--until` too, and number lines from the start of the file.

`lw changes FOLDER` keeps a snapshot of the folder's listing (each file's path, size and last write time) in the `marks` folder of the profile, and on the next run lists what was added, changed or removed since, then takes a new snapshot. `--mark NAME` keeps a separate snapshot, for example one per task. `lw find --changed PATTERN FOLDER` searches only the files added or changed since the snapshot and leaves it as it is. A change that keeps both a file's size and its write time is not seen. For a 163,662-file folder, finding that nothing changed took 0.38 s and `find --changed` 0.29 s, against 9.1 s for a full search with half a million matching lines.

A folder search decodes files that start with a UTF-16 byte-order mark and searches their text; positions in its output are positions in that text as UTF-8. Single files in any encoding Linework opens are searched as their text.

`-i` ignores case, `-F` treats the pattern as plain text, `-w` matches whole words and `-m N` stops after N lines. A pattern with characters such as `.`, `*` or `(` is a regular expression unless `-F` is given. A folder search reads every file the `.gitignore`, `.ignore` and `.rgignore` files leave in (`.gitignore` only inside a Git work tree), skips names that start with a dot and hidden files unless `--hidden` is given, does not follow links, and leaves out any file that contains a zero byte, other than UTF-16 text. Lines are numbered in the same pass as the search, so a large file needs no line index first. The exit code is 0 when something matched, 1 when nothing did and 2 on an error.

Measured on the machine described in "Measured Windows performance" above, both programs excluded from Microsoft Defender scanning, output read through a pipe, median of 7 runs: counting matches in a 10 GB log took 0.49–0.51 s against ripgrep 15.2's 1.57–4.63 s, printing all 84,139 matching lines with numbers took 0.57 s against 1.90 s, and searching a 29,498-file source tree took 0.09–0.13 s against 0.19–0.23 s.

## Implementation and compatibility

The Windows interface, file engine, workflows, recovery, macro handling and workspace integration are written in Rust. The application embeds Scintilla and Lexilla, which are native C++ components, along with compiled Tree-sitter parsers.

### Notepad++ plugins

Use **Tools → Install Notepad++ plugin** and choose the plugin's x64 Unicode DLL. For a multi-file plugin, extract it first and keep its companion files in the folder named after the DLL. Commands appear in **Ctrl+Shift+P** and **Tools → Notepad++ plugin commands**. Linework reloads installed plugins from `%LOCALAPPDATA%\Linework\plugins` on startup.

The native host supplies Scintilla window handles, command registration, shortcuts, notifications, selected Notepad++ messages and floating plugin panels. Plugins execute inside the editor process with the same access as the editor. Install DLLs from a source you trust. To disable a plugin manually, close Linework and move its folder out of the plugins directory.

| Released x64 plugin | Operation checked in Linework |
| --- | --- |
| MIME Tools 3.2 | Encoded and decoded Base64 text |
| Converter 4.7 | Converted ASCII to hex and back |
| NppExport 0.4.0 | Wrote an HTML file and verified its contents |
| JsonTools 8.6 | Formatted JSON and opened its interactive tree viewer |

On a paged file, supported Scintilla calls use 64-bit byte offsets in the complete document. A command can read a range across page boundaries, edit it, and read its own changes before committing. The host serves copied text in calls up to 16 MiB. Larger pointer requests can use a read-only file mapping. A request for a contiguous, NUL-terminated document can require temporary disk space the size of the file. Native plugins control their own allocations and run synchronously. Compatibility depends on the calls and data types each plugin uses. [Native plugin compatibility](NOTEPAD-PLUGINS.md) · [Notepad++ plugin interface](https://npp-user-manual.org/docs/plugins/)

### Sublime Python packages

Use **Tools → Install Sublime package** to choose a `.sublime-package`, ZIP, or unpacked package folder. Use **Ctrl+Shift+P** to find an installed command by name. **Tools → Manage Sublime packages** lists installed packages and lets you remove one.

A bundled CPython interpreter runs package code in a separate process under your Windows account. The compatibility modules expose text, window and application commands, Unicode regions and selections, settings, command arguments, common editor operations, and supported event listeners. Document-changing replies check the originating tab, revision and selection before committing edits as one undo action. Unchanged **StringEncode**, **CaseConversion** and **ChangeQuotes** packages were exercised. The shared command palette was also tested with MIME Tools and StringEncode installed together, including a multibyte selection, save and undo.

On a paged UTF-8 file, the Python host asks Rust for the ranges a command reads. A sparse index translates Sublime's Unicode character positions to file offsets. Its first use scans the backing file on a worker. Subsequent commands share the index. Each returned Python string can contain up to 8 MiB of UTF-8 text. A package can process a larger range in pieces. Supported event listeners can use the same paged API. On standard documents, automatic events and scope snapshots cover up to 2 MiB. Packages that require a different interpreter ABI, a native dependency or other Sublime APIs can require changes. [Sublime Text API reference](https://www.sublimetext.com/docs/api_reference.html)
