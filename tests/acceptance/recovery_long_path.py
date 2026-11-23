"""Verify replacement of a long-path recovery manifest and crash restoration of its draft."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import ctypes
import hashlib
import json
from pathlib import Path
import tempfile
import time

from linework_desktop_check import App, c, g, msg, post, wait


def sha(data):
    return hashlib.sha256(data).hexdigest()


def units(path):
    return len(str(path).encode("utf-16-le")) // 2


def status(app):
    buf = ctypes.create_unicode_buffer(1024)
    msg(g.GetDlgItem(app.h, 620), c.WM_GETTEXT, len(buf), ctypes.addressof(buf))
    return buf.value


def published(app, manifest, previous):
    end = time.monotonic() + 12
    while time.monotonic() < end:
        if manifest.is_file():
            for tab in json.loads(manifest.read_bytes()):
                rec = tab.get("recovery")
                if rec and rec != previous and Path(rec).is_file():
                    return rec
        current = status(app)
        if "Recovery checkpoint:" in current:
            raise AssertionError(current)
        time.sleep(.04)
    raise AssertionError("No new dirty checkpoint. Status: " + status(app))


def append(app, suffix):
    end = msg(app.ed, 2006, 0, 0)
    msg(app.ed, 2160, end, end)
    for character in suffix:
        post(app.ed, c.WM_CHAR, ord(character), 1)


def scenario(exe, root, record):
    profile = root / "profiles"
    while units(profile) <= 280:
        profile /= "recovery profile \u03a9 with spaces"
    profile.mkdir(parents=True)
    (profile / "settings.json").write_bytes(b"{}")
    source = root / "original.txt"
    baseline = "Unchanged on disk.\r\n"
    first = baseline + "Draft one."
    expected = first + " Recovered \u03a9."
    source.write_bytes(baseline.encode())
    record.update(profile_utf16_units=units(profile), original_sha256=sha(source.read_bytes()),
                  expected_draft_sha256=sha(expected.encode()), manifest_published=False,
                  manifest_replaced=False, restored_draft_sha256=None)
    try:
        app = App(exe, root, profile, [source])
        try:
            app.ready(source.name)
            wait("baseline text", lambda: app.text() == baseline)
            session = next(p for p in (profile / "sessions").iterdir() if p.name.endswith(f"-{app.pid}"))
            manifest = session / "recovery.json"
            record["manifest_utf16_units"] = units(manifest)
            append(app, "Draft one.")
            wait("first edit", lambda: app.text() == first)
            initial = published(app, manifest, None)
            record["manifest_published"] = True
            append(app, " Recovered \u03a9.")
            wait("second edit", lambda: app.text() == expected)
            latest = published(app, manifest, initial)
            record.update(manifest_replaced=True, draft_path_utf16_units=units(latest), status=status(app))
        finally:
            app.close()  # terminate only this fixture's editor, after the second checkpoint
        app = App(exe, root, profile, [])
        try:
            dialog = wait("recovery prompt", lambda: app.find("#32770", "Recover last session"))
            post(dialog, c.WM_COMMAND, c.IDYES, g.GetDlgItem(dialog, c.IDYES))
            app.ready(source.name)
            wait("restored draft", lambda: app.text() == expected)
            record["restored_draft_sha256"] = sha(app.text().encode())
        finally:
            app.close()
    finally:
        record["original_after_sha256"] = sha(source.read_bytes())
    assert record["original_after_sha256"] == record["original_sha256"]
    assert record["restored_draft_sha256"] == record["expected_draft_sha256"]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("exe", type=Path)
    parser.add_argument("receipt", type=Path)
    parser.add_argument("--scratch", type=Path)
    parser.add_argument("--expect-path-error", action="store_true")
    args = parser.parse_args()
    exe = args.exe.resolve(strict=True)
    record = {"executable_sha256": sha(exe.read_bytes()), "passed": False}
    with tempfile.TemporaryDirectory(dir=args.scratch) as tmp:
        try:
            scenario(exe, Path(tmp).resolve(), record)
            record["passed"] = True
        except Exception as exc:
            record["failure"] = str(exc)
            record["status"] = str(exc)
    args.receipt.write_text(json.dumps(record, indent=2), encoding="utf-8")
    print(json.dumps(record, ensure_ascii=True), flush=True)
    if args.expect_path_error:
        return (not record["passed"] and "Recovery checkpoint:" in record.get("failure", "")
                and record.get("original_after_sha256") == record.get("original_sha256"))
    return record["passed"]


if __name__ == "__main__":
    with ThreadPoolExecutor(max_workers=1) as worker:
        raise SystemExit(0 if worker.submit(main).result() else 1)
