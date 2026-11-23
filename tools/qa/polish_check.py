"""Drives a built Linework and checks each polish effect in the real editor.

    python tools/qa/polish_check.py [path-to-linework.exe]

The editor runs with a throwaway profile under .scratch and is shown without
being activated. Keys are posted to its own window and the preview is reached
through the WebView2 debugging port, so the mouse, the keyboard and other
programs are not touched. The clipboard is used for the paste check and put
back. Prints one line per check and exits non-zero if any failed.
"""
import asyncio
import base64
import ctypes
import json
import os
import shutil
import subprocess
import sys
import time
import urllib.request
from ctypes import wintypes

import websockets
import win32clipboard

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
EXE = sys.argv[1] if len(sys.argv) > 1 else os.path.join(REPO, "target", "release", "linework.exe")
WORK = os.path.join(REPO, ".scratch", "qa")
PORT = 9333

DOC = """# QA document

First paragraph has a word here and **bold text** plus `code` and [a link](#tasks) end.

Second paragraph for selection and deletion of several words in a row.

- item one
- [ ] task open
- [x] task done

3. three

## Tasks

Last paragraph with [outside](https://example.com/) link.

```
let paragraph_value = 1;
```
""" + "".join(f"\nFiller paragraph number {n} keeps the page long enough to scroll.\n" for n in range(1, 41))

user32 = ctypes.WinDLL("user32", use_last_error=True)
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32")
user32.SendMessageW.argtypes = [wintypes.HWND, wintypes.UINT, ctypes.c_size_t, ctypes.c_ssize_t]
user32.SendMessageW.restype = ctypes.c_ssize_t
user32.PostMessageW.argtypes = [wintypes.HWND, wintypes.UINT, ctypes.c_size_t, ctypes.c_ssize_t]
user32.GetDlgItem.restype = wintypes.HWND
user32.GetDlgItem.argtypes = [wintypes.HWND, ctypes.c_int]
user32.SetFocus.argtypes = [wintypes.HWND]
user32.SetFocus.restype = wintypes.HWND
user32.IsWindowVisible.argtypes = [wintypes.HWND]
user32.GetWindowRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
ENUM = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
user32.EnumWindows.argtypes = [ENUM, wintypes.LPARAM]

WM_KEYDOWN, WM_KEYUP, WM_CHAR = 0x100, 0x101, 0x102
VK_RETURN, VK_TAB, VK_DELETE, VK_INSERT, VK_CONTROL, VK_SHIFT = 0x0D, 0x09, 0x2E, 0x2D, 0x11, 0x10
GETLENGTH, GETCHARAT, GETCURRENTPOS, GETANCHOR, GOTOPOS = 2006, 2007, 2008, 2009, 2025
MARKERGET, GETLINEINDENTATION, SETSEL, LINEFROMPOSITION, UNDO = 2046, 2127, 2160, 2166, 2176
INDICATORVALUEAT, GETCARETSTYLE, GETELEMENTISSET = 2507, 2513, 2756
AUTOCACTIVE, AUTOCCANCEL, VK_SPACE = 2102, 2101, 0x20
POINTX, POINTY = 2164, 2165
PAIR, DIM, FLASH, HEAT = 30, 31, 15, 0b111110

results = []


def check(name, ok, detail=""):
    results.append((name, bool(ok), detail))
    print(("PASS  " if ok else "FAIL  ") + name + (f"  [{detail}]" if detail else ""), flush=True)


def windows_of(pid, cls):
    found = []

    def each(h, _):
        owner = wintypes.DWORD()
        user32.GetWindowThreadProcessId(h, ctypes.byref(owner))
        name = ctypes.create_unicode_buffer(64)
        user32.GetClassNameW(h, name, 64)
        if owner.value == pid and name.value == cls:
            found.append(h)
        return True

    user32.EnumWindows(ENUM(each), 0)
    return found


