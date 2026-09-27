# Releasing oab-instance-mcp for macOS

A release is a **universal (arm64 + x86_64), Developer-ID-signed and Apple-notarized** app plus a
signed/notarized installer package. The package is the normal download; the app zip is for advanced
users and inspection.

## Artifacts

A `vMAJOR.MINOR.PATCH` tag publishes:

| Artifact | Use |
|---|---|
| `oab-instance-mcp-VERSION-universal.pkg` | Recommended. Double-click; installs/configures the app for the logged-in desktop user |
| `oab-instance-mcp-VERSION-universal.app.zip` | Pre-signed app, no package receipt. Contains `Contents/Resources/install-prebuilt.sh` for manual installation |
| `SHA256SUMS` | SHA-256 of both artifacts |

The installer:

1. verifies bundle id, code signature and team `6LPQNY95AQ` **before** stopping the running service;
2. detects the current Tailscale login and MagicDNS name from structured `tailscale status --json`;
3. installs to `~/.local/oab-instance-mcp/oab-instance-mcp.app` without re-signing it;
4. creates a bearer token once at `~/.config/oab-instance-mcp/token`, mode 600, and preserves it on updates;
5. writes/starts the Aqua-user LaunchAgent `dev.openab.instance-mcp`;
6. adds the Playwright upstream when `dev.openab.instance-mcp.pw-mcp` is installed; and
7. configures `tailscale serve --https=8444` to loopback port 8795.

A logged-in GUI user and a logged-in Tailscale app are prerequisites. Package scripts run as root,
but immediately enter the console user's GUI bootstrap namespace and drop to that user; no token,
LaunchAgent or config is written to root's home.

## One-time TCC grant

After the first Developer-ID release install, enable **oab-instance-mcp** once under System Settings →
Privacy & Security:

- Full Disk Access
- Screen & System Audio Recording
- Accessibility

The first switch from the current Apple Development signature to Developer ID may require that one
re-grant. Every later release carries the same bundle id and Developer ID team, so the grant remains.
Neither the installer nor CI ever ad-hoc re-signs a release app. `install-prebuilt.sh` refuses a wrong
or missing TeamIdentifier before replacing the running app.

## Required GitHub environment and secrets

The workflow uses the `release` environment. Create it with required-reviewer protection if the repo
plan supports that, then add:

| Secret | Value |
|---|---|
| `MACOS_APP_CERT_P12_BASE64` | Base64 of the **Developer ID Application** certificate + private key `.p12` |
| `MACOS_APP_CERT_PASSWORD` | `.p12` export password |
| `MACOS_APP_SIGN_IDENTITY` | Exact common name, e.g. `Developer ID Application: Name (6LPQNY95AQ)` |
| `MACOS_INSTALLER_CERT_P12_BASE64` | Base64 of the **Developer ID Installer** certificate + private key `.p12` |
| `MACOS_INSTALLER_CERT_PASSWORD` | `.p12` export password |
| `MACOS_INSTALLER_SIGN_IDENTITY` | Exact common name, e.g. `Developer ID Installer: Name (6LPQNY95AQ)` |
| `APPLE_NOTARY_KEY_P8_BASE64` | Base64 of an App Store Connect API `.p8` key allowed to notarize |
| `APPLE_NOTARY_KEY_ID` | API key id |
| `APPLE_NOTARY_ISSUER_ID` | API issuer id |

**Use team `6LPQNY95AQ` only.** The machine still contains a deprecated team
`UM92U863A8` Developer ID Application certificate; it must never sign these releases. As of
2026-09-27 there is no Developer ID Application or Installer certificate for `6LPQNY95AQ` on the
build machines and the repository has no Actions secrets, so the first signed tag is intentionally
blocked until those assets are created and installed as secrets.

The tag workflow validates that both identity names are Developer ID identities, verifies the app's
TeamIdentifier is exactly `6LPQNY95AQ`, submits/staples both app and pkg, and runs signature/Gatekeeper
checks before creating the GitHub Release. Signing material lives in an ephemeral keychain and is
deleted in an `always()` cleanup step.

## Cut a release

1. Update `let version = "…"` in `Sources/oab-instance-mcp/main.swift` and merge with green CI.
2. Ensure the matching active-team release secrets above exist.
3. Tag the exact main commit and push:

   ```sh
   git tag v0.7.0
   git push origin v0.7.0
   ```

4. The `Release macOS installer` workflow builds/tests, signs, notarizes and publishes. A manual
   dispatch can retry an existing tag; it is not a way to release an untagged commit.
5. Download both artifacts and verify before installing:

   ```sh
   scripts/verify-release.sh \
     oab-instance-mcp-0.7.0-universal.app.zip \
     oab-instance-mcp-0.7.0-universal.pkg
   shasum -a 256 -c SHA256SUMS
   ```

6. Install the `.pkg` on a clean/test Mac, verify `sys_info`, and make one reverse-attach call
   before announcing it.

Never move or recreate a tag after an artifact has been published.

## Local/no-secret smoke

CI runs this on every PR:

```sh
swift build
scripts/test-packaging.sh .build/debug/oab-instance-mcp
```

It assembles the app, proves unsigned input is rejected by the production installer, exercises the
explicit unsigned test seam in a temporary home, verifies token persistence/LaunchAgent arguments,
builds an unsigned flat pkg, expands it, and checks its payload and postinstall scripts. It never
launches an agent or changes the user's Tailscale serve config.

To build a universal unsigned artifact manually on a Mac:

```sh
swift build -c release --arch arm64 --scratch-path /tmp/imcp-arm64
swift build -c release --arch x86_64 --scratch-path /tmp/imcp-x86_64
lipo -create /tmp/imcp-arm64/release/oab-instance-mcp \
             /tmp/imcp-x86_64/release/oab-instance-mcp \
             -output /tmp/oab-instance-mcp
chmod +x /tmp/oab-instance-mcp
scripts/assemble-app.sh /tmp/oab-instance-mcp /tmp/oab-instance-mcp.app 0.6.4
ALLOW_UNSIGNED=1 scripts/package-pkg.sh /tmp/oab-instance-mcp.app /tmp/oab-instance-mcp.pkg 0.6.4
```

Unsigned artifacts are testing inputs only; do not install or publish them.
