# Access control: named tokens and tool profiles

`oab-instance-mcp` gives whoever connects real hands on this machine: a shell, the desktop, a
browser. Named tokens and tool profiles let the machine's **operator** hand each caller only the
tools it needs — e.g. a cloud agent that should browse the web but never run commands.

The operator decides; a client never picks its own profile. If a client could choose, a
compromised client would simply choose `owner`.

## How a request is decided

Every request to `/mcp` or `/attach` passes three checks, in order:

| # | Check | Configured by | Fails with |
|---|---|---|---|
| 1 | **Network** — only tailnet nodes reach the daemon (`tailscale serve`, never Funnel) | `install.sh` | no connection |
| 2 | **Credential** — the `Authorization: Bearer` value is the main token *or* a named token | `--token-file`, `tokens.toml` | `401` (`bad token`, `missing Authorization header`) |
| 3 | **Identity** — the `Tailscale-User-Login` that `tailscale serve` injects is on the allow-list | `--allow-login` | `401` (`login … not allowed`) |

Checks 2 and 3 are AND-combined, exactly as in the Swift build: a named token narrows what a
caller gets, it never replaces the Tailscale identity.

The credential then decides the **profile**:

| Credential | Profile |
|---|---|
| main token (`~/.config/oab-instance-mcp/token`) | `owner` — every tool |
| named token | the profile it was created with |
| named token whose profile is not defined | **denied** (`401 … names undefined profile`) — never widened |

With the profile known:

- **`/mcp`**: an `owner` caller gets the full server. Any other profile gets a *scoped* server:
  tools the profile does not allow are absent from `tools/list`, a `tools/call` for one is
  `Invalid params: unknown tool: <name>` (indistinguishable from a tool that never existed), and
  `sys_info` reports the narrowed list. The model also gets a note that it is on a restricted
  profile.
- **`/attach`** (reverse attach): **owner only**, `403` otherwise. A narrowed caller must not be
  able to lend this machine to anyone, with any profile it likes. The owner may lend with a
  built-in or a custom profile by name: `POST /attach {"profile": "browser", …}`.

Logs name the caller and, for named tokens, the token:

```
jinwei@example.com [hermes] tools/call browser_navigate
deny GET /attach for jinwei@example.com [hermes]: /attach needs the owner profile
```

## Quick start

Give a cloud agent (here: Hermes) the browser and nothing else.

```sh
# 1. Define profiles (once). The installer leaves an example next to the config.
cp ~/.config/oab-instance-mcp/profiles.example.toml ~/.config/oab-instance-mcp/profiles.toml
oab-instance-mcp profile list

# 2. Create a token bound to a profile. It is printed ONCE; only its hash is stored.
oab-instance-mcp token add hermes --profile browser

# 3. Give the printed oabt_… value to the caller as its Bearer token
#    (for Hermes: the MCP_<SERVER>_API_KEY variable), then reload its MCP tools.

# Later: see or withdraw access. Revocation applies to the next request, no restart.
oab-instance-mcp token list
oab-instance-mcp token revoke hermes
```

## Profiles

### Built in

| Profile | Tools |
|---|---|
| `owner` | everything |
| `sandbox` | everything except `exec*`; of the `browser_*` tools only: `browser_navigate`, `browser_navigate_back`, `browser_snapshot`, `browser_find`, `browser_click`, `browser_type`, `browser_fill_form`, `browser_press_key`, `browser_hover`, `browser_select_option`, `browser_wait_for`, `browser_tabs`, `browser_take_screenshot`, `browser_console_messages`, `browser_resize`, `browser_evaluate` |

`sandbox` is the default for reverse attach (an agent in an openab-pty session already has a
shell of its own). Built-in profiles cannot be redefined.

### Custom: `~/.config/oab-instance-mcp/profiles.toml`

```toml
[profiles.browser]
allow = ["sys_info", "browser_*"]
deny  = ["browser_run_code_unsafe", "browser_file_upload", "browser_pdf_save"]
```

Rules:

- **Default deny.** Only tools matched by an `allow` pattern are available.
- **Deny wins.** A tool matched by both `allow` and `deny` is denied.
- **Patterns** are tool names where `*` matches any run of characters: `browser_*`, `exec*`,
  `*_unsafe`, `*`. Everything else is literal.
- **New tools stay denied.** Upstream tool lists are dynamic (Playwright adds tools over time); a
  new tool is available only if an `allow` pattern already matches it. `browser_*` does match
  future browser tools — list them in `deny`, or allow exact names, if that matters.
- **Names**: `[a-z0-9_-]{1,32}`; `owner` and `sandbox` are reserved.
- A profile must allow something (an empty `allow` is rejected); empty patterns are rejected;
  unknown keys are rejected (so `alow = […]` is an error, not a silent no-op).

The shipped examples (`profiles.example.toml`):

