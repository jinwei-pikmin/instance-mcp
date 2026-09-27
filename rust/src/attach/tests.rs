//! Port of `ReverseAttachTests.swift`: the `/attach` plane over HTTP, and the client end
//! to end against a fake openab-pty runtime. Unlike the Swift fake, this one checks the
//! bearer on the upgrade, so the 401 → stop path is exercised for real.

// The upgrade callback's error type is fixed by tungstenite's `Callback` trait.
#![allow(clippy::result_large_err)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as WsRequest, Response as WsResponse,
};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

use super::client::{self, Config, State, Terminal};
use super::*;
use crate::auth::AuthPolicy;
use crate::http::{router, Endpoint};
use crate::mcp::{arg_str, McpServer, Tool, ToolFuture, ToolProfile, ToolResult};

struct Fake(&'static str);
impl Tool for Fake {
    fn name(&self) -> &'static str {
        self.0
    }
    fn description(&self) -> String {
        String::new()
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn call<'a>(&'a self, args: &'a Value) -> ToolFuture<'a> {
        Box::pin(async move {
            Ok(ToolResult::text(
                arg_str(args, "msg").unwrap_or(self.0),
                None,
            ))
        })
    }
}

fn full_server() -> McpServer {
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(Fake("echo")),
        Arc::new(Fake("exec")),
        Arc::new(Fake("exec_start")),
        Arc::new(Fake("screenshot")),
    ];
    McpServer::new("t", "0", Some("base".into()), tools)
}

// MARK: - profiles

#[tokio::test]
async fn sandbox_drops_every_exec_tool_and_carries_its_own_instructions() {
    let s = full_server().scoped(
        ToolProfile::Sandbox,
        sandbox_instructions(ToolProfile::Sandbox, Some("base")),
    );
    assert_eq!(s.tool_names(), vec!["echo", "screenshot"]);
    let i = s.instructions.unwrap();
    assert!(i.starts_with("base\n\n") && i.contains("sandbox"));
    assert_eq!(
        full_server()
            .scoped(ToolProfile::Owner, None)
            .tool_names()
            .len(),
        4
    );
    assert_eq!(
        sandbox_instructions(ToolProfile::Owner, Some("base")).as_deref(),
        Some("base")
    );
}

// MARK: - /attach over HTTP

fn mint_ok(seen: Arc<Mutex<Vec<(String, String, String)>>>) -> MintFn {
    Arc::new(move |rt, session, cred, _ttl| {
        seen.lock().unwrap().push((rt.to_string(), session, cred));
        Box::pin(async {
            Ok(Minted {
                secret: "minted-secret".into(),
                expires_in: Duration::from_secs(120),
            })
        })
    })
}

fn app(attach: Option<MintFn>, enabled: bool) -> (axum::Router, Option<Arc<AttachManager>>) {
    let mgr = enabled.then(|| AttachManager::new(full_server(), attach));
    let auth = AuthPolicy::new(["a@b".into()], None, false);
    (
        router(Endpoint::new(
            "/mcp".into(),
            full_server(),
            auth,
            mgr.clone(),
        )),
        mgr,
    )
}

