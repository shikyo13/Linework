"""Compatibility layer for packages hosted by Linework's isolated CPython process."""

import bisect
import fnmatch
import heapq
import itertools
import json
import os
import re
import threading
import time
from pathlib import Path

IGNORECASE, LITERAL = 2, 1
CLASS_WORD_START, CLASS_WORD_END = 1, 2
CLASS_PUNCTUATION_START, CLASS_PUNCTUATION_END = 4, 8
CLASS_LINE_START, CLASS_LINE_END, CLASS_EMPTY_LINE = 64, 128, 256
MONOSPACE_FONT, KEEP_OPEN_ON_FOCUS_LOST = 1, 2
DRAW_EMPTY, HIDDEN, DRAW_NO_FILL, DRAW_NO_OUTLINE = 1, 128, 256, 512
ENCODED_POSITION, TRANSIENT = 1, 4
OP_EQUAL, OP_NOT_EQUAL, OP_REGEX_MATCH, OP_NOT_REGEX_MATCH = range(4)
OP_REGEX_CONTAINS, OP_NOT_REGEX_CONTAINS = 4, 5
_window = None
_messages = []
_settings = {}
_view_settings = {}
_view_regions = {}
_profile = None
_dispatcher = None
_callbacks = {}
_dialog = None
_timer_lock = threading.Lock()
_timers = []
_sequence = itertools.count()
_timer_wake = lambda: None


class Region:
    def __init__(self, a, b=None, xpos=-1):
        self.a, self.b, self.xpos = int(a), int(a if b is None else b), xpos

    def begin(self):
        return min(self.a, self.b)

    def end(self):
        return max(self.a, self.b)

    def size(self):
        return abs(self.b - self.a)

    def empty(self):
        return self.a == self.b

    def contains(self, x):
        if isinstance(x, Region):
            return self.begin() <= x.begin() and x.end() <= self.end()
        return self.begin() <= x <= self.end()

    def intersects(self, other):
        return (
            max(self.begin(), other.begin()) < min(self.end(), other.end())
            or self == other
        )

    def intersection(self, other):
        a, b = max(self.begin(), other.begin()), min(self.end(), other.end())
        return Region(a, b) if a <= b else Region(0)

    def cover(self, other):
        return Region(min(self.begin(), other.begin()), max(self.end(), other.end()))

    def __lt__(self, other):
        return (self.begin(), self.end()) < (other.begin(), other.end())

    def __eq__(self, other):
        return isinstance(other, Region) and self.a == other.a and self.b == other.b

    def __hash__(self):
        return hash((self.a, self.b))

    def __repr__(self):
        return f"Region({self.a}, {self.b})"


class Selection:
    def __init__(self):
        self._items = []

    def __iter__(self):
        return iter(self._items.copy())

    def __len__(self):
        return len(self._items)

    def __getitem__(self, index):
        return self._items[index]

    def clear(self):
        self._items.clear()

    def add(self, r):
        r = r if isinstance(r, Region) else Region(r)
        for existing in self._items.copy():
            if existing == r or existing.intersects(r):
                if existing != r:
                    r = existing.cover(r)
                self._items.remove(existing)
        self._items.append(r)
        self._items.sort()

    def add_all(self, items):
        items = list(items)
        if not self._items and all(isinstance(r, Region) for r in items):
            items.sort()
            merged = []
            for r in items:
                if merged and (merged[-1] == r or merged[-1].intersects(r)):
                    merged[-1] = merged[-1].cover(r)
                else:
                    merged.append(r)
            self._items = merged
        else:
            for r in items:
                self.add(r)

    def subtract(self, r):
        old, self._items = self._items, []
        for current in old:
            if r.contains(current):
                continue
            if not current.intersects(r):
                self.add(current)
                continue
            if current.begin() < r.begin():
                self.add(Region(current.begin(), r.begin()))
            if current.end() > r.end():
                self.add(Region(r.end(), current.end()))

    def contains(self, region):
        return any(x.contains(region) for x in self)


