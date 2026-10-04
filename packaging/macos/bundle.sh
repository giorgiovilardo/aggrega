#!/bin/sh
# Builds an Apple silicon (arm64) Aggrega.app and packs it into a .dmg.
#
#   packaging/macos/bundle.sh
#
# The version comes from Cargo.toml (which the release workflow rewrites from the
# tag), so Info.plist always matches the version compiled into the binary.
# Needs the Rust target (rustup target add aarch64-apple-darwin) and rsvg-convert
# (brew install librsvg) to render the icon.
# Output: dist/Aggrega.app and dist/aggrega-<version>-macos-arm64.dmg
set -eu
cd "$(dirname "$0")/../.."

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
target="aarch64-apple-darwin"
app="dist/Aggrega.app"

cargo build --release --target "$target"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "target/$target/release/aggrega" "$app/Contents/MacOS/aggrega"
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
cp LICENSE assets/fonts/OFL.txt "$app/Contents/Resources/"

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

# Disk image with the app next to an /Applications link, for drag-to-install.
dmg="dist/aggrega-$version-macos-arm64.dmg"
staging="dist/dmg"
rm -rf "$staging" "$dmg"
mkdir -p "$staging"
ditto "$app" "$staging/Aggrega.app"
ln -s /Applications "$staging/Applications"
hdiutil create -volname Aggrega -srcfolder "$staging" -fs HFS+ -format UDZO -ov "$dmg"
rm -rf "$staging"
echo "$dmg"
