//! Linux backend: host facts from /proc, /sys, /etc and the session environment.

use std::path::PathBuf;

use serde_json::{json, Value};

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
        describe(&self.hostname(), agent_version, tool_names)
    }

    /// `$XDG_STATE_HOME/oab-instance-mcp/jobs` (default `~/.local/state/...`).
    fn job_log_dir(&self) -> PathBuf {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(crate::tools::exec::home_dir()).join(".local/state"));
        base.join("oab-instance-mcp/jobs")
    }
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

fn describe(host: &str, agent_version: &str, tool_names: &[&str]) -> (Value, Vec<String>) {
    let os = os_pretty();
    let hw = hardware();
    let sess = session();
    let user = user_name();
    let tail = super::tailnet_addresses();
    let gui = sess["gui_session"].as_bool().unwrap_or(false);
    // Desktop tools are not in this build yet; say so plainly rather than let calls fail.
    let capabilities = json!({
        "exec": tool_names.contains(&"exec"),
        "screenshot": tool_names.contains(&"screenshot"),
        "input": tool_names.contains(&"mouse"),
        "osascript": false,
    });

    let structured = json!({
        "agent": {"name": "oab-instance-mcp", "version": agent_version, "pid": std::process::id(), "platform": "linux"},
        "host": host,
        "os": os,
        "hardware": hw,
        "user": user,
        "session": sess,
        "gui_session": gui,
        "displays": [],
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
    if !tool_names.contains(&"screenshot") {
        lines.push(
            "→ screenshot / mouse / key are not available in this Linux build yet; use exec".into(),
        );
    }
    lines.push("→ osascript does not exist on Linux; use exec (e.g. gdbus, xdg-open)".into());
    lines.push(format!("agent {agent_version}"));
    (structured, lines)
}
