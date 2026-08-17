#!/bin/sh
# Build ReviewPad and wrap it in a Finder-launchable .app bundle.
#
#   ./scripts/bundle-macos.sh              # host architecture
#   UNIVERSAL=1 ./scripts/bundle-macos.sh  # arm64 + x86_64, as released
#   VERSION=1.2.3 ./scripts/bundle-macos.sh
set -eu

PROJECT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$PROJECT_DIR"

VERSION=${VERSION:-$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)}
DIST="$PROJECT_DIR/dist"
BUNDLE="$DIST/ReviewPad.app"

mkdir -p "$DIST"

# Rebuild the icon from its source SVG so nothing can ship a stale one. Before
# the build, not after: the binary embeds the `.icns` to give the CLI and an
# MCP-launched panel a Dock icon without a bundle around them.
cargo run --quiet --example icon
iconutil -c icns "$PROJECT_DIR/assets/ReviewPad.iconset" -o "$PROJECT_DIR/assets/ReviewPad.icns"

if [ "${UNIVERSAL:-0}" = "1" ]; then
    rustup target add aarch64-apple-darwin x86_64-apple-darwin
    cargo build --release --target aarch64-apple-darwin
    cargo build --release --target x86_64-apple-darwin
    lipo -create -output "$DIST/reviewpad" \
        "target/aarch64-apple-darwin/release/reviewpad" \
        "target/x86_64-apple-darwin/release/reviewpad"
else
    cargo build --release
    cp "target/release/reviewpad" "$DIST/reviewpad"
fi

rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS" "$BUNDLE/Contents/Resources"
cp "$PROJECT_DIR/assets/ReviewPad.icns" "$BUNDLE/Contents/Resources/ReviewPad.icns"

cp "$DIST/reviewpad" "$BUNDLE/Contents/MacOS/reviewpad"
cp "packaging/macos/reviewpad-launcher" "$BUNDLE/Contents/MacOS/reviewpad-launcher"
sed "s/__VERSION__/$VERSION/g" "packaging/macos/Info.plist" > "$BUNDLE/Contents/Info.plist"
chmod +x "$BUNDLE/Contents/MacOS/reviewpad" "$BUNDLE/Contents/MacOS/reviewpad-launcher"

# Ad-hoc signature with a stable identifier: unsigned bundles are killed by
# Gatekeeper on first launch, and a stable id keeps the identity across updates.
codesign --force --deep --sign - --identifier dev.reviewpad.ReviewPad "$BUNDLE"

echo "$BUNDLE"
