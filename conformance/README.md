# Conformance vectors

Language-neutral test vectors that **both** implementations must pass, so behaviour cannot drift
while the Swift daemon (macOS) and the Rust port (`poc/reverse-attach-linux`, Linux) coexist
(#24, `docs/adr/linux-rust-port.md`). The Swift implementation is the oracle.

| File | Covers | Swift test | Rust test |
|---|---|---|---|
| `auth_vectors.json` | `AuthPolicy`: validate + allow/deny decision and exact reason strings | `ConformanceVectorTests.testAuthPolicyVectors` | `auth::conformance` |
| `reverse_attach_vectors.json` | §9.2 close-code / handshake-status disposition, `attachURL` | `ConformanceVectorTests.testReverseAttachVectors` | `attach::conformance` |

Both run in CI on every PR (`swift test`, `cargo test`). Change a vector only for an intended
behaviour change, and make both implementations pass in the same PR.
