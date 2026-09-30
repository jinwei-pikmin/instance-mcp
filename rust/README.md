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
| `screenshot` / `mouse` / `key` | ✅ | ✅ xdg-desktop-portal (GNOME verified; KDE should work, untested); same schema and point coordinates. wlroots (grim/ydotool) not yet |
| `osascript` | ✅ | — no Linux equivalent; use `exec` (`gdbus`, `xdg-open`) |
| Reverse attach (`POST/GET /attach`, `DELETE /attach/{id}`, `--no-attach`) | ✅ | ✅ same grant API, close-code policy and backoff; replies are sent concurrently by JSON-RPC id |
| Reverse-attach grants survive a restart (`grants.json`, `--no-grant-persistence`) | ✅ (#43, secret in Keychain) | ✅ same resume-under-original-id; secret in the 0600 file, as the upstream Linux PoC (#42) |
| `--upstream` (re-serve Playwright MCP as `browser_*`) | ✅ | ✅ same merge / profile filter / session re-init; `install.sh` sets up Playwright MCP when Node.js is present |

Live grants are saved to `$XDG_STATE_HOME/oab-instance-mcp/grants.json` (0600 in a 0700 dir,
atomic) on create / replace / revoke / sweep / end, and re-dialled under their original id after a
restart until they expire; `--no-grant-persistence` keeps them in memory only.

Reverse attach: a human `POST /attach {runtime, session, profile, ttl_secs, secret | admin_credential}`
with the same credential as `/mcp`; this machine dials `{runtime}/tools/attach/{session}` over
`ws://` or `wss://` (rustls, webpki roots) and serves MCP on that socket scoped to the profile.
Under `sandbox` there is no `exec*`; a lent sandbox gets `sys_info`, `screenshot`, `mouse`, `key`
and the allowlisted `browser_*` subset (navigate / read / interact — no `browser_run_code_unsafe`,
file upload, PDF, network inspection, dialogs or `browser_close`).

Desktop (portal backend), verified on GNOME 50 Wayland at 5/3 fractional scaling:

- **Consent once.** The first desktop call shows GNOME's "Remote Desktop" dialog on this
  machine's screen; turn on *Allow Remote Interaction* and Share. The restore token is kept in
  `~/.config/oab-instance-mcp/portal-restore-token` (0600), so later sessions start silently.
  Delete it to revoke. The session closes after 5 min idle (the top-bar indicator goes away).
- **Screenshots** go through the Screenshot portal, which writes a PNG into `~/Pictures`; the
  daemon reads and deletes that file.
- **Pointer on fractional scaling.** The portal validates absolute motion against the logical
  stream size while Mutter reads it in physical pixels, so the daemon moves absolutely as far as
  allowed and finishes with relative motion. Monitor scales come from `org.gnome.Mutter.DisplayConfig`.
- **Typing.** ASCII goes as keysyms; other characters (CJK, accents, symbols) go through the
  IBus/GTK Unicode entry (`ctrl+shift+u`, hex, space), which GTK/Qt apps and browsers accept.
- **Shortcuts** use ctrl; `cmd` is accepted as an alias for ctrl.
- `--no-desktop` turns the three tools off; they are off automatically without a graphical session.
- The service runs with `KillMode=process`, so apps opened via `exec` survive restarts.

Browser (`--upstream browser=http://127.0.0.1:8794/mcp`): `install.sh` installs
`@playwright/mcp` (pinned 0.0.82, as the Swift build) under `~/.local/share/oab-instance-mcp/pw-mcp`
and runs it as the systemd user service `oab-pw-mcp` on loopback only — no auth of its own; the
daemon's auth and profiles front it. The browser is **headed** (a real window on the desktop) with
its own persistent profile, never the user's; it uses the system Google Chrome when present,
otherwise Playwright's Chromium. The daemon re-serves its 32 tools, local names win on collision,
an upstream that is down simply contributes nothing, and a lost upstream session is re-initialized
once. `--no-browser` skips it.

Named tokens and custom profiles — give each caller only what it needs (operator-managed;
a client never picks its own profile). Full reference: [docs/access-control.md](docs/access-control.md).

```sh
cp ~/.config/oab-instance-mcp/profiles.example.toml ~/.config/oab-instance-mcp/profiles.toml
oab-instance-mcp profile list                      # owner, sandbox + yours (risky tools flagged)
oab-instance-mcp token add hermes --profile browser  # prints the token once; stores its SHA-256
oab-instance-mcp token list
oab-instance-mcp token revoke hermes               # effective on the next request, no restart
```

A named token is sent as the caller's Bearer instead of the main token; the Tailscale login
check still applies. Profiles: `allow` / `deny` glob patterns, deny wins, anything not allowed
is denied (including tools an upstream adds later). A narrowed caller gets a scoped server —
omitted tools are absent from `tools/list` and "unknown" on `tools/call`, and `sys_info` reports
the narrowed list. `/attach` accepts the owner token only; it can lend with a custom profile by
name. Both files are re-read on change; a broken file is rejected and the previous version stays.

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

It also publishes a tailnet-only **plain-HTTP twin** at `http://<node>.<tailnet>.ts.net:8080/mcp`
(`--http-port 0` to skip). Same auth; the tailnet is WireGuard-encrypted. It exists for callers
that reach the tailnet through an HTTP proxy — e.g. an agent on a PaaS (Zeabur, Fly, …) next to a
userspace `tailscale` container with `--outbound-http-proxy-listen`: such a caller sets only
`HTTP_PROXY`, so its `https://` traffic (LLM APIs, the web) stays direct.

Caller config:

```sh
claude mcp add --transport http <name> https://<node>.<tailnet>.ts.net:8444/mcp \
  --header "Authorization: Bearer $(cat ~/.config/oab-instance-mcp/token)"
```

Logs: `journalctl --user -u oab-instance-mcp -f`.
