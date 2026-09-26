import Foundation

/// Which tools a connection may see and call. Chosen by *this* side — the Mac is
/// the MCP server, so scoping is a per-connection tool list here, never MCP
/// parsing in a proxy (instance-mcp `docs/adr/reverse-attach.md`).
///
/// `owner` is the logged-in human's own CLI: everything. `sandbox` is an agent in
/// an `openab-pty` session that a human lent this Mac to: the see→act→see loop
/// (screenshot / mouse / key / osascript / sys_info) but **no `exec*`**, because
/// `exec` is a full shell as the desktop user and the agent already has a shell
/// of its own in the sandbox. Widening later (an approval hop in Connect) is a
/// new profile, not an edit to this one.
public enum ToolProfile: String, Codable, Sendable, CaseIterable {
    case owner
    case sandbox

    /// Match is on the tool name.
    public func allows(_ toolName: String) -> Bool {
        switch self {
        case .owner: return true
        case .sandbox:
            if toolName.hasPrefix("exec") { return false }
            if toolName.hasPrefix("browser_") { return Self.sandboxBrowserTools.contains(toolName) }
            return true
        }
    }

    /// Playwright MCP tools a lent sandbox may use: navigate, read, interact.
    /// Not: arbitrary code in the browser process, the filesystem (upload / PDF),
    /// raw network inspection, dialogs, media emulation, closing the browser.
    /// Anything Playwright adds later is denied until listed here.
    public static let sandboxBrowserTools: Set<String> = [
        "browser_navigate", "browser_navigate_back", "browser_snapshot", "browser_find",
        "browser_click", "browser_type", "browser_fill_form", "browser_press_key", "browser_hover",
        "browser_select_option", "browser_wait_for", "browser_tabs", "browser_take_screenshot",
        "browser_console_messages", "browser_resize", "browser_evaluate",
    ]
}

extension MCPServer {
    /// The same server with its tool list narrowed to `profile`. `tools/list`
    /// omits the rest and `tools/call` on an omitted tool is an *unknown tool*
    /// error, indistinguishable from a tool that never existed — the agent is not
    /// told there is something it is not allowed to have.
    public func scoped(to profile: ToolProfile, instructions: String? = nil) -> MCPServer {
        MCPServer(
            name: serverName,
            version: serverVersion,
            instructions: instructions ?? self.instructions,
            tools: allTools.filter { profile.allows($0.name) },
            upstreams: upstreams,
            upstreamFilter: profile
        )
    }
}
