//! Operator-defined access: named bearer tokens bound to tool profiles, and custom profiles.
//!
//! Two files in the config dir (`~/.config/oab-instance-mcp/`), both owned by the machine's
//! operator — a client never chooses its own profile:
//!
//! ```toml
//! # profiles.toml — custom profiles; owner and sandbox are built in and cannot be redefined
//! [profiles.browser]
//! allow = ["sys_info", "browser_*"]
//! deny  = ["browser_run_code_unsafe", "browser_file_upload"]
//!
//! # tokens.toml — written by `oab-instance-mcp token add`; stores only SHA-256 hashes
//! [tokens.hermes]
//! profile = "browser"
//! sha256  = "…"
//! created = "2026-09-30T00:00:00Z"
//! ```
//!
//! Both are re-read when their mtime changes, so adding or revoking a token takes effect on
//! the next request without a restart (a restart would drop reverse-attach grants). A file
//! that fails to parse is rejected with a log line and the last good version stays in force,
//! so a typo can never widen access. A token whose profile is not defined is denied.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::log;
use crate::mcp::{CustomProfile, ToolProfile};

/// Tools that hand out a shell, arbitrary code or files: allowing them in a custom profile is
/// legal but logged, so it is always a conscious choice.
const RISKY_TOOLS: &[&str] = &[
    "exec",
    "exec_start",
    "browser_run_code_unsafe",
    "browser_file_upload",
];

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfilesFile {
    #[serde(default)]
    profiles: BTreeMap<String, ProfileEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileEntry {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokensFile {
    #[serde(default)]
    pub tokens: BTreeMap<String, TokenEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenEntry {
    pub profile: String,
    pub sha256: String,
    pub created: String,
}

/// Names for tokens and profiles: `[a-z0-9_-]{1,32}`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

pub fn sha256_hex(s: &str) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, s.as_bytes());
    d.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn parse_profiles(text: &str) -> Result<BTreeMap<String, Arc<CustomProfile>>, String> {
    let file: ProfilesFile = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut out = BTreeMap::new();
    for (name, e) in file.profiles {
        if ToolProfile::BUILT_IN.contains(&name.as_str()) {
            return Err(format!(
                "profile '{name}' is built in and cannot be redefined"
            ));
        }
        if !valid_name(&name) {
            return Err(format!(
                "profile name '{name}' must match [a-z0-9_-]{{1,32}}"
            ));
        }
        if e.allow.is_empty() {
            return Err(format!("profile '{name}' allows nothing (empty `allow`)"));
        }
        if let Some(p) = e.allow.iter().chain(&e.deny).find(|p| p.is_empty()) {
            return Err(format!("profile '{name}' has an empty pattern {p:?}"));
        }
        out.insert(
            name.clone(),
            Arc::new(CustomProfile {
                name,
                allow: e.allow,
                deny: e.deny,
            }),
        );
    }
    Ok(out)
}

fn parse_tokens(text: &str) -> Result<TokensFile, String> {
    let file: TokensFile = toml::from_str(text).map_err(|e| e.to_string())?;
    for (name, t) in &file.tokens {
        if !valid_name(name) {
            return Err(format!("token name '{name}' must match [a-z0-9_-]{{1,32}}"));
        }
        if t.sha256.len() != 64 || !t.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("token '{name}': sha256 must be 64 hex characters"));
        }
    }
    Ok(file)
}

/// One watched file: parsed value plus the mtime it came from. A missing file is an empty
/// value; a broken one keeps the previous value.
struct Watched<T> {
    path: PathBuf,
    seen: Option<Option<SystemTime>>,
    value: T,
}

impl<T: Default> Watched<T> {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            seen: None,
            value: T::default(),
        }
    }

    fn refresh(&mut self, parse: impl Fn(&str) -> Result<T, String>) {
        let mtime = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if self.seen == Some(mtime) {
            return;
        }
        self.seen = Some(mtime);
        let Some(_) = mtime else {
            self.value = T::default();
            return;
        };
        match std::fs::read_to_string(&self.path)
            .map_err(|e| e.to_string())
            .and_then(|t| parse(&t))
        {
            Ok(v) => {
                self.value = v;
                log(&format!("access: loaded {}", self.path.display()));
            }
            Err(e) => log(&format!(
                "access: REJECTED {} ({e}); keeping the previous version in force",
                self.path.display()
            )),
        }
    }
}

