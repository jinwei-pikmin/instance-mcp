//! Streamable HTTP transport (MCP 2025-03-26+), request/response mode only. Port of
//! `MCPHTTPEndpoint.swift`: every POST gets a plain `application/json` reply, never an SSE
//! stream; GET → 405. `Mcp-Session-Id` is issued on `initialize` and checked afterwards.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use serde_json::Value;

use crate::attach::{AttachError, AttachManager, AttachRequest, DEFAULT_TTL_SECS};
use crate::auth::{AuthPolicy, Decision};
use crate::log;
use crate::mcp::{rpc_error, McpServer, RpcRequest, ToolProfile};

/// Matches Swift `HTTPParser.maxBodyBytes`.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

pub struct Endpoint {
    pub path: String,
    pub server: McpServer,
    pub auth: AuthPolicy,
    sessions: Mutex<HashSet<String>>,
    /// Reverse-attach grants (`/attach`). None ⇒ those routes answer 404.
    attach: Option<Arc<AttachManager>>,
}

impl Endpoint {
    pub fn new(
        path: String,
        server: McpServer,
        auth: AuthPolicy,
        attach: Option<Arc<AttachManager>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            path,
            server,
            auth,
            sessions: Mutex::new(HashSet::new()),
            attach,
        })
    }
}

pub fn router(ep: Arc<Endpoint>) -> Router {
    Router::new()
        .fallback(handle)
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(ep)
}

fn text(status: StatusCode, s: &str) -> Response {
    (
        status,
        [("content-type", "text/plain; charset=utf-8")],
        s.to_string(),
    )
        .into_response()
}

fn json(status: StatusCode, v: &Value) -> Response {
    (
        status,
        [("content-type", "application/json")],
        v.to_string(),
    )
        .into_response()
}

