#!/bin/sh
# Builds dist/Inkswitch.flatpak, a single-file bundle of the application.
#
# The prebuilt binary is packaged as it is, so no Flatpak SDK is needed: only the runtime
# (flatpak install flathub org.freedesktop.Platform//25.08).
set -eu

cd "$(dirname "$0")/.."
APP=io.github.giobinda.Inkswitch
RUNTIME=org.freedesktop.Platform
SDK=org.freedesktop.Sdk
BRANCH=25.08
WORK=target/flatpak
OUT=dist/Inkswitch.flatpak
BIN=$(packaging/build-binary.sh)

rm -rf "$WORK"
mkdir -p "$WORK" dist
# Nothing is compiled in the sandbox, so the runtime stands in for the SDK here...
flatpak build-init "$WORK/build" "$APP" "$RUNTIME" "$RUNTIME" "$BRANCH"
# ...and the metadata names the real one.
sed -i "s|^sdk=.*|sdk=$SDK/x86_64/$BRANCH|" "$WORK/build/metadata"

FILES=$WORK/build/files
install -Dm755 "$BIN" "$FILES/bin/inkswitch"
install -Dm644 packaging/inkswitch.svg "$FILES/share/icons/hicolor/scalable/apps/$APP.svg"
install -d "$FILES/share/applications"
sed "s|^Icon=.*|Icon=$APP|; s|^StartupWMClass=.*|StartupWMClass=$APP|" packaging/inkswitch.desktop > "$FILES/share/applications/$APP.desktop"

# What the driver needs from outside the sandbox:
#   device=all            the tablet (USB), /dev/uinput and the kernel's input devices
#   org.kde.KWin          which window is in front, and mapping the pen to a screen
#   org.kde.keyboard      the keyboard layout in use
#   host-os, xdg-data...  read-only, to find the icons of installed applications
flatpak build-finish "$WORK/build" \
    --command=inkswitch \
    --socket=wayland --socket=fallback-x11 --share=ipc \
    --device=all \
    --talk-name=org.kde.KWin --talk-name=org.kde.keyboard \
    --filesystem=host-os:ro \
    --filesystem=xdg-data/applications:ro --filesystem=xdg-data/icons:ro \
    --filesystem=xdg-data/flatpak/exports:ro --filesystem=/var/lib/flatpak/exports:ro

flatpak build-export "$WORK/repo" "$WORK/build" >/dev/null
flatpak build-bundle --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo "$WORK/repo" "$OUT" "$APP"
echo "built $OUT"