pub struct Access {
    profiles: Mutex<Watched<BTreeMap<String, Arc<CustomProfile>>>>,
    tokens: Mutex<Watched<TokensFile>>,
}

impl std::fmt::Debug for Access {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print token hashes.
        f.write_str("Access { .. }")
    }
}

/// What a presented named token resolves to.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenMatch {
    Profile {
        token: String,
        profile: ToolProfile,
    },
    /// The token is valid but names a profile that is not defined: deny, never guess.
    UndefinedProfile {
        token: String,
        profile: String,
    },
}

impl Access {
    pub fn new(dir: &Path) -> Arc<Self> {
        Arc::new(Self {
            profiles: Mutex::new(Watched::new(dir.join("profiles.toml"))),
            tokens: Mutex::new(Watched::new(dir.join("tokens.toml"))),
        })
    }

    /// Built-in or custom profile by name (current version of profiles.toml).
    pub fn profile(&self, name: &str) -> Option<ToolProfile> {
        if let Some(p) = ToolProfile::built_in(name) {
            return Some(p);
        }
        let mut w = self.profiles.lock().unwrap();
        let first = w.seen.is_none();
        w.refresh(parse_profiles);
        if first {
            warn_risky(&w.value);
        }
        w.value.get(name).cloned().map(ToolProfile::Custom)
    }

    pub fn profile_names(&self) -> Vec<String> {
        let mut w = self.profiles.lock().unwrap();
        w.refresh(parse_profiles);
        ToolProfile::BUILT_IN
            .iter()
            .map(|s| s.to_string())
            .chain(w.value.keys().cloned())
            .collect()
    }

    /// Match a presented bearer against the named tokens (constant-time on the hash).
    pub fn match_token(&self, presented: &str) -> Option<TokenMatch> {
        let hash = sha256_hex(presented);
        let entry = {
            let mut w = self.tokens.lock().unwrap();
            w.refresh(parse_tokens);
            w.value
                .tokens
                .iter()
                .find(|(_, t)| constant_time_eq(t.sha256.as_bytes(), hash.as_bytes()))
                .map(|(n, t)| (n.clone(), t.profile.clone()))
        }?;
        let (token, profile) = entry;
        Some(match self.profile(&profile) {
            Some(p) => TokenMatch::Profile { token, profile: p },
            None => TokenMatch::UndefinedProfile { token, profile },
        })
    }
}

fn warn_risky(profiles: &BTreeMap<String, Arc<CustomProfile>>) {
    for p in profiles.values() {
        let prof = ToolProfile::Custom(p.clone());
        let risky: Vec<&str> = RISKY_TOOLS
            .iter()
            .copied()
            .filter(|t| prof.allows(t))
            .collect();
        if !risky.is_empty() {
            log(&format!(
                "access: WARNING profile '{}' allows {}",
                p.name,
                risky.join(", ")
            ));
        }
    }
}

// MARK: - operator CLI: `oab-instance-mcp token …` / `oab-instance-mcp profile list`

