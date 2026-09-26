import Foundation

/// Transport-independent MCP server core. Handles one JSON-RPC message at a time;
/// the HTTP layer deals with sessions, framing and auth.
public struct MCPServer: Sendable {
    public static let protocolVersion = "2025-06-18"
    /// Older clients (kiro-cli, some SDKs) negotiate these; we speak the same subset.
    public static let supportedVersions: Set<String> = ["2025-06-18", "2025-03-26", "2024-11-05"]

    public let serverName: String
    public let serverVersion: String
    public let instructions: String?
    private let tools: [String: any Tool]
    private let toolOrder: [String]
    /// Loopback MCP servers whose tools are merged into `tools/list` (after the
    /// local ones) and routed on `tools/call`. Resolved at request time, so an
    /// upstream that is down simply contributes nothing. See `UpstreamMCP`.
    public let upstreams: [UpstreamMCP]
    /// Applied to upstream tool names at list/call time. Local tools are already
    /// filtered by `scoped(to:)`; upstream lists are dynamic, so the filter has
    /// to travel with the server.
    public let upstreamFilter: ToolProfile?

    public init(name: String, version: String, instructions: String? = nil, tools: [any Tool],
                upstreams: [UpstreamMCP] = [], upstreamFilter: ToolProfile? = nil) {
        self.serverName = name
        self.serverVersion = version
        self.instructions = instructions
        var map: [String: any Tool] = [:]
        for t in tools { map[t.name] = t }
        self.tools = map
        self.toolOrder = tools.map(\.name)
        self.upstreams = upstreams
        self.upstreamFilter = upstreamFilter
    }

    /// Upstream tools visible under the current filter, as `Tool`s.
    func upstreamTools() async -> [UpstreamTool] {
        var out: [UpstreamTool] = []
        for u in upstreams {
            for d in await u.tools() {
                let t = UpstreamTool(descriptorValue: d, upstream: u)
                if upstreamFilter?.allows(t.name) ?? true { out.append(t) }
            }
        }
        // Local names win on collision: an upstream cannot shadow `screenshot`.
        return out.filter { tools[$0.name] == nil }
    }

    /// Tools in declaration order. Used by `scoped(to:)`.
    public var allTools: [any Tool] { toolOrder.compactMap { tools[$0] } }
    public var toolNames: [String] { toolOrder }

    /// Parse a JSON-RPC message that has already been decoded to a `JSONValue`
    /// (the WebSocket path receives text, not `Data`).
    public static func parse(_ value: JSONValue) -> Result<JSONRPCRequest, JSONRPCError> {
        guard let data = try? JSONCoding.encoder.encode(value) else {
            return .failure(.init(code: JSONRPCError.parseError, message: "unencodable value"))
        }
        return parse(data)
    }

    /// Parse raw bytes into a request. Batches are rejected (removed in 2025-06-18).
    public static func parse(_ data: Data) -> Result<JSONRPCRequest, JSONRPCError> {
        do {
            let req = try JSONCoding.decoder.decode(JSONRPCRequest.self, from: data)
            guard req.jsonrpc == "2.0" else {
                return .failure(.init(code: JSONRPCError.invalidRequest, message: "jsonrpc must be \"2.0\""))
            }
            return .success(req)
        } catch {
            if let arr = try? JSONCoding.decoder.decode([JSONValue].self, from: data), !arr.isEmpty {
                return .failure(.init(code: JSONRPCError.invalidRequest, message: "batch requests are not supported"))
            }
            return .failure(.init(code: JSONRPCError.parseError, message: "Parse error: \(error.localizedDescription)"))
        }
    }

    /// Returns nil for notifications (no response body) — the HTTP layer answers 202.
    public func handle(_ req: JSONRPCRequest) async -> JSONRPCResponse? {
        if req.isNotification {
            // notifications/initialized, notifications/cancelled, … nothing to do yet.
            return nil
        }
        let id = req.id ?? .null
        do {
            let result = try await dispatch(req)
            return JSONRPCResponse(id: id, result: result)
        } catch let e as JSONRPCError {
            return JSONRPCResponse(id: id, error: e)
        } catch {
            return JSONRPCResponse(id: id, error: .init(code: JSONRPCError.internalError, message: "\(error)"))
        }
    }

    private func dispatch(_ req: JSONRPCRequest) async throws -> JSONValue {
        switch req.method {
        case "initialize":
            let requested = req.params?["protocolVersion"]?.stringValue ?? Self.protocolVersion
            let negotiated = Self.supportedVersions.contains(requested) ? requested : Self.protocolVersion
            var result: [String: JSONValue] = [
                "protocolVersion": .string(negotiated),
                "capabilities": ["tools": ["listChanged": false]],
                "serverInfo": ["name": .string(serverName), "version": .string(serverVersion)],
            ]
            if let i = instructions { result["instructions"] = .string(i) }
            return .object(result)

        case "ping":
            return [:]

        case "tools/list":
            var list = toolOrder.compactMap { tools[$0]?.descriptor }
            list += await upstreamTools().map(\.descriptor)
            return ["tools": .array(list)]

        case "tools/call":
            guard let name = req.params?["name"]?.stringValue else {
                throw JSONRPCError.invalidParams("missing tool name")
            }
            var resolved: (any Tool)? = tools[name]
            if resolved == nil, !upstreams.isEmpty {
                resolved = await upstreamTools().first { $0.name == name }
            }
            guard let tool = resolved else {
                throw JSONRPCError.invalidParams("unknown tool: \(name)")
            }
            let args = req.params?["arguments"] ?? [:]
            do {
                return try await tool.call(arguments: args).json
            } catch let e as ToolError {
                // Tool-level failures are results, not protocol errors — the model should see them.
                return ToolResult.error(e.description).json
            }

        case "resources/list", "resources/templates/list":
            return ["resources": []]
        case "prompts/list":
            return ["prompts": []]

        default:
            throw JSONRPCError.methodNotFound(req.method)
        }
    }
}
