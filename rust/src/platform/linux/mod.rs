//! Linux backend: host facts from /proc, /sys, /etc and the session environment; the
//! desktop (screenshot / input) through xdg-desktop-portal.

mod portal;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use serde_json::{json, Value};

use super::desktop::{Desktop, DesktopFacts};
use super::PlatformBackend;

pub struct Linux;

impl PlatformBackend for Linux {
    fn os_label(&self) -> &'static str {
        "Linux"
    }

    fn shell(&self) -> (&'static str, &'static [&'static str]) {
        ("/bin/bash", &["--noprofile", "--norc"])
    }

    fn hostname(&self) -> String {
        read("/proc/sys/kernel/hostname").unwrap_or_else(|| "?".into())
    }

    fn home_of(&self, user: &str) -> Option<String> {
        home_of(user)
    }

    fn describe(&self, agent_version: &str, tool_names: &[&str]) -> (Value, Vec<String>) {
        let desktop = self.desktop().map(|d| (d.status(), d.facts()));
        describe(&self.hostname(), agent_version, tool_names, desktop)
    }

    /// Portal-backed desktop when this process is inside a graphical session (GNOME, KDE…).
    /// wlroots compositors (sway, labwc) do not implement the RemoteDesktop portal; they
    /// need a grim/ydotool backend, not written yet.
    fn desktop(&self) -> Option<Arc<dyn Desktop>> {
        static DESKTOP: OnceLock<Option<Arc<dyn Desktop>>> = OnceLock::new();
        DESKTOP
            .get_or_init(|| {
                let has = |k| std::env::var_os(k).is_some_and(|v| !v.is_empty());
                let gui = has("WAYLAND_DISPLAY") || has("DISPLAY");
                let bus = has("DBUS_SESSION_BUS_ADDRESS") || has("XDG_RUNTIME_DIR");
                (gui && bus)
                    .then(|| Arc::new(portal::Portal::new(config_dir())) as Arc<dyn Desktop>)
            })
            .clone()
    }

    /// `$XDG_STATE_HOME/oab-instance-mcp/jobs` (default `~/.local/state/...`).
    fn job_log_dir(&self) -> PathBuf {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(crate::tools::exec::home_dir()).join(".local/state"));
        base.join("oab-instance-mcp/jobs")
    }
}

/// `$XDG_CONFIG_HOME/oab-instance-mcp` (default `~/.config/...`), where the token lives.
fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(crate::tools::exec::home_dir()).join(".config"))
        .join("oab-instance-mcp")
}

