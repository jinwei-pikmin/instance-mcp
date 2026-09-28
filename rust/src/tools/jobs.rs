//! Background `exec` jobs: `exec_start` / `exec_poll` / `exec_list` / `exec_cancel`.
//! Port of `AsyncExecTools.swift` + `JobRegistry.swift`.
//!
//! Each job's stdout/stderr are tee'd to `<log_dir>/<job_id>.out` / `.err`, which are the
//! single source of truth: `exec_poll` reads them by byte offset, nothing is truncated, and
//! a log survives after the job's metadata is garbage-collected. A human can `tail -f` them.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Map, Value};

use super::exec::{
    build_env, exit_code, home_dir, join_within, killpg, resolve_cwd, spawn, PIPE_GRACE,
};
use crate::mcp::{arg_f64, arg_i64, arg_str, RpcError, Tool, ToolFail, ToolFuture, ToolResult};
use crate::platform;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    /// Process finished on its own.
    Exited,
    /// Cancelled, timed out, or died of a signal.
    Killed,
}

impl JobState {
    fn as_str(self) -> &'static str {
        match self {
            JobState::Running => "running",
            JobState::Exited => "exited",
            JobState::Killed => "killed",
        }
    }
}

struct Finish {
    state: JobState,
    exit_code: Option<i32>,
    finished_at: Option<SystemTime>,
}

pub struct Job {
    pub id: String,
    pub pid: u32,
    pub command: String,
    pub cwd: String,
    pub started_at: SystemTime,
    pub out_path: PathBuf,
    pub err_path: PathBuf,
    fin: Mutex<Finish>,
}

impl Job {
    pub fn state(&self) -> JobState {
        self.fin.lock().unwrap().state
    }
    pub fn exit_code(&self) -> Option<i32> {
        self.fin.lock().unwrap().exit_code
    }
    fn finished_at(&self) -> Option<SystemTime> {
        self.fin.lock().unwrap().finished_at
    }
    fn finish(&self, state: JobState, code: i32) {
        let mut f = self.fin.lock().unwrap();
        if f.state == JobState::Running {
            *f = Finish {
                state,
                exit_code: Some(code),
                finished_at: Some(SystemTime::now()),
            };
        }
    }
}

/// Default and ceiling for how much of each stream one poll returns. Bounded so a noisy
/// build cannot produce a gigabyte response (or overflow the 16 MiB attach frame).
const POLL_DEFAULT_BYTES: u64 = 256 * 1024;
const POLL_MAX_BYTES: u64 = 1024 * 1024;
/// Longest `exec_start` timeout; beyond this, run without one and use `exec_cancel`.
const MAX_JOB_TIMEOUT_SECS: f64 = 7.0 * 86400.0;

struct LogChunk {
    bytes: Vec<u8>,
    /// Offset to pass as `*_since` next time.
    next: u64,
    /// More bytes are already on disk past `next`.
    more: bool,
}

/// Up to `max` bytes of a log file from offset `from`. Unless the file is complete
/// (`finished` and nothing left), a trailing partial UTF-8 character is left for the next
/// poll, so a character split across two polls is not turned into U+FFFD twice.
fn read_log(path: &PathBuf, from: u64, max: u64, finished: bool) -> LogChunk {
    let empty = |next| LogChunk {
        bytes: Vec::new(),
        next,
        more: false,
    };
    let Ok(mut f) = std::fs::File::open(path) else {
        return empty(from);
    };
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    if size <= from {
        return empty(size);
    }
    let want = (size - from).min(max);
    let mut out = Vec::new();
    if f.seek(SeekFrom::Start(from)).is_ok() {
        let _ = f.take(want).read_to_end(&mut out);
    }
    let complete = finished && from + out.len() as u64 >= size;
    if !complete {
        if let Err(e) = std::str::from_utf8(&out) {
            // error_len() == None: the input ended inside a character.
            if e.error_len().is_none() && out.len() - e.valid_up_to() <= 3 && e.valid_up_to() > 0 {
                out.truncate(e.valid_up_to());
            }
        }
    }
    let next = from + out.len() as u64;
    LogChunk {
        bytes: out,
        next,
        more: next < size,
    }
}

