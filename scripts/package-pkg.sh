#!/bin/bash
# Build a macOS installer package around an ALREADY-SIGNED app.
#
# Usage: package-pkg.sh <oab-instance-mcp.app> <output.pkg> [version]
# Env:   PKG_SIGN_IDENTITY="Developer ID Installer: ..."  (optional for smoke;
#                                                            required for release)
#        EXPECT_TEAM=6LPQNY95AQ
#        ALLOW_UNSIGNED=1                                  (tests only)
set -euo pipefail

APP="${1:?usage: package-pkg.sh <app> <output.pkg> [version]}"
OUT="${2:?usage: package-pkg.sh <app> <output.pkg> [version]}"
VERSION="${3:-}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
EXPECT_TEAM="${EXPECT_TEAM:-6LPQNY95AQ}"
IDENTIFIER="${PKG_IDENTIFIER:-dev.openab.instance-mcp.installer}"

[ -d "$APP" ] || { echo "package-pkg: app not found: $APP" >&2; exit 66; }
case "$OUT" in *.pkg) ;; *) echo "package-pkg: output must end in .pkg" >&2; exit 64 ;; esac
if [ -z "$VERSION" ]; then
  VERSION=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")
fi
case "$VERSION" in ''|*[!0-9.]*) echo "package-pkg: invalid version: $VERSION" >&2; exit 64 ;; esac

if [ "${ALLOW_UNSIGNED:-0}" != "1" ]; then
  /usr/bin/codesign --verify --deep --strict "$APP"
  TEAM=$(/usr/bin/codesign -dvv "$APP" 2>&1 | /usr/bin/sed -n 's/^TeamIdentifier=//p')
  [ "$TEAM" = "$EXPECT_TEAM" ] || {
    echo "package-pkg: app team '${TEAM:-none}', expected $EXPECT_TEAM" >&2; exit 65
  }
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
PAYLOAD="$TMP/payload"
SCRIPTS="$TMP/scripts"
mkdir -p "$PAYLOAD/Library/Application Support/OpenAB/instance-mcp" "$SCRIPTS" "$(dirname "$OUT")"
/usr/bin/ditto "$APP" "$PAYLOAD/Library/Application Support/OpenAB/instance-mcp/oab-instance-mcp.app"
cp "$ROOT/scripts/install-prebuilt.sh" "$SCRIPTS/install-prebuilt.sh"
cp "$ROOT/scripts/pkg/postinstall" "$SCRIPTS/postinstall"
chmod 755 "$SCRIPTS/install-prebuilt.sh" "$SCRIPTS/postinstall"

ARGS=(
  --root "$PAYLOAD"
  --scripts "$SCRIPTS"
  --identifier "$IDENTIFIER"
  --version "$VERSION"
  --install-location /
)
if [ -n "${PKG_SIGN_IDENTITY:-}" ]; then
  ARGS+=(--sign "$PKG_SIGN_IDENTITY")
  [ -z "${PKG_KEYCHAIN:-}" ] || ARGS+=(--keychain "$PKG_KEYCHAIN")
elif [ "${ALLOW_UNSIGNED:-0}" != "1" ]; then
  echo "package-pkg: PKG_SIGN_IDENTITY is required (Developer ID Installer)" >&2
  exit 65
fi
rm -f "$OUT"
/usr/bin/pkgbuild "${ARGS[@]}" "$OUT"

/usr/sbin/pkgutil --payload-files "$OUT" | /usr/bin/grep -q \
  '^\./Library/Application Support/OpenAB/instance-mcp/oab-instance-mcp.app/Contents/MacOS/oab-instance-mcp$'
if [ -n "${PKG_SIGN_IDENTITY:-}" ]; then
  /usr/sbin/pkgutil --check-signature "$OUT" | /usr/bin/grep -q 'Developer ID Installer'
fi
echo "packaged: $OUT ($VERSION)"
