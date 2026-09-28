// reverse-attach: makes a Linux node a lendable "hands" node for openab-pty
// reverse-attach. Single self-contained binary: a minimal HTTP/1.1 control
// server (POST/GET /attach, GET/DELETE /attach/{id}) plus a WebSocket dialer that
// connects outbound to a runtime and serves an MCP tool surface.
//
// Synchronous, std threads only. Deps: serde_json + tungstenite (which
// re-exports `http`). Target: aarch64 Debian 13, Rust 1.98.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tungstenite::http::Request;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message;

// ---------------------------------------------------------------------------
// Shared grant registry
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct GrantInfo {
    id: String,
    runtime: String,
    session: String,
    profile: String,
    principal: String,
    state: String,
    ended: Option<String>,
    expires_at_epoch_secs: u64,
    cancelled: Arc<AtomicBool>,
}

type Registry = Arc<Mutex<HashMap<String, GrantInfo>>>;

static GRANT_COUNTER: AtomicU64 = AtomicU64::new(1);

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn new_grant_id() -> String {
    let n = GRANT_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("grant-{}-{}", now_epoch_secs(), n)
}

fn set_state(registry: &Registry, grant_id: &str, state: &str) {
    if let Ok(mut map) = registry.lock() {
        if let Some(g) = map.get_mut(grant_id) {
            g.state = state.to_string();
            g.ended = None;
        }
    }
}

fn set_ended(registry: &Registry, grant_id: &str, reason: &str) {
    if let Ok(mut map) = registry.lock() {
        if let Some(g) = map.get_mut(grant_id) {
            g.state = "ended".to_string();
            g.ended = Some(reason.to_string());
        }
    }
}

/// Exact Swift `MacGrant` shape consumed by OpenAB Connect/Remote. Fields not
/// present in the early PoC (`runtime`, `principal`, standard `id`/state) made a
/// successful POST decode as a generic client parse failure.
fn grant_json(g: &GrantInfo) -> Value {
    let mut out = json!({
        "id": g.id,
        "runtime": g.runtime,
        "session": g.session,
        "profile": g.profile,
        "principal": g.principal,
        "state": g.state,
        "expires_in_secs": g.expires_at_epoch_secs.saturating_sub(now_epoch_secs()),
    });
    if let Some(ended) = &g.ended {
        out["ended"] = Value::String(ended.clone());
    }
    out
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn valid_session(s: &str) -> bool {
    // ^[a-z0-9-]{1,32}$
    let len = s.len();
    if !(1..=32).contains(&len) {
        return false;
    }
    s.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn strip_trailing_slashes(s: &str) -> &str {
    s.trim_end_matches('/')
}

fn attach_url(runtime: &str, session: &str) -> String {
    format!(
        "{}/tools/attach/{}",
        strip_trailing_slashes(runtime),
        session
    )
}

// ---------------------------------------------------------------------------
// Disposition state machine
// ---------------------------------------------------------------------------

enum Disposition {
    Stop(String),
    Redial,
}

fn disposition_close(code: u16) -> Disposition {
    match code {
        4001 => Disposition::Stop("grant_expired".to_string()),
        4002 => Disposition::Stop("replaced".to_string()),
        4004 => Disposition::Stop("session_ended".to_string()),
        4010 => Disposition::Stop("revoked".to_string()),
        _ => Disposition::Redial,
    }
}

fn disposition_handshake(status: u16) -> Disposition {
    match status {
        101 => Disposition::Redial,
        200..=299 => Disposition::Redial,
        429 => Disposition::Redial,
        500..=599 => Disposition::Redial,
        _ => Disposition::Stop(format!("handshake_rejected_{status}")),
    }
}

// ---------------------------------------------------------------------------
// mint(): replicate the Swift runtimeMintRequest contract (http/ws only)
// ---------------------------------------------------------------------------

fn mint(
    runtime: &str,
    session: &str,
    admin_credential: &str,
    ttl_secs: u64,
) -> Result<(String, u64), String> {
    // Map scheme ws->http, wss->https. Only plain http (ws://) is supported.
    let http_base: String = if runtime.starts_with("wss://") {
        return Err(
            "https mint not supported in PoC; use ws:// runtime or pass a pre-minted secret"
                .to_string(),
        );
    } else if let Some(rest) = runtime.strip_prefix("ws://") {
        format!("http://{}", rest)
    } else {
        return Err(format!("unsupported runtime scheme for mint: {}", runtime));
    };

    let base = strip_trailing_slashes(&http_base).to_string();
    let url = format!("{}/admin/sessions/{}/tools-attach", base, session);

    // Parse http://host[:port]/path
    let after_scheme = url
        .strip_prefix("http://")
        .ok_or_else(|| "internal: expected http scheme".to_string())?;
    let (authority, path) = match after_scheme.find('/') {
        Some(idx) => (&after_scheme[..idx], &after_scheme[idx..]),
        None => (after_scheme, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| "bad port".to_string())?,
        ),
        None => (authority.to_string(), 80u16),
    };

    let body = json!({ "ttl_secs": ttl_secs }).to_string();
    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {authority}\r\n\
         Authorization: Bearer {cred}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        path = path,
        authority = authority,
        cred = admin_credential,
        len = body.len(),
        body = body,
    );

    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve failed: {e}"))?
        .next()
        .ok_or_else(|| "no address resolved".to_string())?;

    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
        .map_err(|e| format!("connect failed: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write failed: {e}"))?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("read failed: {e}"))?;
    let text = String::from_utf8_lossy(&raw).to_string();

    // Split headers / body on the first blank line.
    let sep = text
        .find("\r\n\r\n")
        .map(|i| (i, 4))
        .or_else(|| text.find("\n\n").map(|i| (i, 2)));
    let (head, resp_body) = match sep {
        Some((i, off)) => (&text[..i], &text[i + off..]),
        None => (text.as_str(), ""),
    };

    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);

    if !(200..=299).contains(&status) {
        return Err(format!("mint HTTP {status}: {}", resp_body.trim()));
    }

    let v: Value = serde_json::from_str(resp_body.trim())
        .map_err(|e| format!("mint response parse error: {e}"))?;

    let secret = v
        .get("secret")
        .and_then(|s| s.as_str())
        .ok_or_else(|| "mint response missing secret".to_string())?
        .to_string();

    let expires_in = v
        .get("expires_in_secs")
        .or_else(|| v.get("expires_in"))
        .or_else(|| v.get("expiresIn"))
        .and_then(|e| e.as_u64())
        .unwrap_or(ttl_secs);

    Ok((secret, expires_in))
}

// ---------------------------------------------------------------------------
// Tiny blocking HTTP/1.1 client for http:// only (used for the upstream MCP)
// ---------------------------------------------------------------------------

struct HttpReply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

