#!/bin/sh
# Run by the Xcode project: builds the program with cargo and puts it, with its
# resources, where Xcode expects the app's contents. Xcode then signs the app.
set -eu
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$PATH"
[ -n "${LINEWORK_MAC_LIBS:-}" ] || { echo "error: set LINEWORK_MAC_LIBS in mac/Local.xcconfig (see tools/mac-libs.sh)" >&2; exit 1; }
[ -n "${LINEWORK_SCINTILLA:-}" ] || { echo "error: set LINEWORK_SCINTILLA in mac/Local.xcconfig" >&2; exit 1; }
export LINEWORK_MAC_LIBS
cd "$SRCROOT/.."
if [ "$CONFIGURATION" = Debug ]; then profile=debug; cargo build; else profile=release; cargo build --release; fi
built="${CARGO_TARGET_DIR:-$SRCROOT/../target}/$profile/linework"
stage="$DERIVED_FILE_DIR/stage"
tools/mac-app.sh "$built" "$LINEWORK_SCINTILLA" "$stage" >/dev/null
contents="$BUILT_PRODUCTS_DIR/$CONTENTS_FOLDER_PATH"
mkdir -p "$contents/MacOS" "$contents/Resources"
ditto "$stage/Linework.app/Contents/MacOS" "$contents/MacOS"
ditto "$stage/Linework.app/Contents/Resources" "$contents/Resources"
