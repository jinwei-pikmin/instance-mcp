//! `bash`: `bash -c` as the daemon user, process-group timeout, per-stream output caps.

use std::io::Read;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use super::tool_result;

pub fn tool_bash(arguments: &Value) -> Result<Value, (i64, String)> {
    use std::process::Stdio;

    let command = arguments
        .get("command")
        .and_then(|c| c.as_str())
        .filter(|c| !c.is_empty())
        .ok_or_else(|| (-32602, "missing command".to_string()))?;
    let timeout_secs = arguments
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(60)
        .clamp(1, 600);
    let cap = arguments
        .get("max_output_bytes")
        .and_then(|v| v.as_u64())
        .unwrap_or(65_536)
        .clamp(1, 1_048_576) as usize;

    let mut cmd = std::process::Command::new("setsid");
    cmd.arg("bash").arg("-c").arg(command);
    if let Some(cwd) = arguments.get("cwd").and_then(|v| v.as_str()) {
        let cwd = if let Some(rest) = cwd.strip_prefix('~') {
            format!("{}{}", std::env::var("HOME").unwrap_or_default(), rest)
        } else {
            cwd.to_string()
        };
        if !std::path::Path::new(&cwd).is_dir() {
            return Err((-32602, format!("cwd is not a directory: {cwd}")));
        }
        cmd.current_dir(cwd);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let started = std::time::Instant::now();
    let mut child = cmd.spawn().map_err(|e| (-32000, format!("spawn: {e}")))?;
    let pid = child.id() as i32;

    // Drain both pipes on threads so a chatty child cannot block on a full pipe.
    fn drain(mut r: impl Read + Send + 'static, cap: usize) -> thread::JoinHandle<(Vec<u8>, bool)> {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            let mut truncated = false;
            loop {
                match r.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if buf.len() < cap {
                            let take = n.min(cap - buf.len());
                            buf.extend_from_slice(&tmp[..take]);
                            if take < n {
                                truncated = true;
                            }
                        } else {
                            truncated = true;
                        }
                    }
                }
            }
            (buf, truncated)
        })
    }
    let out_t = drain(child.stdout.take().unwrap(), cap);
    let err_t = drain(child.stderr.take().unwrap(), cap);

    let deadline = started + Duration::from_secs(timeout_secs);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    timed_out = true;
                    unsafe {
                        kill(-pid, 9); // whole process group (setsid made pid the leader)
                    }
                    break child.wait().ok();
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break None,
        }
    };
    let (stdout, out_trunc) = out_t.join().unwrap_or_default();
    let (stderr, err_trunc) = err_t.join().unwrap_or_default();

    let exit = if timed_out {
        137
    } else {
        status.and_then(|s| s.code()).unwrap_or(-1)
    };
    let structured = json!({
        "exit_code": exit,
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
        "stdout_truncated": out_trunc,
        "stderr_truncated": err_trunc,
        "timed_out": timed_out,
        "duration_ms": started.elapsed().as_millis() as u64,
        "pid": pid
    });
    Ok(tool_result(structured))
}

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
