// reverse-attach: makes a Linux node a lendable "hands" node for openab-pty
// reverse-attach. Single self-contained binary: a minimal HTTP/1.1 control
// server (POST/GET /attach, GET/DELETE /attach/{id}) plus a WebSocket dialer that
// connects outbound to a runtime and serves an MCP tool surface.
//
// Synchronous, std threads only. Deps: serde_json + tungstenite (which
// re-exports `http`). Target: aarch64 Debian 13, Rust 1.98.

mod attach;
mod auth;
mod http;
mod mcp;
mod platform;
mod tools;

fn main() {
    http::serve();
}
