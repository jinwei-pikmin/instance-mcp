//! This machine's half of reverse attach: dial an `openab-pty` runtime's
//! `GET /tools/attach/{session}`, then act as the MCP **server** on that socket with a
//! per-grant tool profile. Port of `ReverseAttachClient.swift`; contract: openab-pty
//! `CLIENT-CONTRACT.md` §9.2.
//!
//! The runtime is the MCP client here — it sends `initialize` first, then relays the CLI's
//! `tools/list` / `tools/call` with its own ids. Every frame is answered with the same id;
//! frames are handled concurrently so one slow call does not stall the others. No header
//! or frame on this socket is trusted as identity: the grant *is* the auth.
//!
//! Retry ownership is ours (the pod cannot reach us). Close codes decide:
//!
//! | code | meaning (§9.2) | we |
//! |---|---|---|
//! | 4001 | grant TTL elapsed | stop |
//! | 4002 | replaced by a newer attach | stop |
//! | 4004 | session ended | stop |
//! | 4010 | grant revoked | stop |
//! | 4006 / 1000 / error | runtime replaced, dropped | redial with backoff while the grant is valid |
//!
//! A 4xx on the handshake (typically 401: verifier gone) stops; 429 and 5xx wait and redial.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use hyper::http::{HeaderValue, Uri};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use crate::log;
use crate::mcp::{rpc_error, McpServer, RpcRequest, ToolProfile};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    /// 4001
    GrantExpired,
    /// 4002
    Replaced,
    /// 4004
    SessionEnded,
    /// 4010
    Revoked,
    /// HTTP status on upgrade, typically 401.
    HandshakeRejected(u16),
    Cancelled,
    /// Our own grant deadline passed.
    Deadline,
}