| Profile | Grants |
|---|---|
| `viewer` | `sys_info`, `screenshot` |
| `browser` | `sys_info` + every `browser_*` except `browser_run_code_unsafe`, `browser_file_upload`, `browser_pdf_save` |
| `desktop` | `sys_info`, `screenshot`, `mouse`, `key` + every `browser_*` except `browser_run_code_unsafe`, `browser_file_upload` |

`oab-instance-mcp profile list` prints every profile and flags the risky tools a custom profile
allows (`exec`, `exec_start`, `browser_run_code_unsafe`, `browser_file_upload`); the daemon logs
the same warning when it loads the file. Allowing them is legal — it should be a decision, not an
accident.

### Tool reference

| Tool(s) | What it can do |
|---|---|
| `sys_info` | read machine facts; no side effects |
| `exec`, `exec_start`, `exec_poll`, `exec_list`, `exec_cancel` | run any shell command as the desktop user |
| `screenshot` | capture the whole desktop |
| `mouse`, `key` | drive the desktop's pointer and keyboard |
| `browser_navigate`, `browser_snapshot`, `browser_click`, `browser_type`, `browser_fill_form`, … | drive the Playwright browser window |
| `browser_evaluate` | run JavaScript in the current page |
| `browser_run_code_unsafe` | run arbitrary Playwright code in the browser process |
| `browser_file_upload` | upload files from this machine to a web page |
| `browser_pdf_save` | write PDFs to this machine |
| `browser_network_requests`, `browser_network_request` | read the page's network traffic |

Run `tools/list` with the owner token for the authoritative list on your machine.

## Named tokens: `~/.config/oab-instance-mcp/tokens.toml`

Managed with the CLI; you normally never edit it.

```toml
[tokens.hermes]
profile = "browser"
sha256  = "…64 hex…"
created = "2026-09-30T14:43:28Z"
```

- Tokens are `oabt_` + 64 hex characters from the system CSPRNG, printed once at `token add`.
- Only the SHA-256 is stored; the file is written atomically with mode `0600`. Matching hashes
  the presented bearer and compares in constant time.
- The main token (`--token-file`) is separate and always `owner`; `token list` shows it as
  `(--token-file)`.
- To replace a token: `token revoke <name>`, then `token add` again (no silent overwrite).
- Named tokens are consulted only when the daemon runs with a main token (`--token` /
  `--token-file`), which `install.sh` always sets up.

## Reloading and failure behaviour

- Both files are re-read when their modification time changes — adding or revoking a token or
  editing a profile takes effect on the next request. No restart (a restart would also drop
  reverse-attach grants).
- A file that fails to parse or validate is **rejected**: the daemon logs
  `access: REJECTED <file> (<reason>); keeping the previous version in force` and keeps using the
  last good version. A typo can never widen access.
- Deleting `tokens.toml` revokes every named token; deleting `profiles.toml` makes tokens bound to
  custom profiles fail closed (`names undefined profile`).
- A reverse-attach grant keeps the profile it was created with for its whole life; editing
  `profiles.toml` does not change a running grant. Re-lend to apply changes.

## CLI

```
oab-instance-mcp profile list
oab-instance-mcp token add <name> --profile <profile>
oab-instance-mcp token list
oab-instance-mcp token revoke <name>
```

The CLI reads and writes the same config dir as the daemon (`$XDG_CONFIG_HOME/oab-instance-mcp`,
default `~/.config/oab-instance-mcp`); it does not talk to the running daemon.

## What profiles do not protect against

- **Desktop input is close to a shell.** A caller with `mouse` and `key` can open a terminal and
  type. Leave them out (e.g. `browser`, `viewer`) when you mean "no shell".
- **The browser keeps its logins.** The Playwright profile persists cookies; any caller with
  `browser_*` tools acts with whatever accounts are logged in there. Keep that browser for agent
  work only.
- **Tools, not arguments.** A profile allows or denies whole tools; it cannot restrict
  `browser_navigate` to certain sites or `exec` to certain commands.
- **No usage accounting.** `token list` shows creation time only; per-call attribution is in the
  daemon log (`journalctl --user -u oab-instance-mcp`).
- **Owner is owner.** Anyone with the main token has every tool. Keep it for your own clients.

## Troubleshooting

| Symptom | Cause |
|---|---|
| `401` and log `bad token` | the bearer matches neither the main token nor any named token (revoked, mistyped, or the caller still sends an old value) |
| `401` and log `names undefined profile` | the token's profile is missing from `profiles.toml` (renamed, deleted, or the file was rejected) |
| log `access: REJECTED …profiles.toml` | a syntax or validation error; the previous version is still in force — fix the file |
| a tool the caller expects is missing | the profile does not allow it, or `deny` matches it: check with `oab-instance-mcp profile list` |
| `403` on `/attach` | only the owner token may create or list reverse-attach grants |
| `400 profile must be one of […]` on `/attach` | the requested profile is not defined; the error lists the ones that are |
