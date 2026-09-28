//! oab-instance-mcp (Rust) — MCP server exposing this machine to a coding CLI or agent on
//! the tailnet. Same flags, auth and wire contract as the Swift build; see `README.md`.

mod attach;
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
use tools::screen::{KeyTool, MouseTool, ScreenshotTool};
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
    attach: bool,
    desktop: bool,
}

fn usage() -> ! {
    println!(
        "oab-instance-mcp {VERSION} (Rust, {os}) — MCP server exposing this machine (exec / screenshot / mouse / key / sys_info)

USAGE: oab-instance-mcp [--host 127.0.0.1] [--port 8795] [--path /mcp]
                        [--allow-login <email>]... [--token <str> | --token-file <path>]
                        [--insecure-local] [--quiet] [--no-attach] [--no-desktop]

Auth (at least one required unless --insecure-local):
  --allow-login   Tailscale login (from `tailscale serve`'s Tailscale-User-Login header). Repeatable.
  --token         Shared bearer token; clients send `Authorization: Bearer <token>`.
  --token-file    Read the token from a file (trailing newline stripped).
  --insecure-local  Allow unauthenticated requests that arrive on loopback *without*
                    Tailscale headers. For local debugging only.

  --no-attach     Disable the reverse-attach plane (POST/GET /attach, DELETE /attach/{{id}}):
                  the human-credentialed endpoint through which Connect / Remote lends this
                  machine to one openab-pty session (this machine dials the pod).

  --no-desktop    Do not offer screenshot / mouse / key even inside a desktop session.
                  (They are offered automatically when the process sees a graphical session.)

Not yet in the Rust build: --upstream, --menu-bar.

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
        attach: true,
        desktop: true,
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
            "--no-attach" => o.attach = false,
            "--no-desktop" => o.desktop = false,
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

fn instructions(desktop: bool) -> String {
    let mut s = format!(
        "You are operating a real {os} machine ({host}) as its logged-in user; a human may be using it. \
         `exec` is a plain `{shell} -c` shell as that user and is the right tool for files and commands; \
         for long jobs (builds) that outlive one request use `exec_start` then `exec_poll`/`exec_cancel`. \
         Call `sys_info` first to learn which tools this build offers here.",
        os = platform::backend().os_label(),
        host = platform::backend().hostname(),
        shell = platform::backend().shell().0,
    );
    if desktop {
        s.push_str(
            " For the desktop, work in a see→act→see loop: `screenshot`, decide, `mouse`/`key`, then \
             `screenshot` again to confirm — never assume an action landed. `screenshot` at the default \
             scale 1.0 returns one pixel per display point and `mouse` takes display points, so image \
             pixel (x,y) is the click target. To read small text pass `region: {x,y,width,height}` with \
             `scale: 2`; the crop's pixel (px,py) is point (region.x + px/2, region.y + py/2). Shortcuts \
             use ctrl on Linux. The first desktop call may wait for a human to approve remote control \
             on this machine's screen.",
        );
    }
    s
}

#[tokio::main]
async fn main() {
    let opts = parse_args();
    // One TLS crypto provider for every rustls user (wss:// dial, https:// mint).
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
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
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        sys_info.clone(),
        Arc::new(ExecTool),
        Arc::new(ExecStartTool(jobs.clone())),
        Arc::new(ExecPollTool(jobs.clone())),
        Arc::new(ExecListTool(jobs.clone())),
        Arc::new(ExecCancelTool(jobs)),
    ];
    let desktop = if opts.desktop {
        platform::backend().desktop()
    } else {
        None
    };
    if let Some(d) = &desktop {
        tools.push(Arc::new(ScreenshotTool(d.clone())));
        tools.push(Arc::new(MouseTool(d.clone())));
        tools.push(Arc::new(KeyTool(d.clone())));
    }
    let server = McpServer::new(
        "oab-instance-mcp",
        VERSION,
        Some(instructions(desktop.is_some())),
        tools,
    );
    let _ = sys_info.tool_names.set(server.tool_names());
    let tool_list = server.tool_names().join(",");
    let attach = opts
        .attach
        .then(|| attach::AttachManager::new(server.clone(), None));
    let endpoint = http::Endpoint::new(opts.path.clone(), server, auth, attach);

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
        "oab-instance-mcp {VERSION} (rust/{}) starting on http://{addr}{} auth=[logins:{} token:{} insecure-local:{}] attach={} tools=[{tool_list}]",
        std::env::consts::OS,
        opts.path,
        opts.allow_logins.join(","),
        opts.token.is_some(),
        opts.insecure_local,
        opts.attach,
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
