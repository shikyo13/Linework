# Notepad++ plugins in Linework

Extract an x64 Unicode plugin, then use **Tools → Install Notepad++ plugin** and choose its DLL. A package named `Example` can keep its dependencies, translations and resources together in an `Example` folder beside `Example.dll`. Linework copies the folder when its name matches the DLL. For a standalone DLL, select the file directly. Plugins run native code in the editor process.

Use **Tools → Notepad++ plugin commands** to search installed actions. Plugin-defined shortcuts work in the editor. Modeless dialogs accept keyboard input, and registered tool panels open as resizable floating windows.

Linework loads installed plugins from `%LOCALAPPDATA%\Linework\plugins\Example\Example.dll`. Shared plugin settings live in `plugins\Config`. For a manual upgrade or removal, close Linework before replacing or removing that plugin folder. A failed installation leaves a `.linework-disabled` file in the installed folder. Remove this marker after replacing it with a compatible build.

## Compatibility checked on Windows x64

| Released plugin | Operations checked |
| --- | --- |
| [MIME Tools 3.2](https://github.com/npp-plugins/mimetools) | Base64 encoding and decoding, with the original selection recovered byte for byte |
| [Converter 4.7](https://github.com/npp-plugins/converter) | ASCII-to-hex and hex-to-ASCII round trip |
| [NppExport 0.4.0](https://github.com/chcg/NPP_ExportPlugin) | Export to HTML through the plugin’s Save As dialog, with highlighted document content present in the written file |
| [JsonTools 8.6](https://github.com/molsonkiko/JsonToolsNppPlugin/releases/tag/v8.6) | JSON pretty-print preserving values and the native JSON tree viewer opening for the correct file |

All four unmodified plugin binaries ran together against an isolated Linework profile. The tests cover the operations listed.

### Editing across pages

On a 4,297,087,913-byte text file, MIME Tools 3.2 encoded a 615,467-byte selection beginning at byte 4,295,229,413, producing 820,623 Base64 bytes. Converter 4.7 converted that same selection into 1,307,866 bytes of uppercase hexadecimal and line endings. Both selections spanned multiple 384 KiB rendering pages. Each saved file matched an independently calculated SHA-256 digest of the entire expected output. One undo and a second save restored the original digest in each case. In this run the editor's peak sampled working set was 74.59 MiB and Windows recorded peak private commit of 100.95 MiB.

## Host interface

The host provides the Unicode plugin entry points, command IDs and shortcuts, Scintilla edit notifications, buffer and view queries, current file and path variables, document activation and opening, synchronous saves, file notifications, plugin-to-plugin messages, language/encoding/line-ending queries, theme colors, marker and indicator allocation, private Scintilla controls and Lexilla lexer creation. The range APIs accept the pointer-width structures used by Notepad++ x64 through window messages and direct calls.

Linework uses one tab strip shared by both panes. The Notepad++ primary view enumerates the open tabs, and the secondary view exposes the document shown in that pane. On paged files, cached plugin window handles and Scintilla direct calls address global 64-bit byte offsets. Supported operations include selections, targets, text retrieval and replacement, insertion and deletion, literal search, line and character positions, and read-only range pointers. Line-number queries beyond the indexed prefix can require a retry while background indexing finishes.

A paged menu command collects its edits into one undo step. Unsupported Scintilla requests cancel pending changes in that command. An explicit file save inside a command commits the work completed up to that save. Modeless messages sent outside a command commit individually. Native plugins use the real Scintilla widget for ordinary files. Paged files support a subset of its messages, including up to 65,536 disjoint selection ranges. Styling, regex targets, rectangular virtual space and lexer-dependent export on paged files are outside the tested editing interface.

For plugins that copy text, each request can transfer up to 16 MiB. Plugins can process a larger selection in successive requests. A read-only range pointer maps a contiguous disk range directly. For a range spanning multiple pieces, or a whole-document pointer requiring a trailing NUL, Linework streams a temporary file and maps it. This requires scratch disk space and time proportional to the requested range. A DLL can allocate its own buffers directly, so memory consumption also depends on how that DLL implements its command.

Linework recognizes the released Converter 4.7 binary version. It restores its selected endpoints above 4 GiB and runs its ASCII-to-hex action in line-aligned chunks sized to keep the DLL's temporary output allocation below 16 MiB. The DLL uses its installed uppercase, spacing and line-width settings. Its hex-to-ASCII action accepts up to 16 MiB of selected input. MIME Tools Base64 commands use the general transfer budget for each input and output.

Some plugins depend on Notepad++ facilities outside this host. These include Plugins Admin, external lexer-plugin registration, Notepad++ session XML, toolbar button registration, edge docking and dock-layout persistence, full native application menus and internal window classes. Built-in menu command forwarding covers common file, editing and find commands. Additional host messages are implemented individually in the source archive under `src/npp/messages.rs`. x86, ARM64 and ANSI DLLs require different hosts.