async fn handle(
    State(ep): State<Arc<Endpoint>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path();
    if path == "/healthz" {
        return text(StatusCode::OK, "ok\n");
    }
    let is_attach = ep.attach.is_some() && (path == "/attach" || path.starts_with("/attach/"));
    if path != ep.path && !is_attach {
        return text(StatusCode::NOT_FOUND, "not found\n");
    }

    let hdrs: HashMap<String, String> = headers
        .iter()
        .filter_map(|(k, v)| {
            Some((
                k.as_str().to_ascii_lowercase(),
                v.to_str().ok()?.to_string(),
            ))
        })
        .collect();
    let principal = match ep.auth.decide(&hdrs, peer.ip().is_loopback()) {
        Decision::Deny { reason } => {
            let from = hdrs
                .get("x-forwarded-for")
                .map(String::as_str)
                .unwrap_or("local");
            log(&format!("deny {method} {path} from {from}: {reason}"));
            return text(StatusCode::UNAUTHORIZED, "unauthorized\n");
        }
        Decision::Allow { principal } => principal,
    };

    if is_attach {
        // Same AuthPolicy as /mcp: the human's credential, never the sandbox's.
        return attach_route(
            ep.attach.as_ref().unwrap(),
            &method,
            path,
            &hdrs,
            &body,
            &principal,
        )
        .await;
    }

    match method {
        Method::POST => post(&ep, &hdrs, &body, &principal).await,
        Method::DELETE => {
            if let Some(sid) = hdrs.get("mcp-session-id") {
                ep.sessions.lock().unwrap().remove(sid);
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Method::GET => text(
            StatusCode::METHOD_NOT_ALLOWED,
            "server-initiated streams not supported\n",
        ),
        _ => text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n"),
    }
}

fn error_json(status: StatusCode, msg: &str) -> Response {
    json(status, &serde_json::json!({ "error": msg }))
}

/// `POST /attach` create · `GET /attach` list · `GET|DELETE /attach/{id}` read / revoke.
async fn attach_route(
    mgr: &AttachManager,
    method: &Method,
    path: &str,
    hdrs: &HashMap<String, String>,
    body: &[u8],
    principal: &str,
) -> Response {
    mgr.sweep();
    let id = path
        .strip_prefix("/attach")
        .unwrap_or("")
        .trim_start_matches('/');
    match (method, id.is_empty()) {
        (&Method::GET, true) => json(StatusCode::OK, &serde_json::json!({ "grants": mgr.list() })),
        (&Method::POST, true) => {
            let is_json = hdrs
                .get("content-type")
                .is_some_and(|c| c.to_ascii_lowercase().starts_with("application/json"));
            let body: Option<Value> = serde_json::from_slice(body).ok().filter(|_| is_json);
            let Some(body) = body.filter(Value::is_object) else {
                return error_json(StatusCode::BAD_REQUEST, "body must be a JSON object");
            };
            let str_field = |k: &str| body.get(k).and_then(Value::as_str).map(String::from);
            let Some(runtime) = str_field("runtime") else {
                return error_json(
                    StatusCode::BAD_REQUEST,
                    "runtime (ws:// or wss:// URL) is required",
                );
            };
            let Some(session) = str_field("session") else {
                return error_json(StatusCode::BAD_REQUEST, "session is required");
            };
            let profile_str = str_field("profile").unwrap_or_else(|| "sandbox".into());
            let Some(profile) = ToolProfile::parse(&profile_str) else {
                let all: Vec<&str> = ToolProfile::ALL.iter().map(|p| p.as_str()).collect();
                return error_json(
                    StatusCode::BAD_REQUEST,
                    &format!("profile must be one of {all:?}"),
                );
            };
            let ttl_secs = crate::mcp::arg_i64(&body, "ttl_secs").unwrap_or(DEFAULT_TTL_SECS);
            let req = AttachRequest {
                runtime,
                session,
                profile,
                ttl_secs,
                secret: str_field("secret"),
                admin_credential: str_field("admin_credential"),
            };
            match mgr.create(req, principal).await {
                Ok(grant) => json(StatusCode::ACCEPTED, &grant),
                Err(e @ AttachError::BadRequest(_)) => {
                    error_json(StatusCode::BAD_REQUEST, &e.to_string())
                }
                Err(e @ AttachError::MintFailed { .. }) => {
                    error_json(StatusCode::BAD_GATEWAY, &e.to_string())
                }
            }
        }
        (&Method::GET, false) => match mgr.get(id) {
            Some(g) => json(StatusCode::OK, &g),
            None => error_json(StatusCode::NOT_FOUND, "no such grant"),
        },
        (&Method::DELETE, false) => {
            if mgr.revoke(id) {
                StatusCode::NO_CONTENT.into_response()
            } else {
                error_json(StatusCode::NOT_FOUND, "no such grant")
            }
        }
        _ => text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n"),
    }
}

async fn post(
    ep: &Endpoint,
    hdrs: &HashMap<String, String>,
    body: &[u8],
    principal: &str,
) -> Response {
    let ct_json = hdrs
        .get("content-type")
        .is_some_and(|c| c.to_ascii_lowercase().starts_with("application/json"));
    if !ct_json {
        return text(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/json\n",
        );
    }
    let rpc = match RpcRequest::parse(body) {
        Ok(r) => r,
        Err(e) => return json(StatusCode::BAD_REQUEST, &rpc_error(Value::Null, &e)),
    };

    let mut new_session = None;
    if rpc.method == "initialize" {
        let sid = uuid::Uuid::new_v4().to_string();
        ep.sessions.lock().unwrap().insert(sid.clone());
        let client = rpc
            .params
            .as_ref()
            .and_then(|p| p.pointer("/clientInfo/name"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        log(&format!("session {sid} opened by {principal} ({client})"));
        new_session = Some(sid);
    } else if let Some(sid) = hdrs.get("mcp-session-id") {
        if !ep.sessions.lock().unwrap().contains(sid) {
            // Spec: unknown session ⇒ 404 so the client re-initializes.
            return text(StatusCode::NOT_FOUND, "unknown session\n");
        }
    }
    // No session header on a non-initialize request: lenient, so curl probes work.

    let mut resp = match ep.server.handle(&rpc).await {
        None => StatusCode::ACCEPTED.into_response(), // notification
        Some(v) => {
            if rpc.method == "tools/call" {
                let name = rpc
                    .params
                    .as_ref()
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let err = if v.get("error").is_some() {
                    " -> rpc error"
                } else {
                    ""
                };
                log(&format!("{principal} tools/call {name}{err}"));
            }
            json(StatusCode::OK, &v)
        }
    };
    if let Some(sid) = new_session {
        resp.headers_mut()
            .insert("mcp-session-id", HeaderValue::from_str(&sid).unwrap());
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{Tool, ToolFuture, ToolResult};
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::json;
    use tower::ServiceExt;

    struct Ping;
    impl Tool for Ping {
        fn name(&self) -> &'static str {
            "sys_info"
        }
        fn description(&self) -> String {
            String::new()
        }
        fn input_schema(&self) -> Value {
            json!({})
        }
        fn call<'a>(&'a self, _: &'a Value) -> ToolFuture<'a> {
            Box::pin(async { Ok(ToolResult::text("pong", None)) })
        }
    }

    fn app() -> Router {
        let server = McpServer::new("t", "0", None, vec![Arc::new(Ping)]);
        let auth = AuthPolicy::new(["me@x.io".into()], Some("tok".into()), false);
        router(Endpoint::new("/mcp".into(), server, auth, None))
    }

    async fn send(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let mut req = req;
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 5555))));
        let r = app.clone().oneshot(req).await.unwrap();
        let (parts, body) = r.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        (
            parts.status,
            parts.headers,
            String::from_utf8_lossy(&bytes).into(),
        )
    }

    fn post(body: Value, sid: Option<&str>, token: &str) -> Request<Body> {
        let mut b = Request::post("/mcp")
            .header("content-type", "application/json")
            .header("tailscale-user-login", "Me@X.io")
            .header("authorization", format!("Bearer {token}"));
        if let Some(s) = sid {
            b = b.header("mcp-session-id", s);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn full_flow() {
        let app = app();
        let (st, _, body) = send(&app, Request::get("/healthz").body(Body::empty()).unwrap()).await;
        assert_eq!((st, body.as_str()), (StatusCode::OK, "ok\n"));

        let (st, _, _) = send(
            &app,
            post(json!({"jsonrpc":"2.0","id":1,"method":"ping"}), None, "bad"),
        )
        .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);

        let (st, h, _) = send(
            &app,
            post(
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
                None,
                "tok",
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let sid = h
            .get("mcp-session-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let (st, _, _) = send(
            &app,
            post(
                json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                Some(&sid),
                "tok",
            ),
        )
        .await;
        assert_eq!(st, StatusCode::ACCEPTED);

        let call =
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"sys_info"}});
        let (st, _, body) = send(&app, post(call.clone(), Some(&sid), "tok")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["result"]["content"][0]["text"],
            "pong"
        );

        let (st, _, _) = send(&app, post(call, Some("nope"), "tok")).await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        let (st, _, body) = send(&app, post(json!([1]), None, "tok")).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
            -32600
        );
    }
}
