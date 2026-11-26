#!/bin/sh
# Makes assets/mac/Linework.icns from assets/mac/icon.svg, in every size macOS asks for.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
mkdir "$work/Linework.iconset"
sips -s format png "$here/assets/mac/icon.svg" --out "$work/base.png" >/dev/null
for size in 16 32 128 256 512; do
    sips -z $size $size "$work/base.png" --out "$work/Linework.iconset/icon_${size}x${size}.png" >/dev/null
    sips -z $((size * 2)) $((size * 2)) "$work/base.png" --out "$work/Linework.iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$work/Linework.iconset" -o "$here/assets/mac/Linework.icns"
rm -rf "$work"
