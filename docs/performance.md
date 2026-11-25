# Performance findings

What we learned making Linework and `lw` fast on very large files and large folders, with the
measurements behind it. All figures were measured in November 2026 on one Windows 11 PC (Ryzen 7
9800X3D, 8 cores and 16 threads, 96 GiB of memory, NVMe drive). Lower is better for every time and
memory figure.

## Results

### Opening a large log at its last line

Twelve editors, each alone with stock settings, the same window size and the same generated logs.
Time until the final text is on screen, median of three runs. "From disk" means the file was removed
from the Windows file cache before the editor started.

| Editor | 3 GB, cached | 3 GB, from disk | 10 GB, cached | 10 GB, from disk | Memory, 10 GB |
|-|-|-|-|-|-|
| Linework | **0.36 s** | **0.67 s** | 0.70 s | 1.66 s | 174 MiB |
| EmEditor 26.4 | 0.52 s | 1.15 s | **0.48 s** | **0.54 s** | 103 MiB |
| Microsoft Edit 1.2 | 1.03 s | 1.47 s | not opened whole | not opened whole | |
| Notepad++ 8.9 | 3.13 s | 2.77 s | 8.49 s | 8.04 s | 11,000 MiB |
| klogg 22.06 | 3.23 s | 3.23 s | 9.39 s | 9.63 s | 478 MiB |
| 010 Editor 17.0 | 4.20 s | 3.98 s | 12.1 s | 11.3 s | 38 MiB |
| gVim 9.2 | 5.67 s | 5.89 s | 18.6 s | 18.6 s | 11,608 MiB |
| VS Code 1.143 | 17.1 s | 18.5 s | 70.6 s | 75.0 s | 14,507 MiB |
| Kate 26.12 | 47.4 s | 49.0 s | 198 s | 209 s | 33,421 MiB |
| Sublime Text 4215 | 85.7 s | 85.4 s | 278 s | 295 s | 30,302 MiB |

Zed 1.29 reports that the 10 GB file is too large to load, Notepad4 26.10 stays on its loading
message, and Microsoft Edit shows only the first 4 GiB of it, so none of them is ranked on that
file. EmEditor shows the end of a 10 GB file before it has
counted its lines and shows no line numbers; Linework shows numbered lines, so it counts all 84
million lines first, reading from disk at about 6.4 GB/s.

### Find, Replace All and save

Find the last line, replace 25,242 matches, save in place; every saved file checked by SHA-256.

| File | Linework | EmEditor | 010 Editor |
|-|-|-|-|
| 3 GB | **3.10 s** | 3.75 s | 21.6 s |
| 10 GB | **7.20 s** | 8.65 s | 119 s |

Changing one character of a 10 GB file and saving takes Linework about 0.01 s (0.80 s before
version 1.8.0), 010 Editor 0.03 to 0.04 s and EmEditor 3.9 to 5.1 s.

### lw against ripgrep 15.2

Both programs with the same antivirus settings, output piped, medians of 5 to 7 runs.

| Search | ripgrep | lw |
|-|-|-|
| Folder of 29,498 files, text that is nowhere | 0.154 s | 0.102 s |
| Same folder, lines containing 'function', numbered | 0.222 s | 0.137 s |
| 32 GB project, after ignore rules | 1.619 s | 1.288 s |
| List files, 287,000-file tree | 0.165 s | 0.108 s |
| 10 GB log, count matches | 1.63 s | 0.52 s |
| 10 GB log, any case | 4.63 to 5.10 s | 0.51 s |
| 10 GB log, all 84,139 matching lines numbered | 1.88 s | 0.57 s |

`lw` produces byte-identical output and exit codes to ripgrep in all 241 compared command lines.
ripgrep is still faster on regular expressions that match most lines of a very large file (counting
`^2026` on the 10 GB log: 9.0 s against 15.8 s), where `lw` currently uses a single thread.

### Find in folder in the editor

The editor's Find in folder now runs on `lw`'s engine:

| Folder | Before | After |
|-|-|-|
| 29,498 files | 1.7 to 1.9 s | 0.32 to 0.47 s |
| 13 GB project | 5.9 to 6.6 s | 1.2 to 1.3 s |
| 32 GB project | 20 to 21 s | 4.7 to 4.8 s |

