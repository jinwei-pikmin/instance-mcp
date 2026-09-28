//! `exec` (synchronous) plus the shared spawn/cwd plumbing used by the job tools.
//! Port of `ExecTool.swift` + `ExecSupport.swift`.
//!
//! Every command runs as the leader of a *new session* (`setsid` in the child), so a
//! timeout or cancel can `killpg` the whole tree and can never reach the daemon's own group.

use std::collections::HashMap;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

use crate::mcp::{arg_f64, arg_i64, arg_str, RpcError, Tool, ToolFail, ToolFuture, ToolResult};
use crate::platform;

pub fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".into())
}

/// Expand a leading `~` / `~user`, then require an absolute, existing directory. Fails with
/// a message safe to hand back to the model instead of an opaque spawn error.
pub fn resolve_cwd(raw: &str) -> Result<String, String> {
    let expanded = expand_tilde(raw);
    if !expanded.starts_with('/') {
        return Err(format!(
            "cwd must be an absolute path or start with ~ (got: {raw})"
        ));
    }
    match std::fs::metadata(&expanded) {
        Err(_) => Err(format!("cwd does not exist: {expanded}")),
        Ok(m) if !m.is_dir() => Err(format!("cwd is not a directory: {expanded}")),
        Ok(_) => Ok(expanded),
    }
}

fn expand_tilde(raw: &str) -> String {
    let Some(rest) = raw.strip_prefix('~') else {
        return raw.to_string();
    };
    let (user, tail) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if user.is_empty() {
        return format!("{}{tail}", home_dir());
    }
    match platform::backend().home_of(user) {
        Some(h) => format!("{h}{tail}"),
        None => raw.to_string(),
    }
}

/// Daemon environment plus `TERM=dumb` default plus caller-supplied `env`.
pub fn build_env(args: &Value) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    env.entry("TERM".into()).or_insert_with(|| "dumb".into());
    if let Some(extra) = args.get("env").and_then(Value::as_object) {
        for (k, v) in extra {
            if let Some(s) = v.as_str() {
                env.insert(k.clone(), s.to_string());
            }
        }
    }
    env
}

/// Spawn `<shell> -c command` in its own session, stdin=/dev/null, stdout/stderr piped.
pub fn spawn(command: &str, cwd: &str, env: &HashMap<String, String>) -> Result<Child, String> {
    let (shell, flags) = platform::backend().shell();
    let mut cmd = Command::new(shell);
    cmd.args(flags)
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    // SAFETY: setsid is async-signal-safe; nothing else runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().map_err(|e| format!("spawn failed: {e}"))
}

/// Signal a process group we created. Refuses pid 0, which `killpg` would read as "the
/// caller's own group" — the daemon itself.
pub fn killpg(pid: u32, sig: i32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: plain syscall; pid is a session leader we spawned.
    unsafe { libc::killpg(pid as libc::pid_t, sig) == 0 }
}

/// Shell convention: exit status, or 128+signal when killed.
pub fn exit_code(st: ExitStatus) -> (i32, bool) {
    use std::os::unix::process::ExitStatusExt;
    match (st.code(), st.signal()) {
        (Some(c), _) => (c, false),
        (None, Some(s)) => (128 + s, true),
        _ => (-1, false),
    }
}

/// How long to keep reading after the command itself has exited. Pipes normally hit EOF
/// at once; if they do not, a background process it started (`firefox &`, `nohup … &`)
/// inherited them and may hold them for hours, so we stop waiting.
pub const PIPE_GRACE: Duration = Duration::from_millis(300);

