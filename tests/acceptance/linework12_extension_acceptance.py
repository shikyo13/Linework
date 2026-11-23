"""Test unchanged text-editing extensions on a fully written UTF-8 file using a private Windows desktop.

Dependencies: Python 3.11+, pywin32, Pillow, psutil, and sibling linework_desktop_check.py.
Run with --help for paths. The default fixture is over 4 GiB. Use an empty scratch directory
with at least 16 GiB free. Only files below a newly created temporary subdirectory are modified.
"""

import argparse
import base64
import ctypes
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import zipfile

import psutil
from linework_desktop_check import App, c, g, msg, post, wait, win32api


MIB = 1 << 20
PREFIX_ROW = (
    b"INFO request complete path=/sample cost_us=1142 ".ljust(254, b".") + b"\r\n"
)
UNICODE_ROW = "INFO reçu e\u0301 日本語 🦊".encode().ljust(254, b".") + b"\r\n"
BLOCK = UNICODE_ROW + PREFIX_ROW * 4095
RECORD = "part café 🦊 e\u0301 ready\r\n".encode()
PAYLOAD = b"LW_BEGIN:" + RECORD * 20000 + b"!END_LW"
TAIL = b"\r\nAFTER_RANGE visible sentinel; preserve exactly.\r\n"
QUERY = r"(?s)LW_BEGIN:.*?END_LW"
COMMANDS = {
    "mime": ("MIME Tools", "Base64 Encode", "notepad++", "base64 encode"),
    "converter": ("Converter", "ASCII -> HEX", "notepad++", "ascii -> hex"),
    "python": (
        "StringEncode",
        "StringEncode: Base64 Encode",
        "sublime",
        "stringencode: base64 encode",
    ),
}


