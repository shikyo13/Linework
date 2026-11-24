# Contributing to Linework

Linework is a Windows x64 text editor written in Rust. It embeds Scintilla and
Lexilla for the source editor and Microsoft WebView2 for the document preview.
This guide covers building, testing and packaging it from this repository.

## Requirements

- Windows 10 or 11, x64
- Rust 1.94 or later with the `x86_64-pc-windows-msvc` target
- Visual Studio C++ Build Tools and the Windows SDK
- The WebView2 runtime, which current Windows versions include
- For the scripted checks: Python 3.11 or later with `pywin32`, `Pillow`,
  `psutil` and `websockets`
- For the installer: the WiX Toolset 5 command-line tool (`wix`) with the UI
  extension (`wix extension add -g WixToolset.UI.wixext/5.0.2`)

## Build and run

```powershell
cargo build --release --locked
```

`tools\build.ps1` builds the release binary and starts it with a disposable
profile in `.scratch\profile`, so your own settings and session are not
touched. It opens `docs\try-the-effects.md`, a walkthrough of the editor
effects. `-NoRun` only builds, `-Check` only type-checks, and `-Open <file>`
opens another file.

When you run `target\release\linework.exe` yourself, set `LINEWORK_RUNTIME` to
this repository's `runtime` folder so it finds Scintilla, Lexilla, the Python
runtime and the license notices, and set `LINEWORK_PROFILE` to an empty folder
to keep the run away from your own profile in `%LOCALAPPDATA%\Linework`.

## Repository layout

| Path | Contents |
|-|-|
| `src/` | The application: window, commands, editor wiring, preview, Notepad++ plugin host (`src/npp/`), Sublime package bridge |
| `crates/storage/` | The file engine: piece tree, encodings, line index, search, saving, recovery |
| `crates/editor/` | Scintilla integration, syntax highlighting, completion, spelling |
| `crates/project/` | Folder search, symbols and project text handling |
| `assets/` | Icon, application manifest and the preview page |
| `runtime/` | Files shipped beside the executable: Scintilla and Lexilla, the embedded Python and its Sublime API modules, third-party license notices |
| `installer/` | `build.py`, which builds the per-user MSI with WiX |
| `tests/acceptance/` | End-to-end checks that drive a packaged build on a private Windows desktop |
| `tools/` | `build.ps1`, the editor-effects check and the installer payload check |
| `docs/` | User walkthroughs |

The three crates under `crates/` are separate Cargo projects with their own
lock files.

## Tests

```powershell
cargo test --locked
cargo test --manifest-path crates/storage/Cargo.toml
cargo test --manifest-path crates/editor/Cargo.toml
cargo test --manifest-path crates/project/Cargo.toml
```

Some storage tests are marked `#[ignore]` because they need multi-gigabyte
fixtures or a dedicated scratch volume; their attributes say what they need.
One application test creates a private Windows desktop and fails where that is
not allowed, such as inside some remote or sandboxed sessions.

`python tools/qa/polish_check.py [path-to-linework.exe]` drives a release build
and checks each editor and preview effect in the running editor. It shows test
windows on the desktop while it runs and uses a throwaway profile. The scripts
in `tests/acceptance/` exercise saving, undo, recovery and extensions against a
packaged build; run any of them with `--help` for its arguments.

## Packaging

Stage a release folder holding `Linework.exe` (the release binary renamed),
the `runtime` folder without `__pycache__` folders, `README.md`,
`NOTEPAD-PLUGINS.md` and `LICENSE`. Then:

```powershell
python installer/build.py --app <release folder> --output dist\Linework-Setup-<version>-x64.msi
python tools/qa/verify_msi.py <release folder> dist\Linework-Setup-<version>-x64.msi
```

The second command unpacks the MSI and compares every file with the staged
folder.

## Changes

- Keep commits focused and write messages in the Conventional Commits form
  (`feat:`, `fix:`, `chore:`), with a body that says what changed and why.
- Versions follow semantic versioning. The version lives in `Cargo.toml`,
  `Cargo.lock` and the README title.
- Contributions are accepted under the project's license, the GNU General
  Public License version 3 or later.