/// Home directory of `user` from the passwd database.
fn home_of(user: &str) -> Option<String> {
    let name = std::ffi::CString::new(user).ok()?;
    // SAFETY: getpwnam returns a pointer into static storage; we copy out immediately.
    unsafe {
        let pw = libc::getpwnam(name.as_ptr());
        if pw.is_null() {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr((*pw).pw_dir)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

fn user_name() -> String {
    // SAFETY: as above.
    unsafe {
        let pw = libc::getpwuid(libc::geteuid());
        if pw.is_null() {
            return std::env::var("USER").unwrap_or_else(|_| "?".into());
        }
        std::ffi::CStr::from_ptr((*pw).pw_name)
            .to_string_lossy()
            .into_owned()
    }
}

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn os_pretty() -> String {
    let rel = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let name = rel
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim_matches('"').to_string())
        .unwrap_or_else(|| "Linux".into());
    match read("/proc/sys/kernel/osrelease") {
        Some(k) => format!("{name} (kernel {k})"),
        None => name,
    }
}

fn hardware() -> Value {
    let model = [
        read("/sys/class/dmi/id/product_name"),
        read("/sys/firmware/devicetree/base/model"),
    ]
    .into_iter()
    .flatten()
    .next()
    .map(|s| s.trim_end_matches('\0').to_string())
    .unwrap_or_else(|| "?".into());
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let chip = cpuinfo
        .lines()
        .find(|l| l.starts_with("model name") || l.starts_with("Model"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_else(|| std::env::consts::ARCH.to_string());
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    let mem_kb: f64 = std::fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0);
    json!({
        "model": model, "chip": chip, "arch": std::env::consts::ARCH,
        "cores": cores, "memory_gb": (mem_kb / 1_048_576.0).round(),
    })
}

/// The graphical session this daemon can reach, from its environment (a systemd user
/// service gets these when the desktop imports them, as GNOME and KDE do).
fn session() -> Value {
    let var = |k| std::env::var(k).ok().filter(|v: &String| !v.is_empty());
    let wayland = var("WAYLAND_DISPLAY");
    let x11 = var("DISPLAY");
    let kind = var("XDG_SESSION_TYPE").unwrap_or_else(|| {
        if wayland.is_some() {
            "wayland"
        } else if x11.is_some() {
            "x11"
        } else {
            "none"
        }
        .into()
    });
    json!({
        "type": kind,
        "desktop": var("XDG_CURRENT_DESKTOP"),
        "wayland_display": wayland,
        "x11_display": x11,
        "gui_session": wayland.is_some() || x11.is_some(),
    })
}

fn uptime_secs() -> f64 {
    read("/proc/uptime")
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or(0.0)
        .round()
}

fn describe(
    host: &str,
    agent_version: &str,
    tool_names: &[&str],
    desktop: Option<(String, DesktopFacts)>,
) -> (Value, Vec<String>) {
    let os = os_pretty();
    let hw = hardware();
    let sess = session();
    let user = user_name();
    let tail = super::tailnet_addresses();
    let gui = sess["gui_session"].as_bool().unwrap_or(false);
    let capabilities = json!({
        "exec": tool_names.contains(&"exec"),
        "screenshot": tool_names.contains(&"screenshot"),
        "input": tool_names.contains(&"mouse"),
        "osascript": false,
    });

    // Same keys and types as the Swift build's sys_info, so clients written against the Mac
    // (OpenAB Connect decodes this) accept a Linux instance. Linux has no TCC: screen and
    // input "permissions" mean the desktop's remote-control consent has been given.
    let facts = desktop.as_ref().map(|(_, f)| f.clone()).unwrap_or_default();
    let consent = desktop.is_some() && facts.consent;
    let displays: Vec<Value> = facts
        .displays
        .iter()
        .map(|d| {
            json!({
                "id": d.index,
                "main": d.index == 0,
                "origin": {"x": d.x, "y": d.y},
                "points": {"width": d.width, "height": d.height},
                "pixels": {"width": (d.width * d.scale).round(), "height": (d.height * d.scale).round()},
            })
        })
        .collect();
    let permissions = json!({
        "screen_recording": consent,
        "accessibility": consent,
        "full_disk_access": true,
        "full_disk_access_state": "granted",
    });
    let desktop_status = desktop.as_ref().map(|(s, _)| s.clone());

    let structured = json!({
        "agent": {"name": "oab-instance-mcp", "version": agent_version, "pid": std::process::id(), "platform": "linux"},
        "host": host,
        "os": os,
        "hardware": hw,
        "user": user,
        "session": sess,
        "console_user": if gui { user.clone() } else { "none".to_string() },
        "gui_session": gui,
        "displays": displays,
        "permissions": permissions,
        "desktop": desktop_status,
        "tailscale_ips": tail,
        "capabilities": capabilities,
        "uptime_secs": uptime_secs(),
    });

    let mut lines = vec![
        format!(
            "{host} — {os}, {}, {}, {} GB",
            hw["model"].as_str().unwrap_or("?"),
            hw["chip"].as_str().unwrap_or("?"),
            hw["memory_gb"].as_f64().unwrap_or(0.0) as i64
        ),
        format!(
            "user {user}; session {} {}{}",
            sess["type"].as_str().unwrap_or("?"),
            sess["desktop"].as_str().unwrap_or(""),
            if gui {
                " (agent can see the desktop session)"
            } else {
                " (no desktop session in agent env)"
            }
        ),
        format!(
            "tailscale: {}",
            if tail.is_empty() {
                "none".into()
            } else {
                tail.join(", ")
            }
        ),
        format!("tools: {}", tool_names.join(", ")),
    ];
    match &desktop_status {
        Some(d) => lines.push(format!("desktop: {d}")),
        None => lines.push(
            "→ no graphical session in the agent's environment: screenshot / mouse / key are off; use exec"
                .into(),
        ),
    }
    lines.push("→ osascript does not exist on Linux; use exec (e.g. gdbus, xdg-open)".into());
    lines.push(format!("agent {agent_version}"));
    (structured, lines)
}