class Editor:
    def __init__(self, main, hwnd):
        self.main, self.h = main, hwnd

    def sci(self, m, w=0, l=0):
        return user32.SendMessageW(self.h, m, w, l)

    def text(self):
        return bytes(self.sci(GETCHARAT, i) & 255 for i in range(self.sci(GETLENGTH))).decode("utf-8", "replace")

    def pos(self, needle, after=0):
        at = self.text().find(needle)
        assert at >= 0, f"{needle!r} is not in the document"
        return len(self.text()[:at].encode()) + after

    def caret(self):
        return self.sci(GETCURRENTPOS)

    def select(self, a, b):
        self.sci(SETSEL, a, b)

    def goto(self, at):
        self.sci(GOTOPOS, at)

    def selected(self):
        a, b = sorted((self.sci(GETANCHOR), self.caret()))
        return self.text().encode()[a:b].decode()

    def line(self, at):
        return self.sci(LINEFROMPOSITION, at)

    def focus(self):
        """Keep the keyboard on the editor pane. Returns whether it had
        wandered, which a click on the test window causes."""
        user32.GetFocus.restype = wintypes.HWND
        if user32.GetFocus() == self.h:
            return False
        user32.SetFocus(self.h)
        time.sleep(0.05)
        return True

    def char(self, c):
        user32.PostMessageW(self.h, WM_CHAR, ord(c), 1)

    def type(self, s):
        for c in s:
            self.char(c)

    def key(self, vk, ctrl=False, shift=False):
        self.focus()
        state = (ctypes.c_ubyte * 256)()
        if ctrl:
            state[VK_CONTROL] = 0x80
        if shift:
            state[VK_SHIFT] = 0x80
        user32.SetKeyboardState(state)
        scan = user32.MapVirtualKeyW(vk, 0)
        user32.PostMessageW(self.h, WM_KEYDOWN, vk, 1 | scan << 16)
        time.sleep(0.07)
        user32.PostMessageW(self.h, WM_KEYUP, vk, 1 | scan << 16 | 0xC0000000)
        time.sleep(0.03)
        user32.SetKeyboardState((ctypes.c_ubyte * 256)())

    def undo_to(self, want):
        for _ in range(40):
            if self.text() == want:
                return True
            self.sci(UNDO)
            time.sleep(0.03)
        return self.text() == want


def until(test, seconds=2.0, step=0.02):
    end = time.perf_counter() + seconds
    while True:
        value = test()
        if value or time.perf_counter() > end:
            return value
        time.sleep(step)


class Page:
    """The preview pane, through the browser debugging protocol."""

    def __init__(self, ws):
        self.ws, self.n = ws, 0

    async def call(self, method, **params):
        self.n += 1
        mine = self.n
        await self.ws.send(json.dumps({"id": mine, "method": method, "params": params}))
        while True:
            reply = json.loads(await self.ws.recv())
            if reply.get("id") == mine:
                return reply.get("result", reply)

    async def js(self, body):
        code = (
            "(() => { const F = document.getElementById('sheet'), W = F.contentWindow, D = F.contentDocument;"
            "const rectOf = (word) => { const w = D.createTreeWalker(D.body, NodeFilter.SHOW_TEXT); let n;"
            " while ((n = w.nextNode())) { const i = n.data.indexOf(word); if (i >= 0) { const r = D.createRange();"
            " r.setStart(n, i); r.setEnd(n, i + word.length); const b = r.getBoundingClientRect(), f = F.getBoundingClientRect();"
            " return [f.left + b.left + b.width / 2, f.top + b.top + b.height / 2]; } } return null; };"
            "const marks = (prefix) => [...W.CSS.highlights.entries()].filter(([k]) => k.startsWith(prefix))"
            ".flatMap(([, h]) => [...h].map(r => r.toString()));"
            + body
            + " })()"
        )
        out = await self.call("Runtime.evaluate", expression=code, returnByValue=True, awaitPromise=True)
        return out.get("result", {}).get("value")

    async def until(self, body, seconds=2.0):
        end = time.perf_counter() + seconds
        while True:
            value = await self.js(body)
            if value or time.perf_counter() > end:
                return value
            await asyncio.sleep(0.03)

    async def shot(self, name):
        shot = await self.call("Page.captureScreenshot", format="png")
        if "data" in shot:
            with open(os.path.join(WORK, name), "wb") as f:
                f.write(base64.b64decode(shot["data"]))

    async def move(self, x, y, ctrl=False):
        await self.call("Input.dispatchMouseEvent", type="mouseMoved", x=x, y=y, modifiers=2 if ctrl else 0)

    async def click(self, x, y):
        await self.move(x, y)
        for kind in ("mousePressed", "mouseReleased"):
            await self.call("Input.dispatchMouseEvent", type=kind, x=x, y=y, button="left", clickCount=1)


def clipboard_text():
    try:
        win32clipboard.OpenClipboard()
        try:
            if win32clipboard.IsClipboardFormatAvailable(win32clipboard.CF_UNICODETEXT):
                return win32clipboard.GetClipboardData(win32clipboard.CF_UNICODETEXT)
            return None if win32clipboard.CountClipboardFormats() else ""
        finally:
            win32clipboard.CloseClipboard()
    except Exception:
        return None


def set_clipboard(text):
    win32clipboard.OpenClipboard()
    try:
        win32clipboard.EmptyClipboard()
        if text:
            win32clipboard.SetClipboardData(win32clipboard.CF_UNICODETEXT, text)
    finally:
        win32clipboard.CloseClipboard()


