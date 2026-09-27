#!/usr/bin/env bash
# Install the Rust oab-instance-mcp on this Linux machine as a systemd *user* service and
# publish it on the tailnet with `tailscale serve` (TLS + Tailscale-User-Login injection).
#
#   rust/deploy/install.sh [--allow-login you@example.com] [--https-port 8444]
#
# Re-runnable: keeps an existing token, rebuilds and restarts the service.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
crate="$(dirname "$here")"
allow_login=""
https_port=8444
local_port=8795

while [[ $# -gt 0 ]]; do
  case "$1" in
    --allow-login) allow_login="$2"; shift 2 ;;
    --https-port) https_port="$2"; shift 2 ;;
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

echo "==> installing systemd user service"
unit_dir="$HOME/.config/systemd/user"
install -d "$unit_dir"
sed "s|@ALLOW_LOGIN@|$allow_login|" "$here/oab-instance-mcp.service" > "$unit_dir/oab-instance-mcp.service"
systemctl --user daemon-reload
systemctl --user enable oab-instance-mcp.service >/dev/null
systemctl --user restart oab-instance-mcp.service
sleep 1
curl -fsS "http://127.0.0.1:$local_port/healthz" >/dev/null && echo "healthz ok"

echo "==> tailscale serve :$https_port -> 127.0.0.1:$local_port"
if tailscale serve status --json 2>/dev/null | python3 -c '
import json, sys
port, target = sys.argv[1], sys.argv[2]
web = (json.load(sys.stdin) or {}).get("Web") or {}
sys.exit(0 if any(k.endswith(":" + port) and any(h.get("Proxy") == target for h in (v.get("Handlers") or {}).values())
                  for k, v in web.items()) else 1)' "$https_port" "http://127.0.0.1:$local_port"; then
  echo "already configured"
elif ! tailscale serve --bg --https="$https_port" "http://127.0.0.1:$local_port"; then
  echo >&2
  echo "tailscale serve failed (output above). Common causes:" >&2
  echo "  - Serve/HTTPS not enabled on the tailnet: open the link above as a tailnet admin" >&2
  echo "  - no operator rights: sudo tailscale set --operator=\$USER" >&2
  echo "Then re-run this script; the service itself is already running on 127.0.0.1:$local_port." >&2
  exit 1
fi

dns="$(tailscale status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["Self"]["DNSName"].rstrip("."))')"
echo
echo "MCP URL:  https://$dns:$https_port/mcp"
echo "Token:    $token_file"
