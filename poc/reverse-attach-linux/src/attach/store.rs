//! Grants survive a daemon restart (#12).
//!
//! Live grants are written to a mode-600 JSON file so a restart (deploy, crash,
//! `systemctl --user restart`) re-dials every grant whose deadline has not passed,
//! with its original id — instead of the human having to lend again.
//!
//! What is stored: id, runtime, session, profile, principal, absolute expiry and
//! the attach **secret** the node dials with. The secret is TTL-bounded and sits
//! at the same trust level as the bearer token file beside it (both 600 in the
//! daemon user's home). Ended, cancelled and expired grants are never written.
//!
//! Path: `MCP_GRANTS_FILE`, else `$XDG_STATE_HOME/oab-instance-mcp/grants.json`,
//! else `~/.local/state/oab-instance-mcp/grants.json`. `MCP_GRANTS_FILE=off`
//! disables persistence.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use super::{now_epoch_secs, GrantInfo, Registry};

const VERSION: u64 = 1;

/// A grant as persisted: everything needed to resume dialling it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Persisted {
    pub(crate) id: String,
    pub(crate) runtime: String,
    pub(crate) session: String,
    pub(crate) profile: String,
    pub(crate) principal: String,
    pub(crate) expires_at_epoch_secs: u64,
    pub(crate) secret: String,
}

pub(crate) struct GrantStore {
    path: PathBuf,
    /// Serialises writers; the file is always replaced whole.
    write: Mutex<()>,
}

static STORE: OnceLock<Option<GrantStore>> = OnceLock::new();

/// Configure persistence once at startup. Returns the path in use, if any.
pub(crate) fn init_from_env() -> Option<PathBuf> {
    let store = STORE.get_or_init(|| default_path().map(GrantStore::new));
    store.as_ref().map(|s| s.path.clone())
}

fn default_path() -> Option<PathBuf> {
    match std::env::var("MCP_GRANTS_FILE") {
        Ok(v) if v == "off" => return None,
        Ok(v) if !v.is_empty() => return Some(PathBuf::from(v)),
        _ => {}
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("oab-instance-mcp/grants.json"))
}

/// Snapshot the registry's live grants to disk. No-op when persistence is off.
/// Errors are logged, never fatal: losing persistence must not stop lending.
pub(crate) fn save(registry: &Registry) {
    let Some(Some(store)) = STORE.get() else {
        return;
    };
    let grants: Vec<Persisted> = match registry.lock() {
        Ok(map) => map.values().filter_map(persistable).collect(),
        Err(_) => return,
    };
    if let Err(e) = store.write(&grants) {
        eprintln!("grants: could not persist to {}: {e}", store.path.display());
    }
}

/// Load grants that are still within their deadline. Empty when persistence is
/// off or the file is absent/unreadable (the reason is logged).
pub(crate) fn load() -> Vec<Persisted> {
    let Some(Some(store)) = STORE.get() else {
        return Vec::new();
    };
    match store.read() {
        Ok(grants) => grants,
        Err(e) => {
            eprintln!("grants: ignoring {}: {e}", store.path.display());
            Vec::new()
        }
    }
}

fn persistable(g: &GrantInfo) -> Option<Persisted> {
    let live = g.state != "ended"
        && !g.cancelled.load(std::sync::atomic::Ordering::Acquire)
        && g.expires_at_epoch_secs > now_epoch_secs();
    live.then(|| Persisted {
        id: g.id.clone(),
        runtime: g.runtime.clone(),
        session: g.session.clone(),
        profile: g.profile.clone(),
        principal: g.principal.clone(),
        expires_at_epoch_secs: g.expires_at_epoch_secs,
        secret: g.secret.clone(),
    })
}