fn file_size(path: &PathBuf) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn iso(t: SystemTime) -> String {
    DateTime::<Utc>::from(t).to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub struct JobRegistry {
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    pub log_dir: PathBuf,
    /// Keep terminal-job *metadata* this long.
    retention: Duration,
    max_jobs: usize,
}

impl JobRegistry {
    pub fn new(log_dir: PathBuf) -> Arc<Self> {
        let _ = std::fs::create_dir_all(&log_dir);
        let reg = Self {
            jobs: Mutex::new(HashMap::new()),
            log_dir,
            retention: Duration::from_secs(600),
            max_jobs: 64,
        };
        reg.cleanup_old_log_files(Duration::from_secs(7 * 86400));
        Arc::new(reg)
    }

    fn cleanup_old_log_files(&self, max_age: Duration) {
        let Ok(entries) = std::fs::read_dir(&self.log_dir) else {
            return;
        };
        let cutoff = SystemTime::now() - max_age;
        for e in entries.flatten() {
            let p = e.path();
            let is_log = matches!(p.extension().and_then(|x| x.to_str()), Some("out" | "err"));
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|m| m < cutoff);
            if is_log && old {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    fn gc(&self, jobs: &mut HashMap<String, Arc<Job>>) {
        let now = SystemTime::now();
        jobs.retain(|_, j| {
            j.finished_at()
                .is_none_or(|t| now.duration_since(t).unwrap_or_default() <= self.retention)
        });
    }

    /// Start a background job. `timeout` of zero means none.
    pub fn start(
        &self,
        command: &str,
        cwd: &str,
        env: &HashMap<String, String>,
        timeout: Duration,
    ) -> Result<Arc<Job>, String> {
        let mut jobs = self.jobs.lock().unwrap();
        self.gc(&mut jobs);
        if jobs.len() >= self.max_jobs {
            return Err(format!(
                "too many jobs ({}); cancel/let existing ones finish first",
                self.max_jobs
            ));
        }
        let id = format!("job-{}", uuid::Uuid::new_v4());
        let out_path = self.log_dir.join(format!("{id}.out"));
        let err_path = self.log_dir.join(format!("{id}.err"));
        let open = |p: &PathBuf| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
        };
        let (Ok(out_file), Ok(err_file)) = (open(&out_path), open(&err_path)) else {
            return Err(format!(
                "cannot open job log files in {}",
                self.log_dir.display()
            ));
        };

        let mut child = spawn(command, cwd, env)?;
        // No pid means it already exited and was reaped; a 0 here would later make
        // exec_cancel signal the daemon's own process group.
        let pid = child
            .id()
            .ok_or("the command exited before it could be tracked")?;
        let job = Arc::new(Job {
            id: id.clone(),
            pid,
            command: command.into(),
            cwd: cwd.into(),
            started_at: SystemTime::now(),
            out_path,
            err_path,
            fin: Mutex::new(Finish {
                state: JobState::Running,
                exit_code: None,
                finished_at: None,
            }),
        });
        jobs.insert(id, job.clone());

        let (mut so, mut se) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let tee_out = tokio::spawn(async move {
            let _ = tokio::io::copy(&mut so, &mut tokio::fs::File::from_std(out_file)).await;
        });
        let tee_err = tokio::spawn(async move {
            let _ = tokio::io::copy(&mut se, &mut tokio::fs::File::from_std(err_file)).await;
        });
        let j = job.clone();
        tokio::spawn(async move {
            let (status, timed_out) = if timeout.is_zero() {
                (child.wait().await, false)
            } else {
                match tokio::time::timeout(timeout, child.wait()).await {
                    Ok(st) => (st, false),
                    Err(_) => {
                        killpg(pid, libc::SIGKILL);
                        (child.wait().await, true)
                    }
                }
            };
            // Report the job finished shortly after the command exits even if a background
            // process it started still holds the pipes. The tee keeps copying that process's
            // output into the log (cutting it would kill it with SIGPIPE).
            join_within(vec![tee_out, tee_err], PIPE_GRACE).await;
            let (code, signalled) = status.map(exit_code).unwrap_or((-1, false));
            let state = if timed_out || signalled {
                JobState::Killed
            } else {
                JobState::Exited
            };
            j.finish(state, code);
        });
        Ok(job)
    }

    pub fn get(&self, id: &str) -> Option<Arc<Job>> {
        self.jobs.lock().unwrap().get(id).cloned()
    }

    /// All running jobs plus the most recent `recent_finished` terminal ones (newest first).
    pub fn list(&self, recent_finished: usize) -> Vec<Arc<Job>> {
        let mut jobs = self.jobs.lock().unwrap();
        self.gc(&mut jobs);
        let mut running: Vec<_> = jobs
            .values()
            .filter(|j| j.state() == JobState::Running)
            .cloned()
            .collect();
        running.sort_by_key(|j| std::cmp::Reverse(j.started_at));
        let mut done: Vec<_> = jobs
            .values()
            .filter(|j| j.state() != JobState::Running)
            .cloned()
            .collect();
        done.sort_by_key(|j| std::cmp::Reverse(j.finished_at()));
        done.truncate(recent_finished);
        running.extend(done);
        running
    }

    pub fn cancel(&self, id: &str, sig: i32) -> bool {
        match self.get(id) {
            Some(j) if j.state() == JobState::Running => killpg(j.pid, sig),
            _ => false,
        }
    }

    /// Drop a terminal job's metadata (log files stay on disk).
    pub fn drop_job(&self, id: &str) -> bool {
        let mut jobs = self.jobs.lock().unwrap();
        match jobs.get(id) {
            Some(j) if j.state() != JobState::Running => {
                jobs.remove(id);
                true
            }
            _ => false,
        }
    }
}

fn job_id(args: &Value) -> Result<&str, ToolFail> {
    arg_str(args, "job_id").ok_or_else(|| RpcError::invalid_params("job_id is required").into())
}

pub struct ExecStartTool(pub Arc<JobRegistry>);
pub struct ExecPollTool(pub Arc<JobRegistry>);
pub struct ExecListTool(pub Arc<JobRegistry>);
pub struct ExecCancelTool(pub Arc<JobRegistry>);

impl Tool for ExecStartTool {
    fn name(&self) -> &'static str {
        "exec_start"
    }
    fn description(&self) -> String {
        "Start a shell command in the background and return a job_id immediately, for work that \
         outlives one request (e.g. a release build). Poll it with `exec_poll` and stop it with \
         `exec_cancel`. Same shell/session context as `exec`. Output is tee'd to log files and never \
         truncated. Prefer plain `exec` for anything that finishes in a few seconds."
            .into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": format!("Shell command line, run via `{} -c`.", platform::backend().shell().0)},
                "cwd": {"type": "string", "description": "Working directory (a leading ~ is expanded). Default: user's home."},
                "timeout_secs": {"type": "number", "description": "Kill the job after this many seconds (max 604800 = 7 days). 0 = no timeout (stop it with exec_cancel). Default 0."},
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
                .unwrap_or(0.0)
                .clamp(0.0, MAX_JOB_TIMEOUT_SECS);
            let cwd =
                resolve_cwd(arg_str(args, "cwd").unwrap_or(&home_dir())).map_err(ToolFail::Tool)?;
            let job = self
                .0
                .start(
                    command,
                    &cwd,
                    &build_env(args),
                    Duration::from_secs_f64(timeout),
                )
                .map_err(ToolFail::Tool)?;
            Ok(ToolResult::text(
                format!("started {} (pid {}); poll with exec_poll", job.id, job.pid),
                Some(
                    json!({"job_id": job.id, "pid": job.pid, "state": job.state().as_str(), "cwd": cwd}),
                ),
            ))
        })
    }
}