fn http_post(
    url: &str,
    headers: &[(&str, &str)],
    body: &str,
    timeout: Duration,
) -> Result<HttpReply, String> {
    let after_scheme = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("only http:// supported: {url}"))?;
    let (authority, path) = match after_scheme.find('/') {
        Some(idx) => (&after_scheme[..idx], &after_scheme[idx..]),
        None => (after_scheme, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| "bad port".to_string())?,
        ),
        None => (authority.to_string(), 80u16),
    };
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        request.push_str(&format!("{k}: {v}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);

    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve failed: {e}"))?
        .next()
        .ok_or_else(|| "no address resolved".to_string())?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
        .map_err(|e| format!("connect failed: {e}"))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write failed: {e}"))?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("read failed: {e}"))?;
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, resp_body) = match text.find("\r\n\r\n") {
        Some(i) => (&text[..i], &text[i + 4..]),
        None => (text.as_str(), ""),
    };
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let mut hdrs = Vec::new();
    let mut chunked = false;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            if k.trim().eq_ignore_ascii_case("transfer-encoding")
                && v.to_lowercase().contains("chunked")
            {
                chunked = true;
            }
            hdrs.push((k.trim().to_lowercase(), v.trim().to_string()));
        }
    }
    let body = if chunked {
        dechunk(resp_body)
    } else {
        resp_body.to_string()
    };
    Ok(HttpReply {
        status,
        headers: hdrs,
        body,
    })
}

fn dechunk(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(nl) = rest.find("\r\n") {
        let size = usize::from_str_radix(rest[..nl].trim().split(';').next().unwrap_or("0"), 16)
            .unwrap_or(0);
        if size == 0 {
            break;
        }
        let start = nl + 2;
        let end = (start + size).min(rest.len());
        out.push_str(&rest[start..end]);
        rest = rest.get(end + 2..).unwrap_or("");
    }
    out
}

/// JSON body, or the first `data:` line of an SSE body.
fn parse_mcp_body(body: &str) -> Result<Value, String> {
    let t = body.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Ok(v);
    }
    for line in t.lines() {
        if let Some(d) = line.strip_prefix("data:") {
            if let Ok(v) = serde_json::from_str::<Value>(d.trim()) {
                return Ok(v);
            }
        }
    }
    Err(format!("unparseable MCP body: {}", &t[..t.len().min(120)]))
}

// ---------------------------------------------------------------------------
// Upstream MCP (e.g. @playwright/mcp on loopback), re-served under our tools/list.
// Mirrors Swift `UpstreamMCP`: Streamable HTTP request/response, session id held
// here and re-established on 400/404, tools cached 30 s, down → tools absent.
// ---------------------------------------------------------------------------

struct Upstream {
    name: String,
    url: String,
    session_id: Mutex<Option<String>>,
    cache: Mutex<Option<(std::time::Instant, Vec<Value>)>>,
}

impl Upstream {
    /// MCP_UPSTREAM="browser=http://127.0.0.1:8794/mcp[,name=url...]"
    fn from_env() -> Vec<Arc<Upstream>> {
        std::env::var("MCP_UPSTREAM")
            .unwrap_or_default()
            .split(',')
            .filter_map(|e| e.trim().split_once('='))
            .map(|(n, u)| {
                Arc::new(Upstream {
                    name: n.trim().to_string(),
                    url: u.trim().to_string(),
                    session_id: Mutex::new(None),
                    cache: Mutex::new(None),
                })
            })
            .collect()
    }

    fn post(&self, msg: &Value) -> Result<HttpReply, String> {
        let sid = self.session_id.lock().ok().and_then(|g| g.clone());
        let mut headers: Vec<(&str, &str)> = vec![
            ("Content-Type", "application/json"),
            ("Accept", "application/json, text/event-stream"),
        ];
        if let Some(s) = sid.as_deref() {
            headers.push(("Mcp-Session-Id", s));
        }
        http_post(
            &self.url,
            &headers,
            &msg.to_string(),
            Duration::from_secs(90),
        )
    }

