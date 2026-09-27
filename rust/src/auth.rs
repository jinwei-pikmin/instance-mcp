//! Who may call. Port of `AuthPolicy.swift` — same decision table, same order.
//!
//! Evaluated per request from headers `tailscale serve` injects (`Tailscale-User-Login`)
//! plus an optional shared bearer token. Both checks that are configured must pass. With
//! neither configured the server refuses to start — see `validate()` — because a loopback
//! listener behind `tailscale serve` is reachable by every node on the tailnet.

use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Default)]
pub struct AuthPolicy {
    /// Lowercased logins. Empty ⇒ identity not checked.
    pub allowed_logins: HashSet<String>,
    /// Constant-time compared. None ⇒ token not checked.
    pub bearer_token: Option<String>,
    /// Requests from loopback *without* Tailscale headers are allowed only if set.
    pub allow_local_unauthenticated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow { principal: String },
    Deny { reason: String },
}

impl AuthPolicy {
    pub fn new(
        logins: impl IntoIterator<Item = String>,
        token: Option<String>,
        insecure_local: bool,
    ) -> Self {
        Self {
            allowed_logins: logins.into_iter().map(|l| l.to_lowercase()).collect(),
            bearer_token: token,
            allow_local_unauthenticated: insecure_local,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.allowed_logins.is_empty()
            && self.bearer_token.is_none()
            && !self.allow_local_unauthenticated
        {
            return Err(
                "refusing to start with no auth: set --allow-login and/or --token \
                        (or --insecure-local for loopback-only debugging)"
                    .into(),
            );
        }
        Ok(())
    }

    /// `headers` keys must be lowercased. `remote_is_loopback` is true when the TCP peer is
    /// 127.0.0.1/::1 — always the case behind `tailscale serve`, so it alone proves nothing.
    pub fn decide(&self, headers: &HashMap<String, String>, remote_is_loopback: bool) -> Decision {
        let deny = |r: &str| Decision::Deny {
            reason: r.to_string(),
        };
        let login = headers
            .get("tailscale-user-login")
            .map(|l| l.to_lowercase());
        let via_tailscale = login.is_some() || headers.contains_key("x-forwarded-for");
        let local_ok = remote_is_loopback && !via_tailscale && self.allow_local_unauthenticated;

        // Bearer check first: a wrong token is a deny even for an allowlisted login.
        if let Some(expected) = &self.bearer_token {
            let Some(auth) = headers.get("authorization") else {
                return deny("missing Authorization header");
            };
            // Bytes, not str slicing: a multi-byte char at the boundary must not panic.
            const PREFIX: &[u8] = b"bearer ";
            let auth = auth.as_bytes();
            if auth.len() < PREFIX.len() || !auth[..PREFIX.len()].eq_ignore_ascii_case(PREFIX) {
                return deny("Authorization must be Bearer");
            }
            if !constant_time_eq(&auth[PREFIX.len()..], expected.as_bytes()) {
                return deny("bad token");
            }
        }

        if !self.allowed_logins.is_empty() {
            let Some(login) = login else {
                if local_ok {
                    return Decision::Allow {
                        principal: "local".into(),
                    };
                }
                return deny("no Tailscale identity on request");
            };
            if !self.allowed_logins.contains(&login) {
                return Decision::Deny {
                    reason: format!("login {login} not allowed"),
                };
            }
            return Decision::Allow { principal: login };
        }

        if let Some(login) = login {
            return Decision::Allow { principal: login };
        }
        if self.bearer_token.is_some() {
            return Decision::Allow {
                principal: "token".into(),
            };
        }
        if local_ok {
            return Decision::Allow {
                principal: "local".into(),
            };
        }
        deny("unauthenticated")
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
    fn allow(p: &str) -> Decision {
        Decision::Allow {
            principal: p.into(),
        }
    }

    #[test]
    fn refuses_to_start_without_auth() {
        assert!(AuthPolicy::default().validate().is_err());
        assert!(AuthPolicy::new([], None, true).validate().is_ok());
    }

    #[test]
    fn login_and_token_are_and_combined() {
        let p = AuthPolicy::new(["Me@Example.com".into()], Some("s3cret".into()), false);
        let ok = h(&[
            ("tailscale-user-login", "me@example.com"),
            ("authorization", "Bearer s3cret"),
        ]);
        assert_eq!(p.decide(&ok, true), allow("me@example.com"));
        // Leaked tailnet key enrolled as my login, no token → deny.
        assert!(matches!(
            p.decide(&h(&[("tailscale-user-login", "me@example.com")]), true),
            Decision::Deny { .. }
        ));
        let wrong = h(&[
            ("tailscale-user-login", "me@example.com"),
            ("authorization", "Bearer nope"),
        ]);
        assert_eq!(
            p.decide(&wrong, true),
            Decision::Deny {
                reason: "bad token".into()
            }
        );
        let other = h(&[
            ("tailscale-user-login", "you@example.com"),
            ("authorization", "bearer s3cret"),
        ]);
        assert!(matches!(p.decide(&other, true), Decision::Deny { .. }));
    }

    #[test]
    fn insecure_local_only_without_tailscale_headers() {
        let p = AuthPolicy::new(["me@example.com".into()], None, true);
        assert_eq!(p.decide(&h(&[]), true), allow("local"));
        assert!(matches!(p.decide(&h(&[]), false), Decision::Deny { .. }));
        assert!(matches!(
            p.decide(&h(&[("x-forwarded-for", "100.1.2.3")]), true),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn token_only() {
        let p = AuthPolicy::new([], Some("t".into()), false);
        assert_eq!(
            p.decide(&h(&[("authorization", "Bearer t")]), false),
            allow("token")
        );
        assert_eq!(
            p.decide(
                &h(&[
                    ("authorization", "Bearer t"),
                    ("tailscale-user-login", "A@b.c")
                ]),
                false
            ),
            allow("a@b.c")
        );
        assert!(matches!(
            p.decide(&h(&[("authorization", "Basic t")]), false),
            Decision::Deny { .. }
        ));
        // Multi-byte char straddling the prefix boundary: deny, never panic.
        assert!(matches!(
            p.decide(&h(&[("authorization", "Bearer\u{e9}t")]), false),
            Decision::Deny { .. }
        ));
        assert!(matches!(
            p.decide(&h(&[("authorization", "B\u{e9}")]), false),
            Decision::Deny { .. }
        ));
    }
}
