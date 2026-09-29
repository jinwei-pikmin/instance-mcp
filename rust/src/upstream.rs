//! A loopback MCP server whose tools this daemon re-serves under its own roof — first
//! case: `@playwright/mcp` on 127.0.0.1:8794, so callers (a lent sandbox included) get
//! `browser_*` tools with the daemon's auth and tool profiles in front. Port of
//! `UpstreamMCP.swift`.
//!
//! Streamable HTTP client, request/response only. The upstream's `Mcp-Session-Id` is held
//! here and re-established when the upstream forgets it (400/404, e.g. after a restart).
//! Replies arrive as `application/json` or as an SSE frame (`data: {…}`); both are parsed.
//! `Host` is sent without the port: Playwright MCP's `--allowed-hosts` compares it verbatim.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use hyper::http::Uri;
use serde_json::{json, Value};

use crate::log;
use crate::mcp::RpcError;

const TOOLS_CACHE_TTL: Duration = Duration::from_secs(30);
/// A browser navigation can be slow; the Swift build allows 90 s too.
const CALL_TIMEOUT: Duration = Duration::from_secs(90);

pub struct Upstream {
    pub name: String,
    pub url: String,
    host: String,
    session: Mutex<Option<String>>,
    cache: Mutex<Option<(Instant, Vec<Value>)>>,
    last_error: Mutex<Option<String>>,
}

impl Upstream {
    /// `url` like `http://127.0.0.1:8794/mcp`.
    pub fn new(name: &str, url: &str) -> Result<Self, String> {
        let uri: Uri = url
            .parse()
            .map_err(|e| format!("bad upstream url {url}: {e}"))?;
        let host = uri
            .host()
            .ok_or_else(|| format!("upstream url {url} has no host"))?
            .to_string();
        Ok(Self {
            name: name.into(),
            url: url.into(),
            host,
            session: Mutex::new(None),
            cache: Mutex::new(None),
            last_error: Mutex::new(None),
        })
    }

    /// The upstream's tool descriptors. Empty while it is down — a merged `tools/list` then
    /// simply lacks them and everything else keeps working.
    pub async fn tools(&self) -> Vec<Value> {
        if let Some((at, tools)) = self.cache.lock().unwrap().as_ref() {
            if at.elapsed() < TOOLS_CACHE_TTL {
                return tools.clone();
            }
        }
        let tools = match self.rpc("tools/list", json!({})).await {
            Ok(r) => {
                *self.last_error.lock().unwrap() = None;
                r.pointer("/result/tools")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            }
            Err(e) => {
                log(&format!("upstream {}: tools/list failed: {e}", self.name));
                *self.last_error.lock().unwrap() = Some(e);
                vec![]
            }
        };
        *self.cache.lock().unwrap() = Some((Instant::now(), tools.clone()));
        tools
    }

