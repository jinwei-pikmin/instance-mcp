#!/bin/bash
# Playwright MCP beside the Linux hands node. Loopback only; the daemon re-serves it
# via --upstream (never exposed directly). Mirrors poc/pw-mcp/pw-mcp.sh for macOS.
#
# Differences from the Mac:
#  - the headed browser must be launched INTO the desktop seat: WAYLAND_DISPLAY /
#    XDG_RUNTIME_DIR (and DISPLAY for the Xwayland labwc runs). A systemd user unit
#    does not inherit them, so they are set here.
#  - uses the distro Chromium (--executable-path) instead of Playwright's download;
#    Debian's build is arm64-native and already installed on Raspberry Pi OS.
export PATH=/usr/local/bin:/usr/bin:/bin
BASE="$HOME/.local/oab-instance-mcp"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-0}"
export DISPLAY="${DISPLAY:-:0}"
CHROMIUM="${PW_CHROMIUM:-/usr/bin/chromium}"
cd "$BASE/pw-mcp"
exec ./node_modules/.bin/playwright-mcp \
  --host 127.0.0.1 --port 8794 \
  --allowed-hosts "127.0.0.1,localhost,127.0.0.1:8794,localhost:8794" \
  --browser chromium --executable-path "$CHROMIUM" \
  --user-data-dir "$BASE/pw-profile" \
  --output-dir "$BASE/pw-output" \
  --idle-timeout 1800000 \
  --shared-browser-context --caps vision,pdf
