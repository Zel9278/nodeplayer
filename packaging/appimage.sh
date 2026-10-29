#!/bin/sh
# Makes target/appimage/nodeplayer-<arch>.AppImage from the release build. Build first
# (cargo build --release).
#
# Needs appimagetool: on the PATH, or its path in APPIMAGETOOL. Only the program goes in: the
# window, sound and font libraries are the system's (libX11, libasound.so.2, fontconfig), and
# FFmpeg is the ffmpeg / ffprobe programs, which nodeplayer offers to download on first start
# when the system has none.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
tool=${APPIMAGETOOL:-appimagetool}
arch=$(uname -m)
bin=nodeplayer
out="$root/target/appimage"
exe="$root/target/release/$bin"
[ -x "$exe" ] || { echo "no $exe: build it first (cargo build --release)" >&2; exit 1; }

dir="$out/$bin.AppDir"
rm -rf "$dir"
mkdir -p "$dir/usr/bin" "$dir/usr/share/applications" "$dir/usr/share/icons/hicolor/256x256/apps"
cp "$exe" "$dir/usr/bin/"
strip "$dir/usr/bin/$bin" 2>/dev/null || true
cp "$root/assets/$bin.desktop" "$dir/usr/share/applications/"
cp "$root/assets/icon.png" "$dir/usr/share/icons/hicolor/256x256/apps/$bin.png"
# what appimagetool looks for at the top
ln -s "usr/share/applications/$bin.desktop" "$dir/$bin.desktop"
ln -s "usr/share/icons/hicolor/256x256/apps/$bin.png" "$dir/$bin.png"
ln -s "$bin.png" "$dir/.DirIcon"
ln -s "usr/bin/$bin" "$dir/AppRun"
# without FUSE (CI) the tool runs from its own extracted copy
ARCH=$arch APPIMAGE_EXTRACT_AND_RUN=1 "$tool" --no-appstream "$dir" "$out/$bin-$arch.AppImage"