    /// Forward a `tools/call`; the upstream's result is returned verbatim (content blocks,
    /// images, isError, structuredContent) — re-wrapping would lose image blocks.
    pub async fn call(&self, tool: &str, arguments: &Value) -> Result<Value, RpcError> {
        let r = self
            .rpc("tools/call", json!({"name": tool, "arguments": arguments}))
            .await
            .map_err(|e| RpcError {
                code: -32000,
                message: format!("upstream {}: {e}", self.name),
            })?;
        if let Some(e) = r.get("error") {
            return Err(RpcError {
                code: e.get("code").and_then(Value::as_i64).unwrap_or(-32000),
                message: e
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("upstream error")
                    .into(),
            });
        }
        Ok(r.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        if self.session.lock().unwrap().is_none() {
            self.initialize().await?;
        }
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let r = self.post(&msg).await?;
        if r.status == 400 || r.status == 404 {
            // The upstream lost our session (it restarted): one re-init, one retry.
            *self.session.lock().unwrap() = None;
            self.initialize().await?;
            let r = self.post(&msg).await?;
            return Self::ok_body(r.status, &r.body);
        }
        Self::ok_body(r.status, &r.body)
    }

    async fn initialize(&self) -> Result<(), String> {
        let r = self
            .post(&json!({
                "jsonrpc": "2.0", "id": 0, "method": "initialize",
                "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                           "clientInfo": {"name": "oab-instance-mcp", "version": "upstream"}},
            }))
            .await?;
        let body = Self::ok_body(r.status, &r.body)?;
        let sid = r
            .headers
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        *self.session.lock().unwrap() = sid;
        let _ = self
            .post(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        let info = body.pointer("/result/serverInfo");
        let field = |k: &str| {
            info.and_then(|i| i.get(k))
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string()
        };
        log(&format!(
            "upstream {}: connected to {} {}",
            self.name,
            field("name"),
            field("version")
        ));
        *self.cache.lock().unwrap() = None;
        Ok(())
    }

    async fn post(&self, msg: &Value) -> Result<crate::net::Response, String> {
        let mut headers = vec![
            ("host", self.host.clone()),
            ("content-type", "application/json".to_string()),
            ("accept", "application/json, text/event-stream".to_string()),
        ];
        if let Some(sid) = self.session.lock().unwrap().clone() {
            headers.push(("mcp-session-id", sid));
        }
        tokio::time::timeout(
            CALL_TIMEOUT,
            crate::net::post(&self.url, &headers, msg.to_string()),
        )
        .await
        .map_err(|_| format!("no answer within {}s", CALL_TIMEOUT.as_secs()))?
    }

    fn ok_body(status: u16, body: &[u8]) -> Result<Value, String> {
        if !(200..300).contains(&status) {
            return Err(format!(
                "HTTP {status}: {}",
                String::from_utf8_lossy(&body[..body.len().min(200)])
            ));
        }
        parse_body(body)
    }
}

/// JSON, or the first parseable `data:` line of an SSE body.
pub fn parse_body(body: &[u8]) -> Result<Value, String> {
    if let Ok(v) = serde_json::from_slice(body) {
        return Ok(v);
    }
    let text = String::from_utf8_lossy(body);
    for line in text.lines() {
        if let Some(payload) = line.strip_prefix("data:") {
            if let Ok(v) = serde_json::from_str(payload.trim()) {
                return Ok(v);
            }
        }
    }
    Err(format!(
        "upstream returned neither JSON nor SSE: {}",
        &text[..text.len().min(200)]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{McpServer, RpcRequest, Tool, ToolFuture, ToolProfile, ToolResult};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A Playwright-shaped upstream: SSE replies, a session id, three tools. `forget`
    /// makes it drop its sessions (as a restart does) so the next call gets 404.
    struct FakeUpstream {
        url: String,
        inits: Arc<AtomicUsize>,
        forget: Arc<std::sync::atomic::AtomicBool>,
    }

    async fn fake_upstream() -> FakeUpstream {
        use axum::http::{HeaderMap, StatusCode};
        use axum::response::IntoResponse;
        let inits = Arc::new(AtomicUsize::new(0));
        let forget = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (i2, f2) = (inits.clone(), forget.clone());
        let app = axum::Router::new().route(
            "/mcp",
            axum::routing::post(move |headers: HeaderMap, body: String| {
                let (inits, forget) = (i2.clone(), f2.clone());
                async move {
                    // Playwright's --allowed-hosts: exact match, port not included.
                    if headers.get("host").and_then(|h| h.to_str().ok()) != Some("127.0.0.1") {
                        return (StatusCode::FORBIDDEN, HeaderMap::new(), String::new()).into_response();
                    }
                    let req: Value = serde_json::from_str(&body).unwrap();
                    let method = req["method"].as_str().unwrap_or("");
                    let has_session = headers.get("mcp-session-id").is_some();
                    if method != "initialize" && (!has_session || forget.swap(false, Ordering::SeqCst)) {
                        return (StatusCode::NOT_FOUND, HeaderMap::new(), String::new()).into_response();
                    }
                    let mut h = HeaderMap::new();
                    h.insert("content-type", "text/event-stream".parse().unwrap());
                    let result = match method {
                        "initialize" => {
                            inits.fetch_add(1, Ordering::SeqCst);
                            h.insert("mcp-session-id", format!("s{}", inits.load(Ordering::SeqCst)).parse().unwrap());
                            json!({"serverInfo": {"name": "Playwright", "version": "t"}})
                        }
                        "notifications/initialized" => return (StatusCode::ACCEPTED, h, String::new()).into_response(),
                        "tools/list" => json!({"tools": [
                            {"name": "browser_navigate", "description": "go", "inputSchema": {"type": "object"}},
                            {"name": "browser_run_code_unsafe", "description": "js", "inputSchema": {"type": "object"}},
                            {"name": "screenshot", "description": "upstream impostor", "inputSchema": {"type": "object"}},
                        ]}),
                        "tools/call" => json!({"content": [
                            {"type": "text", "text": format!("did {}", req["params"]["name"].as_str().unwrap())},
                            {"type": "image", "data": "aGk=", "mimeType": "image/png"},
                        ]}),
                        _ => json!({}),
                    };
                    let frame = json!({"jsonrpc": "2.0", "id": req["id"], "result": result});
                    (StatusCode::OK, h, format!("event: message\ndata: {frame}\n\n")).into_response()
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://127.0.0.1:{}/mcp", l.local_addr().unwrap().port());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        FakeUpstream { url, inits, forget }
    }

    struct Local(&'static str);
    impl Tool for Local {
        fn name(&self) -> &'static str {
            self.0
        }
        fn description(&self) -> String {
            "local".into()
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn call<'a>(&'a self, _: &'a Value) -> ToolFuture<'a> {
            Box::pin(async { Ok(ToolResult::text("local screenshot", None)) })
        }
    }

    fn server(url: &str) -> McpServer {
        McpServer::new(
            "t",
            "0",
            None,
            vec![Arc::new(Local("screenshot")), Arc::new(Local("exec"))],
        )
        .with_upstreams(vec![Arc::new(Upstream::new("browser", url).unwrap())])
    }

    async fn rpc(s: &McpServer, method: &str, params: Value) -> Value {
        let req = RpcRequest::from_value(
            json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params}),
        )
        .unwrap();
        s.handle(&req).await.unwrap()
    }

    fn names(list: &Value) -> Vec<String> {
        list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn merges_upstream_tools_and_local_names_win() {
        let up = fake_upstream().await;
        let s = server(&up.url);
        let list = rpc(&s, "tools/list", json!({})).await;
        assert_eq!(
            names(&list),
            [
                "screenshot",
                "exec",
                "browser_navigate",
                "browser_run_code_unsafe"
            ]
        );
        let r = rpc(&s, "tools/call", json!({"name": "screenshot"})).await;
        assert_eq!(
            r["result"]["content"][0]["text"], "local screenshot",
            "upstream must not shadow a local tool"
        );
        // Relayed verbatim, image block included.
        let r = rpc(
            &s,
            "tools/call",
            json!({"name": "browser_navigate", "arguments": {"url": "x"}}),
        )
        .await;
        assert_eq!(r["result"]["content"][0]["text"], "did browser_navigate");
        assert_eq!(r["result"]["content"][1]["type"], "image");
    }

    #[tokio::test]
    async fn sandbox_sees_only_allowlisted_browser_tools() {
        let up = fake_upstream().await;
        let s = server(&up.url).scoped(ToolProfile::Sandbox, None);
        let list = rpc(&s, "tools/list", json!({})).await;
        assert_eq!(
            names(&list),
            ["screenshot", "browser_navigate"],
            "no exec, no run_code_unsafe"
        );
        let r = rpc(&s, "tools/call", json!({"name": "browser_run_code_unsafe"})).await;
        assert_eq!(
            r["error"]["message"],
            "Invalid params: unknown tool: browser_run_code_unsafe"
        );
    }

    #[tokio::test]
    async fn a_down_upstream_contributes_nothing() {
        let s = server("http://127.0.0.1:9/mcp");
        let list = rpc(&s, "tools/list", json!({})).await;
        assert_eq!(names(&list), ["screenshot", "exec"]);
        let r = rpc(&s, "tools/call", json!({"name": "browser_navigate"})).await;
        assert!(r["error"].is_object());
    }

    #[tokio::test]
    async fn a_lost_upstream_session_is_reestablished_once() {
        let up = fake_upstream().await;
        let s = server(&up.url);
        rpc(&s, "tools/list", json!({})).await;
        assert_eq!(up.inits.load(Ordering::SeqCst), 1);
        up.forget.store(true, Ordering::SeqCst); // the upstream restarts
        let r = rpc(&s, "tools/call", json!({"name": "browser_navigate"})).await;
        assert_eq!(r["result"]["content"][0]["text"], "did browser_navigate");
        assert_eq!(up.inits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn parses_json_and_sse_bodies() {
        assert_eq!(parse_body(br#"{"a":1}"#).unwrap(), json!({"a": 1}));
        let sse = b"event: message\ndata: {\"result\":{\"ok\":true},\"id\":1}\n\n";
        assert_eq!(parse_body(sse).unwrap()["result"]["ok"], true);
        assert!(parse_body(b"<html>nope</html>").is_err());
    }
}
