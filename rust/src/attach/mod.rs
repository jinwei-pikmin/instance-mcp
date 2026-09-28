//! Reverse attach grants: "lend this machine to that session, with that profile, for that
//! long." Port of `AttachManager.swift`; design: `docs/adr/reverse-attach.md`.
//!
//! A grant is created by a human through `POST /attach` (OpenAB Connect / Remote, or curl)
//! and lives **here**. The granting device need not stay online; this machine dials,
//! redials and gives up on its own. One grant per (runtime, session): a new one for the
//! same target replaces the old, which is also how renewal works. The profile is fixed for
//! a grant's life. Neither the attach secret nor an admin credential is ever stored in the
//! grant record or logged.

pub mod client;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};
use hyper::http::Uri;
use serde_json::{json, Value};

use crate::log;
use crate::mcp::{McpServer, ToolProfile};
use client::{ClientHandle, Config, State};

/// Matches the Swift build (0.6.4): Connect / Remote offer 1, 2, 4, 12 and 24 h leases.
pub const MAX_TTL_SECS: i64 = 24 * 3600;
pub const DEFAULT_TTL_SECS: i64 = 3600;

#[derive(Debug, Clone, PartialEq)]
pub enum AttachError {
    BadRequest(String),
    /// The runtime refused (or could not be reached) when minting. `status` 0 = transport.
    MintFailed {
        status: u16,
        body: String,
    },
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttachError::BadRequest(s) => f.write_str(s),
            AttachError::MintFailed { status, body } => {
                write!(f, "runtime refused to mint (HTTP {status}): {body}")
            }
        }
    }
}

pub struct AttachRequest {
    pub runtime: String,
    pub session: String,
    pub profile: ToolProfile,
    pub ttl_secs: i64,
    /// Exactly one of the two: the caller already minted at the runtime and hands us the
    /// secret, or hands us the runtime's admin credential so we mint (used for that one
    /// call, never stored).
    pub secret: Option<String>,
    pub admin_credential: Option<String>,
}

pub struct Minted {
    pub secret: String,
    pub expires_in: Duration,
}

pub type MintFuture = Pin<Box<dyn Future<Output = Result<Minted, AttachError>> + Send>>;
/// (runtime, session, admin credential, requested ttl) → secret. A seam for tests.
pub type MintFn = Arc<dyn Fn(Uri, String, String, Duration) -> MintFuture + Send + Sync>;

struct Grant {
    id: String,
    runtime: Uri,
    session: String,
    profile: ToolProfile,
    principal: String,
    created_at: SystemTime,
    expires_at: SystemTime,
    client: ClientHandle,
}

fn iso(t: SystemTime) -> String {
    DateTime::<Utc>::from(t).to_rfc3339_opts(SecondsFormat::Secs, true)
}

impl Grant {
    fn json(&self) -> Value {
        let left = self
            .expires_at
            .duration_since(SystemTime::now())
            .unwrap_or_default();
        let mut o = json!({
            "id": self.id,
            "runtime": self.runtime.to_string(),
            "session": self.session,
            "profile": self.profile.as_str(),
            "principal": self.principal,
            "created_at": iso(self.created_at),
            "expires_at": iso(self.expires_at),
            "expires_in_secs": left.as_secs_f64().round() as u64,
        });
        match self.client.state() {
            State::Idle => o["state"] = json!("idle"),
            State::Dialing => o["state"] = json!("dialing"),
            State::Attached => o["state"] = json!("attached"),
            State::WaitingToRedial { seconds } => {
                o["state"] = json!("redialing");
                o["redial_in_secs"] = json!(seconds.round() as u64);
            }
            State::Ended(t) => {
                o["state"] = json!("ended");
                o["ended"] = json!(t.label());
            }
        }
        o
    }
}

pub struct AttachManager {
    base: McpServer,
    grants: Mutex<HashMap<String, Grant>>,
    mint: MintFn,
}

impl AttachManager {
    pub fn new(base: McpServer, mint: Option<MintFn>) -> Arc<Self> {
        Arc::new(Self {
            base,
            grants: Mutex::new(HashMap::new()),
            mint: mint.unwrap_or_else(mint_at_runtime),
        })
    }

