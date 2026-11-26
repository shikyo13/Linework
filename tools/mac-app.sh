#!/bin/sh
# Puts the macOS program into an application bundle.
#
#   tools/mac-app.sh PROGRAM SCINTILLA_SOURCE OUTPUT_FOLDER
#
# PROGRAM is the `linework` binary built at the repository root. SCINTILLA_SOURCE is the
# unpacked Scintilla archive, for the two pointer images Scintilla looks for in
# the bundle. The result is OUTPUT_FOLDER/Linework.app, unsigned. For a signed app,
# build mac/Linework.xcodeproj, which calls this and lets Xcode do the signing.
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
cp "$here"/assets/mac/fonts/*.ttf "$here"/assets/mac/fonts/NOTICE.txt "$here"/assets/mac/fonts/OFL.txt "$app/Contents/Resources/fonts/"
cp "$here/assets/mac/Linework.icns" "$app/Contents/Resources/Linework.icns"
# The package host's Python files (the Python itself is the Mac's own), and
# the licences of what the program is built with.
mkdir -p "$app/Contents/Resources/runtime/sublime"
cp "$here"/runtime/sublime/*.py "$app/Contents/Resources/runtime/sublime/"
cp -R "$here/runtime/licenses" "$app/Contents/Resources/runtime/licenses"
# The same Info.plist the Xcode project uses, with its settings filled in.
sed -e 's/$(EXECUTABLE_NAME)/Linework/' -e 's/$(PRODUCT_BUNDLE_IDENTIFIER)/com.zerosecurity.linework/' \
    -e "s/\$(MARKETING_VERSION)/$version/" -e 's/$(MACOSX_DEPLOYMENT_TARGET)/12.0/' \
    "$here/mac/Info.plist" > "$app/Contents/Info.plist"
echo "$app"
