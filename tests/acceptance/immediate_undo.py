"""Exercise the shipping Save/Undo shortcuts back to back on an unseen desktop."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
from pathlib import Path
import tempfile
import time

from linework_desktop_check import App, c, msg, post, wait


def digest(data):
    return hashlib.sha256(data).hexdigest()


def run(exe, paged):
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        profile = root / "profile"
        profile.mkdir()
        (profile / "settings.json").write_text(json.dumps({"shortcuts": {
            "save": "F13", "undo": "F14", "redo": "F15", "new": "F16"}}))
        path = root / "document.txt"
        prefix = b"Linework\r\n"
        original = prefix + (b"ordinary row\r\n" * (2000000 if paged else 64))
        edited = b"!" + original
        path.write_bytes(original)
        app = App(exe, root, profile, [path])
        try:
            app.ready(path.name)
            wait("loaded original", lambda: app.text(1) == "L")
            msg(app.ed, 2160, 0, 0)
            post(app.ed, c.WM_CHAR, ord("!"), 1)
            wait("edit", lambda: app.text(2) == "!L")
            app.key(13)
            app.key(14)
            wait("save unlocked", lambda: msg(app.ed, 2140, 0, 0) == 0 and path.stat().st_size == len(edited))
            # The disk still contains the first save; Undo changes the editor only.
            saved_hash = digest(path.read_bytes())
            end = time.monotonic() + 2
            while app.text(2) != "Li" and time.monotonic() < end:
                time.sleep(.02)
            undone = app.text(2) == "Li"
            app.key(13)
            if undone:
                wait("restored file length", lambda: path.stat().st_size == len(original))
            wait("second save unlocked", lambda: msg(app.ed, 2140, 0, 0) == 0)
            restored_hash = digest(path.read_bytes())
            redo_exact = None
            other_tab_exact = None
            if undone:
                app.key(15)
                wait("redo survived save", lambda: app.text(2) == "!L")
                app.key(13)
                wait("redone file length", lambda: path.stat().st_size == len(edited))
                redo_exact = digest(path.read_bytes()) == digest(edited)
                wait("redo save unlocked", lambda: msg(app.ed, 2140, 0, 0) == 0)
                app.key(13)
                app.key(14)
                app.key(13)
                app.key(16)
                wait("new empty tab", lambda: msg(app.ed, 2006, 0, 0) == 0)
                wait("save after queued Undo on the original tab", lambda: path.stat().st_size == len(original))
                other_tab_exact = digest(path.read_bytes()) == digest(original) and msg(app.ed, 2006, 0, 0) == 0
            return dict(mode="paged" if paged else "ordinary", bytes=len(original),
                        expected_saved=digest(edited), saved=saved_hash,
                        expected_restored=digest(original), restored=restored_hash,
                        undo_restored_editor=undone, redo_saved_exact=redo_exact,
                        queued_resave_with_other_tab=other_tab_exact,
                        passed=undone and restored_hash==digest(original) and saved_hash==digest(edited) and redo_exact and other_tab_exact)
        finally:
            app.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("exe", type=Path)
    parser.add_argument("receipt", type=Path)
    args = parser.parse_args()
    exe = args.exe.resolve(strict=True)
    results = []
    with ThreadPoolExecutor(max_workers=1) as worker:
        for paged in (False, True):
            results.append(worker.submit(run, exe, paged).result())
    result = dict(executable_sha256=digest(exe.read_bytes()), cases=results)
    args.receipt.write_text(json.dumps(result, indent=2))
    print(json.dumps(result))

    if not all(case['passed'] for case in results):
        raise SystemExit(1)
