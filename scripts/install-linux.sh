#!/bin/bash
# Install the Linux hands node from a release tarball (run ON the target, as the desktop user).
#
#   curl -fsSL https://github.com/openabdev/instance-mcp/releases/latest/download/oab-instance-mcp-VERSION-linux-$(dpkg --print-architecture).tar.gz | tar xz
#   cd oab-instance-mcp-*-linux-* && ./install-linux.sh [--allow-login you@example.com] [--no-browser]
#
# Installs the binary + units under ~/.local/oab-instance-mcp, generates or keeps the bearer
# token, detects the Tailscale login, installs @playwright/mcp if node is present, enables
# linger and `tailscale serve --https=8444`. Idempotent. Full walkthrough: docs/linux-setup.md.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
BASE="$HOME/.local/oab-instance-mcp"
CFG="$HOME/.config/oab-instance-mcp"
UNITS="$HOME/.config/systemd/user"
ALLOW_LOGIN=""; BROWSER=1; PORT=8795; SERVE_PORT=8444
while [ $# -gt 0 ]; do
  case "$1" in
    --allow-login) ALLOW_LOGIN="$2"; shift 2;;
    --no-browser) BROWSER=0; shift;;
    --port) PORT="$2"; shift 2;;
    --serve-port) SERVE_PORT="$2"; shift 2;;
    *) echo "unknown arg: $1" >&2; exit 64;;
  esac
done

need() { command -v "$1" >/dev/null || { echo "missing: $1 (apt-get install $2)" >&2; exit 78; }; }
need tailscale tailscale; need grim grim; need wlrctl wlrctl; need wtype wtype; need python3 python3
[ "$(id -u)" != 0 ] || { echo "run as the desktop user, not root" >&2; exit 64; }

if [ -z "$ALLOW_LOGIN" ]; then
  ALLOW_LOGIN=$(tailscale status --json 2>/dev/null | python3 -c \
    'import sys,json;d=json.load(sys.stdin);print(d["User"][str(d["Self"]["UserID"])]["LoginName"])' 2>/dev/null || true)
  [ -n "$ALLOW_LOGIN" ] || { echo "cannot detect Tailscale login; is this node logged in? (or pass --allow-login)" >&2; exit 78; }
fi

mkdir -p "$BASE" "$CFG" "$UNITS"
install -m 755 "$HERE/oab-instance-mcp" "$BASE/oab-instance-mcp"
if [ ! -s "$CFG/token" ]; then
  head -c 32 /dev/urandom | base64 | tr -d '=+/\n' | head -c 40 > "$CFG/token"; chmod 600 "$CFG/token"
  echo "generated bearer token at $CFG/token"
fi

UPSTREAM=""
if [ "$BROWSER" = 1 ] && command -v node >/dev/null && command -v npm >/dev/null && command -v chromium >/dev/null; then
  install -m 755 "$HERE/pw-mcp.sh" "$BASE/pw-mcp.sh"
  mkdir -p "$BASE/pw-mcp"
  [ -x "$BASE/pw-mcp/node_modules/.bin/playwright-mcp" ] || (cd "$BASE/pw-mcp" && npm install --save-exact @playwright/mcp@0.0.82)
  cat >"$UNITS/oab-pw-mcp.service" <<UNIT
[Unit]
Description=Playwright MCP for the oab-instance-mcp hands node (loopback :8794)
After=graphical-session.target

[Service]
ExecStart=$BASE/pw-mcp.sh
Environment=XDG_RUNTIME_DIR=/run/user/%U
Environment=WAYLAND_DISPLAY=wayland-0
Environment=DISPLAY=:0
Restart=always
RestartSec=3

[Install]
WantedBy=default.target
UNIT
  UPSTREAM="Environment=MCP_UPSTREAM=browser=http://127.0.0.1:8794/mcp"
  if [ -w /etc/chromium/policies/managed ] || sudo -n true 2>/dev/null; then
    sudo mkdir -p /etc/chromium/policies/managed
    echo '{"DefaultMediaStreamSetting": 2, "DefaultNotificationsSetting": 2, "DefaultGeolocationSetting": 2}' \
      | sudo tee /etc/chromium/policies/managed/oab-instance-mcp.json >/dev/null
  else
    echo "note: could not write Chromium policy (no sudo); camera/mic prompts will need a mouse click" >&2
  fi
else
  echo "browser tools skipped (--no-browser, or node/npm/chromium missing)"
fi

cat >"$UNITS/oab-instance-mcp.service" <<UNIT
[Unit]
Description=oab-instance-mcp (Linux hands node: /mcp + /attach)
After=graphical-session.target

[Service]
ExecStart=$BASE/oab-instance-mcp
Environment=BIND=127.0.0.1:$PORT
Environment=MCP_TOKEN_FILE=$CFG/token
Environment=MCP_ALLOW_LOGIN=$ALLOW_LOGIN
$UPSTREAM
Environment=WAYLAND_DISPLAY=wayland-0
Environment=XDG_RUNTIME_DIR=/run/user/%U
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
UNIT

systemctl --user daemon-reload
[ -z "$UPSTREAM" ] || systemctl --user enable --now oab-pw-mcp.service
systemctl --user enable --now oab-instance-mcp.service
systemctl --user restart oab-instance-mcp.service
sudo -n loginctl enable-linger "$USER" 2>/dev/null || loginctl enable-linger "$USER" 2>/dev/null || echo "note: enable linger manually: sudo loginctl enable-linger $USER" >&2

if sudo -n true 2>/dev/null; then
  sudo tailscale serve --bg --https="$SERVE_PORT" "http://127.0.0.1:$PORT" >/dev/null
else
  echo "run: sudo tailscale serve --bg --https=$SERVE_PORT http://127.0.0.1:$PORT" >&2
fi

sleep 1
curl -fsS "127.0.0.1:$PORT/healthz" >/dev/null && echo "daemon: ok"
DNS=$(tailscale status --json | python3 -c 'import sys,json;print(json.load(sys.stdin)["Self"]["DNSName"].rstrip("."))')
echo "MCP URL : https://$DNS:$SERVE_PORT/mcp"
echo "token   : $CFG/token   (allow-login: $ALLOW_LOGIN)"
echo "lend    : POST https://$DNS:$SERVE_PORT/attach   (see docs/linux-setup.md)"
