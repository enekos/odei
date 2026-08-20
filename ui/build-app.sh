#!/bin/bash
# Build Odei.app. SwiftPM only knows how to emit a bare executable, and a bare
# executable gets no Dock icon, no menu bar, and no keyboard focus — so the
# bundle is assembled here rather than in Xcode, which this machine does not
# need to have installed.
set -euo pipefail

cd "$(dirname "$0")"
CONFIG="${1:-release}"
APP="$PWD/Odei.app"

swift build -c "$CONFIG"
BIN="$(swift build -c "$CONFIG" --show-bin-path)/OdeiUI"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/Odei"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Odei</string>
  <key>CFBundleDisplayName</key><string>odei</string>
  <key>CFBundleExecutable</key><string>Odei</string>
  <key>CFBundleIdentifier</key><string>io.enekos.odei.ui</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticTermination</key><false/>
</dict>
</plist>
PLIST

# Ad-hoc signature: unsigned bundles are killed on launch on Apple silicon.
codesign --force --sign - "$APP" >/dev/null 2>&1 || true

echo "built $APP"
echo "run with:  open $APP"
