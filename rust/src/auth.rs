//! Who may call. Port of `AuthPolicy.swift` — same decision table, same order.
//!
//! Evaluated per request from headers `tailscale serve` injects (`Tailscale-User-Login`)
//! plus an optional shared bearer token. Both checks that are configured must pass. With
//! neither configured the server refuses to start — see `validate()` — because a loopback
//! listener behind `tailscale serve` is reachable by every node on the tailnet.
//!
//! Beyond the Swift table: when a bearer token is configured, a presented token that is not
//! it may instead be one of the operator's **named tokens** (`access`), which carries its
//! own tool profile. The main token is `owner`. The login check still applies to both, so a
//! named token never replaces the Tailscale identity, it only narrows what the caller gets.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::access::{Access, TokenMatch};
use crate::mcp::ToolProfile;

#[derive(Debug, Clone, Default)]
pub struct AuthPolicy {
    /// Lowercased logins. Empty ⇒ identity not checked.
    pub allowed_logins: HashSet<String>,
    /// Constant-time compared. None ⇒ token not checked.
    pub bearer_token: Option<String>,
    /// Requests from loopback *without* Tailscale headers are allowed only if set.
    pub allow_local_unauthenticated: bool,
    /// Named tokens with their own profiles. Consulted only when `bearer_token` is set.
    pub access: Option<Arc<Access>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// `principal` names the caller for logs — the login, plus `[token]` for a named token.
    Allow {
        principal: String,
        profile: ToolProfile,
    },
    Deny {
        reason: String,
    },
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
            access: None,
        }
    }

    pub fn with_access(mut self, access: Arc<Access>) -> Self {
        self.access = Some(access);
        self
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
        let mut named_token: Option<(String, ToolProfile)> = None;

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
            let presented = &auth[PREFIX.len()..];
            if !constant_time_eq(presented, expected.as_bytes()) {
                let named = std::str::from_utf8(presented)
                    .ok()
                    .and_then(|t| self.access.as_ref()?.match_token(t));
                match named {
                    Some(TokenMatch::Profile { token, profile: p }) => {
                        named_token = Some((token, p))
                    }
                    Some(TokenMatch::UndefinedProfile { token, profile }) => {
                        return Decision::Deny {
                            reason: format!("token '{token}' names undefined profile '{profile}'"),
                        };
                    }
                    None => return deny("bad token"),
                }
            }
        }
        // Every allow below carries the profile: owner, or the named token's.
        let allow = |principal: String| match &named_token {
            Some((token, profile)) => Decision::Allow {
                principal: format!("{principal} [{token}]"),
                profile: profile.clone(),
            },
            None => Decision::Allow {
                principal,
                profile: ToolProfile::Owner,
            },
        };

        if !self.allowed_logins.is_empty() {
            let Some(login) = login else {
                if local_ok {
                    return allow("local".into());
                }
                return deny("no Tailscale identity on request");
            };
            if !self.allowed_logins.contains(&login) {
                return Decision::Deny {
                    reason: format!("login {login} not allowed"),
                };
            }
            return allow(login);
        }

        if let Some(login) = login {
            return allow(login);
        }
        if self.bearer_token.is_some() {
            return allow("token".into());
        }
        if local_ok {
            return allow("local".into());
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
            profile: ToolProfile::Owner,
        }
    }

    #[test]
    fn named_tokens_carry_their_profile_and_still_need_the_login() {
        let dir = tempfile::tempdir().unwrap();
        let entry = |name: &str, token: &str, profile: &str| {
            format!(
                "[tokens.{name}]\nprofile = \"{profile}\"\nsha256 = \"{}\"\ncreated = \"t\"\n",
                crate::access::sha256_hex(token)
            )
        };
        std::fs::write(
            dir.path().join("tokens.toml"),
            entry("hermes", "oabt_h", "sandbox") + &entry("ghost", "oabt_g", "nope"),
        )
        .unwrap();
        let p = AuthPolicy::new(["me@x.io".into()], Some("main".into()), false)
            .with_access(Access::new(dir.path()));
        let req = |tok: &str, login: &str| {
            h(&[
                ("authorization", &format!("Bearer {tok}")),
                ("tailscale-user-login", login),
            ])
        };
        assert_eq!(p.decide(&req("main", "me@x.io"), true), allow("me@x.io"));
        assert_eq!(
            p.decide(&req("oabt_h", "me@x.io"), true),
            Decision::Allow {
                principal: "me@x.io [hermes]".into(),
                profile: ToolProfile::Sandbox
            }
        );
        // The login check still applies to a named token.
        assert!(matches!(
            p.decide(&req("oabt_h", "you@x.io"), true),
            Decision::Deny { .. }
        ));
        // A token naming a profile that does not exist is denied, never widened.
        assert_eq!(
            p.decide(&req("oabt_g", "me@x.io"), true),
            Decision::Deny {
                reason: "token 'ghost' names undefined profile 'nope'".into()
            }
        );
        assert_eq!(
            p.decide(&req("oabt_zzz", "me@x.io"), true),
            Decision::Deny {
                reason: "bad token".into()
            }
        );
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

/// The shared Swift/Rust vectors (`conformance/auth_vectors.json`, Swift is the oracle).
#[cfg(test)]
mod conformance {
    use super::*;
    use serde_json::Value;

    #[test]
    fn auth_vectors() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../conformance/auth_vectors.json"
        );
        let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let cases = v["cases"].as_array().unwrap();
        assert!(!cases.is_empty());
        for c in cases {
            let name = c["name"].as_str().unwrap();
            let cfg = &c["config"];
            let logins = cfg["allowedLogins"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|l| l.as_str().unwrap().to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let p = AuthPolicy::new(
                logins,
                cfg["bearerToken"].as_str().map(String::from),
                cfg["allowLocalUnauthenticated"].as_bool().unwrap_or(false),
            );
            if let Some(want) = c["validate"].as_str() {
                assert_eq!(p.validate().is_err(), want == "error", "{name}");
                continue;
            }
            let headers: HashMap<String, String> = c["headers"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .map(|(k, v)| (k.to_lowercase(), v.as_str().unwrap().to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let got = p.decide(&headers, c["remoteIsLoopback"].as_bool().unwrap());
            let want = match (c["expect"]["allow"].as_str(), c["expect"]["deny"].as_str()) {
                (Some(principal), None) => Decision::Allow {
                    principal: principal.into(),
                    profile: ToolProfile::Owner,
                },
                (None, Some(reason)) => Decision::Deny {
                    reason: reason.into(),
                },
                _ => panic!("{name}: bad expect"),
            };
            assert_eq!(got, want, "vector: {name}");
        }
    }
}