def snapshot(hwnd, path):
    """The window as the compositor has it, even when something covers it."""
    from PIL import Image

    r = wintypes.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(r))
    w, h = r.right - r.left, r.bottom - r.top
    user32.GetWindowDC.restype = wintypes.HDC
    user32.GetWindowDC.argtypes = [wintypes.HWND]
    gdi32.CreateCompatibleDC.restype = wintypes.HDC
    gdi32.CreateCompatibleDC.argtypes = [wintypes.HDC]
    gdi32.CreateCompatibleBitmap.restype = wintypes.HBITMAP
    gdi32.CreateCompatibleBitmap.argtypes = [wintypes.HDC, ctypes.c_int, ctypes.c_int]
    gdi32.SelectObject.argtypes = [wintypes.HDC, wintypes.HGDIOBJ]
    gdi32.GetBitmapBits.argtypes = [wintypes.HBITMAP, ctypes.c_long, ctypes.c_void_p]
    user32.PrintWindow.argtypes = [wintypes.HWND, wintypes.HDC, wintypes.UINT]
    gdi32.DeleteObject.argtypes = [wintypes.HGDIOBJ]
    gdi32.DeleteDC.argtypes = [wintypes.HDC]
    user32.ReleaseDC.argtypes = [wintypes.HWND, wintypes.HDC]
    dc = user32.GetWindowDC(hwnd)
    mem = gdi32.CreateCompatibleDC(dc)
    bmp = gdi32.CreateCompatibleBitmap(dc, w, h)
    gdi32.SelectObject(mem, bmp)
    user32.PrintWindow(hwnd, mem, 2)
    raw = ctypes.create_string_buffer(w * h * 4)
    gdi32.GetBitmapBits(bmp, w * h * 4, raw)
    Image.frombuffer("RGBA", (w, h), raw, "raw", "BGRA", 0, 1).convert("RGB").save(path)
    gdi32.DeleteObject(bmp)
    gdi32.DeleteDC(mem)
    user32.ReleaseDC(hwnd, dc)


def pixel(ed, x, y):
    """The color at a point of the editor pane, from the compositor's copy
    of the window."""
    from PIL import Image

    path = os.path.join(WORK, "pixel.png")
    snapshot(ed.main, path)
    origin = wintypes.POINT(0, 0)
    user32.ClientToScreen(ed.h, ctypes.byref(origin))
    r = wintypes.RECT()
    user32.GetWindowRect(ed.main, ctypes.byref(r))
    return Image.open(path).convert("RGB").getpixel((origin.x - r.left + x, origin.y - r.top + y))


def watch_glide(pid, ed, move, seconds=0.45):
    """Sample the glide stand-ins while `move` runs. One row per sample:
    time, caret style, whether the line highlight is set, and the rectangle
    of every stand-in that is showing."""
    rows = []
    if ed.focus():
        time.sleep(0.3)
    move()
    start = time.perf_counter()
    while time.perf_counter() - start < seconds:
        shown = []
        for h in windows_of(pid, "LineworkGlide"):
            if user32.IsWindowVisible(h):
                r = wintypes.RECT()
                user32.GetWindowRect(h, ctypes.byref(r))
                shown.append((r.left, r.top, r.right - r.left, r.bottom - r.top))
        rows.append((time.perf_counter() - start, ed.sci(GETCARETSTYLE), ed.sci(GETELEMENTISSET, 50), shown))
        time.sleep(0.001)
    return rows


