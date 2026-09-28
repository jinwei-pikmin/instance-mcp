#!/bin/bash
# Install the Linux hands node from a release tarball (run ON the target, as the desktop user).
#
#   curl -fsSL https://github.com/openabdev/instance-mcp/releases/latest/download/oab-instance-mcp-VERSION-linux-$(dpkg --print-architecture).tar.gz | tar xz
#   cd oab-instance-mcp-*-linux-* && ./install-linux.sh [--allow-login you@example.com] [--no-browser] [--headless-seat]
#
# --headless-seat: for a server install with no desktop (Ubuntu Server, a mini PC in a closet):
#   installs sway and runs it headless (one 1920x1080 virtual output) as user unit oab-seat.service.
#   Boxes that already run a wlroots desktop (Raspberry Pi OS labwc, sway) do not need it.
#
# Installs the binary + units under ~/.local/oab-instance-mcp, generates or keeps the bearer
# token, detects the Tailscale login, installs @playwright/mcp if node is present, enables
# linger and `tailscale serve --https=8444`. Idempotent. Full walkthrough: docs/linux-setup.md.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
BASE="$HOME/.local/oab-instance-mcp"
CFG="$HOME/.config/oab-instance-mcp"
UNITS="$HOME/.config/systemd/user"
ALLOW_LOGIN=""; BROWSER=1; PORT=8795; SERVE_PORT=8444; HEADLESS_SEAT=0
while [ $# -gt 0 ]; do
  case "$1" in
    --allow-login) ALLOW_LOGIN="$2"; shift 2;;
    --no-browser) BROWSER=0; shift;;
    --headless-seat) HEADLESS_SEAT=1; shift;;   # no desktop on this box: run a headless sway as the seat
    --port) PORT="$2"; shift 2;;
    --serve-port) SERVE_PORT="$2"; shift 2;;
    *) echo "unknown arg: $1" >&2; exit 64;;
  esac
done

need() { command -v "$1" >/dev/null || { echo "missing: $1 (apt-get install $2)" >&2; exit 78; }; }
need tailscale tailscale; need grim grim; need wlrctl wlrctl; need wtype wtype; need python3 python3
[ "$(id -u)" != 0 ] || { echo "run as the desktop user, not root" >&2; exit 64; }

# node installed via fnm/nvm is often a symlink with npm/npx only beside the real binary.
NODE_BIN=""
if command -v node >/dev/null; then NODE_BIN="$(dirname "$(readlink -f "$(command -v node)")")"; export PATH="$NODE_BIN:$PATH"; fi

mkdir -p "$BASE" "$CFG" "$UNITS"
if [ "$HEADLESS_SEAT" = 1 ]; then
  need sway sway
  mkdir -p "$HOME/.config/sway"
  [ -f "$HOME/.config/sway/config" ] || cat >"$HOME/.config/sway/config" <<'SWAY'
# Headless seat for the oab-instance-mcp hands node: one virtual 1080p output, no input devices.
output HEADLESS-1 resolution 1920x1080 position 0,0
default_border none
focus_follows_mouse no
SWAY
  cat >"$UNITS/oab-seat.service" <<'UNIT'
[Unit]
Description=Headless sway seat for the oab-instance-mcp hands node

[Service]
Environment=WLR_BACKENDS=headless
Environment=WLR_LIBINPUT_NO_DEVICES=1
Environment=WLR_RENDERER=pixman
Environment=XDG_RUNTIME_DIR=/run/user/%U
Environment=XDG_SESSION_TYPE=wayland
ExecStart=/usr/bin/sway
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
UNIT
  systemctl --user daemon-reload
  systemctl --user enable --now oab-seat.service
  sleep 2
fi
# Which Wayland socket is the seat on? A desktop gives wayland-0; headless sway picks the first
# free name, usually wayland-1.
RT="/run/user/$(id -u)"
WL="$(ls "$RT" 2>/dev/null | grep -E '^wayland-[0-9]+$' | sort -V | tail -1)"
[ -n "$WL" ] || { echo "no Wayland socket in $RT: start a wlroots desktop or pass --headless-seat" >&2; exit 78; }
echo "seat: $WL"

if [ -z "$ALLOW_LOGIN" ]; then
  ALLOW_LOGIN=$(tailscale status --json 2>/dev/null | python3 -c \
    'import sys,json;d=json.load(sys.stdin);print(d["User"][str(d["Self"]["UserID"])]["LoginName"])' 2>/dev/null || true)
  [ -n "$ALLOW_LOGIN" ] || { echo "cannot detect Tailscale login; is this node logged in? (or pass --allow-login)" >&2; exit 78; }
fi

