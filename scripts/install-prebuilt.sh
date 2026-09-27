#!/bin/bash
# Install a PREBUILT, already-signed oab-instance-mcp.app for the logged-in user.
#
# This script never signs or modifies the bundle. That is load-bearing: an ad-hoc
# re-sign changes the app's designated requirement and silently drops the human's
# Full Disk Access / Screen Recording / Accessibility grants. CI release artifacts
# arrive Developer-ID signed; local deploy.sh signs first, then calls this script.
#
# Usage: install-prebuilt.sh <app> [--allow-login <email|auto>]
#
# Test seams (not used by the package): INSTALL_HOME, ALLOW_UNSIGNED=1,
# SKIP_LAUNCH=1, SKIP_TAILSCALE=1.
set -euo pipefail

SOURCE="${1:?usage: install-prebuilt.sh <app> [--allow-login <email|auto>]}"
shift
LOGIN=auto
PORT="${PORT:-8795}"
HTTPS_PORT="${HTTPS_PORT:-8444}"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --allow-login) [ "$#" -ge 2 ] || { echo "missing --allow-login value" >&2; exit 64; }; LOGIN=$2; shift 2 ;;
    --port) [ "$#" -ge 2 ] || exit 64; PORT=$2; shift 2 ;;
    --https-port) [ "$#" -ge 2 ] || exit 64; HTTPS_PORT=$2; shift 2 ;;
    -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 64 ;;
  esac
done

LABEL=dev.openab.instance-mcp
OLD_LABEL=dev.openab.mac-agent
BUNDLE_ID=dev.openab.instance-mcp
EXPECT_TEAM="${EXPECT_TEAM:-6LPQNY95AQ}"
HOME_DIR="${INSTALL_HOME:-$HOME}"
BASE="$HOME_DIR/.local/oab-instance-mcp"
APP="$BASE/oab-instance-mcp.app"
TOKEN_FILE="$HOME_DIR/.config/oab-instance-mcp/token"
PLIST="$HOME_DIR/Library/LaunchAgents/$LABEL.plist"
LOG_DIR="$HOME_DIR/Library/Logs/oab-instance-mcp"
UID_=$(id -u)

case "$SOURCE" in *.app) ;; *) echo "installer: source must be an .app bundle" >&2; exit 64 ;; esac
[ -x "$SOURCE/Contents/MacOS/oab-instance-mcp" ] || {
  echo "installer: app has no executable: $SOURCE" >&2; exit 66
}
[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$SOURCE/Contents/Info.plist" 2>/dev/null)" = "$BUNDLE_ID" ] || {
  echo "installer: unexpected or missing bundle id (wanted $BUNDLE_ID)" >&2; exit 65
}

# Validate all prerequisites before stopping the running service or touching disk.
if [ "${ALLOW_UNSIGNED:-0}" != "1" ]; then
  /usr/bin/codesign --verify --deep --strict "$SOURCE" || {
    echo "installer: invalid code signature" >&2; exit 65
  }
  TEAM=$(/usr/bin/codesign -dvv "$SOURCE" 2>&1 | /usr/bin/sed -n 's/^TeamIdentifier=//p')
  [ "$TEAM" = "$EXPECT_TEAM" ] || {
    echo "installer: refusing team '${TEAM:-none}', expected $EXPECT_TEAM" >&2
    echo "A different/ad-hoc signer is a different app to TCC and would drop existing grants." >&2
    exit 65
  }
fi