    fn initialize(&self) -> Result<(), String> {
        // Never send a stale id on initialize: the upstream answers 404 to it.
        if let Ok(mut g) = self.session_id.lock() {
            *g = None;
        }
        let r = self.post(&json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "instance-mcp-rpi", "version": "0.4.0"}}
        }))?;
        if !(200..300).contains(&r.status) {
            return Err(format!(
                "upstream {} initialize HTTP {}",
                self.name, r.status
            ));
        }
        let sid = r
            .headers
            .iter()
            .find(|(k, _)| k == "mcp-session-id")
            .map(|(_, v)| v.clone());
        if let Ok(mut g) = self.session_id.lock() {
            *g = sid;
        }
        let _ = self.post(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        if let Ok(mut c) = self.cache.lock() {
            *c = None;
        }
        Ok(())
    }

    fn rpc(&self, method: &str, params: Option<Value>) -> Result<Value, String> {
        if self
            .session_id
            .lock()
            .ok()
            .map(|g| g.is_none())
            .unwrap_or(true)
        {
            self.initialize()?;
        }
        let mut msg = json!({"jsonrpc": "2.0", "id": 1, "method": method});
        if let Some(p) = &params {
            msg["params"] = p.clone();
        }
        let mut r = self.post(&msg)?;
        if r.status == 404 || r.status == 400 {
            // Upstream lost our session (restart). One re-init, one retry.
            self.initialize()?;
            r = self.post(&msg)?;
        }
        if !(200..300).contains(&r.status) {
            return Err(format!(
                "upstream {} HTTP {}: {}",
                self.name,
                r.status,
                &r.body[..r.body.len().min(120)]
            ));
        }
        let v = parse_mcp_body(&r.body)?;
        if let Some(e) = v.get("error") {
            return Err(format!("upstream {} error: {e}", self.name));
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    fn tools(&self) -> Vec<Value> {
        if let Ok(c) = self.cache.lock() {
            if let Some((at, t)) = c.as_ref() {
                if at.elapsed() < Duration::from_secs(30) {
                    return t.clone();
                }
            }
        }
        let list = match self.rpc("tools/list", None) {
            Ok(r) => r
                .get("tools")
                .and_then(|t| t.as_array())
                .cloned()
                .unwrap_or_default(),
            Err(e) => {
                eprintln!("upstream {}: {e}", self.name);
                // Do not cache a failure: the next tools/list retries immediately.
                if let Ok(mut c) = self.cache.lock() {
                    *c = None;
                }
                return Vec::new();
            }
        };
        if let Ok(mut c) = self.cache.lock() {
            *c = Some((std::time::Instant::now(), list.clone()));
        }
        list
    }
}

/// Same list as Swift `ToolProfile.sandboxBrowserTools`: navigate / read / interact.
/// Everything else from the upstream (run_code_unsafe, upload, pdf, network, raw
/// mouse-by-coordinate, dialogs, close, and any new tool) is denied under sandbox.
const SANDBOX_BROWSER_TOOLS: &[&str] = &[
    "browser_click",
    "browser_console_messages",
    "browser_evaluate",
    "browser_fill_form",
    "browser_find",
    "browser_hover",
    "browser_navigate",
    "browser_navigate_back",
    "browser_press_key",
    "browser_resize",
    "browser_select_option",
    "browser_snapshot",
    "browser_tabs",
    "browser_take_screenshot",
    "browser_type",
    "browser_wait_for",
];

fn upstream_tool_allowed(name: &str, profile: &str) -> bool {
    profile != "sandbox" || SANDBOX_BROWSER_TOOLS.contains(&name)
}

static UPSTREAMS: Mutex<Vec<Arc<Upstream>>> = Mutex::new(Vec::new());

fn upstreams() -> Vec<Arc<Upstream>> {
    UPSTREAMS.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Upstream tools visible to `profile`, excluding names that collide with local tools.
fn upstream_tools_for(profile: &str, local_names: &[&str]) -> Vec<(Arc<Upstream>, Value)> {
    let mut out = Vec::new();
    for up in upstreams() {
        for t in up.tools() {
            let Some(name) = t.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            if local_names.contains(&name) || !upstream_tool_allowed(name, profile) {
                continue;
            }
            out.push((up.clone(), t));
        }
    }
    out
}

fn upstream_owning(name: &str, profile: &str) -> Option<Arc<Upstream>> {
    if !upstream_tool_allowed(name, profile) {
        return None;
    }
    upstreams().into_iter().find(|up| {
        up.tools()
            .iter()
            .any(|t| t.get("name").and_then(|n| n.as_str()) == Some(name))
    })
}

// ---------------------------------------------------------------------------
// WebSocket dial loop
// ---------------------------------------------------------------------------

struct DialGrant {
    runtime: String,
    session: String,
    secret: String,
    profile: String,
    deadline_epoch_secs: u64,
    registry: Registry,
    grant_id: String,
    cancelled: Arc<AtomicBool>,
}

fn dial_loop(config: DialGrant) {
    let DialGrant {
        runtime,
        session,
        secret,
        profile,
        deadline_epoch_secs,
        registry,
        grant_id,
        cancelled,
    } = config;
    let mut backoff: u64 = 1;
    let cap: u64 = 30;

    loop {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        if now_epoch_secs() >= deadline_epoch_secs {
            break;
        }

        set_state(&registry, &grant_id, "dialing");
        let url = attach_url(&runtime, &session);

        let req = match build_ws_request(&url, &secret) {
            Ok(r) => r,
            Err(_) => {
                set_state(&registry, &grant_id, "redialing");
                if !sleep_until_backoff(deadline_epoch_secs, &mut backoff, cap, cancelled.as_ref())
                {
                    break;
                }
                continue;
            }
        };

        match tungstenite::connect(req) {
            Ok((mut socket, _resp)) => {
                // DELETE must revoke an attached node promptly. The early PoC
                // blocked in read until the runtime sent another frame. A short
                // read timeout lets the cancellation flag close the socket in
                // at most one second on plain ws:// (the deployed rpi1 path).
                if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                }
                backoff = 1; // successful attach resets transient retry history
                set_state(&registry, &grant_id, "attached");
                loop {
                    if cancelled.load(Ordering::Acquire) {
                        let _ = socket.close(None);
                        return;
                    }
                    match socket.read() {
                        Ok(Message::Text(t)) => {
                            if let Some(reply) = answer(t.as_str(), &profile) {
                                if socket.send(Message::Text(reply)).is_err() {
                                    break;
                                }
                            }
                        }
                        Ok(Message::Binary(b)) => {
                            let t = String::from_utf8_lossy(&b).to_string();
                            if let Some(reply) = answer(&t, &profile) {
                                if socket.send(Message::Text(reply)).is_err() {
                                    break;
                                }
                            }
                        }
                        Ok(Message::Ping(p)) => {
                            let _ = socket.send(Message::Pong(p));
                        }
                        Ok(Message::Pong(_)) => {}
                        Ok(Message::Close(frame)) => {
                            let _ = socket.flush();
                            let code = frame.as_ref().map(|f| u16::from(f.code)).unwrap_or(1000);
                            match disposition_close(code) {
                                Disposition::Stop(reason) => {
                                    set_ended(&registry, &grant_id, &reason);
                                    return;
                                }
                                Disposition::Redial => break,
                            }
                        }
                        Ok(Message::Frame(_)) => {}
                        Err(tungstenite::Error::Io(ref error))
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            continue;
                        }
                        Err(_) => break,
                    }
                }
            }
            Err(error) => {
                if let Some(status) = handshake_status(&error) {
                    match disposition_handshake(status) {
                        Disposition::Stop(reason) => {
                            set_ended(&registry, &grant_id, &reason);
                            return;
                        }
                        Disposition::Redial => {}
                    }
                }
            }
        }

        set_state(&registry, &grant_id, "redialing");
        if !sleep_until_backoff(deadline_epoch_secs, &mut backoff, cap, cancelled.as_ref()) {
            break;
        }
    }

    if !cancelled.load(Ordering::Acquire) {
        set_ended(&registry, &grant_id, "deadline");
    }
}

// Sleep min(backoff, remaining_to_deadline), checking DELETE cancellation every
// 100 ms; grow backoff. Returns false at deadline or cancellation.
fn sleep_until_backoff(
    deadline_epoch_secs: u64,
    backoff: &mut u64,
    cap: u64,
    cancelled: &AtomicBool,
) -> bool {
    let now = now_epoch_secs();
    if now >= deadline_epoch_secs || cancelled.load(Ordering::Acquire) {
        return false;
    }
    let remaining = deadline_epoch_secs - now;
    let nap = (*backoff).min(remaining);
    for _ in 0..nap.saturating_mul(10) {
        if cancelled.load(Ordering::Acquire) {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
    *backoff = (*backoff * 2).min(cap);
    now_epoch_secs() < deadline_epoch_secs && !cancelled.load(Ordering::Acquire)
}

fn build_ws_request(url: &str, secret: &str) -> Result<Request<()>, String> {
    use tungstenite::handshake::client::generate_key;

    // Determine host authority for the Host header.
    let after_scheme = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .ok_or_else(|| "attach url missing ws scheme".to_string())?;
    let authority = match after_scheme.find('/') {
        Some(idx) => &after_scheme[..idx],
        None => after_scheme,
    };
    let host = authority
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(authority);
    let _ = host;

    Request::builder()
        .method("GET")
        .uri(url)
        .header("Host", authority)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", generate_key())
        .header("Authorization", format!("Bearer {secret}"))
        .body(())
        .map_err(|e| format!("failed to build request: {e}"))
}

// Extract an HTTP status code from a tungstenite handshake error, if present.
fn handshake_status(err: &tungstenite::Error) -> Option<u16> {
    match err {
        tungstenite::Error::Http(resp) => Some(resp.status().as_u16()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// answer(): MCP JSON-RPC dispatch
// ---------------------------------------------------------------------------

fn answer(text: &str, profile: &str) -> Option<String> {
    let req: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return None,
    };

    // Notifications have no id -> no response.
    let id = req.get("id").cloned()?;

    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    let result: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "instance-mcp-rpi",
                "version": "0.3.0"
            }
        })),
        "tools/list" => Ok(json!({ "tools": tool_list(profile) })),
        "tools/call" => handle_tool_call(&params, profile),
        other => Err((-32601, format!("method not found: {other}"))),
    };

    let response = match result {
        Ok(res) => json!({ "jsonrpc": "2.0", "id": id, "result": res }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        }),
    };

    Some(response.to_string())
}

