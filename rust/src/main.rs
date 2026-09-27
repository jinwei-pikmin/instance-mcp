//! oab-instance-mcp (Rust) — MCP server exposing this machine to a coding CLI or agent on
//! the tailnet. Same flags, auth and wire contract as the Swift build; see `README.md`.

mod auth;
mod http;
mod mcp;
mod platform;
mod tools;

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use auth::AuthPolicy;
use mcp::{McpServer, Tool};
use tools::exec::ExecTool;
use tools::jobs::{ExecCancelTool, ExecListTool, ExecPollTool, ExecStartTool, JobRegistry};
use tools::sysinfo::SysInfoTool;

const VERSION: &str = env!("CARGO_PKG_VERSION");

static QUIET: OnceLock<bool> = OnceLock::new();

pub fn log(msg: &str) {
    if !QUIET.get().copied().unwrap_or(false) {
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        eprintln!("{ts} {msg}");
    }
}

struct Options {
    host: String,
    port: u16,
    path: String,
    allow_logins: Vec<String>,
    token: Option<String>,
    token_file: Option<String>,
    insecure_local: bool,
    quiet: bool,
}

fn usage() -> ! {
    println!(
        "oab-instance-mcp {VERSION} (Rust, {os}) — MCP server exposing this machine (exec / sys_info)

USAGE: oab-instance-mcp [--host 127.0.0.1] [--port 8795] [--path /mcp]
                        [--allow-login <email>]... [--token <str> | --token-file <path>]
                        [--insecure-local] [--quiet]

Auth (at least one required unless --insecure-local):
  --allow-login   Tailscale login (from `tailscale serve`'s Tailscale-User-Login header). Repeatable.
  --token         Shared bearer token; clients send `Authorization: Bearer <token>`.
  --token-file    Read the token from a file (trailing newline stripped).
  --insecure-local  Allow unauthenticated requests that arrive on loopback *without*
                    Tailscale headers. For local debugging only.

Not yet in the Rust build: --upstream, reverse attach (/attach), --menu-bar.

Run it as a systemd *user* service in the logged-in user's session, bound to loopback,
behind `tailscale serve` (see rust/deploy/).",
        os = platform::backend().os_label()
    );
    std::process::exit(64)
}

fn parse_args() -> Options {
    let mut o = Options {
        host: "127.0.0.1".into(),
        port: 8795,
        path: "/mcp".into(),
        allow_logins: vec![],
        token: None,
        token_file: None,
        insecure_local: false,
        quiet: false,
    };
    let mut args = std::env::args().skip(1);
    let next = |flag: &str, args: &mut dyn Iterator<Item = String>| {
        args.next().unwrap_or_else(|| {
            eprintln!("missing value for {flag}");
            usage()
        })
    };
    while let Some(a) = args.next() {
        match a.as_str() {
            "--host" => o.host = next(&a, &mut args),
            "--port" => {
                o.port = next(&a, &mut args).parse().unwrap_or_else(|_| {
                    eprintln!("bad port");
                    usage()
                })
            }
            "--path" => o.path = next(&a, &mut args),
            "--allow-login" => o.allow_logins.push(next(&a, &mut args)),
            "--token" => o.token = Some(next(&a, &mut args)),
            "--token-file" => o.token_file = Some(next(&a, &mut args)),
            "--insecure-local" => o.insecure_local = true,
            "--quiet" => o.quiet = true,
            // Accepted so shared launch scripts work; the attach plane does not exist here yet.
            "--no-attach" => {}
            "--upstream" | "--menu-bar" | "--public-url" => {
                eprintln!("{a} is not supported by the Rust build yet");
                std::process::exit(64)
            }
            "--version" => {
                println!("{VERSION}");
                std::process::exit(0)
            }
            "-h" | "--help" => usage(),
            _ => {
                eprintln!("unknown flag {a}");
                usage()
            }
        }
    }
    if let Some(f) = &o.token_file {
        let path = if let Some(rest) = f.strip_prefix("~/") {
            format!("{}/{rest}", tools::exec::home_dir())
        } else {
            f.clone()
        };
        let t = std::fs::read_to_string(&path).unwrap_or_else(|_| {
            eprintln!("cannot read --token-file {f}");
            std::process::exit(66)
        });
        let t = t.trim().to_string();
        if t.is_empty() {
            eprintln!("--token-file is empty");
            std::process::exit(66)
        }
        o.token = Some(t);
    }
    o
}

fn instructions() -> String {
    format!(
        "You are operating a real {os} machine ({host}) as its logged-in user; a human may be using it. \
         `exec` is a plain `{shell} -c` shell as that user and is the right tool for files and commands; \
         for long jobs (builds) that outlive one request use `exec_start` then `exec_poll`/`exec_cancel`. \
         Call `sys_info` first to learn which tools this build offers here.",
        os = platform::backend().os_label(),
        host = platform::backend().hostname(),
        shell = platform::backend().shell().0,
    )
}

#[tokio::main]
async fn main() {
    let opts = parse_args();
    let _ = QUIET.set(opts.quiet);

    let auth = AuthPolicy::new(
        opts.allow_logins.clone(),
        opts.token.clone(),
        opts.insecure_local,
    );
    if let Err(e) = auth.validate() {
        eprintln!("{e}");
        std::process::exit(64)
    }

    let jobs = JobRegistry::new(platform::backend().job_log_dir());
    let sys_info = Arc::new(SysInfoTool {
        agent_version: VERSION,
        tool_names: OnceLock::new(),
    });
    let tools: Vec<Arc<dyn Tool>> = vec![
        sys_info.clone(),
        Arc::new(ExecTool),
        Arc::new(ExecStartTool(jobs.clone())),
        Arc::new(ExecPollTool(jobs.clone())),
        Arc::new(ExecListTool(jobs.clone())),
        Arc::new(ExecCancelTool(jobs)),
    ];
    let server = McpServer::new("oab-instance-mcp", VERSION, Some(instructions()), tools);
    let _ = sys_info.tool_names.set(server.tool_names());
    let tool_list = server.tool_names().join(",");
    let endpoint = http::Endpoint::new(opts.path.clone(), server, auth);

    let addr: SocketAddr = match format!("{}:{}", opts.host, opts.port).parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("bad --host/--port: {e}");
            std::process::exit(64)
        }
    };
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("failed to start listener: {e}");
            std::process::exit(2)
        }
    };
    log(&format!(
        "oab-instance-mcp {VERSION} (rust/{}) starting on http://{addr}{} auth=[logins:{} token:{} insecure-local:{}] tools=[{tool_list}]",
        std::env::consts::OS,
        opts.path,
        opts.allow_logins.join(","),
        opts.token.is_some(),
        opts.insecure_local,
    ));

    let app = http::router(endpoint).into_make_service_with_connect_info::<SocketAddr>();
    let shutdown = async {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        tokio::select! {
            _ = term.recv() => log("SIGTERM, exiting"),
            _ = tokio::signal::ctrl_c() => log("SIGINT, exiting"),
        }
    };
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
    {
        eprintln!("server error: {e}");
        std::process::exit(2)
    }
}