def source_checks(pid, ed):
    original = ed.text()
    check("document loaded in the editor", original == DOC, f"{len(original)} bytes")

    # Smart typing.
    a = ed.pos("word here")
    ed.select(a, a + 4)
    ed.char("*")
    ok = until(lambda: "a *word* here" in ed.text())
    check("smart typing: * wraps the selection", ok and ed.selected() == "word", ed.selected())
    ed.char("*")
    ok = until(lambda: "a **word** here" in ed.text())
    check("smart typing: a second * makes bold", ok and ed.selected() == "word")
    ed.sci(UNDO)
    one = until(lambda: "a *word* here" in ed.text())
    ed.sci(UNDO)
    check("smart typing: each wrap is one Undo step", one and until(lambda: ed.text() == original))
    for mark in "_~`":
        ed.select(a, a + 4)
        ed.char(mark)
        ok = until(lambda: f"a {mark}word{mark} here" in ed.text())
        check(f"smart typing: {mark} wraps the selection", ok)
        ed.undo_to(original)
    ed.select(a, a + 4)
    ed.char("[")
    ok = until(lambda: "a [word]() here" in ed.text())
    check("smart typing: [ makes a link, caret between the brackets", ok and ed.caret() == ed.pos("() here", 1), ed.caret())
    ed.undo_to(original)

    end = ed.pos("- item one", len("- item one"))
    ed.goto(end)
    ed.key(VK_RETURN)
    ok = until(lambda: "- item one\n- \n- [ ] task open" in ed.text())
    check("smart typing: Enter continues a bullet list", ok and ed.caret() == end + 3, ed.caret())
    row = ed.line(ed.caret())
    ed.key(VK_TAB)
    check("smart typing: Tab nests an empty list item", until(lambda: ed.sci(GETLINEINDENTATION, row) == 2), ed.sci(GETLINEINDENTATION, row))
    ed.key(VK_TAB, shift=True)
    check("smart typing: Shift+Tab lifts it back", until(lambda: ed.sci(GETLINEINDENTATION, row) == 0), ed.sci(GETLINEINDENTATION, row))
    ed.key(VK_RETURN)
    ok = until(lambda: "- item one\n\n- [ ] task open" in ed.text())
    check("smart typing: Enter on an empty item ends the list", ok)
    ed.undo_to(original)
    ed.goto(end)
    ed.key(VK_RETURN)
    until(lambda: ed.text() != original)
    ed.sci(UNDO)
    check("smart typing: a continued item is one Undo step", until(lambda: ed.text() == original))

    ed.goto(ed.pos("3. three", len("3. three")))
    ed.key(VK_RETURN)
    check("smart typing: Enter numbers the next item", until(lambda: "3. three\n4. \n" in ed.text()))
    ed.undo_to(original)
    ed.goto(ed.pos("- [ ] task open", len("- [ ] task open")))
    ed.key(VK_RETURN)
    check("smart typing: Enter continues a task list with an empty box", until(lambda: "task open\n- [ ] \n" in ed.text()))
    ed.undo_to(original)
    para = ed.pos("in a row.", len("in a row."))
    ed.goto(para)
    ed.key(VK_RETURN)
    check("smart typing: Enter in a paragraph is an ordinary line break", until(lambda: "in a row.\n\n\n- item" in ed.text()))
    ed.undo_to(original)

    ed.goto(a + 2)
    ed.key(0x42, ctrl=True)
    ok = until(lambda: "a **word** here" in ed.text())
    check("smart typing: Ctrl+B makes the word at the caret bold", ok and ed.caret() == a + 4, ed.caret())
    ed.key(0x42, ctrl=True)
    check("smart typing: Ctrl+B again takes bold off", until(lambda: ed.text() == original))
    ed.select(a, a + 4)
    ed.key(0x49, ctrl=True)
    ok = until(lambda: "a *word* here" in ed.text())
    check("smart typing: Ctrl+I makes the selection italic", ok and ed.selected() == "word")
    ed.key(0x49, ctrl=True)
    check("smart typing: Ctrl+I again takes italic off", until(lambda: ed.text() == original))
    ed.undo_to(original)

    # A selection shows on the caret's own line, over the line highlight.
    ed.focus()
    ed.select(a, a + 4)
    time.sleep(0.5)
    top = ed.sci(POINTY, 0, a) + 1
    inside = pixel(ed, ed.sci(POINTX, 0, a) + 2, top)
    beside = pixel(ed, ed.sci(POINTX, 0, a - 4) + 2, top)
    apart = sum(abs(i - o) for i, o in zip(inside, beside))
    check("selection: shows on the caret's line", apart > 24, f"selected {inside}, same line unselected {beside}")
    ed.goto(a)

    # Undo marks what it brought back, with the same flash a paste uses.
    ed.select(a, a + 4)
    ed.key(VK_DELETE)
    until(lambda: ed.text() != original)
    time.sleep(0.2)
    ed.sci(UNDO)
    back = until(lambda: ed.text() == original)
    marked = until(lambda: ed.sci(INDICATORVALUEAT, FLASH, a + 1), 1.0, 0.005)
    check("change echo: Undo marks the restored text in the source", back and marked)
    check("change echo: the Undo mark fades", until(lambda: not ed.sci(INDICATORVALUEAT, FLASH, a + 1), 3))
    ed.goto(a)

    # The completion list keeps out of the way in prose.
    listed = lambda: ed.sci(AUTOCACTIVE)
    ed.goto(ed.pos("in a row.", len("in a row.")))
    ed.type(" para")
    until(lambda: "in a row. para" in ed.text())
    time.sleep(0.4)
    check("completion: no list opens by itself in Markdown prose", not listed())
    ed.key(VK_SPACE, ctrl=True)
    check("completion: Ctrl+Space still lists the page's words", until(listed, 1.5))
    ed.sci(AUTOCCANCEL)
    ed.undo_to(original)
    ed.goto(ed.pos("paragraph_value = 1;", len("paragraph_value = 1;")))
    ed.type(" para")
    check("completion: the list still opens by itself inside a code block", until(listed, 1.5))
    ed.sci(AUTOCCANCEL)
    ed.undo_to(original)

    # Pair glow.
    glow = lambda at: ed.sci(INDICATORVALUEAT, PAIR, at)
    open_, close = ed.pos("**bold"), ed.pos("text**", 4)
    ed.goto(ed.pos("bold text", 3))
    ok = until(lambda: glow(open_) and glow(open_ + 1) and glow(close) and glow(close + 1))
    check("pair glow: both ends of bold light up", ok and not glow(open_ + 2) and not glow(open_ - 1))
    time.sleep(0.3)
    snapshot(ed.main, os.path.join(WORK, "pair-glow.png"))
    ed.goto(ed.pos("`code`", 3))
    ok = until(lambda: glow(ed.pos("`code`")) and glow(ed.pos("`code`", 5)) and not glow(open_))
    check("pair glow: moves to the code span", ok)
    ed.goto(ed.pos("[a link]", 3))
    ok = until(lambda: glow(ed.pos("[a link]")) and glow(ed.pos("](#tasks)")) and glow(ed.pos("](#tasks)", 8)))
    check("pair glow: a link lights its bracket and its address", ok)
    ed.goto(ed.pos("Second paragraph", 3))
    check("pair glow: off outside markup", until(lambda: not glow(ed.pos("[a link]")) and not glow(open_)))

    # Paragraph focus.
    dim = lambda at: ed.sci(INDICATORVALUEAT, DIM, at)
    first, second, last = ed.pos("First paragraph", 2), ed.pos("Second paragraph", 2), ed.pos("Filler paragraph number 40", 2)
    ok = until(lambda: dim(first) and not dim(second) and dim(last) and dim(0))
    check("paragraph focus: everything but the caret's paragraph is dimmed", ok, f"{dim(first)} {dim(second)} {dim(last)}")
    ed.goto(first)
    check("paragraph focus: follows the caret", until(lambda: not dim(first) and dim(second) and dim(last)))
    before = ed.text()
    ed.goto(ed.pos("end.\n", 4))
    ed.type(" More")
    ok = until(lambda: "end. More" in ed.text()) and not dim(first) and not dim(ed.pos("More", 2)) and dim(ed.pos("Second paragraph", 2))
    check("paragraph focus: typing keeps the paragraph lit", ok)
    ed.undo_to(before)

    # Glides.
    line_start = ed.pos("Second paragraph")
    ed.goto(line_start)
    time.sleep(0.5)
    rows = watch_glide(pid, ed, lambda: ed.goto(line_start + 30))
    xs = [shown[0][0] for _, _, _, shown in rows if len(shown) == 1 and shown[0][2] <= 20]
    during = [style for _, style, _, shown in rows if shown]
    ok = len(set(xs)) >= 4 and xs == sorted(xs) and rows[-1][3] == [] and rows[-1][1] == 1 and during.count(0) >= 0.8 * len(during) > 0
    span = [t for t, _, _, shown in rows if shown]
    check(
        "gliding cursor: a stand-in travels and the editor's caret returns",
        ok,
        f"{len(set(xs))} positions x {xs[0] if xs else '-'}..{xs[-1] if xs else '-'}, shown {span[0] * 1000:.0f}-{span[-1] * 1000:.0f} ms, own caret off in {during.count(0)} of {len(during)} samples, back on at the end: {rows[-1][1] == 1}" if span else "never shown",
    )
    # Typing moves the caret one character at a time.
    time.sleep(0.3)
    rows = watch_glide(pid, ed, lambda: ed.type("xyz"), 0.5)
    xs = [shown[0][0] for _, _, _, shown in rows if len(shown) == 1 and shown[0][2] <= 20]
    typed = until(lambda: ed.text() != original)
    ok = typed and len(set(xs)) >= 3 and xs == sorted(xs) and rows[-1][3] == [] and rows[-1][1] == 1
    check(
        "gliding cursor: glides while typing and the editor's caret returns",
        ok,
        f"{len(set(xs))} positions x {xs[0] if xs else '-'}..{xs[-1] if xs else '-'}, back on at the end: {rows[-1][1] == 1}",
    )
    ed.undo_to(original)
    ed.goto(line_start + 30)
    time.sleep(0.4)
    rows = watch_glide(pid, ed, lambda: ed.goto(ed.pos("3. three")))
    ys = [max(shown, key=lambda s: s[2])[1] for _, _, _, shown in rows if shown and max(s[2] for s in shown) > 100]
    off = [is_set for _, _, is_set, shown in rows if shown and max(s[2] for s in shown) > 100]
    ok = len(set(ys)) >= 4 and ys == sorted(ys) and rows[-1][3] == [] and rows[-1][2] == 1 and off.count(0) >= 0.8 * len(off) > 0
    check(
        "active line glide: a band travels and the editor's highlight returns",
        ok,
        f"{len(set(ys))} positions y {ys[0] if ys else '-'}..{ys[-1] if ys else '-'}, own highlight off in {off.count(0)} of {len(off)} samples, back on at the end: {rows[-1][2] == 1}",
    )
    return original