    pub fn list(&self) -> Vec<Value> {
        let grants = self.grants.lock().unwrap();
        let mut v: Vec<&Grant> = grants.values().collect();
        v.sort_by_key(|g| g.created_at);
        v.into_iter().map(Grant::json).collect()
    }

    pub fn get(&self, id: &str) -> Option<Value> {
        self.grants.lock().unwrap().get(id).map(Grant::json)
    }

    pub async fn create(&self, req: AttachRequest, principal: &str) -> Result<Value, AttachError> {
        let bad = |s: &str| AttachError::BadRequest(s.into());
        let runtime: Uri = req
            .runtime
            .parse()
            .map_err(|_| bad("runtime must be a ws:// or wss:// URL"))?;
        if !matches!(runtime.scheme_str(), Some("ws" | "wss")) || runtime.host().is_none() {
            return Err(bad("runtime must be a ws:// or wss:// URL"));
        }
        let s = &req.session;
        if s.is_empty()
            || s.len() > 32
            || !s
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(bad("session must match [a-z0-9-]{1,32}"));
        }
        if !(1..=MAX_TTL_SECS).contains(&req.ttl_secs) {
            return Err(AttachError::BadRequest(format!(
                "ttl_secs must be 1...{MAX_TTL_SECS}"
            )));
        }
        let ttl = Duration::from_secs(req.ttl_secs as u64);
        let (secret, ttl) = match (req.secret, req.admin_credential) {
            (Some(s), None) if !s.is_empty() => (s, ttl),
            (None, Some(cred)) if !cred.is_empty() => {
                let m = (self.mint)(runtime.clone(), req.session.clone(), cred, ttl).await?;
                // The runtime's TTL wins; ours is a request, theirs is the grant.
                (m.secret, ttl.min(m.expires_in))
            }
            _ => return Err(bad("provide exactly one of secret or admin_credential")),
        };

        let id = uuid::Uuid::new_v4().to_string();
        let now = SystemTime::now();
        let instructions = sandbox_instructions(req.profile, self.base.instructions.as_deref());
        let scoped = self.base.scoped(req.profile, instructions);
        let cfg = Config::new(
            runtime.clone(),
            req.session.clone(),
            secret,
            req.profile,
            Instant::now() + ttl,
        );

        let mut grants = self.grants.lock().unwrap();
        // Replace any grant for the same target.
        let old: Vec<String> = grants
            .values()
            .filter(|g| g.runtime == runtime && g.session == req.session)
            .map(|g| g.id.clone())
            .collect();
        for o in old {
            Self::revoke_locked(&mut grants, &o, "replaced by a new grant");
        }
        log(&format!(
            "grant {} by {principal}: {} → {}/{} for {}s",
            &id[..8],
            req.profile.as_str(),
            runtime.host().unwrap_or("?"),
            req.session,
            ttl.as_secs()
        ));
        let grant = Grant {
            id: id.clone(),
            runtime,
            session: req.session,
            profile: req.profile,
            principal: principal.into(),
            created_at: now,
            expires_at: now + ttl,
            client: client::start(cfg, scoped),
        };
        let out = grant.json();
        grants.insert(id, grant);
        Ok(out)
    }

    /// Returns false if there was no such grant.
    pub fn revoke(&self, id: &str) -> bool {
        Self::revoke_locked(&mut self.grants.lock().unwrap(), id, "revoked")
    }

    fn revoke_locked(grants: &mut HashMap<String, Grant>, id: &str, reason: &str) -> bool {
        let Some(g) = grants.remove(id) else {
            return false;
        };
        g.client.cancel();
        log(&format!(
            "grant {} {reason} ({})",
            &id[..8.min(id.len())],
            g.session
        ));
        true
    }

    /// Drop grants whose client ended or whose deadline passed. Called from the HTTP layer
    /// opportunistically; nothing depends on it.
    pub fn sweep(&self) {
        let mut grants = self.grants.lock().unwrap();
        let now = SystemTime::now();
        let expired: Vec<String> = grants
            .values()
            .filter(|g| g.expires_at < now)
            .map(|g| g.id.clone())
            .collect();
        for id in expired {
            Self::revoke_locked(&mut grants, &id, "expired");
        }
        grants.retain(|_, g| !matches!(g.client.state(), State::Ended(_)));
    }
}

