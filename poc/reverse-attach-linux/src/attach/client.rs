//! Outbound half of reverse attach: mint an attach secret at the runtime, dial `/tools/attach/{session}` over WebSocket, serve MCP on it with the grant's profile, redial/stop per the disposition table.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
use tungstenite::http::Request;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message;

use super::*;
use crate::mcp::answer;

// mint(): replicate the Swift runtimeMintRequest contract (http/ws only)
// ---------------------------------------------------------------------------

pub(crate) fn mint(
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
// WebSocket dial loop
// ---------------------------------------------------------------------------

pub(crate) struct DialGrant {
    pub(crate) runtime: String,
    pub(crate) session: String,
    pub(crate) secret: String,
    pub(crate) profile: String,
    pub(crate) deadline_epoch_secs: u64,
    pub(crate) registry: Registry,
    pub(crate) grant_id: String,
    pub(crate) cancelled: Arc<AtomicBool>,
}

pub(crate) fn dial_loop(config: DialGrant) {
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
