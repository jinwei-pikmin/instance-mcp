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
pub mod store;

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
    /// Kept only to persist the grant; never part of `json()`.
    secret: String,
    client: ClientHandle,
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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
            "profile": self.profile.name(),
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
    /// None ⇒ grants live in memory only (tests, `--no-grant-persistence`).
    store: Option<store::Store>,
    /// Resolves custom profile names when resuming persisted grants.
    access: Option<Arc<crate::access::Access>>,
    me: std::sync::Weak<AttachManager>,
}

impl AttachManager {
    /// In memory only.
    #[cfg(test)]
    pub fn new(base: McpServer, mint: Option<MintFn>) -> Arc<Self> {
        Self::with_store(base, mint, None, None)
    }

    pub fn with_store(
        base: McpServer,
        mint: Option<MintFn>,
        store: Option<store::Store>,
        access: Option<Arc<crate::access::Access>>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            base,
            grants: Mutex::new(HashMap::new()),
            mint: mint.unwrap_or_else(mint_at_runtime),
            store,
            access,
            me: me.clone(),
        })
    }

    /// Write the live grants (not ended, not expired) to the store, if there is one.
    fn persist(&self) {
        let Some(store) = &self.store else { return };
        let now = SystemTime::now();
        let live: Vec<store::Persisted> = {
            let grants = self.grants.lock().unwrap();
            let mut v: Vec<&Grant> = grants
                .values()
                .filter(|g| g.expires_at > now && !matches!(g.client.state(), State::Ended(_)))
                .collect();
            v.sort_by_key(|g| g.created_at);
            v.into_iter()
                .map(|g| store::Persisted {
                    id: g.id.clone(),
                    runtime: g.runtime.to_string(),
                    session: g.session.clone(),
                    profile: g.profile.name().to_string(),
                    principal: g.principal.clone(),
                    created_at: unix(g.created_at),
                    expires_at: unix(g.expires_at),
                    secret: g.secret.clone(),
                })
                .collect()
        };
        store.save(&live);
    }

    /// Start the dial loop for a grant; when it finishes for good, re-persist so an ended
    /// grant is not resumed after a restart.
    fn start_client(&self, cfg: Config, scoped: McpServer) -> ClientHandle {
        let me = self.me.clone();
        client::start_with_hook(
            cfg,
            scoped,
            Some(Box::new(move || {
                if let Some(m) = me.upgrade() {
                    m.persist();
                }
            })),
        )
    }

    /// Re-dial every persisted grant still inside its deadline, under its original id.
    /// Call once at start. A runtime that has forgotten the grant (pod replaced) answers the
    /// handshake with 401 and the grant ends through the normal disposition. Returns how many
    /// were resumed.
    pub fn resume(&self) -> usize {
        let Some(store) = &self.store else { return 0 };
        let now = SystemTime::now();
        let mut resumed = 0;
        for p in store.load() {
            let expires_at = SystemTime::UNIX_EPOCH + Duration::from_secs(p.expires_at);
            let Some(left) = expires_at.duration_since(now).ok().filter(|d| !d.is_zero()) else {
                continue; // expired while we were down
            };
            let profile = match &self.access {
                Some(a) => a.profile(&p.profile),
                None => ToolProfile::built_in(&p.profile),
            };
            let (Some(profile), Ok(runtime)) = (profile, p.runtime.parse::<Uri>()) else {
                log(&format!(
                    "grant {} not resumed: profile '{}' is not defined (or bad runtime)",
                    &p.id[..8.min(p.id.len())],
                    p.profile
                ));
                continue;
            };
            if self.grants.lock().unwrap().contains_key(&p.id) {
                continue;
            }
            let instructions = sandbox_instructions(&profile, self.base.instructions.as_deref());
            let scoped = self.base.scoped(profile.clone(), instructions);
            let cfg = Config::new(
                runtime.clone(),
                p.session.clone(),
                p.secret.clone(),
                profile.clone(),
                Instant::now() + left,
            );
            log(&format!(
                "grant {} resumed: {} → {}/{} for {}s more",
                &p.id[..8.min(p.id.len())],
                profile.name(),
                runtime.host().unwrap_or("?"),
                p.session,
                left.as_secs()
            ));
            let grant = Grant {
                id: p.id.clone(),
                runtime,
                session: p.session,
                profile,
                principal: p.principal,
                created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(p.created_at),
                expires_at,
                secret: p.secret,
                client: self.start_client(cfg, scoped),
            };
            self.grants.lock().unwrap().insert(p.id, grant);
            resumed += 1;
        }
        // Rewrite without the expired / unresumable ones.
        self.persist();
        resumed
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
        let instructions = sandbox_instructions(&req.profile, self.base.instructions.as_deref());
        let scoped = self.base.scoped(req.profile.clone(), instructions);
        let cfg = Config::new(
            runtime.clone(),
            req.session.clone(),
            secret.clone(),
            req.profile.clone(),
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
            req.profile.name(),
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
            secret,
            client: self.start_client(cfg, scoped),
        };
        let out = grant.json();
        grants.insert(id, grant);
        drop(grants);
        self.persist();
        Ok(out)
    }

    /// Returns false if there was no such grant.
    pub fn revoke(&self, id: &str) -> bool {
        let removed = Self::revoke_locked(&mut self.grants.lock().unwrap(), id, "revoked");
        if removed {
            self.persist();
        }
        removed
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
        let before = grants.len();
        for id in expired {
            Self::revoke_locked(&mut grants, &id, "expired");
        }
        grants.retain(|_, g| !matches!(g.client.state(), State::Ended(_)));
        let changed = grants.len() != before;
        drop(grants);
        if changed {
            self.persist();
        }
    }
}

fn sandbox_instructions(profile: &ToolProfile, base: Option<&str>) -> Option<String> {
    if profile.is_owner() {
        return base.map(String::from);
    }
    let head = base.map(|b| format!("{b}\n\n")).unwrap_or_default();
    Some(format!(
        "{head}You reached this machine through OpenAB Connect: a human lent it to your sandbox \
         session for a limited time and may be watching. This is the `{name}` profile — only the \
         tools that `tools/list` shows exist here (there is no `exec` unless it is listed; you \
         already have a shell in your own session). When `browser_*` tools are listed, drive the \
         browser directly: `browser_navigate` then `browser_snapshot` gives you the page as text. \
         If a tool starts failing with \"not attached\", the grant ended; ask the human to lend \
         the machine again.",
        name = profile.name()
    ))
}

/// Instructions for a direct (non-attach) caller on a narrowed profile, e.g. a named token.
/// None for owner: the base instructions stand.
pub fn restricted_instructions(profile: &ToolProfile, base: Option<&str>) -> Option<String> {
    if profile.is_owner() {
        return None;
    }
    Some(format!(
        "This connection uses the restricted profile `{name}`: only the tools in `tools/list` exist \
         for you. Ignore any guidance below about tools that are not listed.\n\n{base}",
        name = profile.name(),
        base = base.unwrap_or("")
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
            let headers = [
                ("authorization", format!("Bearer {credential}")),
                ("content-type", "application/json".to_string()),
            ];
            let fut = async {
                crate::net::post(&url, &headers, body)
                    .await
                    .map(|r| (r.status, r.body))
            };
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

#[cfg(test)]
mod tests;
