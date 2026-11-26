"""JSON-lines host for installed packages. stdout is reserved for the editor protocol."""

import importlib
import inspect
import json
import os
import shutil
import stat
import sys
import tempfile
import traceback
import zipfile
from pathlib import Path, PurePosixPath

sys.path.insert(0, str(Path(__file__).resolve().parent))
import sublime
import sublime_plugin as plugin

commands, listeners, view_listeners, entries, packages, errors = {}, [], [], [], [], []
modules = []
original_objects = {}
last_view = None
loaded = False


def install(source):
    source = Path(source)
    package_dir = Path(sublime.packages_path())
    package_dir.mkdir(exist_ok=True, parents=True)
    name = source.name if source.is_dir() else source.stem
    if (
        name.lower() in ("user", "sublime", "sublime_plugin", "host")
        or not name.strip()
    ):
        raise ValueError("Choose a named plugin package")
    dest = package_dir / name
    if dest.exists():
        raise ValueError(
            f"{name} is already installed. Remove it from Manage Sublime Packages before reinstalling."
        )
    with tempfile.TemporaryDirectory(dir=package_dir.parent) as temp:
        staged = Path(temp) / "content"
        if source.is_dir():
            if source.resolve() in [staged.resolve(), *staged.resolve().parents]:
                raise ValueError("Choose the plugin's own folder")
            for p in source.rglob("*"):
                if p.is_symlink() or getattr(p, "is_junction", lambda: False)():
                    raise ValueError(f"Linked package member: {p}")
            shutil.copytree(
                source, staged, ignore=shutil.ignore_patterns(".git", "__pycache__")
            )
        else:
            staged.mkdir()
            with zipfile.ZipFile(source) as z:
                files = [f for f in z.infolist() if not f.is_dir()]
                if (
                    len(files) > 20000
                    or sum(f.file_size for f in files) > 256 * 1024 * 1024
                ):
                    raise ValueError(
                        "The package exceeds the 256 MiB or 20,000 file unpacking limit"
                    )
                for f in files:
                    parts = PurePosixPath(f.filename.replace("\\", "/")).parts
                    if (
                        not parts
                        or any(
                            p in ("..", "/") or ":" in p or os.path.isreserved(p)
                            for p in parts
                        )
                        or stat.S_ISLNK(f.external_attr >> 16)
                    ):
                        raise ValueError(f"Invalid package member: {f.filename}")
                    path = staged.joinpath(*parts)
                    if not path.resolve().is_relative_to(staged.resolve()):
                        raise ValueError(f.filename)
                    path.parent.mkdir(exist_ok=True, parents=True)
                    with z.open(f) as src, path.open("xb") as out:
                        shutil.copyfileobj(src, out)
            children = list(staged.iterdir())
            if (
                len(children) == 1
                and children[0].is_dir()
                and source.suffix.lower() == ".zip"
            ):
                staged = children[0]
        staged.rename(dest)
    return name


def discover():
    global loaded
    if loaded:
        return
    folder = Path(sublime.packages_path())
    folder.mkdir(parents=True, exist_ok=True)
    sys.path.insert(0, str(folder))
    for root in sorted(folder.iterdir()):
        if not root.is_dir() or root.name.startswith("."):
            continue
        packages.append(root.name)
        for file in sorted(root.glob("*.py")):
            if file.name.startswith("_"):
                continue
            try:
                mod = importlib.import_module(root.name + "." + file.stem)
                modules.append(mod)
                for cls in vars(mod).values():
                    if not inspect.isclass(cls) or cls.__module__ != mod.__name__:
                        continue
                    if issubclass(
                        cls,
                        (
                            plugin.TextCommand,
                            plugin.WindowCommand,
                            plugin.ApplicationCommand,
                        ),
                    ) and cls.__name__.endswith("Command"):
                        commands[cls.name()] = root.name, cls
                    elif issubclass(cls, plugin.EventListener):
                        listeners.append(cls())
                    elif issubclass(cls, plugin.ViewEventListener):
                        view_listeners.append(cls)
            except Exception as e:
                errors.append(f"{root.name}/{file.name}: {type(e).__name__}: {e}")
        for file in sorted(root.rglob("*.sublime-commands")):
            try:
                rows = sublime.decode_value(file.read_text(encoding="utf-8-sig"))
                entries.extend(
                    (root.name, r)
                    for r in rows
                    if isinstance(r, dict) and "command" in r
                )
            except Exception as e:
                errors.append(f"{file.name}: {e}")
    for mod in modules:
        try:
            if hasattr(mod, "plugin_loaded"):
                mod.plugin_loaded()
        except Exception as e:
            errors.append(f"{mod.__name__}.plugin_loaded: {e}")
    loaded = True


