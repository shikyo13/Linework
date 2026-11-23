# External tools

Choose **Tools → Configure external tools** to edit your personal tool list. Linework creates a starter JSON formatter there the first time you open it. That example uses Python 3.

Each tool has a display name, an executable, optional arguments, and a mode. Arguments are passed individually. The executable receives the selected text through standard input as UTF-8.

| Field | Use |
| --- | --- |
| `name` | Name shown under **Run external tool** |
| `program` | Executable name or absolute path |
| `args` | Array of argument strings |
| `mode` | `output` shows output and diagnostics. `filter` replaces the selection with UTF-8 standard output on success. |
| `shortcut` | Optional chord, for example `Ctrl+Alt+J` |

You can use `{folder}`, `{file}`, and `{filename}` inside arguments. Tools run in the active workspace folder. Launching a filter requires a selection. If the document changes while the filter runs, the original selection is preserved. Successful replacements undo in one step.

## Example

This tool sorts selected lines using a local Python installation.

~~~json
{
  "tools": [
    {
      "name": "Sort selected lines",
      "program": "python",
      "args": [
        "-X", "utf8", "-c",
        "import sys; s=sys.stdin.read(); sys.stdout.write('\\n'.join(sorted(s.splitlines())) + ('\\n' if s.endswith('\\n') else ''))"
      ],
      "mode": "filter",
      "shortcut": "Ctrl+Alt+J"
    }
  ]
}
~~~

## Builds

Set a build command in **Tools → Set build command**, then press **F7**. Linework remembers the command per folder. It recognizes common Rust, Go, CMake, npm, and Python projects and suggests a command where possible.

The Results sidebar shows output, the exit status, and recognized diagnostics. Double-click a diagnostic or press **F4** to navigate. Standard GCC/Clang, MSVC, Rust, and Python location formats are recognized.
