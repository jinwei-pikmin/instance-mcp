#!/bin/bash
# Build-tree deploy for developers. Assembles and signs a temporary app, then
# hands it to install-prebuilt.sh — the same installer used by release packages.
#
# Usage: scripts/deploy.sh <allow-login-email> <codesign-identity>
#
# Team-signed only: installing an ad-hoc bundle drops the human's TCC grants, so
# it is refused unless ALLOW_ADHOC=1. Expected team defaults to 6LPQNY95AQ
# (EXPECT_TEAM=... to change).
#
# Optional keychain env (needed when a dedicated signing keychain is used):
#   KEYCHAIN=<path-to-keychain-db>
#   KEYCHAIN_PASSWORD_FILE=<path-containing-password>
set -euo pipefail

LOGIN="${1:?usage: deploy.sh <allow-login-email> <codesign-identity>}"
IDENTITY="${2:?usage: deploy.sh <allow-login-email> <codesign-identity>}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN="$ROOT/.build/release/oab-instance-mcp"
EXPECT_TEAM="${EXPECT_TEAM:-6LPQNY95AQ}"

[ -x "$BIN" ] || { echo "build first: swift build -c release" >&2; exit 1; }
NEWEST_SRC=$(find "$ROOT/Sources" -name '*.swift' -newer "$BIN" | head -1)
[ -z "$NEWEST_SRC" ] || { echo "binary older than $NEWEST_SRC — rebuild first" >&2; exit 1; }
VERSION=$("$BIN" --version)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
APP="$TMP/oab-instance-mcp.app"

"$ROOT/scripts/assemble-app.sh" "$BIN" "$APP" "$VERSION"

KC_ARGS=()
if [ -n "${KEYCHAIN:-}" ]; then
  if [ -n "${KEYCHAIN_PASSWORD_FILE:-}" ]; then
    security unlock-keychain -p "$(cat "$KEYCHAIN_PASSWORD_FILE")" "$KEYCHAIN"
  fi
  KC_ARGS=(--keychain "$KEYCHAIN")
fi
codesign --force --options runtime --timestamp=none \
  ${KC_ARGS[@]+"${KC_ARGS[@]}"} --sign "$IDENTITY" --identifier dev.openab.instance-mcp "$APP"
codesign --verify --deep --strict "$APP"

TEAM=$(codesign -dvv "$APP" 2>&1 | sed -n 's/^TeamIdentifier=//p')
if [ "${ALLOW_ADHOC:-0}" != "1" ]; then
  if [ -z "$TEAM" ] || [ "$TEAM" = "not set" ]; then
    echo "refusing to install an ad-hoc-signed bundle: it would drop your TCC grants" >&2
    exit 1
  fi
  if [ "$TEAM" != "$EXPECT_TEAM" ]; then
    echo "refusing: signed by team $TEAM, expected $EXPECT_TEAM — a different team is a different app to TCC" >&2
    exit 1
  fi
fi
echo "signed: TeamIdentifier=${TEAM:-not set}"

EXPECT_TEAM="$EXPECT_TEAM" ALLOW_UNSIGNED="${ALLOW_ADHOC:-0}" \
  "$ROOT/scripts/install-prebuilt.sh" "$APP" --allow-login "$LOGIN"