async fn send(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    auth: bool,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if auth {
        b = b
            .header("tailscale-user-login", "a@b")
            .header("x-forwarded-for", "100.1.1.1");
    }
    let mut req = b
        .body(body.map(|v| Body::from(v.to_string())).unwrap_or_default())
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 5555))));
    let r = app.clone().oneshot(req).await.unwrap();
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn attach_needs_the_human_credential_and_is_404_when_disabled() {
    let (a, _) = app(None, true);
    assert_eq!(
        send(&a, "GET", "/attach", None, false).await.0,
        StatusCode::UNAUTHORIZED
    );
    let body = json!({"runtime": "ws://h:1", "session": "s", "secret": "x"});
    assert_eq!(
        send(&a, "POST", "/attach", Some(body), false).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (off, _) = app(None, false);
    assert_eq!(
        send(&off, "GET", "/attach", None, true).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn bad_requests_are_400() {
    let (a, _) = app(None, true);
    let cases = [
        (json!({"session": "s", "secret": "x"}), "runtime"),
        (
            json!({"runtime": "http://h:1", "session": "s", "secret": "x"}),
            "ws://",
        ),
        (
            json!({"runtime": "ws://h:1", "session": "Bad_Name", "secret": "x"}),
            "session",
        ),
        (
            json!({"runtime": "ws://h:1", "session": "s", "secret": "x", "profile": "root"}),
            "profile",
        ),
        (
            json!({"runtime": "ws://h:1", "session": "s", "secret": "x", "ttl_secs": 0}),
            "ttl",
        ),
        (
            json!({"runtime": "ws://h:1", "session": "s"}),
            "exactly one",
        ),
        (
            json!({"runtime": "ws://h:1", "session": "s", "secret": "x", "admin_credential": "y"}),
            "exactly one",
        ),
        (json!([1]), "JSON object"),
    ];
    for (body, needle) in cases {
        let (st, v) = send(&a, "POST", "/attach", Some(body.clone()), true).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            v["error"].as_str().unwrap_or("").contains(needle),
            "{body} → {v}"
        );
    }
}

#[tokio::test]
async fn create_list_replace_revoke() {
    let (a, _) = app(None, true);
    // Unreachable runtime: the grant exists and the client is dialing/redialing.
    let body = json!({"runtime": "ws://127.0.0.1:9", "session": "laptop", "secret": "abc", "ttl_secs": 60});
    let (st, g) = send(&a, "POST", "/attach", Some(body), true).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{g}");
    let id = g["id"].as_str().unwrap().to_string();
    assert_eq!(
        (
            g["profile"].as_str(),
            g["principal"].as_str(),
            g["session"].as_str()
        ),
        (Some("sandbox"), Some("a@b"), Some("laptop"))
    );
    assert!(!g.to_string().contains("abc"), "secret must not be echoed");

    assert_eq!(
        send(&a, "GET", "/attach", None, true).await.1["grants"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        send(&a, "GET", &format!("/attach/{id}"), None, true)
            .await
            .0,
        StatusCode::OK
    );

    // Same target again replaces, not duplicates.
    let body = json!({"runtime": "ws://127.0.0.1:9", "session": "laptop", "secret": "def", "ttl_secs": 60, "profile": "owner"});
    assert_eq!(
        send(&a, "POST", "/attach", Some(body), true).await.0,
        StatusCode::ACCEPTED
    );
    let grants = send(&a, "GET", "/attach", None, true).await.1["grants"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0]["profile"], "owner");
    let id2 = grants[0]["id"].as_str().unwrap().to_string();
    assert_ne!(id, id2);

    assert_eq!(
        send(&a, "DELETE", &format!("/attach/{id2}"), None, true)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(&a, "DELETE", &format!("/attach/{id2}"), None, true)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&a, "GET", "/attach", None, true).await.1["grants"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn admin_credential_path_mints_and_does_not_store_it() {
    let seen = Arc::new(Mutex::new(vec![]));
    let (a, mgr) = app(Some(mint_ok(seen.clone())), true);
    let body = json!({"runtime": "ws://127.0.0.1:9", "session": "s", "admin_credential": "hunter2", "ttl_secs": 3600});
    let (st, g) = send(&a, "POST", "/attach", Some(body), true).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{g}");
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[("ws://127.0.0.1:9/".into(), "s".into(), "hunter2".into())]
    );
    // The runtime said 120 s; that wins over our 3600.
    assert!(g["expires_in_secs"].as_u64().unwrap() <= 120);
    let dumped = serde_json::to_string(&mgr.unwrap().list()).unwrap();
    assert!(!dumped.contains("hunter2") && !dumped.contains("minted-secret"));
}

#[tokio::test]
async fn mint_failure_is_502() {
    let fail: MintFn = Arc::new(|_, _, _, _| {
        Box::pin(async {
            Err(AttachError::MintFailed {
                status: 401,
                body: String::new(),
            })
        })
    });
    let (a, _) = app(Some(fail), true);
    let body = json!({"runtime": "ws://127.0.0.1:9", "session": "s", "admin_credential": "bad"});
    assert_eq!(
        send(&a, "POST", "/attach", Some(body), true).await.0,
        StatusCode::BAD_GATEWAY
    );
}

#[tokio::test]
async fn real_mint_speaks_the_admin_plane_contract() {
    // A one-shot HTTP server playing the runtime's admin plane.
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut s, _) = l.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = s.read(&mut buf).await.unwrap();
        let head = String::from_utf8_lossy(&buf[..n]).to_string();
        let body = r#"{"secret":"fresh","expires_in_secs":90}"#;
        let resp = format!("HTTP/1.1 201 Created\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len());
        s.write_all(resp.as_bytes()).await.unwrap();
        head
    });
    let rt: Uri = format!("ws://127.0.0.1:{port}").parse().unwrap();
    let m = (mint_at_runtime())(rt, "laptop".into(), "adm".into(), Duration::from_secs(3600))
        .await
        .unwrap();
    assert_eq!(
        (m.secret.as_str(), m.expires_in),
        ("fresh", Duration::from_secs(90))
    );
    let head = server.await.unwrap();
    assert!(
        head.starts_with("POST /admin/sessions/laptop/tools-attach HTTP/1.1"),
        "{head}"
    );
    assert!(
        head.to_ascii_lowercase()
            .contains("authorization: bearer adm"),
        "{head}"
    );
}

// MARK: - end to end against a fake openab-pty runtime

struct FakeRuntime {
    port: u16,
    received: Arc<Mutex<Vec<Value>>>,
    upgrades: Arc<Mutex<usize>>,
}

impl FakeRuntime {
    /// Accepts upgrades whose bearer is `secret` (else 401), sends `initialize`,
    /// `tools/list`, a forced `exec`, a permitted `echo`, then closes with `close_code`.
    async fn start(secret: &'static str, close_code: u16) -> Self {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let received = Arc::new(Mutex::new(vec![]));
        let upgrades = Arc::new(Mutex::new(0));
        let (rx, up) = (received.clone(), upgrades.clone());
        tokio::spawn(async move {
            while let Ok((tcp, _)) = l.accept().await {
                *up.lock().unwrap() += 1;
                let rx = rx.clone();
                tokio::spawn(async move {
                    let check =
                        |req: &WsRequest, resp: WsResponse| -> Result<WsResponse, ErrorResponse> {
                            let ok = req.uri().path() == "/tools/attach/laptop"
                                && req
                                    .headers()
                                    .get("authorization")
                                    .and_then(|v| v.to_str().ok())
                                    == Some(&format!("Bearer {secret}"));
                            if ok {
                                Ok(resp)
                            } else {
                                let mut e = ErrorResponse::new(None);
                                *e.status_mut() = StatusCode::UNAUTHORIZED;
                                Err(e)
                            }
                        };
                    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(tcp, check).await else {
                        return;
                    };
                    let script = [
                        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"openab-pty","version":"t"}}}),
                        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
                        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"exec","arguments":{"command":"id"}}}),
                        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"echo","arguments":{"msg":"hi"}}}),
                    ];
                    for frame in script {
                        let expects_reply = frame.get("id").is_some();
                        ws.send(Message::text(frame.to_string())).await.unwrap();
                        if expects_reply {
                            match ws.next().await {
                                Some(Ok(Message::Text(t))) => {
                                    rx.lock().unwrap().push(serde_json::from_str(&t).unwrap())
                                }
                                other => panic!("expected a reply, got {other:?}"),
                            }
                        }
                    }
                    let _ = ws
                        .send(Message::Close(Some(CloseFrame {
                            code: CloseCode::from(close_code),
                            reason: "".into(),
                        })))
                        .await;
                    while ws.next().await.is_some() {}
                });
            }
        });
        FakeRuntime {
            port,
            received,
            upgrades,
        }
    }

    fn url(&self) -> Uri {
        format!("ws://127.0.0.1:{}", self.port).parse().unwrap()
    }
    fn upgrades(&self) -> usize {
        *self.upgrades.lock().unwrap()
    }
}