EFFECTS = {
    "glide_caret": True, "glide_line": True, "heat": True,
    "pair_glow": True, "paragraph_focus": True, "smart_typing": True,
}


def launch(name, doc, settings, loaded):
    """Start the editor on `doc` with a fresh profile. Returns the process
    and the editor once `loaded(editor)` holds."""
    profile = os.path.join(WORK, name)
    os.makedirs(profile)
    with open(os.path.join(profile, "settings.json"), "w", encoding="utf-8") as f:
        json.dump(settings, f)
    env = dict(os.environ)
    env["LINEWORK_PROFILE"] = profile
    if not os.path.isdir(os.path.join(os.path.dirname(EXE), "runtime")):
        env["LINEWORK_RUNTIME"] = os.path.join(REPO, "runtime")
    env["WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS"] = f"--remote-debugging-port={PORT}"
    si = subprocess.STARTUPINFO()
    si.dwFlags = subprocess.STARTF_USESHOWWINDOW
    si.wShowWindow = 4  # shown, not activated
    proc = subprocess.Popen([EXE, doc], env=env, startupinfo=si)
    try:
        main = until(lambda: (windows_of(proc.pid, "LineworkMain") or [None])[0], 15)
        assert main, "the editor window did not appear"
        # The window exists a moment before its editor panes do.
        hwnd = until(lambda: user32.GetDlgItem(main, 700), 10)
        assert hwnd, "the editor pane did not appear"
        ed = Editor(main, hwnd)
        assert until(lambda: loaded(ed), 30), "the document did not load"
        thread = user32.GetWindowThreadProcessId(main, None)
        user32.AttachThreadInput(kernel32.GetCurrentThreadId(), thread, True)
        user32.SetFocus(hwnd)
        time.sleep(0.3)
        return proc, ed
    except BaseException:
        proc.kill()
        raise