impl Terminal {
    pub fn label(self) -> String {
        match self {
            Terminal::GrantExpired => "grant_expired".into(),
            Terminal::Replaced => "replaced".into(),
            Terminal::SessionEnded => "session_ended".into(),
            Terminal::Revoked => "revoked".into(),
            Terminal::HandshakeRejected(s) => format!("handshake_rejected_{s}"),
            Terminal::Cancelled => "cancelled".into(),
            Terminal::Deadline => "deadline".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum State {
    Idle,
    Dialing,
    Attached,
    WaitingToRedial { seconds: f64 },
    Ended(Terminal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Stop(Terminal),
    Redial,
}

pub fn disposition_for_close(code: u16) -> Disposition {
    match code {
        4001 => Disposition::Stop(Terminal::GrantExpired),
        4002 => Disposition::Stop(Terminal::Replaced),
        4004 => Disposition::Stop(Terminal::SessionEnded),
        4010 => Disposition::Stop(Terminal::Revoked),
        _ => Disposition::Redial, // 1000, 1006, 4006, anything else
    }
}

pub fn disposition_for_handshake(status: u16) -> Disposition {
    match status {
        101 | 200..=299 => Disposition::Redial, // not a rejection
        429 | 500..=599 => Disposition::Redial, // throttled or unwell: wait
        s => Disposition::Stop(Terminal::HandshakeRejected(s)),
    }
}

#[derive(Clone)]
pub struct Config {
    /// `ws://` or `wss://` base of the runtime, e.g. `ws://100.111.174.31:8090`.
    pub runtime: Uri,
    pub session: String,
    pub secret: String,
    pub profile: ToolProfile,
    /// When the grant expires as we understand it. Redialling stops here, and an attached
    /// socket is closed here even if the runtime has not closed it first.
    pub deadline: Instant,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Config {
    pub fn new(
        runtime: Uri,
        session: String,
        secret: String,
        profile: ToolProfile,
        deadline: Instant,
    ) -> Self {
        Self {
            runtime,
            session,
            secret,
            profile,
            deadline,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
        }
    }

    /// `{runtime base}/tools/attach/{session}`.
    pub fn attach_url(&self) -> String {
        join_runtime_path(
            &self.runtime,
            None,
            &format!("/tools/attach/{}", self.session),
        )
    }
}

/// `runtime` with its scheme optionally replaced and `suffix` appended to its path (query dropped).
pub fn join_runtime_path(runtime: &Uri, scheme: Option<&str>, suffix: &str) -> String {
    let scheme = scheme.unwrap_or_else(|| runtime.scheme_str().unwrap_or("ws"));
    let auth = runtime.authority().map(|a| a.as_str()).unwrap_or("");
    let base = runtime.path().trim_end_matches('/');
    format!("{scheme}://{auth}{base}{suffix}")
}

/// Handle on a running client: state for status reporting, cancel to stop it.
#[derive(Clone)]
pub struct ClientHandle {
    state: Arc<Mutex<State>>,
    cancel: watch::Sender<bool>,
}

impl ClientHandle {
    pub fn state(&self) -> State {
        *self.state.lock().unwrap()
    }

    /// Stop dialing; an attached socket is closed with 1000.
    pub fn cancel(&self) {
        let _ = self.cancel.send(true);
        set_state(&self.state, State::Ended(Terminal::Cancelled));
    }
}

fn set_state(state: &Mutex<State>, s: State) {
    let mut cur = state.lock().unwrap();
    if !matches!(*cur, State::Ended(_)) {
        // Terminal is sticky.
        *cur = s;
    }
}

/// `server` should already be scoped to `config.profile`; this does not re-scope, so a
/// caller can pass a pre-built, instruction-tailored server.
pub fn start(config: Config, server: McpServer) -> ClientHandle {
    let state = Arc::new(Mutex::new(State::Idle));
    let (cancel, cancel_rx) = watch::channel(false);
    let handle = ClientHandle {
        state: state.clone(),
        cancel,
    };
    tokio::spawn(run(config, Arc::new(server), state, cancel_rx));
    handle
}

async fn run(
    cfg: Config,
    server: Arc<McpServer>,
    state: Arc<Mutex<State>>,
    mut cancel: watch::Receiver<bool>,
) {
    let mut backoff = cfg.initial_backoff;
    loop {
        if *cancel.borrow() {
            return;
        }
        if Instant::now() >= cfg.deadline {
            set_state(&state, State::Ended(Terminal::Deadline));
            return;
        }
        set_state(&state, State::Dialing);
        match dial_once(&cfg, &server, &state, &mut cancel).await {
            Disposition::Stop(t) => {
                log(&format!("attach {}: stopping ({})", cfg.session, t.label()));
                set_state(&state, State::Ended(t));
                return;
            }
            Disposition::Redial => {
                let remaining = cfg.deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    set_state(&state, State::Ended(Terminal::Deadline));
                    return;
                }
                let wait = backoff.min(remaining);
                set_state(
                    &state,
                    State::WaitingToRedial {
                        seconds: wait.as_secs_f64(),
                    },
                );
                log(&format!(
                    "attach {}: redial in {:.1}s",
                    cfg.session,
                    wait.as_secs_f64()
                ));
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = cancel.changed() => return,
                }
                backoff = (backoff * 2).min(cfg.max_backoff);
            }
        }
    }
}

/// One connection lifetime. Returns what to do next.
async fn dial_once(
    cfg: &Config,
    server: &Arc<McpServer>,
    state: &Arc<Mutex<State>>,
    cancel: &mut watch::Receiver<bool>,
) -> Disposition {
    let mut req = match cfg.attach_url().into_client_request() {
        Ok(r) => r,
        Err(e) => {
            log(&format!("attach {}: bad url: {e}", cfg.session));
            return Disposition::Stop(Terminal::HandshakeRejected(0));
        }
    };
    match HeaderValue::from_str(&format!("Bearer {}", cfg.secret)) {
        Ok(v) => {
            req.headers_mut().insert("authorization", v);
        }
        Err(_) => return Disposition::Stop(Terminal::HandshakeRejected(0)),
    }
    let ws_cfg = WebSocketConfig::default()
        .max_message_size(Some(16 << 20))
        .max_frame_size(Some(16 << 20));
    let connect = tokio_tungstenite::connect_async_with_config(req, Some(ws_cfg), false);
    let ws = tokio::select! {
        r = tokio::time::timeout(Duration::from_secs(15), connect) => r,
        _ = cancel.changed() => return Disposition::Stop(Terminal::Cancelled),
    };
    let ws = match ws {
        Err(_) => {
            log(&format!("attach {}: dial timed out", cfg.session));
            return Disposition::Redial;
        }
        Ok(Err(WsError::Http(resp))) => {
            let status = resp.status().as_u16();
            log(&format!(
                "attach {}: handshake rejected {status}",
                cfg.session
            ));
            return disposition_for_handshake(status);
        }
        Ok(Err(e)) => {
            log(&format!("attach {}: dial failed: {e}", cfg.session));
            return Disposition::Redial;
        }
        Ok(Ok((ws, _))) => ws,
    };

    set_state(state, State::Attached);
    let host = cfg.runtime.host().unwrap_or("?");
    log(&format!(
        "attach {}: attached as {} to {host}",
        cfg.session,
        cfg.profile.as_str()
    ));

    let (mut sink, mut stream) = ws.split();
    let (reply_tx, mut reply_rx) = mpsc::channel::<String>(64);
    let deadline = tokio::time::Instant::from_std(cfg.deadline);
    let mut served = 0usize;

    let outcome = loop {
        tokio::select! {
            _ = cancel.changed() => {
                let _ = sink.send(close(CloseCode::Normal, "cancelled")).await;
                break Disposition::Stop(Terminal::Cancelled);
            }
            _ = tokio::time::sleep_until(deadline) => {
                let _ = sink.send(close(CloseCode::Normal, "grant deadline")).await;
                break Disposition::Stop(Terminal::Deadline);
            }
            Some(reply) = reply_rx.recv() => {
                if sink.send(Message::text(reply)).await.is_err() {
                    break Disposition::Redial;
                }
                served += 1;
            }
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(t))) => spawn_answer(server, &cfg.session, t.as_str().to_owned(), &reply_tx),
                Some(Ok(Message::Binary(b))) => {
                    spawn_answer(server, &cfg.session, String::from_utf8_lossy(&b).into_owned(), &reply_tx)
                }
                Some(Ok(Message::Close(frame))) => {
                    let code = frame.map(|f| u16::from(f.code)).unwrap_or(1005);
                    log(&format!("attach {}: closed {code} after {served} calls", cfg.session));
                    break disposition_for_close(code);
                }
                // Pings are answered by tungstenite on the next write/flush.
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    log(&format!("attach {}: socket error {e}", cfg.session));
                    break Disposition::Redial;
                }
                None => break Disposition::Redial,
            }
        }
    };
    outcome
}

