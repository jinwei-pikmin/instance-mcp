//! Live reverse-attach grants on disk, so they survive a daemon restart (an upgrade, a
//! `systemctl restart`, a crash): Swift `#43`, upstream Linux PoC `#42`.
//!
//! One JSON file, `$XDG_STATE_HOME/oab-instance-mcp/grants.json`: mode 0600 in a 0700
//! directory (narrowed again if something widened them), replaced atomically. It holds each
//! grant's attach secret — the macOS build keeps those in the Keychain; here file permissions
//! are the boundary, as for the bearer token next to it. A secret is only good for one
//! runtime session and dies with the grant's TTL. `GET /attach` never returns it.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::log;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Persisted {
    pub id: String,
    pub runtime: String,
    pub session: String,
    /// Profile name; resolved again on resume (a custom profile may be gone by then).
    pub profile: String,
    pub principal: String,
    /// Unix seconds.
    pub created_at: u64,
    pub expires_at: u64,
    pub secret: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    version: u32,
    grants: Vec<Persisted>,
}

pub struct Store {
    pub path: PathBuf,
}

impl Store {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Missing file ⇒ nothing to resume. A corrupt file is logged and treated as empty:
    /// the worst case is that grants have to be lent again, never that one is widened.
    pub fn load(&self) -> Vec<Persisted> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return vec![],
            Err(e) => {
                log(&format!("grants: cannot read {}: {e}", self.path.display()));
                return vec![];
            }
        };
        match serde_json::from_str::<File>(&text) {
            Ok(f) => f.grants,
            Err(e) => {
                log(&format!(
                    "grants: ignoring unreadable {}: {e}",
                    self.path.display()
                ));
                vec![]
            }
        }
    }

    pub fn save(&self, grants: &[Persisted]) {
        if let Err(e) = self.try_save(grants) {
            log(&format!("grants: cannot save {}: {e}", self.path.display()));
        }
    }

    fn try_save(&self, grants: &[Persisted]) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let body = serde_json::to_vec_pretty(&File {
            version: 1,
            grants: grants.to_vec(),
        })?;
        let tmp = self.path.with_extension("json.tmp");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        // `mode` applies only on create; a leftover tmp file might be wider.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(&body)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)
    }
}
