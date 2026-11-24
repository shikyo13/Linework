"""Build the per-user Windows x64 MSI from a release tree with WiX Toolset 5."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tomllib
import uuid
import xml.etree.ElementTree as ET

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
NS = "http://wixtoolset.org/schemas/v4/wxs"
GUID_NAMESPACE = uuid.UUID("0b4c21bf-0b4a-46e6-9e14-cecf823d0950")
DOCS = (
    "USER-GUIDE.md", "NOTEPAD-PLUGINS.md", "LICENSE",
)
ADDITIONAL_TYPES = """
asc asciidoc adoc bashrc bib ccproj cl cmakelist coffee config cshtml csv cu cuh
cuhpp dotenv editorconfig env erb eslintrc fish frag fsproj gitattributes gitconfig
gitignore glsl gpl graphql gql haml hlsl hlsli htaccess htmx inc info inl j2
jinja jinja2 json5 kconfig ld less lock markdown mdx mkdn ngc npmrc nuspec
patch pcss pem po pot pro proto psm1 pxd pxi pyx qml raku rbw rc reg rst sass
sbt scala scheme sed sln slnx srt styl sublime-build sublime-color-scheme
sublime-commands sublime-completions sublime-keymap sublime-macro sublime-menu
sublime-project sublime-settings sublime-snippet sublime-syntax sublime-theme
sublime-workspace svelte t4 tf tfvars tsconfig tsv vcl vert vue wgsl xlf xliff
xquery xsd xsl xslt yaml yml zshrc
""".split()

# These double-click actions run a command, import system settings, open a
# solution, or mount a disk. Explorer's "Edit with Linework" covers them.
SHELL_ACTION_TYPES = {"bat", "cmd", "reg", "sln", "slnx", "vbs", "vhd"}


def extensions():
    languages = json.loads((ROOT / "crates/editor/assets/languages.json").read_text(encoding="utf8"))
    names = set(ADDITIONAL_TYPES)
    for language in languages:
        names.update(language["extensions"])
    names.difference_update(SHELL_ACTION_TYPES)
    for name in names:
        if not re.fullmatch(r"[a-z0-9][a-z0-9_-]*", name):
            raise ValueError(f"Unexpected file extension {name!r}")
    return sorted("." + name for name in names)


def guid(name):
    return str(uuid.uuid5(GUID_NAMESPACE, name.casefold())).upper()


def identifier(prefix, path):
    return prefix + hashlib.sha256(path.casefold().encode("utf8")).hexdigest()[:24]


def payload(app):
    files = [Path("Linework.exe"), *(Path(name) for name in DOCS)]
    files += sorted(p.relative_to(app) for p in (app / "runtime").rglob("*")
                    if p.is_file() and "__pycache__" not in p.parts and p.suffix != ".pyc")
    for required in ("runtime/Scintilla.dll", "runtime/Lexilla.dll", "runtime/python/python.exe",
                     "runtime/python/python314.zip", "runtime/sublime/host.py"):
        if Path(required) not in files:
            raise FileNotFoundError(app / required)
    for rel in files:
        if not (app / rel).is_file():
            raise FileNotFoundError(app / rel)
    return files


def build_fragment(app, output):
    # Identifiers and component GUIDs depend on the installed path, never the build checkout.
    wix = ET.Element("Wix", xmlns=NS)
    fragment = ET.SubElement(wix, "Fragment")
    directories = {Path("."): ET.SubElement(fragment, "DirectoryRef", Id="INSTALLFOLDER")}
    files = payload(app)
    parents = {p for rel in files for p in rel.parents if p != Path(".")}
    for rel in sorted(parents, key=lambda p: (len(p.parts), p.as_posix())):
        directories[rel] = ET.SubElement(directories[rel.parent], "Directory",
                                         Id=identifier("D", rel.as_posix()), Name=rel.name)
    installed_dirs = {Path("."): "INSTALLFOLDER"}
    installed_dirs.update({p: element.attrib["Id"] for p, element in directories.items() if p != Path(".")})
    group = ET.SubElement(fragment, "ComponentGroup", Id="EditorFiles")
    for rel in files:
        path = rel.as_posix()
        component = ET.SubElement(group, "Component", Directory=installed_dirs[rel.parent],
                                  Id=identifier("C", path), Guid=guid("file/" + path))
        file_id = "LineworkExe" if path == "Linework.exe" else identifier("F", path)
        ET.SubElement(component, "File", Id=file_id, Source=str(app / rel), Name=rel.name)
        ET.SubElement(component, "RegistryValue", Root="HKCU", Key=r"Software\Linework\Installer\Payload",
                      Name=identifier("F", path), Type="integer", Value="1", KeyPath="yes")
    folders = ET.SubElement(group, "Component", Id="EditorFolders", Directory="INSTALLFOLDER", Guid=guid("folders"))
    ET.SubElement(folders, "RegistryValue", Root="HKCU", Key=r"Software\Linework\Installer",
                  Name="Folders", Type="integer", Value="1", KeyPath="yes")
    for rel, directory in installed_dirs.items():
        ET.SubElement(folders, "RemoveFolder", Id=identifier("R", rel.as_posix()), Directory=directory, On="uninstall")

    associations = ET.SubElement(fragment, "ComponentGroup", Id="FileTypes")
    for ext in extensions():
        component = ET.SubElement(associations, "Component", Directory="INSTALLFOLDER",
                                  Id=identifier("A", ext), Guid=guid("association/" + ext))
        ET.SubElement(component, "RegistryValue", Root="HKCU", Key=r"Software\Linework\Capabilities\FileAssociations",
                      Name=ext, Type="string", Value="Linework.Document", KeyPath="yes")
        ET.SubElement(component, "RegistryValue", Root="HKCU", Key=rf"Software\Classes\{ext}\OpenWithProgids",
                      Name="Linework.Document", Type="string", Value="")
        ET.SubElement(component, "RegistryValue", Root="HKCU", Key=r"Software\Classes\Applications\Linework.exe\SupportedTypes",
                      Name=ext, Type="string", Value="")
    ET.indent(wix)
    ET.ElementTree(wix).write(output, encoding="utf-8", xml_declaration=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", type=Path, default=ROOT, help="release tree containing Linework.exe and runtime")
    parser.add_argument("--output", type=Path, required=True, help="destination .msi file")
    parser.add_argument("--ui-extension", help="path to WixToolset.UI.wixext.dll, or the installed WiX extension name",
                        default="WixToolset.UI.wixext")
    args = parser.parse_args()
    app, output = args.app.resolve(), args.output.resolve()
    version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf8"))["package"]["version"]
    wix = shutil.which("wix")
    if not wix:
        parser.error("WiX Toolset 5 and its UI extension are required")
    reported = subprocess.check_output([wix, "--version"], text=True, encoding="utf8").strip()
    if not reported.startswith("5."):
        parser.error(f"Expected WiX Toolset 5, found {reported}")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="linework-msi-") as temp:
        scratch = Path(temp)
        generated = scratch / "ReleaseFiles.wxs"
        build_fragment(app, generated)
        package = scratch / "Linework.msi"
        subprocess.run([wix, "build", "-arch", "x64", "-culture", "en-US",
                        "-ext", args.ui_extension, "-d", "AppVersion=" + version,
                        "-d", "ProductCode=" + guid("product/x64/user/" + version),
                        "-d", "ProductRoot=" + str(ROOT),
                        "-intermediatefolder", str(scratch / "obj"), "-pdbtype", "none",
                        "-o", str(package), str(HERE / "Linework.wxs"), str(generated)], check=True)
        # Replace only after compilation succeeds, including when output is on another volume.
        with tempfile.NamedTemporaryFile(dir=output.parent, suffix=".msi", delete=False) as staged:
            tmp = Path(staged.name)
        try:
            shutil.copyfile(package, tmp)
            tmp.replace(output)
        finally:
            tmp.unlink(missing_ok=True)
    print(f"{output}  ({len(extensions())} registered file types)")


if __name__ == "__main__":
    main()
