#!/bin/bash
# Verify downloaded release artifacts locally.
# Usage: verify-release.sh <app.zip> <installer.pkg> [expected-team]
set -euo pipefail

ZIP="${1:?usage: verify-release.sh <app.zip> <pkg> [expected-team]}"
PKG="${2:?usage: verify-release.sh <app.zip> <pkg> [expected-team]}"
TEAM_EXPECTED="${3:-6LPQNY95AQ}"
[ -f "$ZIP" ] && [ -f "$PKG" ] || { echo "verify-release: artifact missing" >&2; exit 66; }
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
/usr/bin/ditto -x -k "$ZIP" "$TMP"
APP=$(find "$TMP" -maxdepth 2 -type d -name 'oab-instance-mcp.app' -print -quit)
[ -n "$APP" ] || { echo "verify-release: app missing from zip" >&2; exit 65; }

/usr/bin/codesign --verify --deep --strict --verbose=2 "$APP"
TEAM=$(/usr/bin/codesign -dvv "$APP" 2>&1 | /usr/bin/sed -n 's/^TeamIdentifier=//p')
[ "$TEAM" = "$TEAM_EXPECTED" ] || { echo "verify-release: app team $TEAM, wanted $TEAM_EXPECTED" >&2; exit 65; }
/usr/bin/lipo -info "$APP/Contents/MacOS/oab-instance-mcp" | /usr/bin/grep -Eq 'x86_64 arm64|arm64 x86_64'
xcrun stapler validate "$APP"
/usr/sbin/spctl -a -vv --type execute "$APP"

SIG=$(/usr/sbin/pkgutil --check-signature "$PKG")
printf '%s\n' "$SIG" | /usr/bin/grep -q 'Developer ID Installer'
printf '%s\n' "$SIG" | /usr/bin/grep -q "$TEAM_EXPECTED"
xcrun stapler validate "$PKG"
/usr/sbin/spctl -a -vv --type install "$PKG"
/usr/sbin/pkgutil --payload-files "$PKG" | /usr/bin/grep -q \
  '^\./Library/Application Support/OpenAB/instance-mcp/oab-instance-mcp.app/Contents/MacOS/oab-instance-mcp$'

echo "release verification: OK (team $TEAM_EXPECTED, universal, signed, notarized, stapled)"