impl Tool for ExecPollTool {
    fn name(&self) -> &'static str {
        "exec_poll"
    }
    fn description(&self) -> String {
        format!(
            "Fetch a background job's state and any output produced since your last poll. Pass the \
             `job_id` from exec_start and, to get only new output, the `stdout_since`/`stderr_since` \
             byte offsets returned by the previous poll. When `state` is `exited` or `killed`, \
             `exit_code` is set; output from the command itself is complete (a background process \
             it started may still append to the log). Each poll returns at most \
             `max_bytes` per stream (default 256 KiB, max 1 MiB); when `stdout_more`/`stderr_more` \
             is true, poll again from `stdout_next`/`stderr_next`. Output is read from the job's log \
             files (never truncated); terminal-job metadata is retained ~10 min, and the log files \
             themselves persist on disk ({}/<job_id>.out/.err).",
            self.0.log_dir.display()
        )
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": {"type": "string"},
                "stdout_since": {"type": "integer", "description": "Byte offset from a prior poll; omit for all stdout."},
                "stderr_since": {"type": "integer", "description": "Byte offset from a prior poll; omit for all stderr."},
                "max_bytes": {"type": "integer", "description": "Per-stream cap for this poll. Default 262144, max 1048576."},
            },
            "required": ["job_id"],
        })
    }
    fn call<'a>(&'a self, args: &'a Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let id = job_id(args)?;
            let Some(job) = self.0.get(id) else {
                return Err(ToolFail::Tool(format!(
                    "unknown job_id: {id} (its metadata may have been garbage-collected; the log files may still exist under {}/)",
                    self.0.log_dir.display()
                )));
            };
            // Read state *before* the logs: a job seen as terminal has all its output on disk.
            let state = job.state();
            let code = job.exit_code();
            let since = |k| arg_i64(args, k).unwrap_or(0).max(0) as u64;
            let max = arg_i64(args, "max_bytes")
                .map(|m| m.max(1) as u64)
                .unwrap_or(POLL_DEFAULT_BYTES)
                .min(POLL_MAX_BYTES);
            let finished = state != JobState::Running;
            let out = read_log(&job.out_path, since("stdout_since"), max, finished);
            let err = read_log(&job.err_path, since("stderr_since"), max, finished);
            let (out_next, err_next, out_more, err_more) = (out.next, err.next, out.more, err.more);
            let stdout = String::from_utf8_lossy(&out.bytes).into_owned();
            let stderr = String::from_utf8_lossy(&err.bytes).into_owned();

            let mut s = Map::new();
            s.insert("job_id".into(), json!(id));
            s.insert("state".into(), json!(state.as_str()));
            s.insert("stdout".into(), json!(stdout));
            s.insert("stderr".into(), json!(stderr));
            s.insert("stdout_next".into(), json!(out_next));
            s.insert("stderr_next".into(), json!(err_next));
            s.insert("stdout_more".into(), json!(out_more));
            s.insert("stderr_more".into(), json!(err_more));
            s.insert("out_path".into(), json!(job.out_path));
            s.insert("err_path".into(), json!(job.err_path));
            if let Some(c) = code {
                s.insert("exit_code".into(), json!(c));
            }

            let mut text = stdout.clone();
            if !stderr.is_empty() {
                text += &format!(
                    "{}[stderr]\n{stderr}",
                    if text.is_empty() { "" } else { "\n" }
                );
            }
            let mut trailer = match code {
                Some(c) => format!("[{} exit {c}", state.as_str()),
                None => format!("[{}", state.as_str()),
            };
            if out_more || err_more {
                trailer += &format!("; more output: poll again with stdout_since={out_next} stderr_since={err_next}");
            }
            trailer += "]";
            text += &format!("{}{trailer}", if text.is_empty() { "" } else { "\n" });
            Ok(ToolResult::text(text, Some(Value::Object(s))))
        })
    }
}

