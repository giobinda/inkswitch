#!/bin/sh
# Builds dist/Inkswitch-x86_64.AppImage.
set -eu

cd "$(dirname "$0")/.."
WORK=target/appimage
APPDIR=$WORK/AppDir
TOOL=$WORK/appimagetool-x86_64.AppImage
OUT=dist/Inkswitch-x86_64.AppImage
BIN=$(packaging/build-binary.sh)

rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/scalable/apps" dist
cp "$BIN" "$APPDIR/usr/bin/inkswitch"
cp packaging/inkswitch.desktop "$APPDIR/usr/share/applications/"
cp packaging/inkswitch.svg "$APPDIR/usr/share/icons/hicolor/scalable/apps/"
# appimagetool looks for these three at the top level.
cp packaging/inkswitch.desktop packaging/inkswitch.svg "$APPDIR/"
ln -s inkswitch.svg "$APPDIR/.DirIcon"
ln -s usr/bin/inkswitch "$APPDIR/AppRun"

if [ ! -x "$TOOL" ]; then
    curl -fL -o "$TOOL" https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
    chmod +x "$TOOL"
fi
ARCH=x86_64 "$TOOL" --appimage-extract-and-run "$APPDIR" "$OUT"
echo "built $OUT"
