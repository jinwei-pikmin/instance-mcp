# reverse-attach (Linux hands node, Rust PoC)

Step-by-step install on a fresh box: [`docs/linux-setup.md`](../../docs/linux-setup.md).

Part of [#27](https://github.com/openabdev/instance-mcp/issues/27) (hands-node registry) and
[#15](https://github.com/openabdev/instance-mcp/issues/15) (Linux-first Rust port). A single
self-contained binary that makes a Linux box a lendable "hands" node for an openab-pty session,
mirroring the Swift `ReverseAttachClient` / `POST /attach` contract:

- `POST /attach` `{runtime, session, profile, ttl_secs, secret | admin_credential}` → HTTP 202
  with the same grant shape Connect decodes: `{id, runtime, session, profile, principal, state,
  expires_in_secs}`. Exactly one credential; the admin path mints at the runtime with
  `{"ttl_secs": N}` (ws:// only in this PoC) and forwards TTL end to end.
- `GET /attach` → `{grants:[…]}`; `GET /attach/{id}` → one grant; `DELETE /attach/{id}` →
  204 and promptly cancels the dial/socket. New grant for the same `(runtime, session)` replaces
  the old one. States match Swift: `idle / dialing / attached / redialing / ended` plus optional
  `ended` reason (`revoked`, `handshake_rejected_401`, `deadline`, …).
- Dial loop: `GET ws://…/tools/attach/{session}` with `Authorization: Bearer <secret>`, redial
  1 s → 30 s backoff until the grant deadline. Disposition per openab-pty §9.2: close `4001`
  expired / `4002` replaced / `4004` session ended / `4010` revoked and handshake non-2xx/429/5xx
  → **stop**; `1000` / `4006` / other / 2xx / 429 / 5xx → **redial**. Same table the
  `reverse-attach-conformance` suite (#24) checks 23/23 against the Swift oracle.
- MCP over the socket: `initialize`, `tools/list`, `tools/call`; notifications get no reply;
  ping → pong. Tools: `sys_info`, `screenshot`, `bash` — in **both** profiles (decision
  2026-09-28: a lent node is only useful if the agent can act on it, and the macOS sandbox
  profile already hands out a shell through `osascript`'s `do shell script`). `bash` = `bash -c`
  as the daemon user with `cwd` (`~` expands), `timeout_secs` (default 60, max 600; the whole
  process group is killed → exit 137, `timed_out=true`), `max_output_bytes` per stream (≤1 MiB).

Deps: `serde_json` + `tungstenite` (rustls). Sync std threads, no tokio. ~28 KB.

## Verified on rpi1 (Debian 13, aarch64, Rust 1.98) — 2026-09-27

```sh
cargo build --release          # 31 s cold, 2.2 MB binary
bash smoke.sh                  # RESULT: 36 passed, 0 failed
```

`smoke.sh` runs the binary against `mock_runtime.py` (stdlib-only mock openab-pty: mint endpoint
+ WS attach endpoint that drives a scripted MCP turn and closes with a scripted code). Covered:

| area | checks |
|---|---|
| validation | non-ws runtime, bad session, both/neither credential, ttl 0, wrong admin → `400 mint failed: mint HTTP 401`, 404 route |
| mint path | `ttl_secs=60` seen by the runtime's mint endpoint; `expires_in_secs=60` in the grant |
| control contract | `POST /attach` 202 with every Connect-required grant field; `GET /attach` wrapper; `GET /attach/{id}`; `DELETE` 204 + removal; principal and dynamic expiry |
| redial / stop | close `1000` → redialed after 1 s; close `4010` → `state=ended, ended=revoked`; wrong secret → `ended=handshake_rejected_401` with exactly one attempt; unreachable runtime → `ended=deadline` |
| MCP | `serverInfo.name=instance-mcp-rpi`; owner list = `sys_info,screenshot,exec`; `sys_info.hostname=rpi1`; `exec` ran on the node; unknown tool / method → `-32601`; notification silent; ping → pong |
| sandbox | list = `sys_info,screenshot`; forced `exec` → error, not executed |
| close handshake | client echoes the Close frame (was a bare EOF before the `socket.flush()` fix in `dial_loop`) |

## Direct `/mcp` for OpenAB Connect's Screens pane (added 2026-09-28)

The same binary now also serves MCP Streamable-HTTP (JSON mode) on `POST /mcp`, gated by an
`AuthPolicy` with the Swift daemon's semantics — `MCP_TOKEN` / `MCP_TOKEN_FILE` (bearer,
constant-time) AND `MCP_ALLOW_LOGIN` (matched against the `Tailscale-User-Login` header that
`tailscale serve` injects); `MCP_INSECURE_LOCAL=1` for loopback debugging; refuses to start with
nothing set. `/healthz` is open. `screenshot` returns MCP `image` content (grim → PNG; a `jpeg`
request falls back to PNG because Debian's grim lacks libjpeg — Connect decodes by content).
`sys_info` carries the `host` / `displays` / `permissions.screen_recording` / `agent.version`
fields the Screens pane reads.

On rpi1: systemd user unit `oab-instance-mcp.service` (BIND 127.0.0.1:8795, seat env
`WAYLAND_DISPLAY=wayland-0`, linger on) + `tailscale serve --bg --https=8444 http://127.0.0.1:8795`
→ `https://rpi1.tailb836bb.ts.net:8444/mcp`. Verified from the laptop: wrong token 401,
initialize 200 with `Mcp-Session-Id`, `sys_info` host=rpi1 displays=1, Connect-shaped screenshot
call (`display 0, scale 1, quality 0.6, format jpeg`) → 1920×1080 PNG in 1.35 s. Pi OS's labwc
desktop keeps a 1920×1080 headless output alive with no monitor attached, so no sway needed.

## `mouse` / `key` (added 2026-09-28, #26)

`mouse` → a persistent wlroots virtual pointer held by the daemon (`src/platform/linux/seat.rs`;
move / click / double_click / right_click / drag / scroll in wheel notches, display
pixels = screenshot pixels at scale 1); `key` → `wtype` (`type` unicode text, `press` combos like
`ctrl+shift+t`, `Return`, `Escape`; modifiers ctrl/shift/alt/super). Both run against the seat's
`wayland-0`; labwc (wlroots) accepts the virtual-pointer and virtual-keyboard protocols with no
config. Verified from the laptop: click into a field → `type` → screenshot shows the text →
`press Escape` clears it, ~0.3 s per call.

## Browser tools via `MCP_UPSTREAM` (added 2026-09-28)

Same pattern as the Mac: `@playwright/mcp` runs beside the daemon on loopback and the daemon
re-serves its tools under its own `tools/list`, filtered by the connection's profile.

- `pw-mcp.sh` + systemd user unit `oab-pw-mcp.service`: `@playwright/mcp@0.0.82` pinned (same
  as `poc/pw-mcp`), **headed** Chromium launched into the labwc seat (`WAYLAND_DISPLAY`,
  `XDG_RUNTIME_DIR`, `DISPLAY=:0` for labwc's Xwayland), using the distro Chromium
  (`--executable-path /usr/bin/chromium`, arm64-native, no Playwright download), persistent
  profile, `--allowed-hosts` must include the `host:port` form or every request is 403.
- Daemon: `MCP_UPSTREAM=browser=http://127.0.0.1:8794/mcp`. Mirrors Swift `UpstreamMCP`:
  request/response Streamable HTTP, upstream `Mcp-Session-Id` held and re-established on
  400/404 (the stale id must be dropped *before* re-`initialize`, or the upstream 404s that too),
  SSE or JSON replies, `tools/list` cached 30 s (failures not cached), upstream down → its
  tools absent. Local names win on collision.
- Profile filter is the verbatim Swift `ToolProfile.sandboxBrowserTools` list (16 tools).
  `owner` sees all 32; `sandbox` never sees `run_code_unsafe`, upload, pdf, network, raw
  mouse-by-coordinate, dialogs, `browser_close`, or any new upstream tool.
- Chromium managed policy `/etc/chromium/policies/managed/oab-instance-mcp.json` blocks
  camera/mic/notification/geolocation prompts: YouTube triggered the xdg-desktop-portal
  "Allow app to use the Camera?" dialog, which nothing but `mouse` could dismiss.

Verified on rpi1: owner `/mcp` → 37 tools (5 local + 32 browser); after a pw-mcp restart the
next call re-initializes and still lists 37. Lent to `kiro-1040` session `mac` (sandbox):
22 tools = 5 local + 16 browser + `instance_status`; forced `browser_run_code_unsafe` →
`-32601 unknown tool`; `browser_navigate` example.com 13 s (Pi 4 class, first page), then
`browser_snapshot` → `heading "Example Domain"`, `link "Learn more"`. The Chromium window is
on rpi1's desktop, so `screenshot` / Connect's Screens pane show what the agent is doing.

## Grant persistence (#12)

Live grants are written to `MCP_GRANTS_FILE` (default `$XDG_STATE_HOME/oab-instance-mcp/grants.json`,
`off` disables), mode 600 in a 0700 directory, replaced atomically on every create / replace /
`DELETE` / terminal end. On start, grants still inside their deadline are re-dialled under the same
id. Smoke covers `kill -9` → restart → redial, same id, secret absent from `GET /attach`, and a
revoked grant not resumed.

## Not yet (vs the Swift implementation)

- `/mcp` has no real session table (an `Mcp-Session-Id` is issued but not checked) and no SSE stream.
- Screenshot is PNG only (≈2 MB per 1080p frame); Connect polls at ≤2 FPS, so expect ~4 MB/s. A
  JPEG encoder in-process (or a grim with libjpeg) is the fix.
- `wss://` runtimes: dial works (rustls), mint over https does not.
- Real-runtime test against the p1 openab-pty pod is pending: rpi1 is **not on the tailnet**
  (no `tailscale` binary; `100.111.174.31:8090` times out) and the pod's `PTY_ADMIN_HASH` is a
  hash, so the admin credential must come from the operator.