async fn wait_ended(h: &client::ClientHandle, within: Duration) -> State {
    let t = Instant::now();
    while t.elapsed() < within {
        if let s @ State::Ended(_) = h.state() {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.state()
}

#[tokio::test]
async fn serves_the_runtime_with_the_sandbox_profile_and_stops_on_revoke() {
    let rt = FakeRuntime::start("s3", 4010).await;
    let cfg = Config::new(
        rt.url(),
        "laptop".into(),
        "s3".into(),
        ToolProfile::Sandbox,
        Instant::now() + Duration::from_secs(30),
    );
    let h = client::start(cfg, full_server().scoped(ToolProfile::Sandbox, None));
    assert_eq!(
        wait_ended(&h, Duration::from_secs(10)).await,
        State::Ended(Terminal::Revoked)
    );

    let got = rt.received.lock().unwrap().clone();
    assert_eq!(got.len(), 4, "{got:?}");
    assert_eq!(got[0]["id"], 1);
    assert_eq!(got[0]["result"]["serverInfo"]["name"], "t");
    let names: Vec<&str> = got[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["echo", "screenshot"],
        "sandbox profile: no exec*"
    );
    assert!(got[2]["error"].is_object(), "exec under sandbox is refused");
    assert_eq!(got[3]["result"]["content"][0]["text"], "hi");
    assert_eq!(rt.upgrades(), 1, "4010 is terminal: no redial");
}

#[tokio::test]
async fn runtime_replaced_redials_with_backoff_until_the_deadline() {
    let rt = FakeRuntime::start("s3", 4006).await;
    let mut cfg = Config::new(
        rt.url(),
        "laptop".into(),
        "s3".into(),
        ToolProfile::Owner,
        Instant::now() + Duration::from_millis(2500),
    );
    cfg.initial_backoff = Duration::from_millis(200);
    let h = client::start(cfg, full_server());
    assert_eq!(
        wait_ended(&h, Duration::from_secs(6)).await,
        State::Ended(Terminal::Deadline)
    );
    assert!(
        rt.upgrades() >= 2,
        "4006 must redial, got {}",
        rt.upgrades()
    );
}

#[tokio::test]
async fn handshake_401_stops_without_redialing() {
    let rt = FakeRuntime::start("right", 1000).await;
    let cfg = Config::new(
        rt.url(),
        "laptop".into(),
        "wrong".into(),
        ToolProfile::Sandbox,
        Instant::now() + Duration::from_secs(30),
    );
    let h = client::start(cfg, full_server());
    assert_eq!(
        wait_ended(&h, Duration::from_secs(5)).await,
        State::Ended(Terminal::HandshakeRejected(401))
    );
    assert_eq!(rt.upgrades(), 1);
}

#[tokio::test]
async fn unreachable_runtime_backs_off_and_stops_at_the_deadline() {
    let mut cfg = Config::new(
        "ws://127.0.0.1:1".parse().unwrap(),
        "x".into(),
        "s".into(),
        ToolProfile::Sandbox,
        Instant::now() + Duration::from_millis(1200),
    );
    cfg.initial_backoff = Duration::from_millis(300);
    let h = client::start(cfg, full_server());
    assert_eq!(
        wait_ended(&h, Duration::from_secs(4)).await,
        State::Ended(Terminal::Deadline)
    );
}

#[tokio::test]
async fn cancel_is_terminal_and_sticky() {
    let cfg = Config::new(
        "ws://127.0.0.1:1".parse().unwrap(),
        "x".into(),
        "s".into(),
        ToolProfile::Sandbox,
        Instant::now() + Duration::from_secs(30),
    );
    let h = client::start(cfg, full_server());
    h.cancel();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.state(), State::Ended(Terminal::Cancelled));
}
