//! Reverse-attach grants: registry, Connect `MacGrant` shape, session/URL validation and the §9.2 disposition table.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

pub mod client;

// Shared grant registry
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct GrantInfo {
    pub(crate) id: String,
    pub(crate) runtime: String,
    pub(crate) session: String,
    pub(crate) profile: String,
    pub(crate) principal: String,
    pub(crate) state: String,
    pub(crate) ended: Option<String>,
    pub(crate) expires_at_epoch_secs: u64,
    pub(crate) cancelled: Arc<AtomicBool>,
}

pub(crate) type Registry = Arc<Mutex<HashMap<String, GrantInfo>>>;

pub(crate) static GRANT_COUNTER: AtomicU64 = AtomicU64::new(1);

pub(crate) fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn new_grant_id() -> String {
    let n = GRANT_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("grant-{}-{}", now_epoch_secs(), n)
}

pub(crate) fn set_state(registry: &Registry, grant_id: &str, state: &str) {
    if let Ok(mut map) = registry.lock() {
        if let Some(g) = map.get_mut(grant_id) {
            g.state = state.to_string();
            g.ended = None;
        }
    }
}

pub(crate) fn set_ended(registry: &Registry, grant_id: &str, reason: &str) {
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
pub(crate) fn grant_json(g: &GrantInfo) -> Value {
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

pub(crate) fn valid_session(s: &str) -> bool {
    // ^[a-z0-9-]{1,32}$
    let len = s.len();
    if !(1..=32).contains(&len) {
        return false;
    }
    s.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub(crate) fn strip_trailing_slashes(s: &str) -> &str {
    s.trim_end_matches('/')
}

pub(crate) fn attach_url(runtime: &str, session: &str) -> String {
    format!(
        "{}/tools/attach/{}",
        strip_trailing_slashes(runtime),
        session
    )
}

// ---------------------------------------------------------------------------
// Disposition state machine
// ---------------------------------------------------------------------------

pub(crate) enum Disposition {
    Stop(String),
    Redial,
}

pub(crate) fn disposition_close(code: u16) -> Disposition {
    match code {
        4001 => Disposition::Stop("grant_expired".to_string()),
        4002 => Disposition::Stop("replaced".to_string()),
        4004 => Disposition::Stop("session_ended".to_string()),
        4010 => Disposition::Stop("revoked".to_string()),
        _ => Disposition::Redial,
    }
}

pub(crate) fn disposition_handshake(status: u16) -> Disposition {
    match status {
        101 => Disposition::Redial,
        200..=299 => Disposition::Redial,
        429 => Disposition::Redial,
        500..=599 => Disposition::Redial,
        _ => Disposition::Stop(format!("handshake_rejected_{status}")),
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod conformance {
    //! `conformance/reverse_attach_vectors.json` — Swift `ReverseAttachClient`
    //! (`disposition(closeCode:)`, `disposition(handshakeStatus:)`, `attachURL`)
    //! is the oracle; the Swift test target reads the same file.
    use super::*;
    use serde_json::Value;

    const VECTORS: &str = include_str!("../../../../conformance/reverse_attach_vectors.json");

    /// The oracle names terminals in Swift case style; this crate reports the
    /// snake_case `ended` reasons of the Connect `MacGrant` contract.
    fn stop_reason(swift: &str) -> &'static str {
        match swift {
            "grantExpired" => "grant_expired",
            "replaced" => "replaced",
            "sessionEnded" => "session_ended",
            "revoked" => "revoked",
            other => panic!("unknown terminal in vectors: {other}"),
        }
    }

    fn describe(d: &Disposition) -> String {
        match d {
            Disposition::Stop(r) => format!("stop({r})"),
            Disposition::Redial => "redial".into(),
        }
    }

    #[test]
    fn disposition_and_attach_url_match_the_swift_oracle_vectors() {
        let doc: Value = serde_json::from_str(VECTORS).unwrap();
        let mut failures = Vec::new();
        let mut n = 0;

        for c in doc["close_code"].as_array().unwrap() {
            n += 1;
            let code = c["code"].as_u64().unwrap() as u16;
            let got = describe(&disposition_close(code));
            let want = match c["expect"]["stop"].as_str() {
                Some(s) => format!("stop({})", stop_reason(s)),
                None => "redial".into(),
            };
            if got != want {
                failures.push(format!("close {code}: got {got}, want {want}"));
            }
        }
        for c in doc["handshake_status"].as_array().unwrap() {
            n += 1;
            let status = c["status"].as_u64().unwrap() as u16;
            let got = describe(&disposition_handshake(status));
            let want = match c["expect"]["stop_rejected"].as_u64() {
                Some(s) => format!("stop(handshake_rejected_{s})"),
                None => "redial".into(),
            };
            if got != want {
                failures.push(format!("handshake {status}: got {got}, want {want}"));
            }
        }
        for c in doc["attach_url"].as_array().unwrap() {
            n += 1;
            let got = attach_url(
                c["runtime"].as_str().unwrap(),
                c["session"].as_str().unwrap(),
            );
            let want = c["expect"].as_str().unwrap();
            if got != want {
                failures.push(format!("attach_url: got {got}, want {want}"));
            }
        }
        assert!(n >= 23, "vector file shrank: {n}");
        assert!(
            failures.is_empty(),
            "drift from Swift oracle:\n{}",
            failures.join("\n")
        );
    }
}
