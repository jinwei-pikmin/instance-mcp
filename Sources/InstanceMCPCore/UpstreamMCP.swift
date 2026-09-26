import Foundation

/// A loopback MCP server whose tools this daemon re-serves under its own roof —
/// first case: `@playwright/mcp` on `127.0.0.1:8794`, so a lent sandbox session
/// gets `browser_*` tools without any path from the pod to the browser (issue #10).
///
/// Streamable HTTP client, request/response only. The upstream's `Mcp-Session-Id`
/// is held here and re-established when the upstream forgets it (400/404). Replies
/// may arrive as `application/json` or as an SSE frame (`data: {…}`); both parsed.
///
/// `Host` is sent as the bare host without the port: Playwright MCP's
/// `--allowed-hosts` compares the header verbatim, and `127.0.0.1:8794` is not on
/// its list while `127.0.0.1` is.
public actor UpstreamMCP {
    public let name: String
    public let url: URL
    /// Prefix every tool name is exposed under. Playwright's tools already carry
    /// `browser_`, so the default is empty; set it for upstreams that do not.
    public let prefix: String
    private let log: @Sendable (String) -> Void
    private let session: URLSession
    private var sessionID: String?
    private var cachedTools: (at: Date, tools: [JSONValue])?
    private let cacheTTL: TimeInterval = 30
    public private(set) var lastError: String?

    public init(name: String, url: URL, prefix: String = "", timeout: TimeInterval = 90,
                log: @escaping @Sendable (String) -> Void = { _ in }) {
        self.name = name; self.url = url; self.prefix = prefix; self.log = log
        let cfg = URLSessionConfiguration.ephemeral
        cfg.timeoutIntervalForRequest = timeout
        self.session = URLSession(configuration: cfg)
    }

    // MARK: public surface

    /// Tool descriptors with `name` prefixed. Empty when the upstream is down —
    /// the merged `tools/list` then simply lacks them (issue #10 acceptance).
    public func tools() async -> [JSONValue] {
        if let c = cachedTools, Date().timeIntervalSince(c.at) < cacheTTL { return c.tools }
        do {
            let r = try await rpc("tools/list", params: nil)
            let list = (r["result"]?["tools"]?.arrayValue ?? []).map { t -> JSONValue in
                guard var o = t.objectValue, let n = o["name"]?.stringValue else { return t }
                o["name"] = .string(prefix + n)
                return .object(o)
            }
            cachedTools = (Date(), list)
            lastError = nil
            return list
        } catch {
            lastError = "\(error)"
            log("upstream \(name): tools/list failed: \(error)")
            cachedTools = (Date(), [])
            return []
        }
    }

    /// Forward a `tools/call`. `name` is the prefixed name the caller used.
    public func call(_ name: String, arguments: JSONValue) async throws -> JSONValue {
        let upstreamName = name.hasPrefix(prefix) ? String(name.dropFirst(prefix.count)) : name
        let r = try await rpc("tools/call", params: ["name": .string(upstreamName), "arguments": arguments])
        if let e = r["error"] { throw JSONRPCError(code: e["code"]?.intValue ?? -32000, message: e["message"]?.stringValue ?? "upstream error") }
        return r["result"] ?? .null
    }

    public func status() async -> JSONValue {
        let n = await tools().count
        return ["name": .string(name), "url": .string(url.absoluteString), "tools": .number(Double(n)),
                "session": .bool(sessionID != nil), "error": lastError.map { .string($0) } ?? .null]
    }

    /// Does not hit the network; for the `sys_info` line.
    public func isKnownHealthy() -> Bool { lastError == nil && (cachedTools?.tools.isEmpty == false) }

    // MARK: wire

    private func rpc(_ method: String, params: JSONValue?) async throws -> JSONValue {
        if sessionID == nil { try await initialize() }
        let (status, body) = try await post(["jsonrpc": "2.0", "id": .number(Double(Int.random(in: 1...1_000_000))), "method": .string(method), "params": params ?? [:]])
        if status == 404 || status == 400 {
            // Upstream lost our session (restart). One re-init, one retry.
            sessionID = nil
            try await initialize()
            let (s2, b2) = try await post(["jsonrpc": "2.0", "id": 1, "method": .string(method), "params": params ?? [:]])
            guard (200..<300).contains(s2) else { throw UpstreamError.http(s2, b2) }
            return try Self.parseBody(b2)
        }
        guard (200..<300).contains(status) else { throw UpstreamError.http(status, body) }
        return try Self.parseBody(body)
    }

    private func initialize() async throws {
        let (status, body, headers) = try await postRaw([
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": ["protocolVersion": "2025-06-18", "capabilities": [:],
                       "clientInfo": ["name": "oab-instance-mcp", "version": "upstream"]],
        ])
        guard (200..<300).contains(status) else { throw UpstreamError.http(status, body) }
        sessionID = headers.first { $0.key.lowercased() == "mcp-session-id" }?.value
        _ = try? await post(["jsonrpc": "2.0", "method": "notifications/initialized"])
        let info = (try? Self.parseBody(body))?["result"]?["serverInfo"]
        log("upstream \(name): connected to \(info?["name"]?.stringValue ?? "?") \(info?["version"]?.stringValue ?? "")")
        cachedTools = nil
    }

    private func post(_ msg: JSONValue) async throws -> (Int, Data) {
        let (s, b, _) = try await postRaw(msg); return (s, b)
    }

    private func postRaw(_ msg: JSONValue) async throws -> (Int, Data, [String: String]) {
        var req = URLRequest(url: url)
        req.httpMethod = "POST"
        req.httpBody = try JSONCoding.encoder.encode(msg)
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.setValue("application/json, text/event-stream", forHTTPHeaderField: "Accept")
        if let h = url.host { req.setValue(h, forHTTPHeaderField: "Host") }
        if let sid = sessionID { req.setValue(sid, forHTTPHeaderField: "Mcp-Session-Id") }
        let (data, resp) = try await session.data(for: req)
        let http = resp as? HTTPURLResponse
        var headers: [String: String] = [:]
        for (k, v) in http?.allHeaderFields ?? [:] { if let k = k as? String, let v = v as? String { headers[k] = v } }
        return (http?.statusCode ?? 0, data, headers)
    }

    /// JSON, or the first `data:` line of an SSE body.
    static func parseBody(_ data: Data) throws -> JSONValue {
        if let v = try? JSONCoding.decoder.decode(JSONValue.self, from: data) { return v }
        let text = String(decoding: data, as: UTF8.self)
        for line in text.split(separator: "\n") where line.hasPrefix("data:") {
            let payload = line.dropFirst(5).trimmingCharacters(in: .whitespaces)
            if let v = try? JSONCoding.decoder.decode(JSONValue.self, from: Data(payload.utf8)) { return v }
        }
        throw UpstreamError.badBody(String(text.prefix(200)))
    }

    public enum UpstreamError: Error, CustomStringConvertible {
        case http(Int, Data)
        case badBody(String)
        public var description: String {
            switch self {
            case .http(let s, let d): return "upstream HTTP \(s): \(String(decoding: d.prefix(200), as: UTF8.self))"
            case .badBody(let s): return "upstream returned neither JSON nor SSE: \(s)"
            }
        }
    }
}

/// A `Tool` that forwards to an upstream. One per upstream tool, built from the
/// upstream's descriptor at list time, so the schema the agent sees is the
/// upstream's own.
struct UpstreamTool: Tool {
    let descriptorValue: JSONValue
    let upstream: UpstreamMCP

    var name: String { descriptorValue["name"]?.stringValue ?? "?" }
    var description: String { descriptorValue["description"]?.stringValue ?? "" }
    var inputSchema: JSONValue { descriptorValue["inputSchema"] ?? ["type": "object"] }
    var descriptor: JSONValue { descriptorValue }

    func call(arguments: JSONValue) async throws -> ToolResult {
        let r = try await upstream.call(name, arguments: arguments)
        // Pass the upstream's result through verbatim: content blocks, isError,
        // structuredContent. Re-wrapping would lose image blocks.
        return ToolResult(passthrough: r)
    }
}

extension ToolResult {
    /// A result whose JSON is exactly `raw` (an upstream's `tools/call` result).
    init(passthrough raw: JSONValue) {
        self.init(content: [], isError: raw["isError"]?.boolValue ?? false, structured: nil)
        self.rawPassthrough = raw
    }
}
