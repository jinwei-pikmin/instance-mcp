#!/usr/bin/env bash
# Install the Rust oab-instance-mcp on this Linux machine as a systemd *user* service and
# publish it on the tailnet with `tailscale serve` (TLS + Tailscale-User-Login injection).
#
#   rust/deploy/install.sh [--allow-login you@example.com] [--https-port 8444] [--http-port 8080|0]
#                          [--no-browser]
#
# Re-runnable: keeps an existing token, rebuilds and restarts the service.
# --http-port 0 skips the tailnet-only plain-HTTP entry.
# With Node.js available it also installs the Playwright MCP (headed browser, loopback only)
# and re-serves its browser_* tools through the daemon; --no-browser skips that.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
crate="$(dirname "$here")"
allow_login=""
https_port=8444
http_port=8080
local_port=8795
browser=auto
pw_version=0.0.82   # pinned, same as the Swift build's poc/pw-mcp

while [[ $# -gt 0 ]]; do
  case "$1" in
    --allow-login) allow_login="$2"; shift 2 ;;
    --https-port) https_port="$2"; shift 2 ;;
    --http-port) http_port="$2"; shift 2 ;;
    --no-browser) browser=no; shift ;;
    *) echo "unknown flag $1" >&2; exit 64 ;;
  esac
done

if [[ -z "$allow_login" ]]; then
  # Our own Tailscale login: the intended shape is an allow-list of one.
  allow_login="$(tailscale status --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["User"][str(d["Self"]["UserID"])]["LoginName"])')"
fi
echo "allow-login: $allow_login"

echo "==> building release binary"
(cd "$crate" && cargo build --release --locked 2>&1 | tail -2)
install -Dm755 "$crate/target/release/oab-instance-mcp" "$HOME/.local/bin/oab-instance-mcp"

token_file="$HOME/.config/oab-instance-mcp/token"
if [[ ! -s "$token_file" ]]; then
  echo "==> generating bearer token at $token_file"
  install -d -m700 "$(dirname "$token_file")"
  (umask 077; head -c 32 /dev/urandom | base64 | tr -d '=+/\n' > "$token_file")
fi
chmod 600 "$token_file"
# Custom profiles are opt-in: leave the example beside the real file, never activate it.
install -m600 "$here/profiles.example.toml" "$(dirname "$token_file")/profiles.example.toml"

unit_dir="$HOME/.config/systemd/user"
install -d "$unit_dir"
extra_args=""
if [[ "$browser" == auto ]] && command -v node >/dev/null && command -v npm >/dev/null; then
  echo "==> Playwright MCP (browser_* tools) on 127.0.0.1:8794"
  data="${XDG_DATA_HOME:-$HOME/.local/share}/oab-instance-mcp"
  install -d "$data/pw-mcp"
  (cd "$data/pw-mcp" && { [[ -f package.json ]] || npm init -y >/dev/null; } \
    && npm install --save-exact --no-fund --no-audit "@playwright/mcp@$pw_version" >/dev/null)
  if [[ ! -x /opt/google/chrome/chrome ]]; then
    echo "   no Google Chrome: fetching Playwright's Chromium (~150 MB, once)"
    (cd "$data/pw-mcp" && npx --yes playwright install chromium)
  fi
  install -m755 "$here/pw-mcp.sh" "$data/pw-mcp.sh"
  # nvm/asdf node is not on systemd's PATH: bake its directory into the unit.
  sed "s|@NODE_DIR@|$(dirname "$(command -v node)")|" "$here/oab-pw-mcp.service" > "$unit_dir/oab-pw-mcp.service"
  systemctl --user daemon-reload
  systemctl --user enable oab-pw-mcp.service >/dev/null
  systemctl --user restart oab-pw-mcp.service
  extra_args="--upstream browser=http://127.0.0.1:8794/mcp"
elif [[ "$browser" == auto ]]; then
  echo "==> no Node.js: skipping the browser (install node and re-run for browser_* tools)"
fi

echo "==> installing systemd user service"
sed -e "s|@ALLOW_LOGIN@|$allow_login|" -e "s|@EXTRA_ARGS@|$extra_args|" \
  "$here/oab-instance-mcp.service" > "$unit_dir/oab-instance-mcp.service"
systemctl --user daemon-reload
systemctl --user enable oab-instance-mcp.service >/dev/null
systemctl --user restart oab-instance-mcp.service
# `a && b` does not trip `set -e`, so check explicitly (and give the service a moment).
for _ in $(seq 10); do
  curl -fsS "http://127.0.0.1:$local_port/healthz" >/dev/null 2>&1 && break
  sleep 0.5
done
if ! curl -fsS "http://127.0.0.1:$local_port/healthz" >/dev/null 2>&1; then
  echo "service did not come up; see: journalctl --user -u oab-instance-mcp -n 50" >&2
  exit 1
fi
echo "healthz ok"

# serve_port https|http PORT — publish the daemon on the tailnet (idempotent).
serve_port() {
  local scheme="$1" port="$2"
  echo "==> tailscale serve $scheme :$port -> 127.0.0.1:$local_port"
  if tailscale serve status --json 2>/dev/null | python3 -c '
import json, sys
port, target = sys.argv[1], sys.argv[2]
web = (json.load(sys.stdin) or {}).get("Web") or {}
sys.exit(0 if any(k.endswith(":" + port) and any(h.get("Proxy") == target for h in (v.get("Handlers") or {}).values())
                  for k, v in web.items()) else 1)' "$port" "http://127.0.0.1:$local_port"; then
    echo "already configured"
  elif ! tailscale serve --bg --"$scheme"="$port" "http://127.0.0.1:$local_port"; then
    echo >&2
    echo "tailscale serve failed (output above). Common causes:" >&2
    echo "  - Serve/HTTPS not enabled on the tailnet: open the link above as a tailnet admin" >&2
    echo "  - no operator rights: sudo tailscale set --operator=\$USER" >&2
    echo "Then re-run this script; the service itself is already running on 127.0.0.1:$local_port." >&2
    exit 1
  fi
}

serve_port https "$https_port"
# Plain-HTTP twin, still tailnet-only (WireGuard-encrypted, same identity headers and token).
# For callers that reach the tailnet through an HTTP proxy: they can set only HTTP_PROXY and
# keep every https:// call (LLM APIs, the web) direct.
if [[ "$http_port" != 0 ]]; then
  serve_port http "$http_port"
fi

dns="$(tailscale status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["Self"]["DNSName"].rstrip("."))')"
echo
echo "MCP URL:  https://$dns:$https_port/mcp"
if [[ "$http_port" != 0 ]]; then
  echo "          http://$dns:$http_port/mcp   (tailnet-only plain HTTP, for proxied callers)"
fi
echo "Token:    $token_file   (owner)"
echo
echo "Narrower access for a caller (e.g. a cloud agent): define a profile in"
echo "  $(dirname "$token_file")/profiles.toml   (see profiles.example.toml there)"
echo "then:  oab-instance-mcp token add <name> --profile <profile>"
