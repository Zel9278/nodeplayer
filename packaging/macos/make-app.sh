#!/bin/sh
# Builds nodeplayer.app around a release binary (macOS only: needs sips, iconutil, codesign).
#   packaging/macos/make-app.sh <binary> <version> <out-dir>
# makes <out-dir>/nodeplayer.app: the binary, Info.plist with the version, the icon, and an
# ad-hoc signature over the whole bundle (Apple silicon runs only signed code, and a bundle
# changed after its binary was signed would read as damaged).
set -eu

binary=$1
version=$2
out=$3
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)

app="$out/nodeplayer.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/nodeplayer"
sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"
printf 'APPL????' > "$app/Contents/PkgInfo"

# The icon, at the sizes the 256-pixel source covers.
iconset=$(mktemp -d)/nodeplayer.iconset
mkdir -p "$iconset"
for size in 16 32 128 256; do
    sips -z "$size" "$size" "$root/assets/icon.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
    half=$((size / 2))
    if [ "$half" -ge 16 ]; then
        cp "$iconset/icon_${size}x${size}.png" "$iconset/icon_${half}x${half}@2x.png"
    fi
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/nodeplayer.icns"

codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"
echo "$app"