class Settings:
    def __init__(self, values=None):
        self._data, self._watch = dict(values or {}), {}

    def get(self, key, default=None):
        return self._data.get(key, default)

    def has(self, key):
        return key in self._data

    def set(self, key, value):
        self._data[key] = value
        for callback in list(self._watch.values()):
            callback()

    def erase(self, key):
        self._data.pop(key, None)
        for callback in list(self._watch.values()):
            callback()

    def add_on_change(self, key, callback):
        self._watch[key] = callback

    def clear_on_change(self, key):
        self._watch.pop(key, None)

    def to_dict(self):
        return self._data.copy()


class Edit:
    pass


class View:
    def __init__(self, doc):
        self._doc = doc
        self._text = doc.get("text", "")
        self._before = self._text
        self._pieces = [(0, len(self._text), None)] if self._text else []
        self._starts = None
        self._sel = Selection()
        self._sel.add_all(Region(a, b) for a, b in doc.get("regions", [[0, 0]]))
        self._settings = _view_settings.setdefault(self.id(), Settings())
        self._settings._data.update(doc.get("settings", {}))
        self._regions = _view_regions.setdefault(self.id(), {})

    def id(self):
        return self._doc["id"]

    def buffer_id(self):
        return self.id()

    def name(self):
        return self._doc.get("name", "")

    def file_name(self):
        return self._doc.get("path")

    def is_dirty(self):
        return self._doc.get("dirty", False) or self._before != self._text

    def is_read_only(self):
        return self._doc.get("readonly", False)

    def change_count(self):
        return self._doc.get("revision", 0)

    def size(self):
        return len(self._text)

    def sel(self):
        return self._sel

    def settings(self):
        return self._settings

    def visible_region(self):
        return Region(*self._doc.get("visible", [0, self.size()]))

    def window(self):
        return _window

    def substr(self, r):
        if isinstance(r, int):
            return self._text[r : r + 1] if r >= 0 else ""
        return self._text[max(0, r.begin()) : max(0, r.end())]

    def replace(self, edit, region, text):
        if not isinstance(edit, Edit):
            raise TypeError("An active Edit object is required")
        if self.is_read_only():
            raise ValueError("View is read-only")
        a, b = (
            min(self.size(), max(0, region.begin())),
            min(self.size(), max(0, region.end())),
        )

        def transform(p):
            if a == b and p == a:
                return a + len(text)
            if p <= a:
                return p
            if p < b:
                return a + min(p - a, len(text))
            return p + len(text) - (b - a)

        def split(at):
            cursor = 0
            for index, (start, end, value) in enumerate(self._pieces):
                if cursor == at:
                    return index
                size = end - start
                if cursor < at < cursor + size:
                    cut = start + at - cursor
                    self._pieces[index : index + 1] = [
                        (start, cut, value),
                        (cut, end, value),
                    ]
                    return index + 1
                cursor += size
            return len(self._pieces)

        first = split(a)
        last = split(b)
        self._pieces[first:last] = [(0, len(text), text)] if text else []
        self._text = self._text[:a] + text + self._text[b:]
        self._starts = None
        for r in self._sel._items:
            r.a, r.b = transform(r.a), transform(r.b)

    def insert(self, edit, point, text):
        self.replace(edit, Region(point), text)
        return len(text)

    def erase(self, edit, region):
        self.replace(edit, region, "")

    def _lines(self):
        if self._starts is None:
            self._starts = [0] + [
                m.end() for m in re.finditer(r"\r\n|\r|\n", self._text)
            ]
        return self._starts

    def rowcol(self, point):
        point = min(max(0, point), self.size())
        row = bisect.bisect_right(self._lines(), point) - 1
        return row, point - self._lines()[row]

    def text_point(self, row, col, clamp_column=True):
        if row >= len(self._lines()):
            return self.size()
        start = self._lines()[max(0, row)]
        return min(
            self.line(start).end() if clamp_column else self.size(), start + max(0, col)
        )

    def _line_bounds(self, x, endings):
        r = x if isinstance(x, Region) else Region(x)
        first, last = (
            self.rowcol(r.begin())[0],
            self.rowcol(max(r.begin(), r.end() - 1))[0],
        )
        a = self._lines()[first]
        b = self._lines()[last + 1] if last + 1 < len(self._lines()) else self.size()
        if not endings:
            if b > a and self._text[b - 1 : b] == "\n":
                b -= 1
            if b > a and self._text[b - 1 : b] == "\r":
                b -= 1
        return Region(a, b)

    def line(self, x):
        return self._line_bounds(x, False)

    def full_line(self, x):
        return self._line_bounds(x, True)

    def lines(self, r):
        first, last = (
            self.rowcol(r.begin())[0],
            self.rowcol(max(r.begin(), r.end() - 1))[0],
        )
        return [self.line(self._lines()[i]) for i in range(first, last + 1)]

    def split_by_newlines(self, region):
        return [region.intersection(r) for r in self.lines(region)]

    def word(self, x):
        r = x if isinstance(x, Region) else Region(x)
        a, b = r.begin(), r.end()

        def wordchar(s):
            return s.isalnum() or s == "_"

        while a > 0 and wordchar(self._text[a - 1]):
            a -= 1
        while b < self.size() and wordchar(self._text[b]):
            b += 1
        return Region(a, b)

    def _scope_run(self, point):
        if "scopes" not in self._doc:
            raise NotImplementedError("Scope data is unavailable for this view")
        starts, names = self._doc["scopes"]
        i = bisect.bisect_right(starts, (max(point, 0), 256)) - 1
        if i < 0:
            return Region(0, self.size()), ""
        a, style = starts[i]
        end = starts[i + 1][0] if i + 1 < len(starts) else self.size()
        return Region(a, end), names[style]

    def scope_name(self, point):
        _, style = self._scope_run(point)
        root = self._doc.get("root_scope", "text.plain")
        if style.startswith("string.quoted"):
            r, _ = self._scope_run(point)
            quote = self.substr(r.begin())
            if quote in ("'", '"'):
                style += ".single" if quote == "'" else ".double"
        return root + " " + (style + "." + root.split(".")[-1] + " " if style else "")

    def extract_scope(self, point):
        return self._scope_run(point)[0]

    def score_selector(self, point, selector):
        return score_selector(self.scope_name(point), selector)

    def match_selector(self, point, selector):
        return self.score_selector(point, selector) > 0

    def find(self, pattern, start, flags=0):
        rx = re.compile(
            re.escape(pattern) if flags & LITERAL else pattern,
            re.M | (re.I if flags & IGNORECASE else 0),
        )
        m = rx.search(self._text, start)
        return Region(*m.span()) if m else Region(-1, -1)

    def find_all(self, pattern, flags=0, fmt=None, extractions=None):
        rx = re.compile(
            re.escape(pattern) if flags & LITERAL else pattern,
            re.M | (re.I if flags & IGNORECASE else 0),
        )
        result = []
        for m in rx.finditer(self._text):
            result.append(Region(*m.span()))
            if fmt is not None and extractions is not None:
                extractions.append(m.expand(fmt))
        return result

    def run_command(self, name, args=None):
        if name == "select_all":
            self.sel().clear()
            self.sel().add(Region(0, self.size()))
            return
        if name in (
            "insert",
            "left_delete",
            "right_delete",
            "upper_case",
            "lower_case",
        ):
            for r in reversed(list(self.sel())):
                if name == "insert":
                    self.replace(Edit(), r, (args or {}).get("characters", ""))
                elif name.endswith("_case"):
                    text = self.substr(r)
                    self.replace(
                        Edit(),
                        r,
                        text.upper() if name == "upper_case" else text.lower(),
                    )
                else:
                    if r.empty():
                        r = Region(
                            r.a,
                            max(0, r.a - 1)
                            if name == "left_delete"
                            else min(self.size(), r.a + 1),
                        )
                    self.erase(Edit(), r)
            return
        _dispatcher(name, args or {}, self)

    def get_regions(self, key):
        return self._regions.get(key, []).copy()

    def add_regions(self, key, regions, *args, **kwargs):
        self._regions[key] = list(regions)

    def erase_regions(self, key):
        self._regions.pop(key, None)

    def set_status(self, key, text):
        status_message(text)

    def erase_status(self, key):
        pass

    def show(self, point, *args):
        self._doc["reveal"] = point.begin() if isinstance(point, Region) else point

    show_at_center = show


