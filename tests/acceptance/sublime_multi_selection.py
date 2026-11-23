"""Private-desktop end-to-end: original Python packages editing distributed selections."""

from linework_desktop_check import *
from pathlib import Path
from concurrent.futures import ThreadPoolExecutor
import zipfile, shutil, os, base64, threading, hashlib, ctypes, time, psutil
import argparse

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument(
    "--exe", required=True, type=Path, help="Path to an extracted Linework.exe"
)
parser.add_argument(
    "--stringencode",
    required=True,
    type=Path,
    help="Unmodified SublimeStringEncode source archive",
)
parser.add_argument(
    "--caseconversion",
    required=True,
    type=Path,
    help="Unmodified CaseConversion source archive",
)
parser.add_argument(
    "--scratch", required=True, type=Path, help="Scratch directory with 12 GiB free"
)
parser.add_argument(
    "--small",
    action="store_true",
    help="Use a 31 MiB file instead of the 3 GiB fixture",
)
args = parser.parse_args()
exe = args.exe.resolve()
root = args.scratch.resolve()
root.mkdir(parents=True, exist_ok=True)


def digest(path):
    with path.open("rb", buffering=0) as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


def changed_hash(path, starts, old, new):
    h = hashlib.sha256()
    with path.open("rb") as f:

        def copy(n):
            while n:
                b = f.read(min(4 << 20, n))
                if not b:
                    raise EOFError
                h.update(b)
                n -= len(b)

        for at in starts:
            copy(at - f.tell())
            assert f.read(len(old)) == old
            h.update(new)
        copy(path.stat().st_size - f.tell())
    return h.hexdigest()


def install(profile):
    packages = profile / "Packages"
    packages.mkdir(parents=True)
    for archive, name in [
        (args.stringencode, "SublimeStringEncode"),
        (args.caseconversion, "CaseConversion"),
    ]:
        dest = (packages / name).resolve()
        with zipfile.ZipFile(archive) as z:
            for member in z.infolist():
                rel = member.filename.partition("/")[2]
                if not rel or member.is_dir():
                    continue
                path = (dest / rel).resolve()
                if not path.is_relative_to(dest) or path == dest:
                    raise ValueError(f"Invalid package member: {member.filename}")
                path.parent.mkdir(parents=True, exist_ok=True)
                with z.open(member) as src, path.open("wb") as out:
                    shutil.copyfileobj(src, out)
    probes = packages / "BridgeTest"
    probes.mkdir()
    (probes / "probe.py").write_text(
        """import sublime,sublime_plugin,time
from pathlib import Path
class ProbeRejectCommand(sublime_plugin.TextCommand):
    def run(self,edit):
        r=self.view.sel()[0]
        self.view.replace(edit,r,'PARTIAL')
        assert self.view.substr(sublime.Region(r.begin(),r.begin()+7))=='PARTIAL'
        raise ValueError('Failed intentionally after edit')
class ProbeStaleCommand(sublime_plugin.TextCommand):
    def run(self,edit):
        self.view.insert(edit,0,'STALE')
        Path(self.view.file_name()+'.pending').write_text('pending')
        time.sleep(.7)
class ProbeOversizeCommand(sublime_plugin.TextCommand):
    def run(self,edit):
        self.view.insert(edit,0,'INVALID')
        self.view.sel().clear()
        self.view.sel().add_all(sublime.Region(i*2) for i in range(65537))
class ProbePointsCommand(sublime_plugin.TextCommand):
    def run(self,edit):
        r=max(self.view.sel(),key=lambda r:r.end())
        text=self.view.substr(r)
        assert text=='caféName\\\\nTwoWords'.replace('\\\\n','\\n'),repr(text)
        row,col=self.view.rowcol(r.begin())
        assert self.view.text_point(row,col)==r.begin()
        assert self.view.line(r).contains(r)
        assert len(self.view.lines(r))==2
        assert self.view.find('TwoWords',r.begin(),sublime.LITERAL).end()==r.end()
        assert self.view.substr(self.view.word(r.end()-1)) .startswith('TwoWords')
        assert self.view.scope_name(r.begin()).startswith('text.plain')
        sublime.status_message('Paged coordinate checks passed')
""",
        encoding="utf-8",
    )
    # Preserve a literal newline escape in the generated Python command.
    f = probes / "probe.py"
    s = f.read_text("utf-8")
    a = s.index("        assert text==")
    b = s.index("        row,col", a)
    f.write_text(
        s[:a]
        + "        assert text=="
        + repr("caféName\nTwoWords")
        + ",repr(text)\n"
        + s[b:],
        encoding="utf-8",
    )