#[derive(Default)]
struct Collected {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Read a stream into `into`, keeping at most `cap` bytes (the rest is drained and dropped
/// so the child never blocks on a full pipe). Shared so a caller that stops waiting still
/// has everything read so far.
async fn read_capped(mut r: impl AsyncRead + Unpin, cap: usize, into: Arc<Mutex<Collected>>) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut c = into.lock().unwrap();
                let room = cap.saturating_sub(c.bytes.len());
                if n > room {
                    c.truncated = true;
                }
                c.bytes.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
}

/// Wait up to `grace` for every task; abort the stragglers. True if all finished.
pub async fn join_within(tasks: Vec<tokio::task::JoinHandle<()>>, grace: Duration) -> bool {
    let aborts: Vec<_> = tasks.iter().map(|t| t.abort_handle()).collect();
    let all = futures_util::future::join_all(tasks);
    if tokio::time::timeout(grace, all).await.is_ok() {
        return true;
    }
    aborts.iter().for_each(|a| a.abort());
    false
}

pub struct Outcome {
    pub exit_code: i32,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// A background process still held stdout/stderr when the command exited; output it
    /// writes later is not captured.
    pub output_detached: bool,
    pub duration: Duration,
}

pub async fn run(
    command: &str,
    cwd: &str,
    env: &HashMap<String, String>,
    timeout: Duration,
    cap: usize,
) -> Result<Outcome, String> {
    let start = Instant::now();
    let mut child = spawn(command, cwd, env)?;
    let pid = child
        .id()
        .ok_or("child exited before it could be tracked")?;
    let (out, err) = (
        Arc::new(Mutex::new(Collected::default())),
        Arc::new(Mutex::new(Collected::default())),
    );
    let readers = vec![
        tokio::spawn(read_capped(child.stdout.take().unwrap(), cap, out.clone())),
        tokio::spawn(read_capped(child.stderr.take().unwrap(), cap, err.clone())),
    ];

    let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(st) => (st.map_err(|e| e.to_string())?, false),
        Err(_) => {
            killpg(pid, libc::SIGKILL);
            (child.wait().await.map_err(|e| e.to_string())?, true)
        }
    };
    // Pipes reach EOF once every writer has exited — unless something outlived the shell.
    let output_detached = !join_within(readers, PIPE_GRACE).await;
    let take = |c: &Arc<Mutex<Collected>>| {
        let c = c.lock().unwrap();
        (String::from_utf8_lossy(&c.bytes).into_owned(), c.truncated)
    };
    let ((stdout, stdout_truncated), (stderr, stderr_truncated)) = (take(&out), take(&err));
    Ok(Outcome {
        exit_code: exit_code(status).0,
        timed_out,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        output_detached,
        duration: start.elapsed(),
    })
}

pub struct ExecTool;

