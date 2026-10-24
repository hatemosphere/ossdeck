#!/bin/sh
# Builds ossdeck.app (notifications and launch at login need a real bundle) and installs it to ~/Applications.
set -eu
cd "$(dirname "$0")"
cargo build --release
app=target/release/ossdeck.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/ossdeck "$app/Contents/MacOS/"
cp assets/icon/AppIcon.icns "$app/Contents/Resources/"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>dev.hatemosphere.ossdeck</string>
  <key>CFBundleName</key><string>ossdeck</string>
  <key>CFBundleExecutable</key><string>ossdeck</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
codesign --force --sign - "$app"
mkdir -p ~/Applications
rm -rf ~/Applications/ossdeck.app
cp -R "$app" ~/Applications/
echo "installed ~/Applications/ossdeck.app"