fn random_token() -> Result<String, String> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "no system randomness")?;
    Ok(format!(
        "oabt_{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}

fn read_tokens(path: &Path) -> Result<TokensFile, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => parse_tokens(&t).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TokensFile::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Atomic write, mode 0600.
fn write_tokens(path: &Path, file: &TokensFile) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let text = format!(
        "# Named bearer tokens for oab-instance-mcp. Managed by `oab-instance-mcp token`.\n\
         # Only SHA-256 hashes are stored; the token itself was shown once at creation.\n\n{}",
        toml::to_string(file).map_err(|e| e.to_string())?
    );
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("toml.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Run a `token` / `profile` subcommand; returns the process exit code.
pub fn cli(dir: &Path, args: &[String]) -> i32 {
    match run_cli(dir, args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn run_cli(dir: &Path, args: &[String]) -> Result<(), String> {
    let tokens_path = dir.join("tokens.toml");
    let access = Access::new(dir);
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["token", "add", name, "--profile", profile] => {
            if !valid_name(name) {
                return Err(format!("token name '{name}' must match [a-z0-9_-]{{1,32}}"));
            }
            if access.profile(profile).is_none() {
                return Err(format!(
                    "no profile '{profile}'; defined: {} (custom ones go in {})",
                    access.profile_names().join(", "),
                    dir.join("profiles.toml").display()
                ));
            }
            let mut file = read_tokens(&tokens_path)?;
            if file.tokens.contains_key(*name) {
                return Err(format!(
                    "token '{name}' exists; `token revoke {name}` first to replace it"
                ));
            }
            let token = random_token()?;
            let created = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            file.tokens.insert(
                name.to_string(),
                TokenEntry {
                    profile: profile.to_string(),
                    sha256: sha256_hex(&token),
                    created,
                },
            );
            write_tokens(&tokens_path, &file)?;
            println!("token '{name}' (profile {profile}) — shown once, store it now:\n\n{token}\n");
            println!("Callers send it as `Authorization: Bearer <token>`; the Tailscale login check still applies.");
            Ok(())
        }
        ["token", "list"] => {
            let file = read_tokens(&tokens_path)?;
            println!("{:<20} {:<16} CREATED", "NAME", "PROFILE");
            println!("{:<20} {:<16} -", "(--token-file)", "owner");
            for (name, t) in &file.tokens {
                let known = if access.profile(&t.profile).is_some() {
                    ""
                } else {
                    "  ⚠ profile not defined: denied"
                };
                println!("{name:<20} {:<16} {}{known}", t.profile, t.created);
            }
            Ok(())
        }
        ["token", "revoke", name] => {
            let mut file = read_tokens(&tokens_path)?;
            if file.tokens.remove(*name).is_none() {
                return Err(format!("no token '{name}'"));
            }
            write_tokens(&tokens_path, &file)?;
            println!("revoked '{name}' — takes effect on the daemon's next request");
            Ok(())
        }
        ["profile", "list"] => {
            println!("owner    built in: every tool");
            println!("sandbox  built in: no exec*, allowlisted browser_* only");
            let path = dir.join("profiles.toml");
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(_) => {
                    println!("(no custom profiles: {} does not exist)", path.display());
                    return Ok(());
                }
            };
            let profiles = parse_profiles(&text).map_err(|e| format!("{}: {e}", path.display()))?;
            for p in profiles.values() {
                println!("{:<8} allow {:?} deny {:?}", p.name, p.allow, p.deny);
                let prof = ToolProfile::Custom(p.clone());
                let risky: Vec<&str> = RISKY_TOOLS
                    .iter()
                    .copied()
                    .filter(|t| prof.allows(t))
                    .collect();
                if !risky.is_empty() {
                    println!("         ⚠ allows {}", risky.join(", "));
                }
            }
            Ok(())
        }
        _ => Err(
            "usage:\n  oab-instance-mcp token add <name> --profile <profile>\n  \
                  oab-instance-mcp token list\n  oab-instance-mcp token revoke <name>\n  \
                  oab-instance-mcp profile list"
                .into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::glob_match;

    #[test]
    fn globs() {
        assert!(glob_match("browser_*", "browser_click"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("exec*", "exec"));
        assert!(glob_match("*_unsafe", "browser_run_code_unsafe"));
        assert!(glob_match("b*r_*k", "browser_click"));
        assert!(!glob_match("browser_*", "exec"));
        assert!(!glob_match("sys_info", "sys_info2"));
    }

    fn profile(text: &str, name: &str) -> ToolProfile {
        ToolProfile::Custom(parse_profiles(text).unwrap()[name].clone())
    }

    #[test]
    fn custom_profiles_default_deny_and_deny_wins() {
        let p = profile(
            r#"[profiles.browser]
               allow = ["sys_info", "browser_*"]
               deny = ["browser_run_code_unsafe"]"#,
            "browser",
        );
        assert!(p.allows("sys_info") && p.allows("browser_click"));
        assert!(!p.allows("browser_run_code_unsafe"), "deny wins");
        assert!(
            !p.allows("exec") && !p.allows("screenshot"),
            "not allowed ⇒ denied"
        );
        assert!(p.allows("browser_future_tool"), "matched by allow pattern");
        assert!(
            !profile(
                r#"[profiles.v]
            allow = ["sys_info"]"#,
                "v"
            )
            .allows("browser_future_tool"),
            "new tools stay denied"
        );
    }

    #[test]
    fn bad_profile_files_are_rejected() {
        for bad in [
            "[profiles.owner]\nallow = [\"*\"]",
            "[profiles.Bad]\nallow = [\"x\"]",
            "[profiles.x]\nallow = []",
            "[profiles.x]\nallow = [\"\"]",
            "[profiles.x]\nallow = [\"a\"]\ntypo = 1",
            "not toml [",
        ] {
            assert!(parse_profiles(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_broken_file_keeps_the_last_good_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");
        std::fs::write(&path, "[profiles.v]\nallow = [\"sys_info\"]").unwrap();
        let a = Access::new(dir.path());
        assert!(a.profile("v").is_some());
        // A typo must not change what is in force (and cannot widen anything).
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, "[profiles.v]\nallow = [\"*\"\n").unwrap();
        let v = a.profile("v").unwrap();
        assert!(v.allows("sys_info") && !v.allows("exec"));
    }

    #[test]
    fn token_lifecycle_through_the_cli() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("profiles.toml"),
            "[profiles.browser]\nallow = [\"browser_*\"]",
        )
        .unwrap();
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            cli(
                dir.path(),
                &s(&["token", "add", "hermes", "--profile", "nope"])
            ),
            1
        );
        assert_eq!(
            cli(
                dir.path(),
                &s(&["token", "add", "hermes", "--profile", "browser"])
            ),
            0
        );
        assert_eq!(
            cli(
                dir.path(),
                &s(&["token", "add", "hermes", "--profile", "browser"])
            ),
            1,
            "no silent replace"
        );
        let tokens = std::fs::read_to_string(dir.path().join("tokens.toml")).unwrap();
        assert!(
            tokens.contains("sha256") && !tokens.contains("oabt_"),
            "only the hash is stored"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("tokens.toml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(cli(dir.path(), &s(&["token", "revoke", "hermes"])), 0);
        assert_eq!(cli(dir.path(), &s(&["token", "revoke", "hermes"])), 1);
    }

    #[test]
    fn tokens_resolve_to_their_profile_and_hot_reload() {
        let dir = tempfile::tempdir().unwrap();
        let a = Access::new(dir.path());
        assert_eq!(a.match_token("oabt_x"), None);
        let entry = |p: &str| {
            format!(
                "[tokens.hermes]\nprofile = \"{p}\"\nsha256 = \"{}\"\ncreated = \"t\"\n",
                sha256_hex("oabt_x")
            )
        };
        std::fs::write(dir.path().join("tokens.toml"), entry("sandbox")).unwrap();
        assert_eq!(
            a.match_token("oabt_x"),
            Some(TokenMatch::Profile {
                token: "hermes".into(),
                profile: ToolProfile::Sandbox
            })
        );
        assert_eq!(a.match_token("oabt_y"), None);
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.path().join("tokens.toml"), entry("gone")).unwrap();
        assert_eq!(
            a.match_token("oabt_x"),
            Some(TokenMatch::UndefinedProfile {
                token: "hermes".into(),
                profile: "gone".into()
            })
        );
        std::fs::remove_file(dir.path().join("tokens.toml")).unwrap();
        assert_eq!(
            a.match_token("oabt_x"),
            None,
            "revoked by deleting the file"
        );
    }
}