impl Tool for ExecTool {
    fn name(&self) -> &'static str {
        "exec"
    }
    fn description(&self) -> String {
        format!(
            "Run a shell command on this machine with `{} -c`, as the daemon's user inside its desktop \
             session environment. Returns exit code, stdout and stderr. Output is truncated to \
             max_output_bytes (default 64 KiB) per stream; a timeout kills the process group and \
             reports timed_out=true.",
            platform::backend().shell().0
        )
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": format!("Shell command line, run via `{} -c`.", platform::backend().shell().0)},
                "cwd": {"type": "string", "description": "Working directory. Default: user's home."},
                "timeout_secs": {"type": "number", "description": "Kill after this many seconds. Default 60, max 600."},
                "max_output_bytes": {"type": "integer", "description": "Per-stream cap. Default 65536, max 1048576."},
                "env": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Extra environment variables."},
            },
            "required": ["command"],
        })
    }

    fn call<'a>(&'a self, args: &'a Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let command = arg_str(args, "command")
                .filter(|c| !c.is_empty())
                .ok_or_else(|| RpcError::invalid_params("command is required"))?;
            let timeout = arg_f64(args, "timeout_secs")
                .unwrap_or(60.0)
                .clamp(0.0, 600.0);
            let cap = arg_i64(args, "max_output_bytes")
                .unwrap_or(65536)
                .clamp(0, 1 << 20) as usize;
            let cwd =
                resolve_cwd(arg_str(args, "cwd").unwrap_or(&home_dir())).map_err(ToolFail::Tool)?;
            let env = build_env(args);

            let r = run(command, &cwd, &env, Duration::from_secs_f64(timeout), cap)
                .await
                .map_err(ToolFail::Tool)?;

            let mut text = r.stdout.clone();
            if !r.stderr.is_empty() {
                text += &format!(
                    "{}[stderr]\n{}",
                    if text.is_empty() { "" } else { "\n" },
                    r.stderr
                );
            }
            let mut trailer = format!("[exit {}", r.exit_code);
            if r.timed_out {
                trailer += &format!(", timed out after {}s", timeout as i64);
            }
            if r.stdout_truncated || r.stderr_truncated {
                trailer += ", output truncated";
            }
            if r.output_detached {
                trailer += ", a background process kept the output open; later output not captured";
            }
            trailer += "]";
            text += &format!("{}{trailer}", if text.is_empty() { "" } else { "\n" });

            let structured = json!({
                "exit_code": r.exit_code,
                "timed_out": r.timed_out,
                "stdout": r.stdout, "stderr": r.stderr,
                "stdout_truncated": r.stdout_truncated, "stderr_truncated": r.stderr_truncated,
                "output_detached": r.output_detached,
                "duration_ms": r.duration.as_millis() as u64,
            });
            let mut res = ToolResult::text(text, Some(structured));
            res.is_error = r.exit_code != 0 || r.timed_out;
            Ok(res)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> HashMap<String, String> {
        build_env(&json!({}))
    }

    #[tokio::test]
    async fn runs_and_captures() {
        let r = run(
            "echo hi; echo oops >&2; exit 3",
            "/",
            &env(),
            Duration::from_secs(5),
            1024,
        )
        .await
        .unwrap();
        assert_eq!(
            (
                r.exit_code,
                r.stdout.as_str(),
                r.stderr.as_str(),
                r.timed_out
            ),
            (3, "hi\n", "oops\n", false)
        );
    }

    #[tokio::test]
    async fn background_process_holding_the_pipe_does_not_hang_exec() {
        // `app &` leaves a grandchild holding stdout after the shell exits: return promptly
        // with what was printed, and say the output was left behind.
        let t = Instant::now();
        let r = run(
            "echo started; sleep 30 &",
            "/",
            &env(),
            Duration::from_secs(20),
            1024,
        )
        .await
        .unwrap();
        assert!(
            t.elapsed() < Duration::from_secs(3),
            "took {:?}",
            t.elapsed()
        );
        assert_eq!(
            (r.exit_code, r.timed_out, r.stdout.as_str()),
            (0, false, "started\n")
        );
        assert!(r.output_detached);
        // A plain command is not flagged.
        assert!(
            !run("echo hi", "/", &env(), Duration::from_secs(5), 1024)
                .await
                .unwrap()
                .output_detached
        );
    }

    #[test]
    fn killpg_refuses_pid_zero() {
        // pid 0 would mean the daemon's own process group.
        assert!(!killpg(0, 0));
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_group() {
        // The background grandchild holds the pipe open; only killpg lets this return.
        let t = Instant::now();
        let r = run(
            "sleep 30 & sleep 30",
            "/",
            &env(),
            Duration::from_millis(300),
            1024,
        )
        .await
        .unwrap();
        assert!(r.timed_out);
        assert_eq!(r.exit_code, 137);
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn truncates_per_stream() {
        let r = run(
            "head -c 5000 /dev/zero",
            "/",
            &env(),
            Duration::from_secs(5),
            100,
        )
        .await
        .unwrap();
        assert!(r.stdout_truncated);
        assert_eq!(r.stdout.len(), 100);
    }

    #[test]
    fn cwd_resolution() {
        assert_eq!(resolve_cwd("~").unwrap(), home_dir());
        assert!(resolve_cwd("relative").unwrap_err().contains("absolute"));
        assert!(resolve_cwd("/definitely/not/here")
            .unwrap_err()
            .contains("does not exist"));
        assert!(resolve_cwd("/etc/hostname")
            .unwrap_err()
            .contains("not a directory"));
    }
}
