#!/bin/bash
# Playwright MCP beside the Linux hands node. Loopback only; the daemon re-serves it
# via --upstream (never exposed directly). Mirrors poc/pw-mcp/pw-mcp.sh for macOS.
#
# Differences from the Mac:
#  - the headed browser must be launched INTO the desktop seat: WAYLAND_DISPLAY /
#    XDG_RUNTIME_DIR (and DISPLAY for the Xwayland labwc runs). A systemd user unit
#    does not inherit them, so they are set here.
#  - uses the distro Chromium (--executable-path) when present (Debian / Raspberry Pi OS), else
#    Playwright's own build; install-linux.sh sets PW_CHROMIUM. On a headless seat there is no
#    Xwayland, so pw-config.json makes Chromium use Wayland natively (--ozone-platform=wayland).
#  - PW_NODE_BIN: the directory of a non-system node (fnm/nvm) so npm/npx resolve.
export PATH="${PW_NODE_BIN:+$PW_NODE_BIN:}/usr/local/bin:/usr/bin:/bin"
export LANG="${LANG:-en_US.UTF-8}"
BASE="$HOME/.local/oab-instance-mcp"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-0}"
export DISPLAY="${DISPLAY:-:0}"
CHROMIUM="${PW_CHROMIUM:-/usr/bin/chromium}"
PW_CONFIG="${PW_CONFIG:-$([ -f "$BASE/pw-config.json" ] && echo "$BASE/pw-config.json")}"
cd "$BASE/pw-mcp"
exec ./node_modules/.bin/playwright-mcp \
  --host 127.0.0.1 --port 8794 \
  --allowed-hosts "127.0.0.1,localhost,127.0.0.1:8794,localhost:8794" \
  --browser chromium --executable-path "$CHROMIUM" \
  --user-data-dir "$BASE/pw-profile" \
  --output-dir "$BASE/pw-output" \
  --idle-timeout 1800000 \
  ${PW_CONFIG:+--config "$PW_CONFIG"} \
  --shared-browser-context --caps vision,pdf