const LOCAL_TOOL_NAMES: &[&str] = &["sys_info", "screenshot", "bash", "mouse", "key"];

fn tool_list(profile: &str) -> Value {
    let sys_info = json!({
        "name": "sys_info",
        "description": "Report OS, CPU, memory, architecture and hostname of this node.",
        "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
    });
    let screenshot = json!({
        "name": "screenshot",
        "description": "Capture the node's Wayland display (via grim) and return it as an image. \
                        Default PNG at scale 0.5 (960x540 for a 1080p output). jpeg only if the \
                        node's grim was built with JPEG support (Debian's is not).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "scale":   { "type": "number", "description": "output scale factor, default 0.5" },
                "format":  { "type": "string", "enum": ["jpeg", "png"], "description": "default png" },
                "quality": { "type": "integer", "description": "jpeg quality 1-100, default 80" }
            },
            "additionalProperties": false
        }
    });
    let bash = json!({
        "name": "bash",
        "description": "Run a command with `bash -c` on this node as the daemon user. Returns stdout, \
                        stderr, exit code and duration. The whole process group is killed on timeout \
                        (exit 137, timed_out=true). Output is capped per stream.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "command":          { "type": "string" },
                "cwd":              { "type": "string", "description": "working directory; leading ~ expands to $HOME" },
                "timeout_secs":     { "type": "integer", "description": "default 60, max 600" },
                "max_output_bytes": { "type": "integer", "description": "per stream, default 65536, max 1048576" }
            },
            "required": ["command"],
            "additionalProperties": false
        }
    });

    let mouse = json!({
        "name": "mouse",
        "description": "Pointer input on this node's Wayland display via wlrctl. Coordinates are display \
                        pixels = screenshot pixels at scale 1 (1920x1080 here). Actions: move, click, \
                        double_click, right_click, drag (x,y → to_x,to_y), scroll (dy/dx, positive = down/right).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["move", "click", "double_click", "right_click", "drag", "scroll"] },
                "x": { "type": "number" }, "y": { "type": "number" },
                "to_x": { "type": "number" }, "to_y": { "type": "number" },
                "dx": { "type": "number" }, "dy": { "type": "number" }
            },
            "required": ["action"],
            "additionalProperties": false
        }
    });
    let key = json!({
        "name": "key",
        "description": "Keyboard input via wtype. `type`: send text (unicode, layout independent). `press`: \
                        a key combo such as \"Return\", \"Tab\", \"ctrl+c\", \"ctrl+shift+t\", \"alt+F4\" \
                        (xkb key names; modifiers ctrl/shift/alt/super).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["type", "press"] },
                "text": { "type": "string" },
                "combo": { "type": "string" }
            },
            "required": ["action"],
            "additionalProperties": false
        }
    });

    // Both profiles get everything: a lent node is only useful if the agent can act on it,
    // and the macOS sandbox profile already leaks a shell through `osascript`.
    let _ = profile;
    let mut tools = vec![sys_info, screenshot, bash, mouse, key];
    for (_, t) in upstream_tools_for(profile, LOCAL_TOOL_NAMES) {
        tools.push(t);
    }
    Value::Array(tools)
}

fn handle_tool_call(params: &Value, profile: &str) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| (-32602, "missing tool name".to_string()))?;
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);

    match name {
        "sys_info" => Ok(tool_result(tool_sys_info())),
        "screenshot" => tool_screenshot(&arguments),
        "bash" => {
            let _ = profile;
            tool_bash(&arguments)
        }
        "mouse" => tool_mouse(&arguments),
        "key" => tool_key(&arguments),
        other => match upstream_owning(other, profile) {
            Some(up) => up
                .rpc(
                    "tools/call",
                    Some(json!({"name": other, "arguments": arguments})),
                )
                .map_err(|e| (-32000, e)),
            None => Err((-32601, format!("unknown tool: {other}"))),
        },
    }
}

// Wrap a structured value into the MCP tool result shape.
fn tool_result(structured: Value) -> Value {
    let text = serde_json::to_string_pretty(&structured).unwrap_or_else(|_| structured.to_string());
    json!({
        "content": [ { "type": "text", "text": text } ],
        "structuredContent": structured
    })
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

fn read_file_trim(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn tool_sys_info() -> Value {
    // PRETTY_NAME from /etc/os-release
    let pretty_name = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|content| {
            content.lines().find_map(|line| {
                line.strip_prefix("PRETTY_NAME=")
                    .map(|v| v.trim().trim_matches('"').to_string())
            })
        })
        .unwrap_or_else(|| "unknown".to_string());

    // processor count from /proc/cpuinfo
    let cpu_count = std::fs::read_to_string("/proc/cpuinfo")
        .map(|c| c.lines().filter(|l| l.starts_with("processor")).count())
        .unwrap_or(0);

    // MemTotal from /proc/meminfo
    let mem_total = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find_map(|line| line.strip_prefix("MemTotal:").map(|v| v.trim().to_string()))
        })
        .unwrap_or_else(|| "unknown".to_string());

    let arch = std::env::consts::ARCH;

    let hostname =
        read_file_trim("/proc/sys/kernel/hostname").unwrap_or_else(|| "unknown".to_string());

    // `host` / `displays` / `permissions` / `agent` are what OpenAB Connect's
    // Screens pane reads; the rest is ours.
    let display_ok = std::process::Command::new("grim")
        .env(
            "WAYLAND_DISPLAY",
            std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into()),
        )
        .env(
            "XDG_RUNTIME_DIR",
            std::env::var("XDG_RUNTIME_DIR")
                .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc_getuid() })),
        )
        .args(["-s", "0.05", "-t", "png", "-"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    json!({
        "host": hostname,
        "hostname": hostname,
        "os": pretty_name,
        "cpu_count": cpu_count,
        "mem_total": mem_total,
        "arch": arch,
        "displays": if display_ok { json!([{ "index": 0, "kind": "wayland" }]) } else { json!([]) },
        "permissions": { "screen_recording": display_ok, "accessibility": false },
        "agent": { "name": "instance-mcp-rpi", "version": "0.3.0", "platform": "linux" }
    })
}

fn tool_screenshot(arguments: &Value) -> Result<Value, (i64, String)> {
    let scale = arguments
        .get("scale")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.5);
    let format = arguments
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("png")
        .to_string();
    let quality = arguments
        .get("quality")
        .and_then(|v| v.as_i64())
        .unwrap_or(80);
    if !(0.05..=2.0).contains(&scale) {
        return Err((-32602, "scale must be in 0.05..=2.0".to_string()));
    }
    if format != "jpeg" && format != "png" {
        return Err((-32602, "format must be jpeg or png".to_string()));
    }
    if !(1..=100).contains(&quality) {
        return Err((-32602, "quality must be in 1..=100".to_string()));
    }

    let mut out = grim_command(scale, &format, quality)
        .output()
        .map_err(|e| (-32000, format!("grim: {e}")))?;
    let mut format = format;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if format == "jpeg" && err.contains("jpeg support disabled") {
            // Debian's grim is built without libjpeg; callers (Connect asks for
            // jpeg) decode by content, so hand back PNG instead of failing.
            format = "png".to_string();
            out = grim_command(scale, &format, quality)
                .output()
                .map_err(|e| (-32000, format!("grim: {e}")))?;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                return Err((-32000, format!("grim failed ({}): {err}", out.status)));
            }
        } else {
            return Err((-32000, format!("grim failed ({}): {err}", out.status)));
        }
    }
    let mime = if format == "png" {
        "image/png"
    } else {
        "image/jpeg"
    };
    let data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &out.stdout);
    Ok(json!({
        "content": [ { "type": "image", "mimeType": mime, "data": data } ],
        "structuredContent": { "bytes": out.stdout.len(), "scale": scale, "format": format }
    }))
}