def command(name, args, view):
    if name not in commands:
        raise ValueError(f"Unsupported editor or package command: {name}")
    _, cls = commands[name]
    if issubclass(cls, plugin.TextCommand):
        instance = cls(view)
    elif issubclass(cls, plugin.WindowCommand):
        instance = cls(sublime.active_window())
    else:
        instance = cls()
    if not instance.is_enabled(**args):
        raise ValueError(f"{name} is disabled in this view")
    if issubclass(cls, plugin.TextCommand):
        return instance.run(sublime.Edit(), **args)
    return instance.run(**args)


def list_commands():
    result, represented = [], set()
    for root, entry in entries:
        name = entry["command"]
        if name not in commands:
            continue
        result.append(
            dict(
                label=f"{entry.get('caption', name)}  ·  {root}",
                name=name,
                args=entry.get("args", {}),
            )
        )
        represented.add(name)
    for name, (root, cls) in commands.items():
        if name not in represented:
            result.append(
                dict(
                    label=f"{name.replace('_', ' ').title()}  ·  {root}",
                    name=name,
                    args={},
                )
            )
    return sorted(result, key=lambda e: e["label"].casefold())


def response(view):
    if isinstance(view, sublime.PagedView):
        # Coordinates are Unicode points. The IPC worker owns the edited tree and
        # translates returned coordinates before the UI validates the revision.
        return dict(regions=[(min(view.size(), max(0, r.a)), min(view.size(), max(0, r.b))) for r in view.sel()],
                    messages=list(sublime._messages), dialog=sublime._dialog,
                    reveal=view._doc.get("reveal"))
    # Pieces referencing the source remain ordered. Project replaced intervals back
    # to the native editor in a single undo action, preserving untouched anchors.
    edits, cursor, added = [], 0, []
    for a, b, value in view._pieces + [(len(view._before), len(view._before), None)]:
        if value is not None:
            added.append(value[a:b])
            continue
        if a != cursor or added:
            edits.append((cursor, a, "".join(added)))
            added = []
        cursor = b

    def byte_offsets(text, points):
        out, prev, byte = {}, 0, 0
        for p in sorted(set(points)):
            byte += len(text[prev:p].encode("utf-8"))
            out[p], prev = byte, p
        return out

    old = byte_offsets(view._before, [p for a, b, _ in edits for p in (a, b)])
    regions = [
        (max(0, min(r.a, view.size())), max(0, min(r.b, view.size())))
        for r in view.sel()
    ]
    reveal = view._doc.get("reveal")
    if reveal is not None:
        reveal = max(0, min(reveal, view.size()))
    new = byte_offsets(
        view._text,
        [p for r in regions for p in r] + ([reveal] if reveal is not None else []),
    )
    return dict(
        edits=[(old[a], old[b], v) for a, b, v in edits],
        regions=[(new[a], new[b]) for a, b in regions],
        messages=list(sublime._messages),
        dialog=sublime._dialog,
        reveal=new.get(reveal),
    )


def bind(doc, folders):
    global last_view
    # Scintilla's UTF-8 byte positions become Sublime's Unicode code point positions.
    cls = sublime.PagedView if doc.get("paged") else sublime.View
    if not doc.get("paged"):
        raw = doc["text"].encode("utf-8")
        mapped = {}
        prev = point = 0
        style_starts = [n for n, _ in doc["scopes"][0]] if "scopes" in doc else []
        style_starts.extend(doc.get("visible", []))
        endpoints = {n for r in doc["regions"] for n in r}
        for byte in sorted(set(style_starts).union(endpoints)):
            if not 0 <= byte <= len(raw): raise ValueError("Byte offset outside the document")
            edge = byte
            while edge < len(raw) and raw[edge] & 0xc0 == 0x80: edge -= 1
            if edge != byte and byte in endpoints:
                raise ValueError("Selection splits a UTF-8 character")
            point += len(raw[prev:edge].decode("utf-8"))
            mapped[byte], prev = point, edge
        doc["regions"] = [(mapped[a], mapped[b]) for a, b in doc["regions"]]
        if "visible" in doc:
            doc["visible"] = [mapped[p] for p in doc["visible"]]
        if "scopes" in doc:
            doc["scopes"][0] = [(mapped[a], style) for a, style in doc["scopes"][0]]
    if last_view is None or type(last_view) is not cls or last_view.id() != doc["id"]:
        last_view = cls(doc)
    else:
        last_view.__init__(doc)
    sublime._window = sublime.Window(last_view, folders)
    return last_view