def sha_file(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def describe_bytes(data):
    return {
        "bytes": len(data),
        "sha256": hashlib.sha256(data).hexdigest(),
        "first_32_hex": data[:32].hex(),
        "last_32_hex": data[-32:].hex(),
    }


def summary_plugin(path, src, dest):
    shutil.copytree(path.parent, dest / path.stem)
    info = win32api.GetFileVersionInfo(str(path), "\\")
    ms, ls = info["ProductVersionMS"], info["ProductVersionLS"]
    return {
        "source": src,
        "file": path.name,
        "sha256": sha_file(path),
        "version": ".".join(map(str, [ms >> 16, ms & 65535, ls >> 16, ls & 65535])),
    }


def install_stringencode(path, dest):
    info = {
        "source": "https://github.com/colinta/SublimeStringEncode",
        "name": "SublimeStringEncode",
    }
    if path.is_file():
        info["archive_sha256"] = sha_file(path)
        with zipfile.ZipFile(path) as archive:
            info["archive_comment"] = archive.comment.decode("utf-8", "replace")
            plugin = next(
                (
                    PurePosixPath(x)
                    for x in archive.namelist()
                    if PurePosixPath(x).name == "string_encode.py"
                ),
                None,
            )
            if plugin is None:
                raise ValueError(
                    "The archive does not contain the unmodified StringEncode module"
                )
            for entry in archive.infolist():
                member = PurePosixPath(entry.filename)
                if entry.is_dir() or not member.is_relative_to(plugin.parent):
                    continue
                rel = member.relative_to(plugin.parent)
                if any(
                    p in ("", ".", "..") or ":" in p or "\\" in p for p in rel.parts
                ):
                    raise ValueError(f"Invalid package path {entry.filename}")
                output = dest.joinpath(*rel.parts)
                output.parent.mkdir(parents=True, exist_ok=True)
                with archive.open(entry) as source, output.open("wb") as target:
                    shutil.copyfileobj(source, target)
    else:
        shutil.copytree(
            path, dest, ignore=shutil.ignore_patterns(".git", "__pycache__", "*.pyc")
        )
    info["module_sha256"] = sha_file(dest / "string_encode.py")
    manifest = [
        (p.relative_to(dest).as_posix(), sha_file(p))
        for p in sorted(dest.rglob("*"))
        if p.is_file()
    ]
    info["package_manifest_sha256"] = hashlib.sha256(
        json.dumps(manifest, separators=(",", ":")).encode()
    ).hexdigest()
    return info


def materialize(path, prefix_mib):
    assert len(BLOCK) == MIB
    original_prefix = hashlib.sha256()
    with path.open("xb") as f:
        for _ in range(prefix_mib):
            f.write(BLOCK)
            original_prefix.update(BLOCK)
        f.write(PAYLOAD)
        f.write(TAIL)
        f.flush()
        os.fsync(f.fileno())

    def expected(payload):
        h = original_prefix.copy()
        h.update(payload)
        h.update(TAIL)
        return h.hexdigest()

    attributes = path.stat().st_file_attributes
    assert not attributes & 0x200, "Unexpected sparse fixture"
    assert not attributes & 0x800, "Unexpected compressed fixture"
    start = prefix_mib * MIB
    original = expected(PAYLOAD)
    assert path.stat().st_size == start + len(PAYLOAD) + len(TAIL)
    assert sha_file(path) == original
    return {
        "encoding": "UTF-8, CRLF",
        "bytes": path.stat().st_size,
        "sha256": original,
        "fully_written": True,
        "windows_file_attributes": attributes,
        "prefix_mib": prefix_mib,
        "prefix_unit": "1 MiB block: one Unicode and 4,095 ASCII 256-byte records",
        "range_start_byte": start,
        "range_end_byte_exclusive": start + len(PAYLOAD),
        "range_start_unicode_scalar": len(BLOCK.decode()) * prefix_mib,
        "range_end_unicode_scalar_exclusive": len(BLOCK.decode()) * prefix_mib
        + len(PAYLOAD.decode()),
        "selection": describe_bytes(PAYLOAD),
        "payload_record_count": 20000,
        "payload_record_utf8_hex": RECORD.hex(),
        "suffix": describe_bytes(TAIL),
    }, expected


class TreeMemory:
    """Sample all participating processes. Include OS lifetime peaks to catch peaks between samples."""

    def __init__(self, pid, interval):
        self.pid, self.interval = pid, interval
        self.root = psutil.Process(pid)
        self.phase = "startup"
        self.stopped = threading.Event()
        self.lock = threading.Lock()
        self.phases, self.processes = {}, {}
        self.last, self.max_gap, self.failure = None, 0, None
        self.thread = threading.Thread(
            target=self.poll, name="process-tree-memory", daemon=True
        )
        self.thread.start()

    def set(self, name):
        with self.lock:
            self.phase = name

    def poll(self):
        try:
            while not self.stopped.is_set():
                begin = time.monotonic()
                if self.last is not None:
                    self.max_gap = max(self.max_gap, begin - self.last)
                self.last = begin
                with self.lock:
                    phase = self.phase
                total_rss = total_private = 0
                cpu_delta = 0.0
                for process in [self.root, *self.root.children(recursive=True)]:
                    try:
                        with process.oneshot():
                            mem = process.memory_info()
                            name = process.name()
                            cpu = process.cpu_times()
                    except psutil.NoSuchProcess:
                        continue
                    total_rss += mem.rss
                    total_private += mem.private
                    item = self.processes.setdefault(
                        str(process.pid),
                        {"image": name, "os_peak_ws": 0, "os_peak_pagefile": 0},
                    )
                    cpu_delta += max(
                        0.0,
                        cpu.user
                        + cpu.system
                        - item.get("user_cpu_s", 0)
                        - item.get("kernel_cpu_s", 0),
                    )
                    item.update(user_cpu_s=cpu.user, kernel_cpu_s=cpu.system)
                    item["os_peak_ws"] = max(item["os_peak_ws"], mem.peak_wset)
                    item["os_peak_pagefile"] = max(
                        item["os_peak_pagefile"], mem.peak_pagefile
                    )
                stats = self.phases.setdefault(
                    phase,
                    {"samples": 0, "sampled_peak_ws": 0, "sampled_peak_private": 0},
                )
                stats["estimated_process_cpu_s"] = round(
                    stats.get("estimated_process_cpu_s", 0) + cpu_delta, 6
                )
                stats.update(
                    samples=stats["samples"] + 1,
                    last_ws=total_rss,
                    last_private=total_private,
                    sampled_peak_ws=max(stats["sampled_peak_ws"], total_rss),
                    sampled_peak_private=max(
                        stats["sampled_peak_private"], total_private
                    ),
                )
                self.stopped.wait(
                    max(0.001, self.interval - (time.monotonic() - begin))
                )
        except Exception as error:
            self.failure = repr(error)

    def finish(self):
        self.stopped.set()
        self.thread.join(10)
        assert not self.thread.is_alive()
        if self.failure:
            raise AssertionError("Process-tree monitoring failed: " + self.failure)
        assert self.phases
        return {
            "target_interval_ms": round(self.interval * 1000),
            "largest_sample_gap_ms": round(self.max_gap * 1000, 2),
            "metrics": "Sampled sum of private committed bytes and working sets across editor and descendants; OS-reported peaks per process since launch. Sums of working sets may count shared pages twice.",
            "phases": self.phases,
            "processes": self.processes,
            "total_observed_process_cpu_s": sum(
                p["user_cpu_s"] + p["kernel_cpu_s"] for p in self.processes.values()
            ),
            "aggregate_sampled_peak_private": max(
                s["sampled_peak_private"] for s in self.phases.values()
            ),
            "aggregate_sampled_peak_ws": max(
                s["sampled_peak_ws"] for s in self.phases.values()
            ),
        }


def status(app, field):
    return g.GetWindowText(g.GetDlgItem(app.h, field))


def positions(app):
    found = re.search(r"Byte ([\d,]+) of ([\d,]+)", status(app, 621))
    if not found:
        return None
    return tuple(int(s.replace(",", "")) for s in found.groups())


def visible_form(app, title):
    def ready():
        dialog = app.find("LineworkDialog", title)
        return dialog if dialog and g.IsWindowVisible(dialog) else None

    return wait(title, ready)


def open_document(app, path):
    app.key(18, app.h)
    dialog = wait("open-file chooser", lambda: app.find("#32770"))

    def filename():
        edits = []
        g.EnumChildWindows(
            dialog,
            lambda h, _: (
                edits.append(h)
                if g.GetClassName(h) == "Edit" and g.IsWindowVisible(h)
                else None
            ),
            None,
        )
        return next(iter(edits), 0)

    msg(wait("file name field", filename), c.WM_SETTEXT, 0, str(path))
    post(g.GetDlgItem(dialog, 1), c.BM_CLICK, 0, 0)
    app.ready(path.name)

    def visible_editor():
        views = []
        g.EnumChildWindows(
            app.h,
            lambda h, _: (
                views.append(h)
                if g.GetClassName(h) == "Scintilla" and g.IsWindowVisible(h)
                else None
            ),
            None,
        )
        return views[0] if len(views) == 1 else 0

    app.ed = wait("one visible editing surface", visible_editor)
    wait(
        "global document length", lambda: positions(app) == (0, path.stat().st_size), 45
    )
    # Exercise the application's accessible form for enabling the regex search option.
    app.key(19)
    opts = visible_form(app, "Find options")
    msg(g.GetDlgItem(opts, 212), c.BM_SETCHECK, 1, 0)
    app.accept(opts)


def choose(app, term, ecosystem, expected):
    app.key(13)
    d = visible_form(app, "Command palette")
    app.field(d, 500, term)
    post(g.GetDlgItem(d, 500), c.WM_KEYUP, c.VK_END, 0)
    items = g.GetDlgItem(d, 501)

    def match():
        for i in range(msg(items, c.LB_GETCOUNT, 0, 0)):
            buf = ctypes.create_unicode_buffer(msg(items, c.LB_GETTEXTLEN, i, 0) + 1)
            msg(items, c.LB_GETTEXT, i, ctypes.addressof(buf))
            parts = buf.value.lower().split("\t")
            if len(parts) >= 2 and parts[0] == ecosystem:
                labels = [s.strip() for s in re.split("[/·]", parts[1])]
                if expected in labels:
                    return i, buf.value
        return None

    index, caption = wait(f"extension palette action {term}", match, 10)
    msg(items, c.LB_SETCURSEL, index, 0)
    app.accept(d)
    return caption


def select_range(app, start, total):
    app.key(16)
    d = visible_form(app, "Go to")
    app.field(d, 100, f"@{start - 512}")
    app.accept(d)
    wait("requested global byte", lambda: positions(app) == (start - 512, total))
    app.key(17)
    box = g.GetDlgItem(app.h, 610)
    wait("Find bar visible", lambda: g.IsWindowVisible(box))
    msg(box, c.WM_SETTEXT, 0, QUERY)
    post(box, c.WM_KEYDOWN, c.VK_RETURN, 1)
    wait(
        "full-file matched selection",
        lambda: "selected" in status(app, 620) and positions(app) == (start, total),
        45,
    )
    post(box, c.WM_KEYDOWN, c.VK_ESCAPE, 1)
    wait("Find bar dismissed", lambda: not g.IsWindowVisible(box))
    pos_global = positions(app)[0]
    pos_local = msg(app.ed, 2008, 0, 0)
    page_start = pos_global - pos_local
    visible = msg(app.ed, 2006, 0, 0)
    page_end = page_start + visible
    lo, hi = msg(app.ed, 2143, 0, 0), msg(app.ed, 2145, 0, 0)
    assert page_start <= start < page_end < start + len(PAYLOAD), (
        page_start,
        start,
        page_end,
        len(PAYLOAD),
    )
    assert (page_start + lo, page_start + hi) == (start, page_end)
    assert bytes(msg(app.ed, 2007, lo + i, 0) & 255 for i in range(9)) == PAYLOAD[:9]
    return {
        "selection_method": "Go to byte offset, Find with regular expression, then close Find",
        "global_caret": pos_global,
        "physical_page_base": page_start,
        "physical_page_end_exclusive": page_end,
        "visible_page_bytes": visible,
        "locally_projected_selection_bytes": hi - lo,
        "confirmed_offpage_selected_bytes": len(PAYLOAD) - (hi - lo),
        "selection_status": status(app, 620),
    }


def bytes_for(command):
    if command == "converter":
        output = bytearray()
        for pos in range(0, len(PAYLOAD), 16):
            line = PAYLOAD[pos : pos + 16]
            output.extend(line.hex().upper().encode("ascii"))
            if len(line) == 16:
                output.extend(b"\r\n")
        return bytes(output)
    encoded = base64.b64encode(PAYLOAD)
    return encoded.rstrip(b"=") if command == "mime" else encoded


def save(app, path, total):
    app.key(15)

    def finished():
        try:
            written_size = path.stat().st_size
        except FileNotFoundError:
            return False
        return (
            written_size == total
            and not g.GetWindowText(app.h).endswith(" · Unsaved — Linework")
            and not msg(app.ed, 2140, 0, 0)
        )

    wait("save finished", finished, 150)


def exercise(app, path, fixture, expected, memory, command):
    title, term, ecosystem, expected_caption = COMMANDS[command]
    start, total = fixture["range_start_byte"], fixture["bytes"]
    memory.set(command + " selection")
    record = select_range(app, start, total)
    print(
        f"{title}: selection crosses the displayed page at {record['physical_page_end_exclusive']:,} bytes",
        flush=True,
    )
    after = bytes_for(command)
    record.update(
        command=title,
        replacement=describe_bytes(after),
        expected_output_sha256=expected(after),
    )
    memory.set(command + " execute")
    begin = time.monotonic()
    record["palette_caption"] = choose(app, term, ecosystem, expected_caption)
    modified = total - len(PAYLOAD) + len(after)
    try:
        wait(
            "length-changing edit",
            lambda: (
                positions(app)
                and positions(app)[1] == modified
                and g.GetWindowText(app.h).endswith(" · Unsaved — Linework")
            ),
            150,
        )
    except AssertionError:
        print(
            "Editor status:",
            status(app, 620),
            status(app, 621),
            "expected length",
            modified,
            flush=True,
        )
        raise
    record["elapsed_palette_to_edit_s"] = round(time.monotonic() - begin, 3)
    memory.set(command + " save")
    save(app, path, modified)
    memory.set(command + " verify saved file")
    with path.open("rb") as s:
        s.seek(start)
        assert s.read(len(after)) == after and s.read() == TAIL
    record["saved_sha256"] = sha_file(path)
    assert record["saved_sha256"] == record["expected_output_sha256"]
    memory.set(command + " undo")
    app.key(14)
    wait(
        "one native Undo restores file length",
        lambda: positions(app) and positions(app)[1] == total,
        30,
    )
    memory.set(command + " save after undo")
    save(app, path, total)
    memory.set(command + " verify undo")
    record["restored_sha256"] = sha_file(path)
    assert record["restored_sha256"] == fixture["sha256"]
    print(
        f"PASS {title} at byte {start:,}; {record['confirmed_offpage_selected_bytes']:,} selected bytes outside active page; edited and restored full SHA-256",
        flush=True,
    )
    return record


def run(args):
    started = datetime.now(timezone.utc).isoformat()
    args.scratch.mkdir(exist_ok=True, parents=True)
    total = args.prefix_mib * MIB + len(PAYLOAD) + len(TAIL)
    if shutil.disk_usage(args.scratch).free < total * 3 + MIB * 300:
        raise RuntimeError(
            "Insufficient scratch space for three fully materialized file versions"
        )
    result = {
        "started_utc": started,
        "executable_sha256": sha_file(args.exe),
        "platform": platform.platform(),
        "physical_memory_bytes": psutil.virtual_memory().total,
        "private_desktop": True,
        "commands": [],
        "plugins": {},
        "status": "incomplete",
    }
    with tempfile.TemporaryDirectory(dir=args.scratch, prefix="lw12-") as temporary:
        root = Path(temporary)
        profile = root / "profile"
        profile.mkdir()
        (profile / "settings.json").write_text(
            json.dumps(
                {
                    "shortcuts": {
                        "palette": "F13",
                        "undo": "F14",
                        "save": "F15",
                        "goto": "F16",
                        "find": "F17",
                        "open": "F18",
                        "findoptions": "F19",
                    }
                }
            )
        )
        for name, path, url in [
            ("mime", args.mime, "https://github.com/npp-plugins/mimetools"),
            ("converter", args.converter, "https://github.com/npp-plugins/converter"),
        ]:
            if name in args.commands:
                result["plugins"][name] = summary_plugin(path, url, profile / "plugins")
        if "converter" in args.commands:
            cfg = profile / "plugins/Config/converter.ini"
            cfg.parent.mkdir(exist_ok=True)
            cfg.write_bytes(
                b"[ascii2Hex]\r\ninsertSpace=0\r\nuppercase=1\r\nnbCharPerLine=16\r\n"
            )
            result["plugins"]["converter"]["settings"] = (
                "Explicit upstream defaults: uppercase contiguous hex codes, CRLF after every 16 input bytes including a final complete line"
            )
        if "python" in args.commands:
            result["plugins"]["python"] = install_stringencode(
                args.stringencode, profile / "Packages/SublimeStringEncode"
            )
        path = root / "boundary.txt"
        fixture, expected = materialize(path, args.prefix_mib)
        result["fixture"] = fixture
        print(
            f"Fixture verified: {fixture['bytes']:,} bytes; selection {len(PAYLOAD):,} bytes at {fixture['range_start_byte']:,}",
            flush=True,
        )
        app = App(args.exe, root, profile, [])
        memory = TreeMemory(app.pid, args.interval_ms / 1000)
        try:
            if "python" in args.commands:
                bundled_python = (
                    args.exe.parent / "runtime/python/python.exe"
                ).resolve(strict=True)
                wait(
                    "bundled CPython helper",
                    lambda: any(
                        "python" in p.name().lower()
                        and Path(p.exe()).resolve() == bundled_python
                        for p in psutil.Process(app.pid).children(recursive=True)
                    ),
                    20,
                )
            memory.set("baseline with extensions, no file")
            time.sleep(1)
            memory.set("open file and index")
            open_document(app, path)
            time.sleep(0.25)
            for command in args.commands:
                result["commands"].append(
                    exercise(app, path, fixture, expected, memory, command)
                )
            result["status"] = "passed"
        except Exception as error:
            result["error"] = repr(error)
            raise
        finally:
            try:
                metrics = memory.finish()
                result["process_tree_memory"] = metrics
                if fixture["bytes"] > 4 * 1024**3:
                    # Sum of lifetime high-water marks is conservative since peaks may occur at different times.
                    private_bound = sum(
                        p["os_peak_pagefile"] for p in metrics["processes"].values()
                    )
                    metrics["sum_per_process_lifetime_peak_commit"] = private_bound
                    if (
                        private_bound >= fixture["bytes"]
                        and result["status"] == "passed"
                    ):
                        result["status"] = "failed"
                        result["error"] = "Process-tree commit reached the fixture size"
                args.output.parent.mkdir(parents=True, exist_ok=True)
                args.output.write_text(
                    json.dumps(result, indent=2, ensure_ascii=False) + "\n", "utf-8"
                )
                if result["status"] == "failed":
                    raise AssertionError(result["error"])
            finally:
                app.close()
    print("Results:", args.output, flush=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--exe",
        required=True,
        type=Path,
        help="Extracted release Linework.exe, next to its runtime folder",
    )
    ap.add_argument(
        "--mime",
        type=Path,
        help="Path to unmodified mimeTools.dll in its original plugin folder",
    )
    ap.add_argument(
        "--converter",
        type=Path,
        help="Path to unmodified NppConverter.dll in its original plugin folder",
    )
    ap.add_argument(
        "--stringencode",
        type=Path,
        help="Path to unchanged StringEncode repo folder or ZIP",
    )
    ap.add_argument(
        "--commands",
        default=["mime", "converter", "python"],
        nargs="+",
        choices=list(COMMANDS),
    )
    ap.add_argument("--scratch", required=True, type=Path)
    ap.add_argument("--output", required=True, type=Path)
    ap.add_argument(
        "--prefix-mib",
        type=int,
        default=4098,
        help="Use 64 for a smaller paged control run, 4098 crosses 4 GiB",
    )
    ap.add_argument("--interval-ms", type=int, default=25)
    args = ap.parse_args()
    if args.prefix_mib < 25 or args.interval_ms < 5:
        ap.error(
            "Use a prefix of at least 25 MiB and a sampling interval of at least 5 ms"
        )
    for key in ["exe", "scratch", "output", "mime", "converter", "stringencode"]:
        value = getattr(args, key)
        if value is not None:
            setattr(args, key, value.resolve())
    for name, key in [
        ("mime", "mime"),
        ("converter", "converter"),
        ("python", "stringencode"),
    ]:
        if name in args.commands and not getattr(args, key):
            ap.error(f"--{key} is required for {name}")
    with ThreadPoolExecutor(1) as pool:
        pool.submit(run, args).result()


if __name__ == "__main__":
    main()
