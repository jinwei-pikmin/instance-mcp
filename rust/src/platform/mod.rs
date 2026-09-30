//! Per-OS backend. Everything the core needs from the host goes through `PlatformBackend`,
//! so the MCP core, auth and exec tools never assume an OS (ADR `linux-rust-port.md`).
//! The implementation is selected at build time with `#[cfg(target_os)]`; `backend()`
//! returns it.

pub mod desktop;
#[cfg(target_os = "linux")]
mod linux;

#[cfg(not(target_os = "linux"))]
compile_error!("only the Linux backend exists so far; macOS still ships the Swift build");

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use desktop::Desktop;

pub trait PlatformBackend: Send + Sync {
    /// Human name of the OS family, used in tool descriptions ("Linux", "macOS").
    fn os_label(&self) -> &'static str;

    /// Shell for `exec*`: program and flags placed before `-c <command>`. Flags should skip
    /// rc files so runs are reproducible (the Swift build uses `zsh -f`).
    fn shell(&self) -> (&'static str, &'static [&'static str]);

    fn hostname(&self) -> String;

    /// Home directory of `user`, for `~user` expansion in `cwd`.
    fn home_of(&self, user: &str) -> Option<String>;

    /// Structured facts plus human summary lines for `sys_info`. `tool_names` is the
    /// server's actual tool list, so the report says what works here.
    fn describe(&self, agent_version: &str, tool_names: &[&str]) -> (Value, Vec<String>);

    /// Where background job logs go.
    fn job_log_dir(&self) -> PathBuf;

    /// The operator's config dir: token, named tokens, custom profiles, portal consent.
    fn config_dir(&self) -> PathBuf;

    /// Screen and input, if this host has a desktop we can drive. None ⇒ the screenshot /
    /// mouse / key tools are not registered (headless nodes keep exec and sys_info).
    fn desktop(&self) -> Option<Arc<dyn Desktop>>;
}

#[cfg(target_os = "linux")]
static BACKEND: linux::Linux = linux::Linux;

/// The backend compiled for this OS.
pub fn backend() -> &'static dyn PlatformBackend {
    &BACKEND
}

/// Interfaces with a CGNAT (100.64/10) v4 or fd7a:115c:a1e0::/48 v6 address — Tailscale's
/// ranges. `getifaddrs` is POSIX, so backends share it.
pub fn tailnet_addresses() -> Vec<String> {
    let mut out = Vec::new();
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs/freeifaddrs pair; we only read the list while it is alive.
    unsafe {
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut p = ifap;
        while !p.is_null() {
            let sa = (*p).ifa_addr;
            if !sa.is_null() {
                match (*sa).sa_family as i32 {
                    libc::AF_INET => {
                        let a = &*(sa as *const libc::sockaddr_in);
                        let ip = std::net::Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr));
                        let o = ip.octets();
                        if o[0] == 100 && (64..=127).contains(&o[1]) {
                            out.push(ip.to_string());
                        }
                    }
                    libc::AF_INET6 => {
                        let a = &*(sa as *const libc::sockaddr_in6);
                        let ip = std::net::Ipv6Addr::from(a.sin6_addr.s6_addr);
                        let s = ip.segments();
                        if s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0 {
                            out.push(ip.to_string());
                        }
                    }
                    _ => {}
                }
            }
            p = (*p).ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_reports_a_usable_host() {
        let b = backend();
        assert!(std::path::Path::new(b.shell().0).exists());
        assert_eq!(b.home_of("root").as_deref(), Some("/root"));
        assert!(b.job_log_dir().ends_with("oab-instance-mcp/jobs"));
        let (facts, lines) = b.describe("test", &["sys_info", "exec"]);
        assert_eq!(facts["host"], b.hostname());
        assert_eq!(facts["capabilities"]["exec"], true);
        assert_eq!(facts["capabilities"]["screenshot"], false);
        // Swift-compatible keys that Mac-first clients decode.
        for k in [
            "agent",
            "host",
            "os",
            "hardware",
            "user",
            "console_user",
            "gui_session",
            "displays",
            "tailscale_ips",
            "permissions",
            "uptime_secs",
        ] {
            assert!(!facts[k].is_null(), "sys_info lacks {k}");
        }
        for k in [
            "screen_recording",
            "accessibility",
            "full_disk_access",
            "full_disk_access_state",
        ] {
            assert!(!facts["permissions"][k].is_null(), "permissions lacks {k}");
        }
        assert!(lines.last().unwrap().ends_with("test"));
        // No desktop tools in the list ⇒ no desktop facts, whatever the session has.
        assert_eq!(facts["desktop"], Value::Null);
        assert_eq!(facts["displays"], serde_json::json!([]));
        assert_eq!(facts["permissions"]["screen_recording"], false);
        // A list without exec (the sandbox profile) must not be told to use exec.
        let (facts, lines) = b.describe("test", &["sys_info", "screenshot"]);
        assert_eq!(facts["capabilities"]["exec"], false);
        assert!(lines.iter().all(|l| !l.contains("exec")), "{lines:?}");
    }
}