## Where the time goes

Matching text is a small part of searching files. In a 29,498-file search on one thread, 42.7% of
the time went to reading, 30.3% to opening files, 9.7% to closing them, 11% to listing folders, 2.3%
to ignore rules and 0.5% to matching. One cached file costs about 12.6 us to open, 9.0 us plus 160 ns
per KB for the first read, and 3.2 us to close. Most of the gains came from changing how Linework asks
Windows for things, not from faster matching code.

## Findings about Windows file access

These apply to any Windows program that reads many files or very large ones.

| Finding | Effect |
|-|-|
| Ask a read for exactly the bytes you expect, never for a whole spare buffer | Windows prepares every page offered: one search went from 4.6 s to 1.06 s |
| List folders without `FIND_FIRST_EX_LARGE_FETCH` | 1.7 times faster on 40,219 mostly small folders |
| Give each thread its own file handle (`ReOpenFile`) | Reads on one synchronous handle are completed one at a time |
| Never open a file for what the folder listing already reports | Size and write time come with the listing |
| Read a large file's beginning, and the rest only if it is text | A 32 GB project went from 3.1 s to 1.3 s |
| Count line numbers in the same pass as the search | Looking up each match's line afterwards re-read 5.4 GB: 1.73 s became 0.55 s |
| Do not start background work a command does not need | A line count running beside a search nearly doubled it |
| Avoid NTFS transactions for small saves | Committing one discards all of the file's cached pages, so the next read of a 10 GB file took 430 to 790 ms |

### The cache limit on a single file

One cached file could be read at about 22 GB/s no matter how many threads read it; two files
together reached 35 GB/s. To find out why, we decompiled and read the relevant parts of the Windows
kernel. When a read reaches a 256 KiB cache view that begins on a 1 MiB boundary, Windows unmaps
every view in the 8 MiB behind it, so each pass over a file pays the full per-page setup cost again.
The distance is set by the registry value `CacheUnmapBehindLengthInMB` (1 to 128 MB) and cannot be
turned off, and the random-access hint does not affect it.

Linework now reads the first quarter of each megabyte a little ahead of the rest of the scan, which
moves that unmapping out of the pass, and tidies the views in the background afterwards. A pass over
a 10 GB cached file went from 0.49 to 0.53 s to 0.33 to 0.36 s. In `lw`, counting a common word in
the 10 GB log went from 0.569 s to 0.470 s.

### File-system filters

Every file open passes through the file-system filter drivers on the drive. Microsoft Defender's
work was about a third of the cost of an open, and the Xbox Gaming Services install filter
(`gameflt`) about a quarter, even with no Xbox game installed. One third-party security product made
every folder listing and file open by unsigned programs about nine times slower. With antivirus exclusions in place and
the gaming filter detached from the data drive, filters cost 4 to 9% of a folder search. Reading the
Gaming Services code showed why the gaming filter comes back after being disabled: a repair routine
re-enables it and a periodic check attaches it to each drive again.

### Saving small changes safely

Linework writes only the changed bytes into the file. Before writing, it records the bytes it is
about to overwrite, so an interrupted save is undone the next time it starts. While it writes, it
holds shared byte-range locks on the rest of the file and an open section, which together prevent
other programs from writing to or truncating the file.

## Measuring fairly

- Times are taken from the editor's own window: the moment its text area reaches its final state.
- Results are checked: saved files by SHA-256, search output byte for byte against another tool,
  final screens by eye. Two editors whose times looked fastest on 10 GB had not opened the file.
- Cache state, antivirus exclusions and whether output is piped or discarded each changed a
  program's speed by 40% or more, so every comparison holds them equal.
- Builds compared against each other run as separate processes a few seconds apart, because a
  finished process's cache views are released only after about two seconds.

## Still open

- Regular expressions that match most lines of a very large file run on one thread in `lw` and in
  regular-expression Replace All; splitting them across threads as plain-text search already is
  should close the gap with ripgrep.
- EmEditor's Replace All step on 10 GB takes 0.25 s against Linework's 0.59 s. EmEditor keeps the
  whole file mapped in memory after opening it; Linework deliberately does not hold large files open
  that way.