// grim needs WAYLAND_DISPLAY + XDG_RUNTIME_DIR; a systemd unit or an ssh-started
// daemon does not have them. Default to the seat's usual values.
fn grim_command(scale: f64, format: &str, quality: i64) -> std::process::Command {
    let mut cmd = std::process::Command::new("grim");
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        cmd.env("WAYLAND_DISPLAY", "wayland-0");
    }
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        cmd.env(
            "XDG_RUNTIME_DIR",
            format!("/run/user/{}", unsafe { libc_getuid() }),
        );
    }
    cmd.arg("-s").arg(format!("{scale}")).arg("-t").arg(format);
    if format == "jpeg" {
        cmd.arg("-q").arg(quality.to_string());
    }
    cmd.arg("-"); // stdout
    cmd
}

extern "C" {
    fn getuid() -> u32;
}
unsafe fn libc_getuid() -> u32 {
    getuid()
}

fn tool_bash(arguments: &Value) -> Result<Value, (i64, String)> {
    use std::process::Stdio;

    let command = arguments
        .get("command")
        .and_then(|c| c.as_str())
        .filter(|c| !c.is_empty())
        .ok_or_else(|| (-32602, "missing command".to_string()))?;
    let timeout_secs = arguments
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(60)
        .clamp(1, 600);
    let cap = arguments
        .get("max_output_bytes")
        .and_then(|v| v.as_u64())
        .unwrap_or(65_536)
        .clamp(1, 1_048_576) as usize;

    let mut cmd = std::process::Command::new("setsid");
    cmd.arg("bash").arg("-c").arg(command);
    if let Some(cwd) = arguments.get("cwd").and_then(|v| v.as_str()) {
        let cwd = if let Some(rest) = cwd.strip_prefix('~') {
            format!("{}{}", std::env::var("HOME").unwrap_or_default(), rest)
        } else {
            cwd.to_string()
        };
        if !std::path::Path::new(&cwd).is_dir() {
            return Err((-32602, format!("cwd is not a directory: {cwd}")));
        }
        cmd.current_dir(cwd);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let started = std::time::Instant::now();
    let mut child = cmd.spawn().map_err(|e| (-32000, format!("spawn: {e}")))?;
    let pid = child.id() as i32;

    // Drain both pipes on threads so a chatty child cannot block on a full pipe.
    fn drain(mut r: impl Read + Send + 'static, cap: usize) -> thread::JoinHandle<(Vec<u8>, bool)> {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            let mut truncated = false;
            loop {
                match r.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if buf.len() < cap {
                            let take = n.min(cap - buf.len());
                            buf.extend_from_slice(&tmp[..take]);
                            if take < n {
                                truncated = true;
                            }
                        } else {
                            truncated = true;
                        }
                    }
                }
            }
            (buf, truncated)
        })
    }
    let out_t = drain(child.stdout.take().unwrap(), cap);
    let err_t = drain(child.stderr.take().unwrap(), cap);

    let deadline = started + Duration::from_secs(timeout_secs);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    timed_out = true;
                    unsafe {
                        kill(-pid, 9); // whole process group (setsid made pid the leader)
                    }
                    break child.wait().ok();
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break None,
        }
    };
    let (stdout, out_trunc) = out_t.join().unwrap_or_default();
    let (stderr, err_trunc) = err_t.join().unwrap_or_default();

    let exit = if timed_out {
        137
    } else {
        status.and_then(|s| s.code()).unwrap_or(-1)
    };
    let structured = json!({
        "exit_code": exit,
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
        "stdout_truncated": out_trunc,
        "stderr_truncated": err_trunc,
        "timed_out": timed_out,
        "duration_ms": started.elapsed().as_millis() as u64,
        "pid": pid
    });
    Ok(tool_result(structured))
}

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

// ---------------------------------------------------------------------------
// mouse / key — wlrctl (virtual pointer) + wtype (virtual keyboard) on the seat
// ---------------------------------------------------------------------------

fn seat_command(bin: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(bin);
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        cmd.env("WAYLAND_DISPLAY", "wayland-0");
    }
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        cmd.env(
            "XDG_RUNTIME_DIR",
            format!("/run/user/{}", unsafe { libc_getuid() }),
        );
    }
    cmd
}

