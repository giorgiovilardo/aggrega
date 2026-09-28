#!/bin/sh
# Builds a universal (Apple silicon + Intel) Aggrega.app and zips it.
#
#   packaging/macos/bundle.sh [version]    # default: version from Cargo.toml
#
# Needs both Rust targets (rustup target add aarch64-apple-darwin x86_64-apple-darwin)
# and rsvg-convert (brew install librsvg) to render the icon.
# Output: dist/Aggrega.app and dist/aggrega-<version>-macos-universal.zip
set -eu
cd "$(dirname "$0")/../.."

version="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)}"
targets="aarch64-apple-darwin x86_64-apple-darwin"
app="dist/Aggrega.app"

for t in $targets; do
    cargo build --release --target "$t"
done

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
lipo -create -output "$app/Contents/MacOS/aggrega" \
    $(for t in $targets; do echo "target/$t/release/aggrega"; done)
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"

# .icns from the SVG: every size iconutil expects, at 1x and 2x.
iconset="dist/aggrega.iconset"
rm -rf "$iconset" && mkdir -p "$iconset"
for s in 16 32 128 256 512; do
    rsvg-convert -w "$s" -h "$s" assets/aggrega.svg -o "$iconset/icon_${s}x${s}.png"
    rsvg-convert -w $((s * 2)) -h $((s * 2)) assets/aggrega.svg -o "$iconset/icon_${s}x${s}@2x.png"
done
iconutil -c icns -o "$app/Contents/Resources/aggrega.icns" "$iconset"
rm -rf "$iconset"

# Unsigned builds still need an ad-hoc signature to launch on Apple silicon.
codesign --force --deep --sign - "$app"

# ditto keeps the bundle's permissions and metadata intact, unlike plain zip.
zip="dist/aggrega-$version-macos-universal.zip"
rm -f "$zip"
ditto -c -k --keepParent "$app" "$zip"
echo "$zip"
