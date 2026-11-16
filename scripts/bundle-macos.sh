#!/bin/sh
# Build ConvertCraft.app on macOS: the release binary, an icon generated from
# assets/logos/logo.png, and Info.plist, ad-hoc signed. Output:
# target/release/ConvertCraft.app (under $CARGO_TARGET_DIR when set).
set -eu

if [ "$(uname -s)" != Darwin ]; then
    echo "bundle-macos.sh builds a macOS app and must run on macOS" >&2
    exit 1
fi

cd "$(dirname "$0")/.."
cargo build --release --locked

release="${CARGO_TARGET_DIR:-target}/release"
app="$release/ConvertCraft.app"
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$release/convertcraft" "$app/Contents/MacOS/convertcraft"

iconset="$work/ConvertCraft.iconset"
mkdir "$iconset"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" assets/logos/logo.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z "$double" "$double" assets/logos/logo.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil --convert icns "$iconset" --output "$app/Contents/Resources/ConvertCraft.icns"

cat >"$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>ConvertCraft</string>
    <key>CFBundleExecutable</key>
    <string>convertcraft</string>
    <key>CFBundleIconFile</key>
    <string>ConvertCraft</string>
    <key>CFBundleIdentifier</key>
    <string>io.github.pierrunoyt.convertcraft</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>ConvertCraft</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$version</string>
    <key>CFBundleVersion</key>
    <string>$version</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.utilities</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF
plutil -lint "$app/Contents/Info.plist" >/dev/null

# Ad-hoc signature: enough to run on the machine that built it. Copies downloaded from
# elsewhere need a Developer ID signature and notarization to pass Gatekeeper.
codesign --force --sign - "$app"
echo "Built $app"