TS="${TAILSCALE_CLI:-}"
if [ "${SKIP_TAILSCALE:-0}" != "1" ]; then
  if [ -z "$TS" ]; then
    if [ -x /Applications/Tailscale.app/Contents/MacOS/Tailscale ]; then
      TS=/Applications/Tailscale.app/Contents/MacOS/Tailscale
    elif command -v tailscale >/dev/null 2>&1; then
      TS=$(command -v tailscale)
    else
      echo "installer: Tailscale is required (install and log in first)" >&2
      exit 69
    fi
  fi
  STATUS=$(mktemp)
  trap 'rm -f "$STATUS"' EXIT
  "$TS" status --self --peers=false --json >"$STATUS" || {
    echo "installer: cannot read Tailscale status (is it logged in?)" >&2; exit 69
  }
  DNSNAME=$(/usr/bin/plutil -extract Self.DNSName raw -o - "$STATUS" 2>/dev/null | /usr/bin/sed 's/\.$//' || true)
  case "$DNSNAME" in
    ''|*'Could not extract'*) echo "installer: Tailscale self DNSName is empty or missing" >&2; exit 69 ;;
  esac
  if [ "$LOGIN" = auto ]; then
    USER_ID=$(/usr/bin/plutil -extract Self.UserID raw -o - "$STATUS" 2>/dev/null || true)
    case "$USER_ID" in
      ''|*[!0-9]*)
        echo "installer: Tailscale self UserID is empty or missing" >&2
        exit 69
        ;;
    esac
    LOGIN=$(/usr/bin/plutil -extract "User.$USER_ID.LoginName" raw -o - "$STATUS" 2>/dev/null || true)
  fi
  rm -f "$STATUS"
  trap - EXIT
else
  DNSNAME="${TEST_DNSNAME:-localhost}"
  [ "$LOGIN" != auto ] || LOGIN="${TEST_LOGIN:-test@example.invalid}"
fi
  case "$LOGIN" in
    ''|*'Could not extract'*) echo "installer: could not determine the Tailscale login" >&2; exit 69 ;;
  esac

VERSION=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$SOURCE/Contents/Info.plist")
NEW_REQ=$(/usr/bin/codesign -d -r- "$SOURCE" 2>&1 | /usr/bin/sed -n 's/^designated => //p' || true)
OLD_REQ=""
if [ -d "$APP" ]; then
  OLD_REQ=$(/usr/bin/codesign -d -r- "$APP" 2>&1 | /usr/bin/sed -n 's/^designated => //p' || true)
fi

mkdir -p "$BASE" "$LOG_DIR" "$(dirname "$TOKEN_FILE")" "$(dirname "$PLIST")"

# One-shot migration from the pre-0.4.0 name, before generation so the old token
# wins and existing clients keep working.
if [ -s "$HOME_DIR/.config/oab-mac-agent/token" ] && [ ! -s "$TOKEN_FILE" ]; then
  mv "$HOME_DIR/.config/oab-mac-agent/token" "$TOKEN_FILE"
fi

# Generate once and preserve across every update. No release artifact or package
# ever contains a user bearer token. An atomic mkdir lock prevents two concurrent
# Installer.app attempts from racing and rotating the token clients already hold.
(
  LOCK="$TOKEN_FILE.lock"
  acquired=0
  for _ in $(/usr/bin/seq 1 50); do
    if mkdir "$LOCK" 2>/dev/null; then acquired=1; break; fi
    sleep 0.1
  done
  [ "$acquired" = 1 ] || { echo "installer: timed out waiting for token lock" >&2; exit 70; }
  TMP_TOKEN="$LOCK/token"
  trap 'rm -rf "$LOCK"' EXIT
  if [ ! -s "$TOKEN_FILE" ]; then
    ( umask 077; /usr/bin/openssl rand -hex 32 >"$TMP_TOKEN" )
    chmod 600 "$TMP_TOKEN"
    mv "$TMP_TOKEN" "$TOKEN_FILE"
  fi
)
chmod 600 "$TOKEN_FILE"

if [ "${SKIP_LAUNCH:-0}" != "1" ]; then
  /bin/launchctl bootout "gui/$UID_/$LABEL" >/dev/null 2>&1 || true
  /bin/launchctl bootout "gui/$UID_/$OLD_LABEL" >/dev/null 2>&1 || true
  for _ in $(/usr/bin/seq 1 40); do
    /bin/launchctl print "gui/$UID_/$LABEL" >/dev/null 2>&1 || break
    sleep 0.25
  done
  if /bin/launchctl print "gui/$UID_/$LABEL" >/dev/null 2>&1; then
    echo "installer: the old LaunchAgent did not stop after 10 seconds; app was not replaced" >&2
    echo "Try: launchctl bootout gui/$UID_/$LABEL, then run the installer again." >&2
    exit 70
  fi
