#!/bin/sh
# Builds Scintilla and Lexilla as static libraries for the macOS build of Linework.
#
#   tools/mac-libs.sh SCINTILLA_SOURCE LEXILLA_SOURCE OUTPUT_FOLDER
#
# The two source folders are the unpacked archives from https://www.scintilla.org/
# (scintilla569.tgz and lexilla555.tgz match the DLLs in runtime/). The output folder
# receives libscintilla.a and liblexilla.a; point LINEWORK_MAC_LIBS at it when building.
set -eu
[ $# -eq 3 ] || { echo "usage: $0 SCINTILLA_SOURCE LEXILLA_SOURCE OUTPUT_FOLDER" >&2; exit 2; }
scintilla=$(cd "$1" && pwd)
lexilla=$(cd "$2" && pwd)
mkdir -p "$3"
out=$(cd "$3" && pwd)
jobs=$(sysctl -n hw.perflevel0.logicalcpu 2>/dev/null || sysctl -n hw.ncpu)
common="-c -O2 -DNDEBUG -std=c++17 -arch arm64 -mmacosx-version-min=11.0 -fvisibility-inlines-hidden -w"

build() {
    name=$1 flags=$2
    shift 2
    objects="$out/objects-$name"
    rm -rf "$objects"
    mkdir -p "$objects"
    printf '%s\n' "$@" | xargs -P "$jobs" -n 1 sh -c \
        'file=${3##*/}; exec clang++ $1 "$3" -o "$2/${file%.*}.o"' sh "$common $flags" "$objects"
    rm -f "$out/lib$name.a"
    libtool -static -o "$out/lib$name.a" "$objects"/*.o
    rm -rf "$objects"
}

# One change to Scintilla, made in a copy: a font's height above and below the
# line is rounded up, as Scintilla's Windows build does, not to the nearest whole.
# With the nearest whole, Cascadia Mono at 11 points sets lines 21 apart where
# Windows sets them 22 apart.
changed="$out/changed"
rm -rf "$changed"
mkdir -p "$changed"
sed -e 's/return ascent + 0\.5f;/return std::ceil(ascent);/' -e 's/return descent + 0\.5f;/return std::ceil(descent);/' \
    "$scintilla/cocoa/PlatCocoa.mm" > "$changed/PlatCocoa.mm"
[ "$(grep -c 'return std::ceil(\(ascent\|descent\));' "$changed/PlatCocoa.mm")" -eq 2 ] ||
    { echo "$0: PlatCocoa.mm is not as expected; the font height change was not made" >&2; exit 1; }
sources=$(ls "$scintilla"/src/*.cxx "$scintilla"/cocoa/*.mm | grep -v '/cocoa/PlatCocoa\.mm$')
# shellcheck disable=SC2086
build scintilla "-fobjc-arc -I$scintilla/include -I$scintilla/src -I$scintilla/cocoa" $sources "$changed/PlatCocoa.mm"
rm -rf "$changed"
build lexilla "-I$lexilla/include -I$lexilla/lexlib -I$scintilla/include" \
    "$lexilla"/src/Lexilla.cxx "$lexilla"/lexlib/*.cxx "$lexilla"/lexers/*.cxx
cat "$scintilla/version.txt" > "$out/scintilla-version.txt"
cat "$lexilla/version.txt" > "$out/lexilla-version.txt"
ls -l "$out"/*.a
