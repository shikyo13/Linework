#!/bin/sh
# Puts the macOS program into an application bundle.
#
#   tools/mac-app.sh PROGRAM SCINTILLA_SOURCE OUTPUT_FOLDER
#
# PROGRAM is the `linework` binary built at the repository root. SCINTILLA_SOURCE is the
# unpacked Scintilla archive, for the two pointer images Scintilla looks for in
# the bundle. The result is OUTPUT_FOLDER/Linework.app, not yet signed.
set -eu
[ $# -eq 3 ] || { echo "usage: $0 PROGRAM SCINTILLA_SOURCE OUTPUT_FOLDER" >&2; exit 2; }
program=$1
images=$2/cocoa/res
here=$(cd "$(dirname "$0")/.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$here/Cargo.toml" | head -1)
app=$3/Linework.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$program" "$app/Contents/MacOS/Linework"
for image in mac_cursor_busy mac_cursor_flipped; do
    tiffutil -cathidpicheck "$images/$image.png" "$images/$image@2x.png" -out "$app/Contents/Resources/$image.tiff" 2>/dev/null
done
mkdir -p "$app/Contents/Resources/fonts"
cp "$here"/assets/mac/fonts/*.ttf "$here"/assets/mac/fonts/NOTICE.txt "$app/Contents/Resources/fonts/"
cp "$here/assets/mac/Linework.icns" "$app/Contents/Resources/Linework.icns"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Linework</string>
    <key>CFBundleDisplayName</key><string>Linework</string>
    <key>CFBundleExecutable</key><string>Linework</string>
    <key>CFBundleIconFile</key><string>Linework</string>
    <key>CFBundleIdentifier</key><string>io.github.shikyo13.Linework</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key><string>Text</string>
            <key>CFBundleTypeRole</key><string>Editor</string>
            <key>LSHandlerRank</key><string>Alternate</string>
            <key>LSItemContentTypes</key>
            <array><string>public.text</string><string>public.data</string></array>
        </dict>
    </array>
</dict>
</plist>
PLIST
echo "$app"