fn sandbox_instructions(profile: ToolProfile, base: Option<&str>) -> Option<String> {
    if profile != ToolProfile::Sandbox {
        return base.map(String::from);
    }
    let head = base.map(|b| format!("{b}\n\n")).unwrap_or_default();
    Some(format!(
        "{head}You reached this machine through OpenAB Connect: a human lent it to your sandbox \
         session for a limited time and may be watching. This is the `sandbox` profile — there is \
         no `exec` tool here (you already have a shell in your own session); use the tools that \
         `tools/list` shows, and — when `browser_*` tools are listed — drive the browser directly: \
         `browser_navigate` then `browser_snapshot` gives you the page as text. If a tool starts \
         failing with \"not attached\", the grant ended; ask the human to lend the machine again."
    ))
}

// MARK: - mint at the runtime's admin plane

/// `POST {runtime as http(s)}/admin/sessions/{session}/tools-attach` with the admin
/// credential and body `{"ttl_secs": <requested>}`; expects `201 {"secret": …,
/// "expires_in_secs": …}`. Without the body openab-pty mints its default hour and every
/// longer lease silently shrinks to 1 h (fixed upstream in 0.6.4). The credential is used
/// for this one request and dropped.
fn mint_at_runtime() -> MintFn {
    Arc::new(|runtime, session, credential, ttl| {
        Box::pin(async move {
            let transport = |e: String| AttachError::MintFailed { status: 0, body: e };
            let https = runtime.scheme_str() == Some("wss");
            let url = client::join_runtime_path(
                &runtime,
                Some(if https { "https" } else { "http" }),
                &format!("/admin/sessions/{session}/tools-attach"),
            );
            let body = serde_json::json!({ "ttl_secs": ttl.as_secs() }).to_string();
            let fut = http_post(&url, &credential, https, body);
            let (status, body) = tokio::time::timeout(Duration::from_secs(15), fut)
                .await
                .map_err(|_| transport("timed out".into()))?
                .map_err(transport)?;
            let parsed: Option<Value> = serde_json::from_slice(&body).ok();
            let secret = parsed
                .as_ref()
                .and_then(|v| v.get("secret"))
                .and_then(Value::as_str);
            match (status, secret) {
                (201, Some(s)) => {
                    let secs = parsed
                        .as_ref()
                        .and_then(|v| v.get("expires_in_secs"))
                        .and_then(Value::as_u64)
                        .unwrap_or(3600);
                    Ok(Minted {
                        secret: s.to_string(),
                        expires_in: Duration::from_secs(secs),
                    })
                }
                _ => Err(AttachError::MintFailed {
                    status,
                    body: String::from_utf8_lossy(&body[..body.len().min(200)]).into_owned(),
                }),
            }
        })
    })
}

/// Minimal one-shot HTTP/1.1 POST of a JSON body over plain TCP or rustls.
async fn http_post(
    url: &str,
    bearer: &str,
    https: bool,
    json: String,
) -> Result<(u16, Vec<u8>), String> {
    use http_body_util::{BodyExt, Full};
    use hyper::body::Bytes;
    use hyper_util::rt::TokioIo;

    let uri: Uri = url.parse().map_err(|e| format!("bad url: {e}"))?;
    let host = uri.host().ok_or("no host")?.to_string();
    let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
    let req = hyper::Request::post(uri.path_and_query().map(|p| p.as_str()).unwrap_or("/"))
        .header("host", uri.authority().map(|a| a.as_str()).unwrap_or(&host))
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .header("content-length", json.len().to_string())
        .body(Full::new(Bytes::from(json)))
        .map_err(|e| e.to_string())?;

    let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| e.to_string())?;

    async fn send<S>(io: S, req: hyper::Request<Full<Bytes>>) -> Result<(u16, Vec<u8>), String>
    where
        S: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    {
        let (mut tx, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| e.to_string())?;
        tokio::spawn(conn);
        let resp = tx.send_request(req).await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .to_bytes();
        Ok((status, body.to_vec()))
    }

    if https {
        use tokio_rustls::rustls;
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let name =
            rustls::pki_types::ServerName::try_from(host.clone()).map_err(|e| e.to_string())?;
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .map_err(|e| e.to_string())?;
        send(TokioIo::new(tls), req).await
    } else {
        send(TokioIo::new(tcp), req).await
    }
}

#[cfg(test)]
mod tests;