fn run_seat(bin: &str, args: &[String]) -> Result<(), (i64, String)> {
    let out = seat_command(bin)
        .args(args)
        .output()
        .map_err(|e| (-32000, format!("{bin}: {e}")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err((
            -32000,
            format!(
                "{bin} {} failed ({}): {}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ))
    }
}

fn sv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// wlrctl only knows relative motion: pin to the top-left corner, then move by (x, y).
fn pointer_goto(x: f64, y: f64) -> Result<(), (i64, String)> {
    run_seat("wlrctl", &sv(&["pointer", "move", "-20000", "-20000"]))?;
    run_seat(
        "wlrctl",
        &sv(&[
            "pointer",
            "move",
            &format!("{}", x.round() as i64),
            &format!("{}", y.round() as i64),
        ]),
    )
}

fn num(args: &Value, k: &str) -> Option<f64> {
    args.get(k).and_then(|v| v.as_f64())
}

fn tool_mouse(arguments: &Value) -> Result<Value, (i64, String)> {
    let action = arguments
        .get("action")
        .and_then(|a| a.as_str())
        .ok_or_else(|| (-32602, "missing action".to_string()))?;
    let xy = |a: &Value| -> Result<(f64, f64), (i64, String)> {
        match (num(a, "x"), num(a, "y")) {
            (Some(x), Some(y)) => Ok((x, y)),
            _ => Err((-32602, format!("{action} needs x and y"))),
        }
    };
    match action {
        "move" => {
            let (x, y) = xy(arguments)?;
            pointer_goto(x, y)?;
        }
        "click" | "double_click" | "right_click" => {
            if let (Some(x), Some(y)) = (num(arguments, "x"), num(arguments, "y")) {
                pointer_goto(x, y)?;
                thread::sleep(Duration::from_millis(40));
            }
            let button = if action == "right_click" {
                "right"
            } else {
                "left"
            };
            run_seat("wlrctl", &sv(&["pointer", "click", button]))?;
            if action == "double_click" {
                thread::sleep(Duration::from_millis(60));
                run_seat("wlrctl", &sv(&["pointer", "click", button]))?;
            }
        }
        "drag" => {
            let (x, y) = xy(arguments)?;
            let (tx, ty) = match (num(arguments, "to_x"), num(arguments, "to_y")) {
                (Some(a), Some(b)) => (a, b),
                _ => return Err((-32602, "drag needs to_x and to_y".to_string())),
            };
            pointer_goto(x, y)?;
            run_seat("wlrctl", &sv(&["pointer", "press", "left"]))?;
            thread::sleep(Duration::from_millis(60));
            run_seat(
                "wlrctl",
                &sv(&[
                    "pointer",
                    "move",
                    &format!("{}", (tx - x).round() as i64),
                    &format!("{}", (ty - y).round() as i64),
                ]),
            )?;
            thread::sleep(Duration::from_millis(60));
            run_seat("wlrctl", &sv(&["pointer", "release", "left"]))?;
        }
        "scroll" => {
            if let (Some(x), Some(y)) = (num(arguments, "x"), num(arguments, "y")) {
                pointer_goto(x, y)?;
            }
            let dy = num(arguments, "dy").unwrap_or(0.0);
            let dx = num(arguments, "dx").unwrap_or(0.0);
            run_seat(
                "wlrctl",
                &sv(&[
                    "pointer",
                    "scroll",
                    &format!("{}", dy.round() as i64),
                    &format!("{}", dx.round() as i64),
                ]),
            )?;
        }
        other => return Err((-32602, format!("unknown mouse action: {other}"))),
    }
    Ok(tool_result(json!({ "ok": true, "action": action })))
}

fn tool_key(arguments: &Value) -> Result<Value, (i64, String)> {
    let action = arguments
        .get("action")
        .and_then(|a| a.as_str())
        .ok_or_else(|| (-32602, "missing action".to_string()))?;
    match action {
        "type" => {
            let text = arguments
                .get("text")
                .and_then(|t| t.as_str())
                .ok_or_else(|| (-32602, "type needs text".to_string()))?;
            // `--` so text starting with '-' is not parsed as a flag.
            run_seat("wtype", &sv(&["--", text]))?;
            Ok(tool_result(
                json!({ "ok": true, "action": "type", "chars": text.chars().count() }),
            ))
        }
        "press" => {
            let combo = arguments
                .get("combo")
                .and_then(|c| c.as_str())
                .ok_or_else(|| (-32602, "press needs combo".to_string()))?;
            let parts: Vec<&str> = combo
                .split('+')
                .map(|p| p.trim())
                .filter(|p| !p.is_empty())
                .collect();
            let (mods, keys): (Vec<&str>, Vec<&str>) = parts.iter().partition(|p| {
                matches!(
                    p.to_ascii_lowercase().as_str(),
                    "ctrl"
                        | "control"
                        | "shift"
                        | "alt"
                        | "super"
                        | "cmd"
                        | "meta"
                        | "win"
                        | "altgr"
                )
            });
            if keys.len() != 1 {
                return Err((
                    -32602,
                    format!("combo must have exactly one non-modifier key: {combo}"),
                ));
            }
            let norm = |m: &str| match m.to_ascii_lowercase().as_str() {
                "control" => "ctrl".to_string(),
                "cmd" | "meta" | "win" | "super" => "logo".to_string(),
                x => x.to_string(),
            };
            let key = match keys[0] {
                "Enter" | "enter" | "return" => "Return".to_string(),
                "esc" | "Esc" => "Escape".to_string(),
                "tab" => "Tab".to_string(),
                "space" => "space".to_string(),
                k => k.to_string(),
            };
            let mut args: Vec<String> = Vec::new();
            for m in &mods {
                args.push("-M".into());
                args.push(norm(m));
            }
            args.push("-k".into());
            args.push(key.clone());
            for m in mods.iter().rev() {
                args.push("-m".into());
                args.push(norm(m));
            }
            run_seat("wtype", &args)?;
            Ok(tool_result(
                json!({ "ok": true, "action": "press", "combo": combo }),
            ))
        }
        other => Err((-32602, format!("unknown key action: {other}"))),
    }
}

// ---------------------------------------------------------------------------
// AuthPolicy — same shape as the Swift daemon: every configured check must pass.
//   MCP_TOKEN / MCP_TOKEN_FILE   require `Authorization: Bearer` (constant-time compare)
//   MCP_ALLOW_LOGIN              comma list matched against `Tailscale-User-Login`
//                                (injected+overwritten by `tailscale serve`)
//   MCP_INSECURE_LOCAL=1         allow bare loopback requests with nothing else set
// ---------------------------------------------------------------------------

struct AuthPolicy {
    token: Option<String>,
    allow_logins: Vec<String>,
    insecure_local: bool,
}

impl AuthPolicy {
    fn from_env() -> Result<AuthPolicy, String> {
        let mut token = std::env::var("MCP_TOKEN").ok().filter(|s| !s.is_empty());
        if token.is_none() {
            if let Ok(path) = std::env::var("MCP_TOKEN_FILE") {
                let t = std::fs::read_to_string(&path)
                    .map_err(|e| format!("MCP_TOKEN_FILE {path}: {e}"))?;
                let t = t.trim().to_string();
                if !t.is_empty() {
                    token = Some(t);
                }
            }
        }
        let allow_logins: Vec<String> = std::env::var("MCP_ALLOW_LOGIN")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        let insecure_local = matches!(
            std::env::var("MCP_INSECURE_LOCAL").as_deref(),
            Ok("1") | Ok("true")
        );
        if token.is_none() && allow_logins.is_empty() && !insecure_local {
            return Err(
                "refusing to start with no auth: set MCP_TOKEN / MCP_TOKEN_FILE and/or \
                        MCP_ALLOW_LOGIN (or MCP_INSECURE_LOCAL=1 for loopback debugging)"
                    .to_string(),
            );
        }
        Ok(AuthPolicy {
            token,
            allow_logins,
            insecure_local,
        })
    }

    fn describe(&self) -> String {
        format!(
            "token={} allow_login={:?} insecure_local={}",
            self.token.is_some(),
            self.allow_logins,
            self.insecure_local
        )
    }

    /// The authenticated principal matches the Swift AuthPolicy contract:
    /// allowlisted Tailscale login, `token`, or debug-only `local`.
    fn check(&self, req: &HttpRequest) -> Result<String, String> {
        if let Some(expected) = &self.token {
            let got = req
                .authorization
                .as_deref()
                .and_then(|a| a.strip_prefix("Bearer "))
                .unwrap_or("");
            if !constant_time_eq(got.as_bytes(), expected.as_bytes()) {
                return Err("bearer token missing or wrong".to_string());
            }
        }
        let login = req.ts_login.as_deref().map(|l| l.to_lowercase());
        if !self.allow_logins.is_empty() {
            match login {
                Some(ref l) if self.allow_logins.contains(l) => return Ok(l.clone()),
                Some(l) => return Err(format!("login {l} not allowed")),
                None if self.insecure_local && req.peer_is_loopback => {
                    return Ok("local".to_string())
                }
                None => return Err("no Tailscale-User-Login header".to_string()),
            }
        }
        if let Some(login) = login {
            return Ok(login);
        }
        if self.token.is_some() {
            return Ok("token".to_string());
        }
        if self.insecure_local && req.peer_is_loopback {
            return Ok("local".to_string());
        }
        Err("unauthenticated".to_string())
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// HTTP control server
// ---------------------------------------------------------------------------

fn main() {
    let bind = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:8790".to_string());
    let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
    let policy: Arc<AuthPolicy> = match AuthPolicy::from_env() {
        Ok(p) => Arc::new(p),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    eprintln!("auth: {}", policy.describe());
    let ups = Upstream::from_env();
    for u in &ups {
        eprintln!("upstream {} = {}", u.name, u.url);
    }
    if let Ok(mut g) = UPSTREAMS.lock() {
        *g = ups;
    }

    let listener = match TcpListener::bind(&bind) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("failed to bind {bind}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("reverse-attach control server listening on {bind}");

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let reg = registry.clone();
                let pol = policy.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_conn(s, reg, pol) {
                        eprintln!("connection error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
}

struct HttpRequest {
    method: String,
    path: String,
    body: String,
    authorization: Option<String>,
    ts_login: Option<String>,
    peer_is_loopback: bool,
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, String> {
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();

    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];

    // Read until we have the full headers.
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut tmp).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                break pos + 4;
            }
            return Err("connection closed before headers complete".to_string());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 1_048_576 {
            return Err("request headers too large".to_string());
        }
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = header_text.lines();
    let request_line = lines.next().ok_or_else(|| "empty request".to_string())?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    // Headers we care about.
    let mut content_length: usize = 0;
    let mut authorization = None;
    let mut ts_login = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse::<usize>().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.to_string());
            } else if name.eq_ignore_ascii_case("tailscale-user-login") {
                ts_login = Some(value.to_string());
            }
        }
    }
    let peer_is_loopback = stream
        .peer_addr()
        .map(|a| a.ip().is_loopback())
        .unwrap_or(false);

    // Cap the body before auth runs: an unauthenticated peer must not be able to
    // make us buffer an arbitrary Content-Length. 4 MiB covers every real request
    // (the largest is a tools/call with a screenshot-sized argument, far smaller).
    const MAX_BODY: usize = 4 * 1024 * 1024;
    if content_length > MAX_BODY {
        return Err(format!(
            "request body too large: {content_length} > {MAX_BODY}"
        ));
    }

    // Read remaining body bytes.
    let mut body_bytes: Vec<u8> = buf[header_end..].to_vec();
    while body_bytes.len() < content_length {
        let n = stream
            .read(&mut tmp)
            .map_err(|e| format!("read body: {e}"))?;
        if n == 0 {
            break;
        }
        body_bytes.extend_from_slice(&tmp[..n]);
    }
    body_bytes.truncate(content_length);

    let body = String::from_utf8_lossy(&body_bytes).to_string();

    Ok(HttpRequest {
        method,
        path,
        body,
        authorization,
        ts_login,
        peer_is_loopback,
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn write_raw(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
    extra_headers: &[(&str, &str)],
) {
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in extra_headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

/// MCP Streamable HTTP (JSON response mode). Direct callers — a CLI or the
/// OpenAB Connect Screens pane — get the full `owner` tool surface; the
/// sandbox narrowing only applies to reverse-attached sessions.
fn handle_mcp(stream: &mut TcpStream, body: &str) -> Result<(), String> {
    let is_initialize = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("method")
                .and_then(|m| m.as_str())
                .map(|m| m == "initialize")
        })
        .unwrap_or(false);
    match answer(body, "owner") {
        Some(reply) => {
            let sid = format!(
                "s-{}-{}",
                now_epoch_secs(),
                GRANT_COUNTER.fetch_add(1, Ordering::SeqCst)
            );
            let extra: Vec<(&str, &str)> = if is_initialize {
                vec![("Mcp-Session-Id", &sid)]
            } else {
                vec![]
            };
            write_raw(
                stream,
                200,
                "OK",
                "application/json",
                reply.as_bytes(),
                &extra,
            );
        }
        None => write_raw(stream, 202, "Accepted", "text/plain", b"", &[]),
    }
    Ok(())
}

fn write_response(stream: &mut TcpStream, status: u16, reason: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        status = status,
        reason = reason,
        len = body.len(),
        body = body,
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn handle_conn(
    mut stream: TcpStream,
    registry: Registry,
    policy: Arc<AuthPolicy>,
) -> Result<(), String> {
    let req = read_http_request(&mut stream)?;

    // Strip query string from path for routing.
    let route = req.path.split('?').next().unwrap_or("").to_string();

    if req.method == "GET" && route == "/healthz" {
        write_raw(&mut stream, 200, "OK", "text/plain", b"ok", &[]);
        return Ok(());
    }

    let principal = match policy.check(&req) {
        Ok(principal) => principal,
        Err(reason) => {
            eprintln!("deny {} {} : {reason}", req.method, route);
            write_response(
                &mut stream,
                401,
                "Unauthorized",
                "{\"error\":\"unauthorized\"}",
            );
            return Ok(());
        }
    };

    match (req.method.as_str(), route.as_str()) {
        ("POST", "/mcp") => handle_mcp(&mut stream, &req.body),
        ("GET", "/mcp") => {
            // No server-initiated stream in this PoC.
            write_response(
                &mut stream,
                405,
                "Method Not Allowed",
                "{\"error\":\"no SSE stream\"}",
            );
            Ok(())
        }
        ("DELETE", "/mcp") => {
            write_raw(&mut stream, 204, "No Content", "text/plain", b"", &[]);
            Ok(())
        }
        ("POST", "/attach") => handle_attach(&mut stream, &req.body, registry, &principal),
        ("GET", "/attach") => handle_attachments(&mut stream, registry),
        _ if route.starts_with("/attach/") => {
            let grant_id = route.trim_start_matches("/attach/");
            match req.method.as_str() {
                "GET" => handle_attachment(&mut stream, registry, grant_id),
                "DELETE" => handle_delete_attachment(&mut stream, registry, grant_id),
                _ => {
                    write_response(
                        &mut stream,
                        405,
                        "Method Not Allowed",
                        "{\"error\":\"method not allowed\"}",
                    );
                    Ok(())
                }
            }
        }
        _ => {
            let body = json!({ "error": "not found" }).to_string();
            write_response(&mut stream, 404, "Not Found", &body);
            Ok(())
        }
    }
}

fn bad_request(stream: &mut TcpStream, message: &str) -> Result<(), String> {
    let body = json!({ "error": message }).to_string();
    write_response(stream, 400, "Bad Request", &body);
    Ok(())
}

fn handle_attach(
    stream: &mut TcpStream,
    body: &str,
    registry: Registry,
    principal: &str,
) -> Result<(), String> {
    let parsed: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return bad_request(stream, &format!("invalid JSON: {e}")),
    };

    let runtime = parsed.get("runtime").and_then(|v| v.as_str()).unwrap_or("");
    let session = parsed.get("session").and_then(|v| v.as_str()).unwrap_or("");
    let profile = parsed
        .get("profile")
        .and_then(|v| v.as_str())
        .unwrap_or("owner")
        .to_string();
    let ttl_secs = parsed
        .get("ttl_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(3600);
    let secret = parsed.get("secret").and_then(|v| v.as_str()).unwrap_or("");
    let admin_credential = parsed
        .get("admin_credential")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Validation.
    if !(runtime.starts_with("ws://") || runtime.starts_with("wss://")) {
        return bad_request(stream, "runtime must start with ws:// or wss://");
    }
    if !valid_session(session) {
        return bad_request(stream, "session must match ^[a-z0-9-]{1,32}$");
    }
    // Only the two known profiles. Anything else must not silently widen to owner.
    if profile != "owner" && profile != "sandbox" {
        return bad_request(stream, "profile must be owner or sandbox");
    }
    if !(1..=86400).contains(&ttl_secs) {
        return bad_request(stream, "ttl_secs must be in 1..=86400");
    }
    let has_secret = !secret.is_empty();
    let has_admin = !admin_credential.is_empty();
    if has_secret == has_admin {
        return bad_request(
            stream,
            "exactly one of secret / admin_credential must be provided",
        );
    }

    // Resolve secret + deadline.
    let (effective_secret, expires_in_secs): (String, u64) = if has_admin {
        match mint(runtime, session, admin_credential, ttl_secs) {
            Ok((s, expires_in)) => (s, ttl_secs.min(expires_in)),
            Err(e) => return bad_request(stream, &format!("mint failed: {e}")),
        }
    } else {
        (secret.to_string(), ttl_secs)
    };

    let deadline = now_epoch_secs() + expires_in_secs;
    let grant_id = new_grant_id();
    let cancelled = Arc::new(AtomicBool::new(false));

    {
        let mut map = registry
            .lock()
            .map_err(|_| "registry poisoned".to_string())?;
        // Same replacement semantics as Swift AttachManager: one grant per
        // (runtime, session). A new grant cancels and removes the incumbent.
        let replaced: Vec<String> = map
            .iter()
            .filter(|(_, g)| g.runtime == runtime && g.session == session)
            .map(|(id, _)| id.clone())
            .collect();
        for id in replaced {
            if let Some(old) = map.remove(&id) {
                old.cancelled.store(true, Ordering::Release);
            }
        }
        map.insert(
            grant_id.clone(),
            GrantInfo {
                id: grant_id.clone(),
                runtime: runtime.to_string(),
                session: session.to_string(),
                profile: profile.clone(),
                principal: principal.to_string(),
                state: "idle".to_string(),
                ended: None,
                expires_at_epoch_secs: deadline,
                cancelled: cancelled.clone(),
            },
        );
    }

    // Spawn the dial loop thread.
    let reg = registry.clone();
    let runtime_owned = runtime.to_string();
    let session_owned = session.to_string();
    let profile_owned = profile.clone();
    let gid = grant_id.clone();
    thread::spawn(move || {
        dial_loop(DialGrant {
            runtime: runtime_owned,
            session: session_owned,
            secret: effective_secret,
            profile: profile_owned,
            deadline_epoch_secs: deadline,
            registry: reg,
            grant_id: gid,
            cancelled,
        });
    });

    let response = {
        let map = registry
            .lock()
            .map_err(|_| "registry poisoned".to_string())?;
        grant_json(
            map.get(&grant_id)
                .ok_or_else(|| "grant disappeared".to_string())?,
        )
    };
    write_response(stream, 202, "Accepted", &response.to_string());
    Ok(())
}

fn handle_attachments(stream: &mut TcpStream, registry: Registry) -> Result<(), String> {
    let grants: Vec<Value> = {
        let map = registry
            .lock()
            .map_err(|_| "registry poisoned".to_string())?;
        map.values().map(grant_json).collect()
    };
    let body = json!({ "grants": grants }).to_string();
    write_response(stream, 200, "OK", &body);
    Ok(())
}

fn handle_attachment(
    stream: &mut TcpStream,
    registry: Registry,
    grant_id: &str,
) -> Result<(), String> {
    let grant = {
        let map = registry
            .lock()
            .map_err(|_| "registry poisoned".to_string())?;
        map.get(grant_id).map(grant_json)
    };
    match grant {
        Some(grant) => write_response(stream, 200, "OK", &grant.to_string()),
        None => write_response(stream, 404, "Not Found", "{\"error\":\"no such grant\"}"),
    }
    Ok(())
}

fn handle_delete_attachment(
    stream: &mut TcpStream,
    registry: Registry,
    grant_id: &str,
) -> Result<(), String> {
    let removed = registry
        .lock()
        .map_err(|_| "registry poisoned".to_string())?
        .remove(grant_id);
    match removed {
        Some(grant) => {
            grant.cancelled.store(true, Ordering::Release);
            write_raw(stream, 204, "No Content", "text/plain", b"", &[]);
        }
        None => write_response(stream, 404, "Not Found", "{\"error\":\"no such grant\"}"),
    }
    Ok(())
}