fn close(code: CloseCode, reason: &'static str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }))
}

fn spawn_answer(server: &Arc<McpServer>, session: &str, text: String, tx: &mpsc::Sender<String>) {
    let (server, session, tx) = (server.clone(), session.to_string(), tx.clone());
    tokio::spawn(async move {
        if let Some(reply) = answer(&server, &session, &text).await {
            let _ = tx.send(reply).await;
        }
    });
}

/// One inbound frame → one reply (None for notifications). Exactly the HTTP endpoint's
/// dispatch, minus sessions and auth: the socket *is* the session, the grant *is* the auth.
pub async fn answer(server: &McpServer, session: &str, text: &str) -> Option<String> {
    let rpc = match RpcRequest::parse(text.as_bytes()) {
        Ok(r) => r,
        Err(e) => return Some(rpc_error(Value::Null, &e).to_string()),
    };
    let reply = server.handle(&rpc).await?;
    if rpc.method == "tools/call" {
        let name = rpc
            .params
            .as_ref()
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        let err = if reply.get("error").is_some() {
            " -> rpc error"
        } else {
            ""
        };
        log(&format!("sandbox[{session}] tools/call {name}{err}"));
    }
    Some(reply.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_close_codes_stop() {
        assert_eq!(
            disposition_for_close(4001),
            Disposition::Stop(Terminal::GrantExpired)
        );
        assert_eq!(
            disposition_for_close(4002),
            Disposition::Stop(Terminal::Replaced)
        );
        assert_eq!(
            disposition_for_close(4004),
            Disposition::Stop(Terminal::SessionEnded)
        );
        assert_eq!(
            disposition_for_close(4010),
            Disposition::Stop(Terminal::Revoked)
        );
    }

    #[test]
    fn recoverable_close_codes_redial() {
        for c in [1000, 1001, 1006, 4006, 4999] {
            assert_eq!(disposition_for_close(c), Disposition::Redial, "{c}");
        }
    }

    #[test]
    fn handshake_rejection_stops_but_throttle_waits() {
        assert_eq!(
            disposition_for_handshake(401),
            Disposition::Stop(Terminal::HandshakeRejected(401))
        );
        assert_eq!(
            disposition_for_handshake(404),
            Disposition::Stop(Terminal::HandshakeRejected(404))
        );
        assert_eq!(disposition_for_handshake(429), Disposition::Redial);
        assert_eq!(disposition_for_handshake(503), Disposition::Redial);
    }

    #[test]
    fn attach_url_is_built_from_the_runtime_base() {
        let mk = |rt: &str| {
            Config::new(
                rt.parse().unwrap(),
                "laptop".into(),
                "s".into(),
                ToolProfile::Sandbox,
                Instant::now(),
            )
            .attach_url()
        };
        assert_eq!(
            mk("ws://100.1.2.3:8090"),
            "ws://100.1.2.3:8090/tools/attach/laptop"
        );
        assert_eq!(
            mk("wss://pod.x.ts.net/"),
            "wss://pod.x.ts.net/tools/attach/laptop"
        );
        assert_eq!(
            mk("wss://pod.x.ts.net/pty/"),
            "wss://pod.x.ts.net/pty/tools/attach/laptop"
        );
        let rt: Uri = "wss://pod.x.ts.net/base".parse().unwrap();
        assert_eq!(
            join_runtime_path(&rt, Some("https"), "/admin"),
            "https://pod.x.ts.net/base/admin"
        );
    }
}
