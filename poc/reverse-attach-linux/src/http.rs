//! HTTP/1.1, both directions, on std sockets: a tiny blocking client (upstream MCP, http:// only) and the loopback control server with its routes.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use crate::attach::client::{dial_loop, mint, DialGrant};
use crate::attach::{
    grant_json, new_grant_id, now_epoch_secs, valid_session, GrantInfo, Registry, GRANT_COUNTER,
};
use crate::auth::AuthPolicy;
use crate::mcp::{answer, Upstream, UPSTREAMS};

// Tiny blocking HTTP/1.1 client for http:// only (used for the upstream MCP)
// ---------------------------------------------------------------------------

pub(crate) struct HttpReply {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: String,
}

pub(crate) fn http_post(
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
pub(crate) fn parse_mcp_body(body: &str) -> Result<Value, String> {
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
// HTTP control server
// ---------------------------------------------------------------------------

pub fn serve() {
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

pub(crate) struct HttpRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) body: String,
    pub(crate) authorization: Option<String>,
    pub(crate) ts_login: Option<String>,
    /// Present when the request was relayed by `tailscale serve` (or any proxy);
    /// such a request is never "local", whatever its TCP peer says.
    pub(crate) forwarded_for: Option<String>,
    pub(crate) peer_is_loopback: bool,
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
    let mut forwarded_for = None;
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
            } else if name.eq_ignore_ascii_case("x-forwarded-for") {
                forwarded_for = Some(value.to_string());
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
        forwarded_for,
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
