# oab-instance-mcp — Rust / Linux build

Phase 1 of [`docs/adr/linux-rust-port.md`](../docs/adr/linux-rust-port.md): the MCP core
re-implemented in Rust with a Linux backend. Same flags, auth and wire contract as the Swift
build, so callers (kiro-cli, claude, openab) configure it exactly like the Mac one.

| | Swift (macOS) | Rust (Linux) |
|---|---|---|
| MCP Streamable HTTP, sessions, `/healthz` | ✅ | ✅ |
| Auth: `--allow-login` AND bearer token, `--insecure-local` | ✅ | ✅ same decision table |
| `sys_info` | ✅ | ✅ `/proc`, `/sys`, os-release, session env, tailnet IPs |
| `exec`, `exec_start/poll/list/cancel` | ✅ `zsh -f` | ✅ `bash --noprofile --norc`, `setsid` + `killpg` |
| `screenshot` / `mouse` / `key` | ✅ | ⏳ phase 3 (xdg-desktop-portal ScreenCast/RemoteDesktop on GNOME/KDE, grim/ydotool on wlroots) |
| `osascript` | ✅ | — no Linux equivalent; use `exec` (`gdbus`, `xdg-open`) |
| Reverse attach (`/attach`), `--upstream` | ✅ | ⏳ phase 2 |

Job logs: `$XDG_STATE_HOME/oab-instance-mcp/jobs/<job_id>.out|.err` (default `~/.local/state/…`).

## Build and test

```sh
cd rust
cargo test
cargo build --release
```

## Install on a Linux desktop

```sh
rust/deploy/install.sh                    # allow-login defaults to this node's Tailscale login
```

It builds the release binary into `~/.local/bin`, creates `~/.config/oab-instance-mcp/token`
(mode 600, kept across re-installs), installs and starts the systemd **user** service
`oab-instance-mcp` bound to `127.0.0.1:8795`, and runs
`tailscale serve --bg --https=8444 http://127.0.0.1:8795` so callers reach it at
`https://<node>.<tailnet>.ts.net:8444/mcp` with the Tailscale identity injected.
`tailscale serve` needs operator rights once: `sudo tailscale set --operator=$USER`.

Caller config:

```sh
claude mcp add --transport http <name> https://<node>.<tailnet>.ts.net:8444/mcp \
  --header "Authorization: Bearer $(cat ~/.config/oab-instance-mcp/token)"
```

Logs: `journalctl --user -u oab-instance-mcp -f`.
