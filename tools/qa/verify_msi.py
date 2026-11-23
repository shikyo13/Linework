"""Check that an installer holds exactly the staged release tree.

    python tools/qa/verify_msi.py dist/Linework dist/Linework-Setup-X-x64.msi

Unpacks the installer into a temporary folder, without installing it, and
compares the hash of every payload file with the staged one.
"""
import hashlib
import subprocess
import sys
from pathlib import Path
from tempfile import TemporaryDirectory

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "installer"))
import build


def digest(path):
    with open(path, "rb") as reader:
        return hashlib.file_digest(reader, "sha256").hexdigest()


def main():
    app, msi = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()
    with TemporaryDirectory(prefix="linework-msi-check-") as temp:
        subprocess.run(["msiexec.exe", "/a", str(msi), "/qn", "/norestart", f"TARGETDIR={temp}"], check=True, timeout=120)
        unpacked = list(Path(temp).rglob("Linework.exe"))
        assert len(unpacked) == 1, unpacked
        tree = unpacked[0].parent
        files = build.payload(app)
        wrong = [str(p) for p in files if digest(tree / p) != digest(app / p)]
        assert not wrong, wrong
        print(f"{len(files)} payload files match; Linework.exe {digest(unpacked[0])}")


if __name__ == "__main__":
    main()