def run():
    small = args.small
    with tempfile.TemporaryDirectory(dir=root, prefix="py-large-") as t:
        fixture = Path(t)
        profile = fixture / "profile"
        profile.mkdir()
        install(profile)
        (profile / "settings.json").write_text(
            json.dumps(
                {
                    "shortcuts": {
                        "palette": "F13",
                        "selmatches": "F14",
                        "save": "F15",
                        "undo": "F16",
                        "goto": "F17",
                    }
                }
            )
        )
        file = fixture / "records.txt"
        token = "caféName\nTwoWords".encode()
        before = len("caféName\n".encode())
        remote = 30 * (1 << 20) if small else 3 * (1 << 30)
        starts = [4096 - before, 384 * 1024 - before, remote - before]
        size = remote + 1024 * 1024
        line = b"2026-11-21 category=fixture; fields=payload".ljust(127, b" ") + b"\n"
        chunk = line * 32768
        with file.open("wb") as f:
            remain = size
            while remain:
                n = min(len(chunk), remain)
                f.write(chunk[:n])
                remain -= n
            f.seek(0)
            f.write("Δ🦊".encode())
            for offset in starts:
                f.seek(offset)
                f.write(token)
        expected = {
            txt: changed_hash(file, starts, token, txt)
            for txt in [base64.b64encode(token), b"caf_name_two_words"]
        }
        original = digest(file)
        app = App(exe, fixture, profile, [file])
        proc = psutil.Process(app.pid)
        stopped = threading.Event()
        peaks = {}
        recent = {}
        pypath = [""]

        def sample():
            while not stopped.is_set():
                try:
                    for ps in [proc, *proc.children()]:
                        alias = "editor" if ps.pid == proc.pid else "python"
                        if alias == "python":
                            pypath[0] = ps.exe()
                        m = ps.memory_info()
                        row = {"rss": m.rss, "private": m.private}
                        recent[alias] = row
                        prior = peaks.setdefault(alias, dict(row))
                        for k, n in row.items():
                            prior[k] = max(prior[k], n)
                except psutil.Error:
                    pass
                stopped.wait(0.018)

        th = threading.Thread(target=sample)
        th.start()
        left = g.GetDlgItem(app.h, 620)
        right = g.GetDlgItem(app.h, 621)

        def label():
            return g.GetWindowText(left)

        def palette(query):
            app.key(13)
            d = wait(
                "package palette", lambda: app.find("LineworkDialog", "Command palette")
            )
            app.field(d, 500, query)
            post(g.GetDlgItem(d, 500), c.WM_KEYUP, c.VK_END, 0)
            control = g.GetDlgItem(d, 501)

            def row():
                for i in range(msg(control, c.LB_GETCOUNT, 0, 0)):
                    buf = ctypes.create_unicode_buffer(
                        msg(control, c.LB_GETTEXTLEN, i, 0) + 1
                    )
                    msg(control, c.LB_GETTEXT, i, ctypes.addressof(buf))
                    if (
                        query.lower() in buf.value.lower()
                        and "sublime" in buf.value.lower()
                    ):
                        return i + 1

            idx = wait(query, row) - 1
            msg(control, c.LB_SETCURSEL, idx, 0)
            app.accept(d)

        def selected():
            post(app.ed, c.WM_KEYDOWN, c.VK_ESCAPE, 1)
            wait(
                "global ranges cleared",
                lambda: "3 carets" not in g.GetWindowText(right),
                10,
            )
            msg(app.ed, 2160, starts[0], starts[0] + len(token))
            app.key(14)
            wait(
                "three nonadjacent global ranges",
                lambda: (
                    "3 carets" in g.GetWindowText(right) or "3 selections" in label()
                ),
                150,
            )

        def file_size():
            try:
                return file.stat().st_size
            except FileNotFoundError:
                return -1

        try:
            app.ready(file.name)
            wait(
                "indexed and first viewport loaded",
                lambda: app.text(20).startswith("Δ🦊"),
                15,
            )
            time.sleep(1.4)
            visible = msg(app.ed, 2006, 0, 0)
            assert starts[1] < visible < starts[1] + len(token), (visible, starts)
            print(
                "First viewport ends",
                visible,
                "inside edit",
                starts[1],
                starts[1] + len(token),
                "distant edit",
                starts[2],
                flush=True,
            )
            selected()
            palette("Probe Points")
            wait(
                "Unicode positions, line, word and search",
                lambda: "coordinate checks passed" in label(),
                35,
            )
            print(
                "PASS distant Unicode rowcol, text_point, line, lines, word, scope and search",
                flush=True,
            )
            palette("Probe Reject")
            wait(
                "rollback on package exception",
                lambda: "Failed intentionally" in label(),
                20,
            )
            verify(
                "rollback original page",
                app.text(5000).encode()[starts[0] : starts[0] + len(token)],
                token,
            )
            palette("Probe Oversize")
            wait(
                "too many regions rejected atomically", lambda: "65,536" in label(), 30
            )
            assert app.text(8).startswith("Δ🦊")
            print(
                "PASS failed selection projection discarded all package edits",
                flush=True,
            )
            for q, out in [
                ("StringEncode: Base64 Encode", base64.b64encode(token)),
                ("Convert Case: snake_case", b"caf_name_two_words"),
            ]:
                selected()
                palette(q)
                wait(
                    "package transaction " + q,
                    lambda: (
                        app.text(5000).encode()[starts[0] : starts[0] + len(out)] == out
                    ),
                    30,
                )
                app.key(15)
                wait(
                    "saved edited length",
                    lambda: file_size() == size + 3 * (len(out) - len(token)),
                    120,
                )
                wait("saved", lambda: not g.GetWindowText(app.h).endswith(" · Unsaved — Linework"), 60)
                verify(q + " full-file SHA-256", digest(file), expected[out])
                app.key(16)
                wait(
                    "undo first range",
                    lambda: (
                        app.text(5000).encode()[starts[0] : starts[0] + len(token)]
                        == token
                    ),
                )
                app.key(15)
                wait("original length", lambda: file_size() == size, 120)
                wait(
                    "saved undo", lambda: not g.GetWindowText(app.h).endswith(" · Unsaved — Linework"), 60
                )
                verify(q + " one undo full-file SHA-256", digest(file), original)
            post(app.ed, c.WM_KEYDOWN, c.VK_ESCAPE, 1)
            time.sleep(0.2)
            msg(app.ed, 2160, 0, 0)
            palette("StringEncode: Base64 Encode")
            wait("unbounded Python string refused", lambda: "8 MiB" in label(), 30)
            verify(
                "whole buffer request left tab clean",
                g.GetWindowText(app.h).endswith(" · Unsaved — Linework"),
                False,
            )
            palette("Probe Stale")
            wait(
                "package edit before delay",
                lambda: Path(str(file) + ".pending").is_file(),
            )
            post(app.ed, c.WM_CHAR, ord("!"), 1)
            wait("stale result rejected", lambda: "changed" in label(), 10)
            assert app.text(8).startswith("!Δ") and "STALE" not in app.text(20)
            print(
                "PASS concurrent user edit preserved and stale command rejected",
                flush=True,
            )
            print(
                json.dumps(
                    {
                        "file_bytes": size,
                        "offsets": starts,
                        "original_sha256": original,
                        "peaks": peaks,
                        "python_image": pypath[0],
                    }
                ),
                flush=True,
            )
        except Exception:
            print(
                "FAIL state",
                label(),
                g.GetWindowText(right),
                g.GetWindowText(app.h),
                app.text(42),
                peaks,
                flush=True,
            )
            raise
        finally:
            stopped.set()
            th.join()
            app.close()


with ThreadPoolExecutor(max_workers=1) as e:
    e.submit(run).result()