impl Tool for ExecListTool {
    fn name(&self) -> &'static str {
        "exec_list"
    }
    fn description(&self) -> String {
        "List background jobs: all currently running ones plus the 10 most recently finished. \
         For each: job_id, state (running/exited/killed), pid, exit_code, command, cwd, \
         started_at, finished_at, and current stdout/stderr byte sizes. Use it to recover a \
         forgotten job_id or to see what is running; then `exec_poll` a specific job."
            .into()
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn call<'a>(&'a self, _args: &'a Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let jobs = self.0.list(10);
            let arr: Vec<Value> = jobs
                .iter()
                .map(|j| {
                    let mut o = json!({
                        "job_id": j.id, "state": j.state().as_str(), "pid": j.pid,
                        "command": j.command, "cwd": j.cwd, "started_at": iso(j.started_at),
                        "stdout_bytes": file_size(&j.out_path), "stderr_bytes": file_size(&j.err_path),
                    });
                    if let Some(c) = j.exit_code() {
                        o["exit_code"] = json!(c);
                    }
                    if let Some(t) = j.finished_at() {
                        o["finished_at"] = json!(iso(t));
                    }
                    o
                })
                .collect();
            let running = jobs
                .iter()
                .filter(|j| j.state() == JobState::Running)
                .count();
            let text = if jobs.is_empty() {
                "no jobs".to_string()
            } else {
                let lines: Vec<String> = jobs
                    .iter()
                    .map(|j| {
                        let ex = j
                            .exit_code()
                            .map(|c| format!(" exit {c}"))
                            .unwrap_or_default();
                        format!(
                            "{}  {}{ex}  pid {}  {}",
                            j.id,
                            j.state().as_str(),
                            j.pid,
                            j.command
                        )
                    })
                    .collect();
                format!(
                    "{}\n[{running} running, {} recent finished]",
                    lines.join("\n"),
                    jobs.len() - running
                )
            };
            Ok(ToolResult::text(text, Some(json!({"jobs": arr}))))
        })
    }
}

