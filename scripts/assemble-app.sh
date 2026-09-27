#!/bin/bash
# Assemble the minimal oab-instance-mcp.app bundle around a prebuilt binary.
#
# This script does NOT sign. Local deploy.sh signs with Apple Development; the
# release workflow signs with Developer ID Application. Keeping assembly separate
# makes the bytes, bundle id and entitlements identical in both paths — important
# because TCC grants are keyed on the signed bundle identity.
#
# Usage: assemble-app.sh <binary> <output.app> [version]
set -euo pipefail

BIN="${1:?usage: assemble-app.sh <binary> <output.app> [version]}"
OUT="${2:?usage: assemble-app.sh <binary> <output.app> [version]}"
VERSION="${3:-}"
BUNDLE_ID="${BUNDLE_ID:-dev.openab.instance-mcp}"

[ -f "$BIN" ] && [ -x "$BIN" ] || {
  echo "assemble-app: binary is missing or not executable: $BIN" >&2
  exit 66
}
case "$OUT" in
  *.app) ;;
  *) echo "assemble-app: output must end in .app: $OUT" >&2; exit 64 ;;
esac

if [ -z "$VERSION" ]; then
  VERSION=$("$BIN" --version 2>/dev/null) || {
    echo "assemble-app: pass a version when the binary cannot run on this host" >&2
    exit 65
  }
fi
# CFBundleShortVersionString: one to three dot-separated non-negative integers.
case "$VERSION" in
  ''|*[!0-9.]*) echo "assemble-app: invalid version: $VERSION" >&2; exit 64 ;;
esac
IFS=. read -r -a VERSION_PARTS <<<"$VERSION"
[ "${#VERSION_PARTS[@]}" -ge 1 ] && [ "${#VERSION_PARTS[@]}" -le 3 ] || {
  echo "assemble-app: version must have 1–3 numeric components: $VERSION" >&2
  exit 64
}
for part in "${VERSION_PARTS[@]}"; do
  [ -n "$part" ] || { echo "assemble-app: empty version component: $VERSION" >&2; exit 64; }
done

PARENT=$(dirname "$OUT")
NAME=$(basename "$OUT")
mkdir -p "$PARENT"
TMP=$(mktemp -d "$PARENT/.${NAME}.assemble.XXXXXX")
trap 'rm -rf "$TMP"' EXIT
APP="$TMP/$NAME"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
/usr/bin/ditto "$BIN" "$APP/Contents/MacOS/oab-instance-mcp"
chmod 755 "$APP/Contents/MacOS/oab-instance-mcp"
# Makes the advanced .app.zip artifact self-contained: after unzipping, run
#   app/Contents/Resources/install-prebuilt.sh app
# The signed/notarized .pkg remains the normal one-click install path.
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cp "$SCRIPT_DIR/install-prebuilt.sh" "$APP/Contents/Resources/install-prebuilt.sh"
chmod 755 "$APP/Contents/Resources/install-prebuilt.sh"

cat >"$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleName</key><string>oab-instance-mcp</string>
  <key>CFBundleDisplayName</key><string>OpenAB Instance MCP</string>
  <key>CFBundleExecutable</key><string>oab-instance-mcp</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>LSUIElement</key><true/>
  <!-- Reverse attach dials openab-pty pods as ws:// / http://. The hop is
       WireGuard and the pod deliberately holds no TLS key. ATS otherwise rejects
       every 100.64.0.0/10 URL; NSAllowsLocalNetworking does not cover CGNAT. -->
  <key>NSAppTransportSecurity</key><dict>
    <key>NSAllowsArbitraryLoads</key><true/>
  </dict>
  <key>NSHumanReadableCopyright</key><string>OpenAB</string>
</dict></plist>
EOF

/usr/bin/plutil -lint "$APP/Contents/Info.plist" >/dev/null
[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$APP/Contents/Info.plist")" = "$BUNDLE_ID" ]
[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")" = "$VERSION" ]

rm -rf "$OUT"
mv "$APP" "$OUT"
echo "assembled: $OUT ($VERSION, $BUNDLE_ID)"
