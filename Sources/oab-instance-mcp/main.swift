import AppKit
import ApplicationServices
import CoreGraphics
import Foundation
import InstanceMCPCore

let version = "0.6.0"

struct Options {
    var host = "127.0.0.1"
    var port: UInt16 = 8795
    var path = "/mcp"
    var allowLogins: Set<String> = []
    var token: String? = nil
    var tokenFile: String? = nil
    var insecureLocal = false
    var quiet = false
    var menuBar = false
    var publicURL: String? = nil
    var attach = true
    /// name=url pairs; each is a loopback MCP server whose tools are re-served.
    var upstreams: [(String, URL)] = []
}

func usage() -> Never {
    print("""
    oab-instance-mcp \(version) — MCP server exposing this Mac (exec / screenshot / mouse / key / osascript / sys_info)

    USAGE: oab-instance-mcp [--host 127.0.0.1] [--port 8795] [--path /mcp]
                        [--allow-login <email>]... [--token <str> | --token-file <path>]
                        [--insecure-local] [--quiet] [--menu-bar] [--public-url <https://…/mcp>]

    Auth (at least one required unless --insecure-local):
      --allow-login   Tailscale login (from `tailscale serve`'s Tailscale-User-Login header). Repeatable.
      --token         Shared bearer token; clients send `Authorization: Bearer <token>`.
      --token-file    Read the token from a file (trailing newline stripped).
      --insecure-local  Allow unauthenticated requests that arrive on loopback *without*
                        Tailscale headers. For local debugging only.

      --upstream <name=url>  Re-serve the tools of a loopback MCP server (e.g. the Playwright MCP
                      at browser=http://127.0.0.1:8794/mcp) under this daemon, subject to the
                      connection's tool profile. Repeatable. Tools appear with the upstream's
                      own names; sandbox sees an allowlisted subset of browser_*.
      --no-attach     Disable the reverse-attach plane (POST/GET /attach, DELETE /attach/{id}):
                      the human-credentialed endpoint through which Connect / Remote lends this
                      Mac to one openab-pty session (this Mac dials the pod; see the ADR).
      --menu-bar      Show a status item in the menu bar (permissions, activity, restart/quit).
      --public-url    The URL clients use (shown/copied from the menu); defaults to the local one.

    Run it as a LaunchAgent in the logged-in user's GUI session (gui/<uid>), not a
    LaunchDaemon — screenshot and input tools need the Aqua session and TCC grants.
    """)
    exit(64)
}

var opts = Options()
var args = Array(CommandLine.arguments.dropFirst())
func next(_ flag: String) -> String {
    guard !args.isEmpty else { fputs("missing value for \(flag)\n", stderr); usage() }
    return args.removeFirst()
}
while !args.isEmpty {
    let a = args.removeFirst()
    switch a {
    case "--host": opts.host = next(a)
    case "--port":
        guard let p = UInt16(next(a)) else { fputs("bad port\n", stderr); usage() }
        opts.port = p
    case "--path": opts.path = next(a)
    case "--allow-login": opts.allowLogins.insert(next(a))
    case "--token": opts.token = next(a)
    case "--token-file": opts.tokenFile = next(a)
    case "--insecure-local": opts.insecureLocal = true
    case "--quiet": opts.quiet = true
    case "--menu-bar": opts.menuBar = true
    case "--public-url": opts.publicURL = next(a)
    case "--no-attach": opts.attach = false
    case "--upstream":
        let v = next(a)
        guard let eq = v.firstIndex(of: "="), let u = URL(string: String(v[v.index(after: eq)...])),
              u.host != nil else { fputs("--upstream wants name=http://host:port/path\n", stderr); usage() }
        opts.upstreams.append((String(v[..<eq]), u))
    case "--version": print(version); exit(0)
    case "-h", "--help": usage()
    default: fputs("unknown flag \(a)\n", stderr); usage()
    }
}

if let f = opts.tokenFile {
    guard let s = try? String(contentsOfFile: (f as NSString).expandingTildeInPath, encoding: .utf8) else {
        fputs("cannot read --token-file \(f)\n", stderr); exit(66)
    }
    let t = s.trimmingCharacters(in: .whitespacesAndNewlines)
    guard !t.isEmpty else { fputs("--token-file is empty\n", stderr); exit(66) }
    opts.token = t
}

let auth = AuthPolicy(allowedLogins: opts.allowLogins, bearerToken: opts.token, allowLocalUnauthenticated: opts.insecureLocal)
do { try auth.validate() } catch { fputs("\(error)\n", stderr); exit(64) }

