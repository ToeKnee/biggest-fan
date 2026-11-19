#!/bin/sh
# Regenerate the fixed-size icon PNGs the .deb ships from the scalable
# SVG. Run from the repo root; needs inkscape.
#
#   sh tools/render-icons.sh
set -eu

APP_ID=net.ynottony.ten.BiggestFan
SRC="packaging/icons/hicolor/scalable/apps/${APP_ID}.svg"

for size in 16 24 32 48 64 96 128 256 512; do
    dir="packaging/icons/hicolor/${size}x${size}/apps"
    mkdir -p "$dir"
    inkscape -w "$size" -h "$size" "$SRC" -o "$dir/${APP_ID}.png"
done
