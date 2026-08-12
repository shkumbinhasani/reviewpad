#!/bin/sh
set -eu

PROJECT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$PROJECT_DIR"

cargo build --release

BUNDLE_DIR="$PROJECT_DIR/target/release/ReviewPad.app"
CONTENTS_DIR="$BUNDLE_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"

mkdir -p "$MACOS_DIR"
cp "$PROJECT_DIR/target/release/reviewpad" "$MACOS_DIR/reviewpad"
cp "$PROJECT_DIR/packaging/macos/reviewpad-launcher" "$MACOS_DIR/reviewpad-launcher"
cp "$PROJECT_DIR/packaging/macos/Info.plist" "$CONTENTS_DIR/Info.plist"
chmod +x "$MACOS_DIR/reviewpad" "$MACOS_DIR/reviewpad-launcher"

echo "$BUNDLE_DIR"
