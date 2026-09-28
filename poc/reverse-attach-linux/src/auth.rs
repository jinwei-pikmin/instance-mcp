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

    /// The authenticated principal matches the Swift AuthPolicy contract:
    /// allowlisted Tailscale login, `token`, or debug-only `local`.
    pub(crate) fn check(&self, req: &HttpRequest) -> Result<String, String> {
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