class PagedView(View):
    """A Unicode-coordinate view of a file-backed document transaction."""
    def __init__(self, doc):
        super().__init__(doc)
        self._length = doc["size"]
        self._changed = False
        self._page = (-1, "")

    def size(self):
        return self._length

    def is_dirty(self):
        return self._changed or self._doc.get("dirty", False)

    def _clamp(self, point):
        return min(self._length, max(0, int(point)))

    def substr(self, region):
        if isinstance(region, int):
            if not 0 <= region < self._length:
                return ""
            start, text = self._page
            if not start <= region < start + len(text):
                start = (region // 4096) * 4096
                text = _view_request(self, "read", start, min(self._length,start+4096))
                self._page = start, text
            return text[region-start]
        a, b = self._clamp(region.begin()), self._clamp(region.end())
        return _view_request(self, "read", a, b)

    def replace(self, edit, region, text):
        if not isinstance(edit, Edit):
            raise TypeError("An active Edit object is required")
        if self.is_read_only():
            raise ValueError("View is read-only")
        if not isinstance(text, str):
            raise TypeError("Replacement text must be a string")
        if len(text) > 8*1024*1024 or len(text.encode("utf-8")) > 8*1024*1024:
            raise ValueError("A single package edit exceeds 8 MiB")
        a, b = self._clamp(region.begin()), self._clamp(region.end())
        length = len(text)
        _view_request(self, "replace", a, b, text)
        def shifted(point):
            if a == b and point == a:
                return a + length
            if point <= a:
                return point
            if point < b:
                return a + min(point-a, length)
            return point + length - (b-a)
        for r in self._sel._items:
            r.a, r.b = shifted(r.a), shifted(r.b)
        self._length += length - (b-a)
        self._page = (-1, "")
        self._changed = True

    def rowcol(self, point):
        return tuple(_view_request(self,"rowcol",self._clamp(point)))

    def text_point(self, row, col, clamp_column=True):
        return _view_request(self,"point",min(max(0,row),self.size()),min(max(0,col),self.size()),bool(clamp_column))

    def _line_bounds(self, x, endings):
        r = x if isinstance(x,Region) else Region(x)
        first = self._clamp(r.begin())
        last = self._clamp(max(first,r.end()-1))
        begin, end, full = _view_request(self,"line",first)
        if last >= full and last != first:
            _, end, full = _view_request(self,"line",last)
        return Region(begin,full if endings else end)

    def lines(self, region):
        a = self._clamp(region.begin())
        b = self._clamp(max(a,region.end()-1))
        return [Region(start,end) for start,end in _view_request(self,"lines",a,b)]

    def word(self, x):
        r = x if isinstance(x,Region) else Region(x)
        return Region(*_view_request(self,"word",self._clamp(r.begin()),self._clamp(r.end())))

    def _scope_run(self, point):
        a,b,name = _view_request(self,"scope",self._clamp(point))
        return Region(a,b),name

    def find(self, pattern, start, flags=0):
        return Region(*_view_request(self,"find",pattern,self._clamp(start),int(flags)))

    def find_all(self, pattern, flags=0, fmt=None, extractions=None):
        ranges = [Region(a,b) for a,b in _view_request(self,"find_all",pattern,0,int(flags))]
        if fmt is not None and extractions is not None:
            rx = re.compile(re.escape(pattern) if flags & LITERAL else pattern,
                            re.M | (re.I if flags & IGNORECASE else 0))
            for r in ranges:
                match = rx.fullmatch(self.substr(r))
                if match is None:
                    raise ValueError("Extraction pattern requires surrounding context")
                extractions.append(match.expand(fmt))
        return ranges


class Window:
    def __init__(self, view, folders):
        self._view, self._folders = view, folders

    def id(self):
        return 1

    def active_view(self):
        return self._view

    def folders(self):
        return self._folders.copy()

    def views(self):
        return [self._view] if self._view else []

    def run_command(self, name, args=None):
        self._view.run_command(name, args)

    def extract_variables(self):
        p = Path(self._view.file_name() or "")
        return {
            "file": str(p),
            "file_name": p.name,
            "file_base_name": p.stem,
            "file_extension": p.suffix.lstrip("."),
            "file_path": str(p.parent),
            "folder": next(iter(self._folders), ""),
        }

    def show_quick_panel(self, items, on_done, *args, **kwargs):
        _dialog_request(
            "pick",
            "",
            "",
            [
                " · ".join(str(v) for v in x)
                if isinstance(x, (tuple, list))
                else str(x)
                for x in items
            ],
            on_done,
        )

    def show_input_panel(self, caption, initial_text, on_done, on_change, on_cancel):
        _dialog_request(
            "input",
            caption,
            initial_text,
            [],
            lambda value: (
                on_cancel()
                if value is None and on_cancel
                else on_done(value)
                if value is not None
                else None
            ),
        )

    def status_message(self, message):
        status_message(message)


def _dialog_request(kind, title, initial, items, callback):
    global _dialog
    token = id(callback)
    _callbacks[token] = callback
    _dialog = dict(kind=kind, title=title, initial=initial, items=items, token=token)


def active_window():
    return _window


def windows():
    return [_window] if _window else []


def platform():
    return "windows"


def arch():
    return "x64"


def version():
    return "4200"


def packages_path():
    return str(_profile / "Packages")


def installed_packages_path():
    return str(_profile / "Installed Packages")


def cache_path():
    path = _profile / "Cache"
    path.mkdir(exist_ok=True, parents=True)
    return str(path)


def set_timeout(callback, delay=0):
    with _timer_lock:
        heapq.heappush(
            _timers,
            (time.monotonic() + max(delay, 0) / 1000, next(_sequence), callback),
        )
    _timer_wake()


def set_timeout_async(callback, delay=0):
    set_timeout(callback, delay)


def _timer_delay():
    with _timer_lock:
        if _timers:
            return max(0, int((_timers[0][0] - time.monotonic()) * 1000))
    return None


def _run_timers():
    now = time.monotonic()
    for _ in range(64):
        with _timer_lock:
            if not _timers or _timers[0][0] > now:
                return
            _, _, callback = heapq.heappop(_timers)
        callback()


def _resource(name):
    parts = Path(name.replace("\\", "/")).parts
    if not parts or parts[0] != "Packages" or any(p in ("..", ".") for p in parts):
        raise ValueError(name)
    p = (_profile.joinpath(*parts)).resolve()
    if not p.is_relative_to((_profile / "Packages").resolve()):
        raise ValueError(name)
    return p


def load_resource(name):
    return _resource(name).read_text(encoding="utf-8-sig")


def load_binary_resource(name):
    return _resource(name).read_bytes()


def find_resources(pattern):
    return sorted(
        p.relative_to(_profile).as_posix()
        for p in (_profile / "Packages").rglob("*")
        if p.is_file() and fnmatch.fnmatchcase(p.name, pattern)
    )


def decode_value(value):
    # Sublime resource JSON permits comments and trailing commas. Keep quoted strings intact.
    value = re.sub(
        r'"(?:\\.|[^"\\])*"|//[^\r\n]*|/\*.*?\*/',
        lambda m: m[0] if m[0][0] == '"' else " ",
        value,
        flags=re.S,
    )
    value = re.sub(
        r'"(?:\\.|[^"\\])*"|,(?=\s*[\]}])', lambda m: "" if m[0] == "," else m[0], value
    )
    return json.loads(value)


def encode_value(value, pretty=False):
    return json.dumps(value, ensure_ascii=False, indent=4 if pretty else None)


def load_settings(name):
    if Path(name).name != name:
        raise ValueError("Expected a settings filename")
    if name not in _settings:
        data = {}
        for resource in sorted(
            find_resources(name), key=lambda r: (r.startswith("Packages/User/"), r)
        ):
            data.update(decode_value(load_resource(resource)))
        _settings[name] = Settings(data)
    return _settings[name]


def save_settings(name):
    if Path(name).name != name:
        raise ValueError("Expected a settings filename")
    p = _profile / "Packages" / "User" / name
    p.parent.mkdir(exist_ok=True, parents=True)
    p.write_text(encode_value(load_settings(name).to_dict(), True), encoding="utf-8")


def status_message(message):
    _messages.append(str(message))


def score_selector(scope_name, selector):
    def chain(s):
        words = scope_name.split()
        total = 0
        for token in s.strip(" ()").split():
            if token == "*":
                continue
            for i, scope in enumerate(words):
                if scope == token or scope.startswith(token + "."):
                    total += len(token.split("."))
                    words = words[i + 1 :]
                    break
            else:
                return 0
        return max(1, total)

    best = 0
    for expression in re.split(r"[,|]", selector):
        inc, *excluded = re.split(r"\s+-\s+", expression)
        if not any(chain(part) for part in excluded):
            best = max(best, chain(inc))
    return best


def message_dialog(message):
    _messages.append(str(message))


def error_message(message):
    _messages.append(str(message))


def _clipboard():
    import ctypes as c
    from ctypes import wintypes as w

    user, kernel = c.windll.user32, c.windll.kernel32
    user.GetClipboardData.argtypes = [w.UINT]
    user.GetClipboardData.restype = w.HANDLE
    user.SetClipboardData.argtypes = [w.UINT, w.HANDLE]
    user.SetClipboardData.restype = w.HANDLE
    kernel.GlobalLock.argtypes = [w.HGLOBAL]
    kernel.GlobalLock.restype = c.c_void_p
    kernel.GlobalUnlock.argtypes = [w.HGLOBAL]
    kernel.GlobalAlloc.argtypes = [w.UINT, c.c_size_t]
    kernel.GlobalAlloc.restype = w.HGLOBAL
    kernel.GlobalFree.argtypes = [w.HGLOBAL]
    # Associate clipboard ownership with a hidden message-only window. Opening
    # with NULL would clear ownership on EmptyClipboard and break SetClipboardData.
    user.CreateWindowExW.argtypes = [
        w.DWORD,
        w.LPCWSTR,
        w.LPCWSTR,
        w.DWORD,
        c.c_int,
        c.c_int,
        c.c_int,
        c.c_int,
        w.HWND,
        w.HMENU,
        w.HINSTANCE,
        c.c_void_p,
    ]
    user.CreateWindowExW.restype = w.HWND
    user.OpenClipboard.argtypes = [w.HWND]
    user.DestroyWindow.argtypes = [w.HWND]
    hwnd = user.CreateWindowExW(
        0, "STATIC", "", 0, 0, 0, 0, 0, w.HWND(-3), None, None, None
    )
    for _ in range(8):
        if user.OpenClipboard(hwnd):
            return c, user, kernel, hwnd
        time.sleep(0.005)
    user.DestroyWindow(hwnd)
    raise OSError("The clipboard is busy")


def get_clipboard(size_limit=16_777_216):
    c, user, kernel, hwnd = _clipboard()
    try:
        h = user.GetClipboardData(13)
        if not h:
            return ""
        p = kernel.GlobalLock(h)
        if not p:
            return ""
        try:
            kernel.GlobalSize.argtypes = [c.c_void_p]
            kernel.GlobalSize.restype = c.c_size_t
            length = min(kernel.GlobalSize(h) // 2, max(size_limit, 0))
            return c.wstring_at(p, length).partition("\0")[0]
        finally:
            kernel.GlobalUnlock(h)
    finally:
        user.CloseClipboard()
        user.DestroyWindow(hwnd)


def set_clipboard(text):
    c, user, kernel, hwnd = _clipboard()
    h = None
    try:
        raw = (text + "\0").encode("utf-16-le")
        h = kernel.GlobalAlloc(2, len(raw))
        p = kernel.GlobalLock(h) if h else None
        if not p:
            raise OSError("Cannot allocate clipboard text")
        c.memmove(p, raw, len(raw))
        kernel.GlobalUnlock(h)
        if not user.EmptyClipboard() or not user.SetClipboardData(13, h):
            raise OSError("Cannot write clipboard text")
        h = None
    finally:
        if h:
            kernel.GlobalFree(h)
        user.CloseClipboard()
        user.DestroyWindow(hwnd)


def expand_variables(text, variables):
    if isinstance(text, list):
        return [expand_variables(s, variables) for s in text]
    if isinstance(text, dict):
        return {k: expand_variables(s, variables) for k, s in text.items()}
    if not isinstance(text, str):
        return text
    return re.sub(
        r"\$\{(\w+)(?::([^}]*))?\}|\$(\w+)",
        lambda m: variables.get(m[1] or m[3], m[2] or ""),
        text,
    )