install -m 755 "$HERE/oab-instance-mcp" "$BASE/oab-instance-mcp"
if [ ! -s "$CFG/token" ]; then
  head -c 32 /dev/urandom | base64 | tr -d '=+/\n' | head -c 40 > "$CFG/token"; chmod 600 "$CFG/token"
  echo "generated bearer token at $CFG/token"
fi

UPSTREAM=""
if [ "$BROWSER" = 1 ] && command -v node >/dev/null && command -v npm >/dev/null; then
  install -m 755 "$HERE/pw-mcp.sh" "$BASE/pw-mcp.sh"
  mkdir -p "$BASE/pw-mcp"
  [ -x "$BASE/pw-mcp/node_modules/.bin/playwright-mcp" ] || (cd "$BASE/pw-mcp" && npm install --save-exact @playwright/mcp@0.0.82)
  # Browser binary: the distro Chromium when there is one (Debian, Raspberry Pi OS); otherwise
  # Playwright's own build (Ubuntu ships Chromium only as a snap, which cannot run under a
  # user unit without a session).
  CHROMIUM="$(command -v chromium || true)"
  if [ -z "$CHROMIUM" ]; then
    (cd "$BASE/pw-mcp" && npx playwright install chromium >/dev/null)
    CHROMIUM="$(find "$HOME/.cache/ms-playwright" -path '*/chrome-linux*/chrome' -type f | sort | tail -1)"
    # Ubuntu 24.04+ restricts unprivileged user namespaces with AppArmor; Chromium's sandbox
    # then aborts with "No usable sandbox". Grant userns to this binary path only (the
    # distro-endorsed fix; keeps the sandbox on rather than --no-sandbox).
    if [ -d /etc/apparmor.d ] && [ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null)" = 1 ] && sudo -n true 2>/dev/null; then
      sudo tee /etc/apparmor.d/playwright-chromium >/dev/null <<'AA'
abi <abi/4.0>,
include <tunables/global>
profile playwright-chromium /home/*/.cache/ms-playwright/chromium-*/chrome-linux*/chrome flags=(unconfined) {
  userns,
  include if exists <local/playwright-chromium>
}
AA
      sudo apparmor_parser -r /etc/apparmor.d/playwright-chromium
    fi
  fi
  [ -n "$CHROMIUM" ] || { echo "no Chromium found and Playwright download failed" >&2; exit 78; }
  # A headless seat has no Xwayland and no locale: run Chromium on Wayland natively, in English.
  cat >"$BASE/pw-config.json" <<'JSON'
{ "browser": { "launchOptions": { "args": ["--ozone-platform=wayland", "--lang=en-US"] },
               "contextOptions": { "locale": "en-US" } } }
JSON
  # Fonts: a server install has none, and pages render as boxes.
  if ! fc-list 2>/dev/null | grep -qiE "dejavu|liberation|noto"; then
    sudo -n apt-get install -y -qq fonts-dejavu-core fonts-liberation fonts-noto-cjk fonts-noto-color-emoji >/dev/null 2>&1 || \
      echo "note: install fonts (fonts-dejavu-core fonts-noto-cjk) or pages render as boxes" >&2
  fi
  cat >"$UNITS/oab-pw-mcp.service" <<UNIT
[Unit]
Description=Playwright MCP for the oab-instance-mcp hands node (loopback :8794)
After=graphical-session.target oab-seat.service

[Service]
ExecStart=$BASE/pw-mcp.sh
Environment=XDG_RUNTIME_DIR=/run/user/%U
Environment=WAYLAND_DISPLAY=$WL
Environment=DISPLAY=:0
Environment=PW_CHROMIUM=$CHROMIUM
Environment=PW_NODE_BIN=$NODE_BIN
Restart=always
RestartSec=3

[Install]
WantedBy=default.target
UNIT
  UPSTREAM="Environment=MCP_UPSTREAM=browser=http://127.0.0.1:8794/mcp"
  if sudo -n true 2>/dev/null; then
    sudo mkdir -p /etc/chromium/policies/managed
    echo '{"DefaultMediaStreamSetting": 2, "DefaultNotificationsSetting": 2, "DefaultGeolocationSetting": 2}' \
      | sudo tee /etc/chromium/policies/managed/oab-instance-mcp.json >/dev/null
  else
    echo "note: could not write Chromium policy (no sudo); camera/mic prompts will need a mouse click" >&2
  fi
else
  echo "browser tools skipped (--no-browser, or node/npm missing)"
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
Environment=WAYLAND_DISPLAY=$WL
Environment=XDG_RUNTIME_DIR=/run/user/%U
Restart=always
RestartSec=2
# Apps opened via `bash` belong to the desktop, not to this daemon: a restart must not kill them.
KillMode=process

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