def handle(request):
    global last_view
    op = request["op"]
    if op == "close":
        view_id = request["id"]
        previous, previous_window = last_view, sublime._window
        try:
            closing = bind(request["doc"], request.get("folders", [])) if "doc" in request else (
                last_view if last_view is not None and last_view.id() == view_id else None
            )
            if closing is not None:
                for listener in listeners:
                    fn = getattr(listener, "on_close", None)
                    if fn: fn(closing)
            for (cls, vid), obj in list(original_objects.items()):
                if vid == view_id:
                    if closing is not None:
                        obj.view = closing
                    fn = getattr(obj, "on_close", None)
                    if fn: fn()
        finally:
            for key in [k for k in original_objects if k[1] == view_id]:
                del original_objects[key]
            sublime._view_settings.pop(view_id, None)
            sublime._view_regions.pop(view_id, None)
            if previous is not None and previous.id() != view_id:
                last_view, sublime._window = previous, previous_window
            elif last_view is not None and last_view.id() == view_id:
                last_view, sublime._window = None, None
        return {}
    if op == "install":
        return {"installed": install(request["path"])}
    if op == "remove":
        root = Path(sublime.packages_path()).resolve()
        name = request["package"]
        p = (root / name).resolve()
        if p.parent != root or p.name.casefold() == "user":
            raise ValueError("Invalid package folder")
        # Preserve removed packages, including any package-local user changes.
        removed = sublime._profile / "Removed Packages"
        removed.mkdir(exist_ok=True)
        p.rename(removed / (name + "-" + __import__("uuid").uuid4().hex[:8]))
        return {"removed": name}
    sublime._messages.clear()
    sublime._dialog = None
    view = (
        bind(request["doc"], request.get("folders", [])) if "doc" in request else None
    )
    discover()
    if op == "commands":
        return {"commands": list_commands(), "errors": errors}
    if op == "packages":
        return {"packages": packages, "errors": errors}
    if op == "listeners":
        return {
            "listeners": sorted(
                {
                    key
                    for obj in listeners + view_listeners
                    for key in dir(obj)
                    if key.startswith("on_")
                }
            ),
            "commands": list_commands(),
            "errors": errors,
        }
    if op == "poll":
        return {}
    if op == "timers":
        sublime._run_timers()
    elif op == "run":
        command(request["name"], request.get("args", {}), view)
    elif op == "callback":
        sublime._callbacks.pop(request["token"])(request.get("value"))
    elif op == "events":
        for key in request["events"]:
            for obj in listeners:
                if callable(fn := getattr(obj, key, None)):
                    fn(view)
            for cls in view_listeners:
                if cls.is_applicable(view.settings()):
                    cache_key = (cls, view.id())
                    if cache_key not in original_objects:
                        original_objects[cache_key] = cls(view)
                    obj = original_objects[cache_key]
                    obj.view = view
                    if callable(fn := getattr(obj, key, None)):
                        fn()
    else:
        raise ValueError(op)
    return response(view)


def main():
    global last_view
    transport = sys.stdout
    sys.stdout = sys.stderr
    sublime._profile = Path(sys.argv[1])
    if len(sys.argv) > 2 and os.name == "nt":
        import ctypes
        from ctypes import wintypes
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.OpenEventW.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.LPCWSTR]
        kernel.OpenEventW.restype = wintypes.HANDLE
        kernel.SetEvent.argtypes = [wintypes.HANDLE]
        wake_handle = kernel.OpenEventW(2, False, sys.argv[2])
        if wake_handle:
            sublime._timer_wake = lambda: kernel.SetEvent(wake_handle)
    elif len(sys.argv) > 2:
        # Away from Windows the editor listens on a named pipe in the temporary
        # folder, named after the event; a byte written to it is the signal.
        name = "".join(c if c.isalnum() or c in "-_" else "_" for c in sys.argv[2])
        try:
            wake_pipe = os.open(Path(tempfile.gettempdir()) / "linework-events" / name, os.O_WRONLY | os.O_NONBLOCK)
        except OSError:
            wake_pipe = None
        def wake():
            try:
                os.write(wake_pipe, b"1")
            except OSError:
                pass
        if wake_pipe is not None:
            sublime._timer_wake = wake
    sublime._dispatcher = command
    def view_request(view, method, *args):
        if threading.current_thread() is not threading.main_thread() or view is not last_view or not active[0]:
            raise RuntimeError("Paged view access requires a current package command or event")
        transport.write(json.dumps({"view_request":{"method":method,"args":args}},ensure_ascii=False)+"\n")
        transport.flush()
        result = json.loads(sys.stdin.readline())
        if "error" in result:
            raise RuntimeError(result["error"])
        return result["result"]
    import threading
    active = [False]
    sublime._view_request = view_request
    for line in sys.stdin:
        request = {}
        previous, previous_window = last_view, sublime._window
        try:
            request = json.loads(line)
            active[0] = bool(request.get("doc",{}).get("paged"))
            result = handle(request)
        except Exception as e:
            traceback.print_exc()
            result = {"error": f"{type(e).__name__}: {e}"}
        if request.get("transient_view"):
            last_view, sublime._window = previous, previous_window
        del previous, previous_window, request, line
        active[0] = False
        result.update(
            timers=sublime._timer_delay() is not None,
            delay=sublime._timer_delay(),
        )
        transport.write(json.dumps(result, ensure_ascii=True) + "\n")
        transport.flush()
    # Plugins may own non-daemon threads. The editor has closed its pipe.
    os._exit(0)


if __name__ == "__main__":
    main()
