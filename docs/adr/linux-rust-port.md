# ADR: Linux-first Rust port — one core, per-OS backends

- **Status:** Proposed
- **Date:** 2026-09-27
- **Related:** [openabdev/instance-mcp#15](https://github.com/openabdev/instance-mcp/issues/15) (FR + phased plan + arch diagrams), [`reverse-attach.md`](reverse-attach.md), [`sandbox-adapter.md`](sandbox-adapter.md)
- **Review:** Two independent reviewers (fable/claude-fable-5.1, sol/gpt-5.6-sol) returned NEEDS_CHANGES; this revision incorporates their convergent findings. See PR discussion.

## Context

`oab-instance-mcp` today is a Swift package targeting `.macOS(.v14)`, ~3,073 lines, linking
ScreenCaptureKit / CoreGraphics / Network. It lets a coding CLI drive a machine via MCP tools
(screenshot, mouse, key, exec, sysinfo, osascript).

The strategic target is shifting: **Linux is a first-class citizen, arguably the primary one.**
Cheap Linux mini PCs and VMs are easy to acquire and scale horizontally; a Mac mini is
expensive and comparatively rare. We should optimize for deploying many inexpensive Linux
nodes, which favors a single self-contained binary, trivial cross-compile (x86_64 + arm64),
and a strong Linux systems story.

### On the code split (corrected framing)

An earlier draft leaned on "~62% of the code is platform-agnostic" to argue the port is
"tractable." That framing is wrong and reviewers rightly flagged it. This is a **rewrite, not a
port**: zero Swift lines carry over. "Platform-agnostic" describes the *concept*, not
*reuse* — the ~62% (MCP dispatch, JSON-RPC, HTTP, auth policy, **reverse-attach**, upstream
proxy, job registry) must be **re-implemented in Rust and re-earn every security property**.
That is the *higher-risk* majority of the work. The genuinely de-riskable part ("shell out to
grim/ydotool/proc") is the ~640-line platform-specific *minority*. Effort and risk must be
sized against re-implementing and re-securing the core, not against the small platform shim.

Target environment verified on `rpi1` (Raspberry Pi 5): aarch64, Debian 13 (trixie), labwc
(Wayland) + lightdm, `grim`/`wlr-randr`/`scrot` installed, `/dev/uinput` present (root-only),
Swift not installed. **Caveat: this is one hand-configured node.** It does not establish that
cheap headless Linux VMs (often no compositor, no `WAYLAND_DISPLAY`, no seat) can run the
graphical tools, nor that non-wlroots desktops (GNOME/KDE) work with grim/wlr-randr.

## Alternatives considered

| Alternative | Why rejected / status |
|---|---|
| **Swift for Linux (one Swift codebase, reuse the actual code)** | *Reconsidered, not dismissed.* The prior rejection ("weak Linux systems ecosystem: uinput/Wayland need C FFI") is largely neutralized by the shell-out plan (grim/ydotool/proc need no mature crates). Swift-on-Linux + shell-out would reuse real code (not a rewrite), avoid re-securing auth/reverse-attach, and ship Linux fastest. It remains a live contender pending the PoC; the case for Rust rests on deployment (static binary, cross-compile) and the long-term unify-on-Rust endgame, not on ecosystem alone. |
| **Thin Rust/Go Linux host, keep the proven Swift core on macOS** | *Newly added.* Write only a small Linux binary implementing the platform tools + minimal MCP transport, sharing a conformance suite; leave the working Swift macOS build untouched. Ships Linux with far less re-secured surface. Weaker on the "single unified codebase" endgame; kept as the leading fallback if re-securing the full core in Rust proves too costly. |
| **Immediately rewrite everything (macOS included) in Rust now** | Cleanest end state but throws away the working Swift build up front and front-loads the largest effort, including objc2/ScreenCaptureKit bindings before Linux ships. |
| **Two permanent codebases (Swift macOS + Rust Linux, no convergence)** | Locks in a permanent sync trap. Note the transition period below still has this drift risk until Phase 3; the conformance suite is the mitigation. |
| **Node/TypeScript or Go rewrite** | Deploy reasonably, but Rust wins on self-contained binary, Linux systems crates, and first-class x86_64+arm64 cross-compile, and is the strongest path to later unify macOS via FFI. |

## Decision

**Rewrite in Rust, Linux-first, as one cross-platform core with per-OS backends behind a
`PlatformBackend` trait (`#[cfg(target_os)]`); keep Swift as a transitional macOS backend until
the Rust macOS backend reaches parity, then retire it — CONDITIONAL on the phase gates below.**

0. **Phase 0 — de-risk the two biggest unknowns before committing to the full buildout.**
   - **macOS FFI spike:** prove `objc2`/`core-graphics` can do CGEvent input + one
     ScreenCaptureKit frame + a TCC prompt, in a **signed + notarized + hardened-runtime**
     build produced from CI. If this is impractical, the "unify on Rust / retire Swift"
     endgame fails and we fall back to the thin-host alternative — decide before Phase 1.
   - **Headless/seat spike:** prove screenshot/input work (or explicitly cannot) on a cheap
     headless Linux VM with no pre-configured compositor — via a headless wlroots/virtual
     compositor, or declare a "graphical seat required" provisioning contract.
1. **Phase 1 — Rust Linux core, cross-platform by design.** Rust MCP core (HTTP transport,
   JSON-RPC, dispatch, **auth + reverse-attach**, exec/async-exec/job registry, upstream
   proxy) + Linux backend (screenshot `grim`, input `wlrctl` (pointer+keyboard via wlroots virtual-pointer/keyboard, no uinput; `wtype` alt for keys; wlroots-only, uinput/udev is the non-wlroots fallback),
   sysinfo `/proc`+`/sys`+`wlr-randr`; drop `osascript`). All platform specifics behind
   `PlatformBackend`. **TLS: rustls** (pure-Rust, static-link, cross-compile friendly) —
   chosen explicitly to honor the self-contained-binary goal; dependency set
   (tokio/hyper/rustls/tungstenite/serde) audited with `cargo-deny`/`cargo-audit`.
2. **Phase 2 — macOS backend (FFI) in the same codebase** (objc2/core-graphics: CGEvent,
   ScreenCaptureKit, sysctl/TCC), only after Phase 0's spike proved it viable. Converges to a
   single unified Rust codebase.
3. **Phase 3 — retire Swift** once the Rust macOS backend meets the parity definition below.

### Security parity (reverse-attach) is a first-order requirement, not an afterthought

The core carries a bespoke auth/authz protocol (see `reverse-attach.md`): the Mac dials the
pod over **WSS**, the pod stores only a **sha256 verifier**, the **shell never holds a
credential**, and **tool profiles are scoped per connection**. Re-implementing this in Rust
risks subtle divergence (constant-time verifier compare, TLS peer/cert handling, WS
upgrade/origin checks, per-dispatch profile enforcement). Therefore:
- A **versioned MCP + reverse-attach conformance / differential test suite** is the shared
  anti-drift contract. Both the Swift and Rust implementations must pass it; it is the real
  mechanism that prevents protocol drift during the Swift↔Rust transition.

### "Self-contained binary" — honest dependency/deployment reality

The Rust binary is self-contained for its own code + rustls, but it is **not** literally
scp-and-run: it depends on external CLIs/daemons on graphical Linux nodes (`grim`, `wlrctl`, `wlr-randr`) and a live wlroots compositor
seat. NOTE: `grim`/`wlrctl` use wlroots protocols (wlr-screencopy, virtual-pointer/keyboard) and are **wlroots-only** -- GNOME/Mutter and KDE/KWin do not implement them; those need xdg-desktop-portal/PipeWire (screenshot) and a `/dev/uinput`+udev fallback (input), which stays the compositor-agnostic path. The deployment story must package these (image/cloud-init) and the binary must
**probe capabilities + detect versions at startup** and degrade explicitly (e.g. headless
node → graphical tools disabled, exec/sysinfo/MCP still available).

### Validation gate (must pass before Status → Accepted)

The PoC must exercise the **highest-risk** components, not just the easy ones:
- (a) Rust MCP-over-HTTP handles `initialize` + tool dispatch.
- (b) `sys_info` reads `/proc` + `wlr-randr`; `screenshot` shells out to `grim`; input via `wlrctl` (pointer+keyboard). [#23: input functionally verified on x86_64 headless sway (move/click/type, not pixel-accuracy); `wlrctl` package present on aarch64 but input not re-tested there.]
- (c) **auth + reverse-attach**: WSS dial-out, sha256 verifier with constant-time compare,
  credential-never-in-shell, per-connection tool-profile enforcement — validated against the
  conformance suite.
- (d) Phase 0 spikes (macOS FFI signed/notarized; headless/seat) resolved.

### Prior art

The "one core, per-OS backend behind a trait" shape is standard in Rust cross-platform tools
(ripgrep, alacritty, wezterm) isolating OS specifics behind `#[cfg(target_os)]`. Shelling out
to `grim`/`ydotool` mirrors compositor-agnostic Linux automation tools. Retiring an incumbent
only after a reimplementation reaches parity is strangler-fig migration.

## Open questions

- Is full-core Rust worth re-securing auth/reverse-attach, vs the thin-host alternative that
  keeps the proven Swift core? Decide with Phase 0 + PoC evidence.
- Concrete definition of "parity" before retiring Swift: a per-tool conformance checklist +
  the reverse-attach differential suite passing on both backends.
- Wayland scope: wlroots-only, or add xdg-desktop-portal/PipeWire for GNOME/KDE?
- Headless fleet: virtual compositor vs "graphical seat required" provisioning contract?
- macOS release: cross-compile feasibility + CI runners for signing/notarization from Rust.
