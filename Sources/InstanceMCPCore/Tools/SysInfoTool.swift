import Foundation
import CoreGraphics
import ScreenCaptureKit

/// Cheap orientation call for a model that just connected: what machine is this, what
/// can I see, which permissions are missing. No side effects.
public struct SysInfoTool: Tool {
    public let name = "sys_info"
    public let description = """
        Describe this Mac: hostname, macOS version, hardware, logged-in GUI user, displays, \
        Tailscale addresses, and which TCC permissions (Screen Recording, Accessibility, Full Disk \
        Access) the agent currently holds. Call this first to learn what the other tools can do here.
        """
    public let inputSchema: JSONValue = ["type": "object", "properties": [:]]

    public let agentVersion: String
    public init(agentVersion: String) { self.agentVersion = agentVersion }

    public func call(arguments: JSONValue) async throws -> ToolResult {
        let pi = ProcessInfo.processInfo
        let os = pi.operatingSystemVersion
        let host = Host.current().localizedName ?? pi.hostName

        var hw: [String: JSONValue] = [:]
        hw["model"] = .string(sysctlString("hw.model") ?? "?")
        hw["chip"] = .string(sysctlString("machdep.cpu.brand_string") ?? "?")
        hw["cores"] = .number(Double(pi.activeProcessorCount))
        hw["memory_gb"] = .number((Double(pi.physicalMemory) / 1_073_741_824).rounded())

        // Displays (CoreGraphics; works without Screen Recording).
        var ids = [CGDirectDisplayID](repeating: 0, count: 16); var count: UInt32 = 0
        CGGetActiveDisplayList(16, &ids, &count)
        let displays: [JSONValue] = ids.prefix(Int(count)).map { id in
            let b = CGDisplayBounds(id)
            let mode = CGDisplayCopyDisplayMode(id)
            return [
                "id": .number(Double(id)),
                "main": .bool(id == CGMainDisplayID()),
                "origin": ["x": .number(b.origin.x), "y": .number(b.origin.y)],
                "points": ["width": .number(b.width), "height": .number(b.height)],
                "pixels": ["width": .number(Double(mode?.pixelWidth ?? 0)), "height": .number(Double(mode?.pixelHeight ?? 0))],
            ]
        }

        // TCC. Public non-prompting checks for screen/AX; FDA is an actual
        // open+close of a protected database (no content read).
        let permissions = PermissionProbe.current()
        let screenRecording = permissions.screenRecording.isGranted
        let accessibility = permissions.accessibility.isGranted
        let fullDiskAccess = permissions.fullDiskAccess.isGranted
        let fullDiskAccessState: String
        switch permissions.fullDiskAccess {
        case .granted: fullDiskAccessState = "granted"
        case .denied: fullDiskAccessState = "denied"
        case .unknown: fullDiskAccessState = "unknown"
        }

        let console = consoleUser()
        let tail = tailnetAddresses()

        let structured: JSONValue = [
            "agent": ["name": "oab-instance-mcp", "version": .string(agentVersion), "pid": .number(Double(pi.processIdentifier))],
            "host": .string(host),
            "os": .string("macOS \(os.majorVersion).\(os.minorVersion).\(os.patchVersion)"),
            "hardware": .object(hw),
            "user": .string(NSUserName()),
            "console_user": .string(console ?? "none"),
            "gui_session": .bool(console != nil && console == NSUserName()),
            "displays": .array(displays),
            "tailscale_ips": .array(tail.map { .string($0) }),
            "permissions": [
                "screen_recording": .bool(screenRecording),
                "accessibility": .bool(accessibility),
                "full_disk_access": .bool(fullDiskAccess),
                "full_disk_access_state": .string(fullDiskAccessState),
            ],
            "uptime_secs": .number(pi.systemUptime.rounded()),
        ]

        var lines: [String] = []
        lines.append("\(host) — macOS \(os.majorVersion).\(os.minorVersion).\(os.patchVersion), \(hw["model"]?.stringValue ?? "?"), \(hw["chip"]?.stringValue ?? "?"), \(Int(hw["memory_gb"]?.doubleValue ?? 0)) GB")
        lines.append("user \(NSUserName()); console user \(console ?? "none")\(console == NSUserName() ? " (agent is in the GUI session)" : " (agent NOT in GUI session — screenshot/input will fail)")")
        lines.append("displays: " + displays.map { d in
            "\(Int(d["points"]?["width"]?.doubleValue ?? 0))×\(Int(d["points"]?["height"]?.doubleValue ?? 0))pt\(d["main"]?.boolValue == true ? " (main)" : "")"
        }.joined(separator: ", "))
        lines.append("tailscale: \(tail.isEmpty ? "none" : tail.joined(separator: ", "))")
        lines.append("permissions: screen_recording=\(screenRecording) accessibility=\(accessibility) full_disk_access=\(fullDiskAccessState)")
        if !screenRecording { lines.append("→ screenshot will fail until Screen Recording is granted to oab-instance-mcp") }
        if !accessibility { lines.append("→ mouse/key will fail until Accessibility is granted to oab-instance-mcp") }
        if !fullDiskAccess { lines.append("→ protected Mail/Messages/Safari files will fail until Full Disk Access is granted to oab-instance-mcp") }
        lines.append("agent \(agentVersion)")
        return ToolResult(content: [.text(lines.joined(separator: "\n"))], structured: structured)
    }

    func sysctlString(_ name: String) -> String? {
        var size = 0
        guard sysctlbyname(name, nil, &size, nil, 0) == 0, size > 0 else { return nil }
        var buf = [CChar](repeating: 0, count: size)
        guard sysctlbyname(name, &buf, &size, nil, 0) == 0 else { return nil }
        return String(cString: buf)
    }

    func consoleUser() -> String? {
        var st = stat()
        guard stat("/dev/console", &st) == 0, let pw = getpwuid(st.st_uid) else { return nil }
        return String(cString: pw.pointee.pw_name)
    }

    /// Interfaces with a CGNAT (100.64/10) v4 or fd7a:115c:a1e0::/48 v6 address — Tailscale's ranges.
    func tailnetAddresses() -> [String] {
        var out: [String] = []
        var ifap: UnsafeMutablePointer<ifaddrs>? = nil
        guard getifaddrs(&ifap) == 0, let first = ifap else { return [] }
        defer { freeifaddrs(ifap) }
        for p in sequence(first: first, next: { $0.pointee.ifa_next }) {
            guard let sa = p.pointee.ifa_addr else { continue }
            var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            let len = socklen_t(sa.pointee.sa_family == AF_INET ? MemoryLayout<sockaddr_in>.size : MemoryLayout<sockaddr_in6>.size)
            guard sa.pointee.sa_family == AF_INET || sa.pointee.sa_family == AF_INET6 else { continue }
            guard getnameinfo(sa, len, &host, socklen_t(host.count), nil, 0, NI_NUMERICHOST) == 0 else { continue }
            let s = String(cString: host)
            if s.hasPrefix("100.") , let second = s.split(separator: ".").dropFirst().first, let n = Int(second), (64...127).contains(n) {
                out.append(s)
            } else if s.lowercased().hasPrefix("fd7a:115c:a1e0") {
                out.append(s.components(separatedBy: "%").first ?? s)
            }
        }
        return out
    }
}