fi
rm -f "$HOME_DIR/Library/LaunchAgents/$OLD_LABEL.plist"

# Copy to a sibling and verify there, then replace atomically. `ditto` preserves
# the Developer ID signature and notarization ticket; we never run codesign here.
STAGED="$BASE/.oab-instance-mcp.app.installing.$$"
BACKUP="$BASE/.oab-instance-mcp.app.previous.$$"
rm -rf "$STAGED" "$BACKUP"
/usr/bin/ditto "$SOURCE" "$STAGED"
if [ "${ALLOW_UNSIGNED:-0}" != "1" ]; then
  /usr/bin/codesign --verify --deep --strict "$STAGED"
fi
if [ -e "$APP" ]; then mv "$APP" "$BACKUP"; fi
mv "$STAGED" "$APP"
rm -rf "$BACKUP"

# Build the plist with PlistBuddy rather than interpolated XML: paths/logins with
# XML metacharacters remain data, not markup.
rm -f "$PLIST"
/usr/bin/plutil -create xml1 "$PLIST"
PB=/usr/libexec/PlistBuddy
"$PB" -c "Add :Label string $LABEL" "$PLIST"
"$PB" -c 'Add :ProgramArguments array' "$PLIST"
ARGS=(
  "$APP/Contents/MacOS/oab-instance-mcp"
  --port "$PORT"
  --allow-login "$LOGIN"
  --token-file "$TOKEN_FILE"
  --menu-bar
  --public-url "https://$DNSNAME:$HTTPS_PORT/mcp"
)
if [ "${SKIP_LAUNCH:-0}" != "1" ] && /bin/launchctl print "gui/$UID_/dev.openab.instance-mcp.pw-mcp" >/dev/null 2>&1; then
  ARGS+=(--upstream browser=http://127.0.0.1:8794/mcp)
elif [ "${TEST_WITH_UPSTREAM:-0}" = "1" ]; then
  ARGS+=(--upstream browser=http://127.0.0.1:8794/mcp)
fi
for i in "${!ARGS[@]}"; do "$PB" -c "Add :ProgramArguments:$i string ${ARGS[$i]}" "$PLIST"; done
"$PB" -c 'Add :RunAtLoad bool true' "$PLIST"
"$PB" -c 'Add :KeepAlive bool true' "$PLIST"
"$PB" -c 'Add :ProcessType string Interactive' "$PLIST"
"$PB" -c "Add :StandardOutPath string $LOG_DIR/agent.log" "$PLIST"
"$PB" -c "Add :StandardErrorPath string $LOG_DIR/agent.log" "$PLIST"
/usr/bin/plutil -lint "$PLIST" >/dev/null

if [ "${SKIP_LAUNCH:-0}" != "1" ]; then
  /bin/launchctl bootstrap "gui/$UID_" "$PLIST"
  for _ in $(/usr/bin/seq 1 20); do
    /usr/bin/curl -s -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null && break
    sleep 0.25
  done
  /usr/bin/curl -fsS -m 2 "http://127.0.0.1:$PORT/healthz" >/dev/null || {
    echo "installer: LaunchAgent did not become healthy; see $LOG_DIR/agent.log" >&2
    exit 70
  }
fi
if [ "${SKIP_TAILSCALE:-0}" != "1" ]; then
  "$TS" serve --bg --https="$HTTPS_PORT" "http://127.0.0.1:$PORT" >/dev/null
fi

if [ -n "$OLD_REQ" ] && [ -n "$NEW_REQ" ] && [ "$OLD_REQ" != "$NEW_REQ" ]; then
  echo "NOTE: the signing requirement changed; macOS may ask once to re-grant TCC permissions."
  echo "Future Developer-ID releases keep this requirement stable."
fi

echo "installed oab-instance-mcp $VERSION"
echo "  app: $APP"
echo "  login: $LOGIN"
echo "  MCP: https://$DNSNAME:$HTTPS_PORT/mcp"
echo "  token: $TOKEN_FILE (copy it from the menu bar; not printed)"
echo "  one-time: enable Full Disk Access, Screen Recording and Accessibility in System Settings"
