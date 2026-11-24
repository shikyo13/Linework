# Linework 1.7.0

Linework is a Windows x64 text editor for code, notes and multi-gigabyte text files. Run **Linework-Setup-1.7.0-x64.msi** to install it for your Windows account. The installer adds Linework to Start, Windows Default Apps and Explorer’s **Edit with Linework** menu. To change file associations, choose **Choose Linework file defaults** in Start. Files opened from Explorer join your existing Linework window. Use **Settings → Apps → Installed apps** to remove the installation. Your documents and editor preferences are stored separately.

For a portable copy, extract the application ZIP and launch **Linework.exe** alongside the **runtime** folder. Both editions accept files and folders dropped onto the window or passed on the command line.

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

A dense CSV transformation replaced **45 million Unicode regex matches** in **24.03 seconds** in the 1.0 storage benchmark. Input grew from 855,000,000 bytes to 3,375,000,000 bytes, with an 8.92 MiB peak engine-process working set. Small replacements stay in memory. Dense replacements stream through temporary storage and commit as one undoable edit. Full-file hashes verified the output, undo, redo and original input. In the Windows app, a separate 220,000-match replacement grew a normal tab into a paged document, saved the result and restored the original bytes with one undo after save. [Recorded benchmark results](benchmarks.json)

## Implementation and compatibility

The Windows interface, file engine, workflows, recovery, macro handling and workspace integration are written in Rust. The application embeds Scintilla and Lexilla, which are native C++ components, along with compiled Tree-sitter parsers. The earlier Quire prototype used C# and WPF on .NET 10. Linework 1.1 restores its single title bar, workspace rail, open-editor list, and rounded controls in the Rust Windows interface.

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

The [1.2 extension test results](EXTENSION-RESULTS.md) document the exact packages, file sizes, memory measurements and saved-file checks. Use the [comparison rubric](COMPARISON-RUBRIC.md) and [fillable scorecard](COMPARISON-SCORECARD.csv) to evaluate the three editors on the same workloads.

[Community research and individual sources](COMMUNITY.md)

## Build from source

Use x64 Windows, Rust with the MSVC target, Visual Studio C++ Build Tools and the Windows SDK. Run these commands from the source directory containing Cargo.toml. The Cargo configuration statically links the MSVC runtime for the application.

```powershell
cargo build --release --locked
Copy-Item target\release\linework.exe .\Linework.exe
.\Linework.exe
```

For the storage benchmark, allow at least 7 GiB of free scratch space. The command creates and verifies real files, then removes the temporary input and output.

```powershell
cargo run --release --manifest-path crates/storage/Cargo.toml --example large_file -- 3
```

## License

Copyright (C) 2026 the Linework authors.

Linework is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version. It is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See [LICENSE](LICENSE) for the full text.

Bundled third-party components keep their own licenses, listed in [runtime/licenses](runtime/licenses) and [runtime/python/LICENSE.txt](runtime/python/LICENSE.txt): Scintilla and Lexilla, CPython, Oniguruma, the Sublime HQ syntax definitions, Spellbook (MPL 2.0), the SCOWL English dictionary and the Rust crates in Cargo.lock.