impl GrantStore {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            write: Mutex::new(()),
        }
    }

    pub(crate) fn write(&self, grants: &[Persisted]) -> std::io::Result<()> {
        let _guard = self.write.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(dir) = self.path.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let body = json!({
            "version": VERSION,
            "grants": grants.iter().map(|g| json!({
                "id": g.id,
                "runtime": g.runtime,
                "session": g.session,
                "profile": g.profile,
                "principal": g.principal,
                "expires_at_epoch_secs": g.expires_at_epoch_secs,
                "secret": g.secret,
            })).collect::<Vec<_>>(),
        });
        let tmp = tmp_path(&self.path);
        let _ = fs::remove_file(&tmp);
        {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(body.to_string().as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path)
    }

    pub(crate) fn read(&self) -> Result<Vec<Persisted>, String> {
        let meta = match fs::metadata(&self.path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.to_string()),
        };
        // The file holds attach secrets. If something widened it, narrow it back
        // before trusting it, and say so.
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            eprintln!(
                "grants: {} was mode {mode:o}; resetting to 600",
                self.path.display()
            );
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }
        let text = fs::read_to_string(&self.path).map_err(|e| e.to_string())?;
        parse(&text, now_epoch_secs())
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Parse a grants file, keeping only well-formed grants still inside their
/// deadline at `now`.
pub(crate) fn parse(text: &str, now: u64) -> Result<Vec<Persisted>, String> {
    let doc: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    if doc["version"].as_u64() != Some(VERSION) {
        return Err(format!("unsupported version {}", doc["version"]));
    }
    let s = |g: &Value, k: &str| g[k].as_str().map(String::from);
    Ok(doc["grants"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|g| {
            Some(Persisted {
                id: s(g, "id")?,
                runtime: s(g, "runtime")?,
                session: s(g, "session")?,
                profile: s(g, "profile")?,
                principal: s(g, "principal")?,
                expires_at_epoch_secs: g["expires_at_epoch_secs"].as_u64()?,
                secret: s(g, "secret")?,
            })
        })
        .filter(|g| g.expires_at_epoch_secs > now)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(id: &str, expires: u64) -> Persisted {
        Persisted {
            id: id.into(),
            runtime: "ws://127.0.0.1:1".into(),
            session: "s".into(),
            profile: "sandbox".into(),
            principal: "you@example.com".into(),
            expires_at_epoch_secs: expires,
            secret: "sec".into(),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ra-store-{name}-{}-{}",
            std::process::id(),
            now_epoch_secs()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir.join("sub/grants.json")
    }

    #[test]
    fn round_trips_with_owner_only_permissions() {
        let path = scratch("rt");
        let store = GrantStore::new(path.clone());
        let later = now_epoch_secs() + 3600;
        store
            .write(&[grant("a", later), grant("b", later)])
            .unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            store.read().unwrap(),
            vec![grant("a", later), grant("b", later)]
        );
        // Replacing is whole-file and leaves no temp file behind.
        store.write(&[grant("b", later)]).unwrap();
        assert_eq!(store.read().unwrap(), vec![grant("b", later)]);
        assert!(!tmp_path(&path).exists());
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn expired_and_malformed_grants_are_dropped_on_load() {
        let now = 1_000_000;
        let text = json!({"version": 1, "grants": [
            {"id":"live","runtime":"ws://x","session":"s","profile":"owner","principal":"p","expires_at_epoch_secs": now + 5,"secret":"k"},
            {"id":"expired","runtime":"ws://x","session":"s","profile":"owner","principal":"p","expires_at_epoch_secs": now,"secret":"k"},
            {"id":"no-secret","runtime":"ws://x","session":"s","profile":"owner","principal":"p","expires_at_epoch_secs": now + 5}
        ]})
        .to_string();
        let got = parse(&text, now).unwrap();
        assert_eq!(
            got.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(),
            vec!["live"]
        );
        assert!(parse("{\"version\":2,\"grants\":[]}", now).is_err());
        assert!(parse("not json", now).is_err());
    }

    #[test]
    fn a_widened_file_is_narrowed_before_it_is_read() {
        let path = scratch("mode");
        let store = GrantStore::new(path.clone());
        store.write(&[grant("a", now_epoch_secs() + 60)]).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(store.read().unwrap().len(), 1);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_missing_file_is_simply_empty() {
        let store = GrantStore::new(scratch("missing"));
        assert!(store.read().unwrap().is_empty());
    }
}