impl Tool for ExecCancelTool {
    fn name(&self) -> &'static str {
        "exec_cancel"
    }
    fn description(&self) -> String {
        "Stop a background job by signalling its process group, or drop a finished job to free \
         its buffers. `signal` is `KILL` (default, immediate) or `TERM` (let it clean up). \
         Poll once more afterwards to read the final output."
            .into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": {"type": "string"},
                "signal": {"type": "string", "enum": ["KILL", "TERM"], "default": "KILL"},
            },
            "required": ["job_id"],
        })
    }
    fn call<'a>(&'a self, args: &'a Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let id = job_id(args)?;
            let Some(job) = self.0.get(id) else {
                return Err(ToolFail::Tool(format!("unknown job_id: {id}")));
            };
            let state = job.state();
            if state != JobState::Running {
                self.0.drop_job(id);
                return Ok(ToolResult::text(
                    format!(
                        "job {id} already {} (exit {}); dropped",
                        state.as_str(),
                        job.exit_code().unwrap_or(-1)
                    ),
                    Some(json!({"job_id": id, "state": state.as_str(), "dropped": true})),
                ));
            }
            let term = arg_str(args, "signal") == Some("TERM");
            let ok = self
                .0
                .cancel(id, if term { libc::SIGTERM } else { libc::SIGKILL });
            let name = if term { "TERM" } else { "KILL" };
            Ok(ToolResult::text(
                if ok {
                    format!("signalled {id} ({name}); poll for final output")
                } else {
                    format!("could not signal {id}")
                },
                Some(json!({"job_id": id, "signalled": ok, "state": state.as_str()})),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn wait_done(reg: &JobRegistry, id: &str) -> Arc<Job> {
        for _ in 0..100 {
            let j = reg.get(id).unwrap();
            if j.state() != JobState::Running {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("job did not finish");
    }

    #[tokio::test]
    async fn start_poll_incrementally_and_finish() {
        let dir = tempfile::tempdir().unwrap();
        let reg = JobRegistry::new(dir.path().into());
        let env = build_env(&json!({}));
        let job = reg
            .start(
                "printf abc; printf def >&2; exit 4",
                "/",
                &env,
                Duration::ZERO,
            )
            .unwrap();
        let j = wait_done(&reg, &job.id).await;
        assert_eq!((j.state(), j.exit_code()), (JobState::Exited, Some(4)));
        let rd = |p, from| {
            let c = read_log(p, from, POLL_DEFAULT_BYTES, true);
            (c.bytes, c.next)
        };
        assert_eq!(rd(&j.out_path, 0), (b"abc".to_vec(), 3));
        assert_eq!(rd(&j.out_path, 1), (b"bc".to_vec(), 3));
        assert_eq!(rd(&j.out_path, 3), (vec![], 3));
        assert_eq!(rd(&j.err_path, 0).0, b"def");
    }

    #[test]
    fn polls_are_capped_and_never_split_a_character() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.out");
        std::fs::write(&p, "ab中文").unwrap(); // a b [e4 b8 ad] [e6 96 87]
                                               // Cap lands inside 中 (bytes 2..5): keep only "ab" and resume at 2.
        let c = read_log(&p, 0, 4, false);
        assert_eq!((c.bytes.as_slice(), c.next, c.more), (&b"ab"[..], 2, true));
        let c = read_log(&p, 2, 4, false);
        assert_eq!(
            (String::from_utf8(c.bytes).unwrap().as_str(), c.next, c.more),
            ("中", 5, true)
        );
        let c = read_log(&p, 5, 64, true);
        assert_eq!(
            (String::from_utf8(c.bytes).unwrap().as_str(), c.next, c.more),
            ("文", 8, false)
        );
        // A finished file ending in a broken byte is still returned, so polling can finish.
        std::fs::write(&p, b"ok\xe4").unwrap();
        assert_eq!(read_log(&p, 0, 64, true).next, 3);
    }

    #[tokio::test]
    async fn huge_timeouts_are_clamped_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let t = ExecStartTool(JobRegistry::new(dir.path().into()));
        let r = t
            .call(&json!({"command": "true", "timeout_secs": 1e20, "cwd": "/"}))
            .await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn a_background_child_of_a_job_survives_and_its_output_is_logged() {
        let dir = tempfile::tempdir().unwrap();
        let reg = JobRegistry::new(dir.path().into());
        let job = reg
            .start(
                "(sleep 0.6; echo late) &",
                "/",
                &build_env(&json!({})),
                Duration::ZERO,
            )
            .unwrap();
        let j = wait_done(&reg, &job.id).await;
        assert_eq!(j.state(), JobState::Exited);
        for _ in 0..40 {
            if read_log(&j.out_path, 0, 64, true).bytes == b"late\n" {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("background output was cut off (its writer died of SIGPIPE?)");
    }

    #[tokio::test]
    async fn a_background_child_does_not_keep_the_job_running() {
        let dir = tempfile::tempdir().unwrap();
        let reg = JobRegistry::new(dir.path().into());
        let job = reg
            .start(
                "echo up; sleep 30 &",
                "/",
                &build_env(&json!({})),
                Duration::ZERO,
            )
            .unwrap();
        let j = wait_done(&reg, &job.id).await; // panics after ~5 s if still running
        assert_eq!((j.state(), j.exit_code()), (JobState::Exited, Some(0)));
        assert_eq!(read_log(&j.out_path, 0, 64, true).bytes, b"up\n");
    }

    #[tokio::test]
    async fn cancel_and_timeout_mark_killed() {
        let dir = tempfile::tempdir().unwrap();
        let reg = JobRegistry::new(dir.path().into());
        let env = build_env(&json!({}));
        let a = reg
            .start("sleep 30 & sleep 30", "/", &env, Duration::ZERO)
            .unwrap();
        assert!(reg.cancel(&a.id, libc::SIGTERM));
        let a = wait_done(&reg, &a.id).await;
        assert_eq!(
            (a.state(), a.exit_code()),
            (JobState::Killed, Some(128 + libc::SIGTERM))
        );

        let b = reg
            .start("sleep 30", "/", &env, Duration::from_millis(200))
            .unwrap();
        assert_eq!(wait_done(&reg, &b.id).await.state(), JobState::Killed);
        assert_eq!(reg.list(10).len(), 2);
        assert!(reg.drop_job(&b.id));
        assert!(reg.get(&b.id).is_none());
    }
}
