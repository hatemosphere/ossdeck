#!/bin/sh
# Regenerates AppIcon.icns and menubar.png from the SVGs. Needs rsvg-convert (brew install librsvg).
set -eu
cd "$(dirname "$0")"
set_dir=$(mktemp -d)/AppIcon.iconset
mkdir -p "$set_dir"
for s in 16 32 128 256 512; do
  rsvg-convert -w $s -h $s AppIcon.svg -o "$set_dir/icon_${s}x${s}.png"
  rsvg-convert -w $((s * 2)) -h $((s * 2)) AppIcon.svg -o "$set_dir/icon_${s}x${s}@2x.png"
done
iconutil -c icns "$set_dir" -o AppIcon.icns
rsvg-convert -w 36 -h 36 menubar.svg -o menubar.png
