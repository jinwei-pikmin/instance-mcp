//! AuthPolicy — same decision table as the Swift daemon: every configured check must pass.

use crate::http::HttpRequest;

// AuthPolicy — same shape as the Swift daemon: every configured check must pass.
//   MCP_TOKEN / MCP_TOKEN_FILE   require `Authorization: Bearer` (constant-time compare)
//   MCP_ALLOW_LOGIN              comma list matched against `Tailscale-User-Login`
//                                (injected+overwritten by `tailscale serve`)
//   MCP_INSECURE_LOCAL=1         allow bare loopback requests with nothing else set
// ---------------------------------------------------------------------------

pub(crate) struct AuthPolicy {
    token: Option<String>,
    allow_logins: Vec<String>,
    insecure_local: bool,
}

impl AuthPolicy {
    pub(crate) fn from_env() -> Result<AuthPolicy, String> {
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
        Self::new(token, allow_logins, insecure_local)
    }

    /// Refuses a policy with nothing configured, like Swift `validate()`: a
    /// loopback listener behind `tailscale serve` is reachable by the tailnet.
    pub(crate) fn new(
        token: Option<String>,
        allow_logins: Vec<String>,
        insecure_local: bool,
    ) -> Result<AuthPolicy, String> {
        let allow_logins: Vec<String> = allow_logins.iter().map(|l| l.to_lowercase()).collect();
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

    pub(crate) fn describe(&self) -> String {
        format!(
            "token={} allow_login={:?} insecure_local={}",
            self.token.is_some(),
            self.allow_logins,
            self.insecure_local
        )
    }

    /// Adapter from a parsed request to [`AuthPolicy::decide`].
    pub(crate) fn check(&self, req: &HttpRequest) -> Result<String, String> {
        match self.decide(
            req.authorization.as_deref(),
            req.ts_login.as_deref(),
            req.forwarded_for.is_some(),
            req.peer_is_loopback,
        ) {
            Decision::Allow(principal) => Ok(principal),
            Decision::Deny(reason) => Err(reason),
        }
    }

    /// The decision table, as a pure function. Same algorithm, order and reason
    /// strings as Swift `AuthPolicy.decide(headers:remoteIsLoopback:)`, pinned by
    /// `conformance/auth_vectors.json` (Swift is the oracle).
    ///
    /// - Bearer first: a wrong token is a deny even for an allow-listed login.
    /// - The `Bearer` scheme is case-insensitive; the token is not.
    /// - A request relayed by `tailscale serve` (it carries `Tailscale-User-Login`
    ///   or `X-Forwarded-For`) is never "local", whatever its TCP peer is.
    pub(crate) fn decide(
        &self,
        authorization: Option<&str>,
        ts_login: Option<&str>,
        forwarded: bool,
        remote_is_loopback: bool,
    ) -> Decision {
        let login = ts_login.map(str::to_lowercase);
        let via_tailscale = login.is_some() || forwarded;
        let local_ok = remote_is_loopback && !via_tailscale && self.insecure_local;

        if let Some(expected) = &self.token {
            let Some(auth) = authorization else {
                return Decision::Deny("missing Authorization header".into());
            };
            const PREFIX: &str = "bearer ";
            let scheme_ok = auth
                .get(..PREFIX.len())
                .is_some_and(|p| p.eq_ignore_ascii_case(PREFIX));
            if !scheme_ok {
                return Decision::Deny("Authorization must be Bearer".into());
            }
            if !constant_time_eq(&auth.as_bytes()[PREFIX.len()..], expected.as_bytes()) {
                return Decision::Deny("bad token".into());
            }
        }

        if !self.allow_logins.is_empty() {
            return match login {
                None if local_ok => Decision::Allow("local".into()),
                None => Decision::Deny("no Tailscale identity on request".into()),
                Some(l) if self.allow_logins.contains(&l) => Decision::Allow(l),
                Some(l) => Decision::Deny(format!("login {l} not allowed")),
            };
        }

        if let Some(l) = login {
            return Decision::Allow(l);
        }
        if self.token.is_some() {
            return Decision::Allow("token".into());
        }
        if local_ok {
            return Decision::Allow("local".into());
        }
        Decision::Deny("unauthenticated".into())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Allow(String),
    Deny(String),
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

#[cfg(test)]
mod conformance {
    //! `conformance/auth_vectors.json` — shared with the Swift test target; Swift
    //! is the oracle. A case that fails here is a behaviour drift, not a test to
    //! edit.
    use super::*;
    use serde_json::Value;

    const VECTORS: &str = include_str!("../../../conformance/auth_vectors.json");

    fn policy(config: &Value) -> Result<AuthPolicy, String> {
        let logins = config["allowedLogins"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let token = config["bearerToken"].as_str().map(String::from);
        let local = config["allowLocalUnauthenticated"]
            .as_bool()
            .unwrap_or(false);
        AuthPolicy::new(token, logins, local)
    }

    #[test]
    fn auth_policy_matches_the_swift_oracle_vectors() {
        let doc: Value = serde_json::from_str(VECTORS).unwrap();
        let cases = doc["cases"].as_array().unwrap();
        assert!(cases.len() >= 16, "vector file shrank: {}", cases.len());
        let mut failures = Vec::new();
        for case in cases {
            let name = case["name"].as_str().unwrap();
            if case["validate"].as_str() == Some("error") {
                if policy(&case["config"]).is_ok() {
                    failures.push(format!("{name}: validate should have failed"));
                }
                continue;
            }
            let policy = policy(&case["config"]).unwrap_or_else(|e| panic!("{name}: {e}"));
            let headers: std::collections::HashMap<String, String> = case["headers"]
                .as_object()
                .map(|h| {
                    h.iter()
                        .map(|(k, v)| (k.to_lowercase(), v.as_str().unwrap_or("").to_owned()))
                        .collect()
                })
                .unwrap_or_default();
            let got = policy.decide(
                headers.get("authorization").map(String::as_str),
                headers.get("tailscale-user-login").map(String::as_str),
                headers.contains_key("x-forwarded-for"),
                case["remoteIsLoopback"].as_bool().unwrap_or(false),
            );
            let want = match (
                case["expect"]["allow"].as_str(),
                case["expect"]["deny"].as_str(),
            ) {
                (Some(p), _) => Decision::Allow(p.into()),
                (_, Some(r)) => Decision::Deny(r.into()),
                _ => panic!("{name}: vector has no expect"),
            };
            if got != want {
                failures.push(format!("{name}: got {got:?}, want {want:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "drift from Swift oracle:\n{}",
            failures.join("\n")
        );
    }
}
