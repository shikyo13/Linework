#!/usr/bin/env python3
"""Writes the declarations of crates/win32, the macOS stand-in for windows-sys.

    tools/win32-gen.py WINDOWS_SYS_SOURCE

WINDOWS_SYS_SOURCE is the unpacked windows-sys 0.59 crate (for example
~/.cargo/registry/src/<index>/windows-sys-0.59.0). The front end in src/ is read for
the Windows names it uses; each constant and type is copied from windows-sys, and each
function is declared with its windows-sys signature. A function written by hand in
crates/win32/src/imp is forwarded to; one not written yet reports its name once when
called and returns zero. Run again after adding to either side.
"""
import collections, glob, os, re, sys

here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
registry = os.path.join(sys.argv[1], "src", "Windows", "Win32")
out = os.path.join(here, "crates", "win32", "src", "Win32")
MODULES = ["Foundation", "Graphics/Gdi", "Graphics/GdiPlus", "Graphics/Dwm", "Security", "Storage/FileSystem",
           "System/Com", "System/DataExchange", "System/LibraryLoader", "System/Memory", "System/SystemInformation",
           "System/Threading", "System/WindowsProgramming", "UI/Accessibility", "UI/Controls", "UI/Controls/Dialogs",
           "UI/HiDpi", "UI/Input/KeyboardAndMouse", "UI/Shell", "UI/WindowsAndMessaging"]
# Parts of the front end that macOS replaces whole, so their Windows names are not needed.
REPLACED = {"preview_web.rs", "sidebar_scene.rs", "sidebar_visual.rs", "review_regressions.rs"}
FUNCTION = re.compile(r'windows_targets::link!\("[^"]+" "(\w+)" fn (\w+)\((.*)\)(?: -> (.*))?\);$')

parsed = {}
def items(module):
    """name -> (kind, text) for one windows-sys module."""
    if module in parsed:
        return parsed[module]
    found, path = {}, os.path.join(registry, module, "mod.rs")
    lines = open(path).read().split("\n") if os.path.exists(path) else []
    at = 0
    while at < len(lines):
        start = at
        while lines[at].startswith("#["):
            at += 1
        line = lines[at]
        head = [l for l in lines[start:at] if not l.startswith("#[cfg(feature")]
        if any(l.startswith("#[cfg") and "aarch64" not in l for l in head):
            at += 1  # another processor's variant
            if line.startswith("pub struct") or line.startswith("pub union"):
                while lines[at - 1] != "}":
                    at += 1
            continue
        head = [l for l in head if not l.startswith("#[cfg")]
        function = FUNCTION.match(line)
        if function:
            found.setdefault(function.group(2), ("fn", function.groups()))
        elif m := re.match(r"pub const (\w+): .* = .*;$", line):
            found.setdefault(m.group(1), ("const", line))
        elif m := re.match(r"pub type (\w+) = .*;$", line):
            found.setdefault(m.group(1), ("type", line))
        elif m := re.match(r"pub struct (\w+)\(.*\);$", line):
            found.setdefault(m.group(1), ("type", "\n".join(head + [line])))
        elif m := re.match(r"pub (?:struct|union) (\w+) \{$", line):
            end = at
            while lines[end] != "}":
                end += 1
            found.setdefault(m.group(1), ("type", "\n".join(head + lines[at:end + 1])))
            at = end
        at += 1
    parsed[module] = found
    return found

def used_names():
    names = set()
    for path in glob.glob(os.path.join(here, "src", "*.rs")):
        if os.path.basename(path) in REPLACED:
            continue
        text = re.sub(r"//[^\n]*", "", open(path).read())
        names.update(re.findall(r"\b[A-Za-z_][A-Za-z0-9_]*\b", text))
    return names

def resolve(module, path):
    """The module a `super::`-relative path inside `module` points into."""
    parts = module.split("/")
    for step in path.split("::"):
        if step == "super":
            parts.pop()
        elif step:
            parts.append(step)
    return "/".join(parts)

wanted = collections.defaultdict(set)
queue = [(module, name) for module in MODULES for name in used_names() if name in items(module)]
while queue:
    module, name = queue.pop()
    if name in wanted[module] or name not in items(module):
        continue
    wanted[module].add(name)
    kind, text = items(module)[name]
    text = " ".join(part or "" for part in text) if kind == "fn" else text
    for path, other in re.findall(r"((?:super::)+(?:\w+::)*)\s*(\w+)", text):
        queue.append((resolve(module, path.rstrip(":")), other))
    for other in re.findall(r"\b[A-Za-z_][A-Za-z0-9_]*\b", re.sub(r"(?:super::)+(?:\w+::)*\s*\w+", "", text)):
        queue.append((module, other))

written = set()
for path in glob.glob(os.path.join(here, "crates", "win32", "src", "imp", "*.rs")):
    written.update(re.findall(r"pub unsafe fn (\w+)\(", open(path).read()))

tree, missing = collections.defaultdict(set), []
for module in wanted:
    parts = module.split("/")
    for depth in range(len(parts)):
        tree["/".join(parts[:depth])].add(parts[depth])
for module in sorted(set(wanted) | set(tree)):
    if not module:
        continue
    body = ["// Generated by tools/win32-gen.py from windows-sys 0.59 (MIT OR Apache-2.0). Do not edit.",
            "#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals, clippy::all)]"]
    body += [f"pub mod {child};" for child in sorted(tree.get(module, ()))]
    for name in sorted(wanted.get(module, ())):
        kind, text = items(module)[name]
        if kind != "fn":
            body.append(text)
            continue
        _, _, parameters, result = text
        arguments = ", ".join(re.findall(r"(\w+) :", parameters))
        result = f" -> {result}" if result else ""
        if name in written:
            call = f"unsafe {{ crate::imp::{name}({arguments}) }}"
        else:
            missing.append(name)
            unused = "".join(f"let _ = {argument}; " for argument in arguments.split(", ") if argument)
            call = f'{unused}crate::missing("{name}"); unsafe {{ core::mem::zeroed() }}'
        body.append(f'pub unsafe extern "system" fn {name}({parameters}){result} {{ {call} }}')
    target = os.path.join(out, module, "mod.rs")
    os.makedirs(os.path.dirname(target), exist_ok=True)
    open(target, "w").write("\n".join(body) + "\n")
top = ["// Generated by tools/win32-gen.py. Do not edit.", "#![allow(non_snake_case)]"]
open(os.path.join(out, "mod.rs"), "w").write("\n".join(top + [f"pub mod {child};" for child in sorted(tree[""])]) + "\n")
counts = collections.Counter(items(module)[name][0] for module in wanted for name in wanted[module])
print(dict(counts), "written by hand:", counts["fn"] - len(missing), "not yet:", len(missing))
