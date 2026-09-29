#!/usr/bin/env bash
# Playwright MCP beside oab-instance-mcp: a headed browser in the desktop session, re-served
# by the daemon as browser_* tools (--upstream browser=http://127.0.0.1:8794/mcp). Loopback
# only — it has no auth of its own; the daemon's auth and tool profiles guard it.
# Linux port of poc/pw-mcp/pw-mcp.sh; the gotchas documented there apply.
set -euo pipefail

base="${XDG_DATA_HOME:-$HOME/.local/share}/oab-instance-mcp"
state="${XDG_STATE_HOME:-$HOME/.local/state}/oab-instance-mcp"
mkdir -p "$state/pw-output"

# A system Google Chrome saves a ~150 MB Chromium download; otherwise use Playwright's own
# (install it once with: npx playwright install chromium).
browser=chromium
if [[ -x /opt/google/chrome/chrome ]]; then
  browser=chrome
fi

cd "$base/pw-mcp"
# --allowed-hosts matches the Host header exactly; the daemon sends the bare host.
# --shared-browser-context: every MCP session (each agent run) reuses one browser on the
#   persistent profile instead of fighting over it ("Browser is already in use").
# --user-data-dir: its own profile, never the user's Chrome profile.
exec ./node_modules/.bin/playwright-mcp \
  --host 127.0.0.1 --port 8794 \
  --allowed-hosts "127.0.0.1,localhost" \
  --user-data-dir "$base/pw-profile" \
  --output-dir "$state/pw-output" \
  --idle-timeout 1800000 \
  --browser "$browser" --shared-browser-context --caps vision,pdf