def calm_pass(doc):
    """Reduce motion: the caret and the line highlight move at once."""
    settings = {"sidebar": False, "editor_effects": EFFECTS, "preview_cues": {"calm": True}}
    proc, ed = launch("calm", doc, settings, lambda ed: ed.sci(GETLENGTH) == len(DOC.encode()))
    try:
        start = ed.pos("Second paragraph")
        ed.goto(start)
        time.sleep(0.4)
        rows = watch_glide(proc.pid, ed, lambda: ed.goto(start + 30), 0.3)
        rows += watch_glide(proc.pid, ed, lambda: ed.goto(ed.pos("3. three")), 0.3)
        quiet = all(not shown and style == 1 and is_set == 1 for _, style, is_set, shown in rows)
        check(
            "reduce motion: no glide, the editor's own caret and highlight throughout",
            quiet and ed.caret() == ed.pos("3. three"),
            f"{len(rows)} samples",
        )
    finally:
        proc.kill()


def large_pass():
    """A file past the 24 MiB paging limit still opens and takes typing
    with every editor effect switched on."""
    big = os.path.join(WORK, "big.md")
    block = ("A paragraph of **bold** and `code` words to make the file large.\n" * 6 + "\n") * 2000
    with open(big, "w", encoding="utf-8", newline="\n") as f:
        for _ in range(33):
            f.write(block)
    size = os.path.getsize(big)
    proc, ed = launch("large", big, {"sidebar": False, "editor_effects": EFFECTS}, lambda ed: ed.sci(GETLENGTH) > 1000)
    try:
        paged = ed.sci(GETLENGTH) < size
        ed.goto(ed.sci(GETLENGTH) // 2)
        time.sleep(0.3)
        before = ed.sci(GETLENGTH)
        clock = time.perf_counter()
        ed.type("typed")
        done = until(lambda: ed.sci(GETLENGTH) == before + 5, 5, 0.001)
        took = time.perf_counter() - clock
        alive = proc.poll() is None
        check(
            "large file: a paged document takes typing with every effect on",
            paged and alive and done,
            f"{size / 2**20:.0f} MiB file, {ed.sci(GETLENGTH) / 1024:.0f} KiB in the pane, five characters in {took * 1000:.0f} ms",
        )
    finally:
        proc.kill()
        os.remove(big)


async def run():
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)
    doc = os.path.join(WORK, "qa.md")
    with open(doc, "w", encoding="utf-8", newline="\n") as f:
        f.write(DOC)
    settings = {"document_preview": True, "preview_width": 520, "sidebar": False, "editor_effects": EFFECTS}
    proc, ed = launch("profile", doc, settings, lambda ed: ed.sci(GETLENGTH) == len(DOC.encode()))
    main = ed.main
    kept_clipboard = None
    try:
        original = source_checks(proc.pid, ed)

        # The preview.
        targets = until(lambda: preview_targets(), 20, 0.3)
        assert targets, "the preview's debugging port did not answer"
        async with websockets.connect(targets[0]["webSocketDebuggerUrl"], max_size=None) as ws:
            page = Page(ws)
            ready = await page.until("return D && D.body && D.body.innerText.includes('QA document');", 15)
            check("preview is showing the document", ready)

            ed.goto(ed.pos("in a row.", len("in a row.")))
            ed.type(" fresh")
            ok = await page.until("return marks('lw-ink').some(t => t.includes('fresh'));", 1.5)
            await page.shot("fresh-ink.png")
            check("fresh ink: typed text is painted warm in the preview", ok, str(await page.js("return marks('lw-ink');")))
            gone = await page.until("return marks('lw-ink').length === 0;", 3)
            check("fresh ink: settles back to plain text", gone)

            a = ed.pos("selection and deletion")
            ed.select(a, a + len("selection and deletion"))
            ok = await page.until("return marks('lw-sel').join('') === 'selection and deletion';", 2)
            await page.shot("mirror.png")
            check("selection mirror: the source selection is lit in the preview", ok, str(await page.js("return marks('lw-sel');")))
            ed.key(VK_DELETE)
            ok = await page.until("return [...D.querySelectorAll('.lw-ghost')].some(g => g.textContent.includes('selection and deletion'));", 1.5)
            await page.shot("ghost.png")
            check("deletion ghost: removed words linger struck through", ok)
            ok = await page.until("return D.querySelectorAll('.lw-ghost').length === 0 && marks('lw-sel').length === 0;", 3)
            check("deletion ghost and mirror: both clear afterwards", ok)
            ed.sci(UNDO)
            until(lambda: "selection and deletion" in ed.text())

            # Paste, through Ctrl+V and through Shift+Insert.
            kept_clipboard = clipboard_text()
            if kept_clipboard is None:
                check("change echo: paste (skipped, the clipboard holds something that is not text)", False)
            else:
                for name, press in (("Ctrl+V", lambda: ed.key(0x56, ctrl=True)), ("Shift+Insert", lambda: ed.key(VK_INSERT, shift=True))):
                    set_clipboard("pasted words")
                    spot = ed.pos("Second paragraph for", len("Second paragraph for"))
                    ed.goto(spot)
                    press()
                    landed = until(lambda: "forpasted words" in ed.text())
                    boxed = until(lambda: ed.sci(INDICATORVALUEAT, FLASH, spot + 2) and ed.sci(INDICATORVALUEAT, FLASH, spot + 11), 1.5)
                    washed = await page.until("return marks('lw-echo').some(t => t.includes('pasted words'));", 1.5)
                    await page.shot("paste-echo.png")
                    snapshot(main, os.path.join(WORK, "paste-source.png"))
                    check(f"change echo: {name} boxes the pasted text in the source", landed and boxed)
                    check(f"change echo: {name} washes the pasted text in the preview", washed, str(await page.js("return marks('lw-echo');")))
                    cleared = until(lambda: not ed.sci(INDICATORVALUEAT, FLASH, spot + 2), 3)
                    check(f"change echo: {name} source box fades", cleared)
                    ed.sci(UNDO)
                    until(lambda: "forpasted" not in ed.text())
                set_clipboard(kept_clipboard)
                kept_clipboard = None

            # Task boxes.
            await asyncio.sleep(0.4)
            spot = await page.js(
                "const b = [...D.querySelectorAll('input[type=checkbox]')].find(b => !b.checked); if (!b) return null;"
                "const r = b.getBoundingClientRect(), f = F.getBoundingClientRect();"
                "return [f.left + r.left + r.width / 2, f.top + r.top + r.height / 2, b.disabled];"
            )
            if spot:
                await page.click(spot[0], spot[1])
            ok = until(lambda: "- [x] task open" in ed.text())
            check("task box: ticking it in the preview edits the source", ok, str(spot))
            ed.sci(UNDO)
            check("task box: Undo in the source takes the tick back", until(lambda: "- [ ] task open" in ed.text()))

            # Click to source.
            await asyncio.sleep(0.4)
            spot = await page.js("return rectOf('several');")
            want = ed.pos("several")
            ed.goto(0)
            await page.click(spot[0], spot[1])
            ok = until(lambda: want <= ed.caret() <= want + 7)
            row_start = ed.pos("Second paragraph")
            flashed = until(lambda: ed.sci(INDICATORVALUEAT, FLASH, row_start), 0.9, 0.005)
            check("click to source: the caret lands inside the clicked word", ok, f"caret {ed.caret()}, word {want}")
            check("click to source: the landing line flashes", flashed)

            # A double-click selects the word and marks nothing.
            ed.goto(0)
            await asyncio.sleep(0.8)
            await page.move(spot[0], spot[1])
            for count in (1, 2):
                for kind in ("mousePressed", "mouseReleased"):
                    await page.call("Input.dispatchMouseEvent", type=kind, x=spot[0], y=spot[1], button="left", clickCount=count)
                await asyncio.sleep(0.06)
            flashed = until(lambda: ed.sci(INDICATORVALUEAT, FLASH, row_start), 0.6, 0.005)
            check("double-click in the preview: selects that word in the source", until(lambda: ed.selected() == "several"), ed.selected())
            check("double-click in the preview: nothing flashes", not flashed)

            # Peek.
            spot = await page.js("return rectOf('a link');")
            await page.move(spot[0] - 40, spot[1] - 30)
            await page.move(spot[0], spot[1])
            text = await page.until("const p = document.getElementById('peek'); return p.classList.contains('on') && p.innerText;", 1.5)
            check("peek: a link to a heading shows that section", bool(text) and "Tasks" in text, repr(text)[:90])
            spot = await page.js("return rectOf('outside');")
            await page.move(spot[0], spot[1] - 60)
            await page.move(spot[0], spot[1])
            text = await page.until("const p = document.getElementById('peek'); return p.classList.contains('on') && p.innerText.includes('example.com') && p.innerText;", 1.5)
            check("peek: an outside link shows its address", bool(text), repr(text)[:90])
            spot = await page.js("return rectOf('item one');")
            await page.move(spot[0], spot[1] - 80)
            await page.move(spot[0], spot[1], ctrl=True)
            await page.move(spot[0] + 2, spot[1], ctrl=True)
            text = await page.until("const p = document.getElementById('peek'); return p.classList.contains('on') && p.innerText.includes('- item one') && p.innerText;", 2)
            await page.shot("peek.png")
            check("peek: Ctrl+hover shows a block's Markdown", bool(text), repr(text)[:90])
            await page.move(5, 5)

            # Follow.
            await page.js("W.scrollTo(0, 0); return 1;")
            ed.goto(ed.sci(GETLENGTH))
            await asyncio.sleep(0.7)
            stay = await page.js("return W.scrollY;")
            check("follow cursor off: the preview keeps its own place", stay == 0, f"scrollY {stay}")
            await page.js("document.getElementById('fx-follow').click(); return 1;")
            await asyncio.sleep(0.3)
            ed.goto(0)
            await asyncio.sleep(0.3)
            ed.goto(ed.sci(GETLENGTH))
            moved = await page.until("return W.scrollY > 200 && W.scrollY;", 2.5)
            check("follow cursor on: the preview scrolls to the source caret", bool(moved), f"scrollY {moved}")
            await page.js("document.getElementById('fx-follow').click(); return 1;")


        # Edit heat, read from the gutter markers. A mark takes ten
        # seconds to cool away.
        heat = lambda row: ed.sci(MARKERGET, row) & HEAT
        at = ed.pos("in a row.", len("in a row."))
        row = ed.line(at)
        ed.goto(at)
        ed.type(" Hot")
        warmed = time.perf_counter()
        check("edit heat: an edited line gets the warmest gutter mark", until(lambda: heat(row) == 0b10), bin(heat(row)))
        ed.goto(0)
        ed.key(VK_RETURN)
        ok = until(lambda: heat(row + 1) == 0b10 and heat(0) and heat(1))
        check("edit heat: the mark stays with its line when lines are added above", ok, f"{bin(heat(row))} {bin(heat(row + 1))}")
        hot_row = row + 1

        cool = until(lambda: heat(hot_row) not in (0, 0b10), 4.5)
        check("edit heat: the mark cools in steps", cool, f"{bin(heat(hot_row))} at {time.perf_counter() - warmed:.1f} s")
        time.sleep(max(0, 10.4 - (time.perf_counter() - warmed)))
        check("edit heat: gone after ten seconds", until(lambda: heat(hot_row) == 0, 1.5), f"{bin(heat(hot_row))} at {time.perf_counter() - warmed:.1f} s")

        ed.goto(ed.pos("bold text", 3))
        ed.type("!")
        time.sleep(0.4)
        snapshot(main, os.path.join(WORK, "editor.png"))
        check("editor's own caret and line highlight are on at the end", ed.sci(GETCARETSTYLE) == 1 and ed.sci(GETELEMENTISSET, 50) == 1)
    finally:
        if kept_clipboard is not None:
            set_clipboard(kept_clipboard)
        proc.kill()
    with open(doc, "w", encoding="utf-8", newline="\n") as f:
        f.write(DOC)
    calm_pass(doc)
    large_pass()
    failed = [name for name, ok, _ in results if not ok]
    print(f"\n{len(results) - len(failed)} of {len(results)} checks passed")
    for name in failed:
        print("  failed: " + name)
    return 1 if failed else 0


def preview_targets():
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{PORT}/json", timeout=1) as r:
            return [t for t in json.load(r) if t.get("type") == "page" and t.get("webSocketDebuggerUrl")]
    except Exception:
        return None


if __name__ == "__main__":
    sys.exit(asyncio.run(run()))