let fmt = ISO8601DateFormatter()
nonisolated(unsafe) var statusItem: StatusItemController? = nil
let log: @Sendable (String) -> Void = { msg in
    if !opts.quiet {
        FileHandle.standardError.write(Data("\(fmt.string(from: Date())) \(msg)\n".utf8))
    }
    if statusItem != nil {
        DispatchQueue.main.async { MainActor.assumeIsolated { statusItem?.observe(msg) } }
    }
}

let server = MCPServer(
    name: "oab-instance-mcp",
    version: version,
    instructions: """
        You are operating a real Mac (\(Host.current().localizedName ?? "unknown")) through its logged-in \
        desktop session; a human may be watching the screen. Work in a see→act→see loop: `screenshot`, \
        decide, `mouse`/`key`/`osascript`, then `screenshot` again to confirm — never assume an action landed.
        Coordinates: `screenshot` at the default scale 1.0 returns one pixel per display point, and `mouse` \
        takes display points, so image pixel (x,y) is the click target. To read small text (menu bar, dialogs) \
        pass `region: {x,y,width,height}` with `scale: 2`; the crop's pixel (px,py) is point \
        (region.x + px/2, region.y + py/2). Prefer `osascript` over pixel-hunting for scriptable apps \
        (activate, quit, window titles, Safari URLs). If `osascript` times out, a permission dialog is \
        probably showing: screenshot it and click Allow. `exec` is a plain `zsh -f` shell as the desktop user \
        (add `/opt/homebrew/bin` to PATH via `env` if needed) and is the right tool for files and commands; \
        for long jobs (builds) that outlive one request use `exec_start` then `exec_poll`/`exec_cancel`. \
        Call `sys_info` when unsure which permissions or displays exist.\(opts.upstreams.isEmpty ? "" : """
         A browser is available through the `browser_*` tools (Playwright, a real window on this Mac's \
        desktop): prefer `browser_navigate` + `browser_snapshot` (accessibility tree as text) to read a \
        page, and `browser_click` / `browser_type` / `browser_fill_form` to act — they are exact and need \
        no screenshots. Fall back to `screenshot` only for things outside the browser.
        """)
        """,
    tools: [SysInfoTool(agentVersion: version), ExecTool(), ExecStartTool(), ExecPollTool(), ExecListTool(), ExecCancelTool(), ScreenshotTool(), MouseTool(), KeyTool(), OsascriptTool()],
    upstreams: opts.upstreams.map { UpstreamMCP(name: $0.0, url: $0.1, log: log) }
)
let attachManager: AttachManager? = opts.attach ? AttachManager(server: server, log: log) : nil
let endpoint = MCPHTTPEndpoint(path: opts.path, server: server, auth: auth, attach: attachManager, log: log)

// Must be a global: a `let` inside `do {}` is released after the block and the
// listener's [weak self] handlers silently stop accepting connections.
let http: LoopbackHTTPServer
do {
    http = try LoopbackHTTPServer(host: opts.host, port: opts.port, endpoint: endpoint, log: log)
} catch {
    fputs("failed to start listener: \(error)\n", stderr); exit(2)
}
log("oab-instance-mcp \(version) starting on http://\(opts.host):\(opts.port)\(opts.path) " +
    "auth=[logins:\(opts.allowLogins.sorted().joined(separator: ",")) token:\(opts.token != nil) insecure-local:\(opts.insecureLocal)] " +
    "attach=\(opts.attach) upstreams=[\(opts.upstreams.map { $0.0 }.joined(separator: ","))] " +
    "screen_recording=\(CGPreflightScreenCaptureAccess()) accessibility=\(AXIsProcessTrusted())")
http.start()

signal(SIGPIPE, SIG_IGN)
let stop = DispatchSource.makeSignalSource(signal: SIGTERM, queue: .main)
signal(SIGTERM, SIG_IGN)
stop.setEventHandler { log("SIGTERM, exiting"); exit(0) }
stop.resume()

if opts.menuBar {
    let app = NSApplication.shared
    app.setActivationPolicy(.accessory)     // no Dock icon, no main menu; status item only
    let logPath = NSHomeDirectory() + "/Library/Logs/oab-instance-mcp/agent.log"
    let publicURL = opts.publicURL ?? "http://\(opts.host):\(opts.port)\(opts.path)"
    statusItem = MainActor.assumeIsolated {
        StatusItemController(version: version, url: publicURL, logPath: logPath, token: opts.token)
    }
    app.run()
} else {
    RunLoop.main.run()
}
