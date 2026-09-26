import Foundation
import Network
import XCTest
@testable import InstanceMCPCore

/// A tiny Streamable-HTTP MCP server on loopback that plays `@playwright/mcp`:
/// issues an `Mcp-Session-Id` on initialize, 400s without it afterwards, answers
/// `tools/list` with three `browser_*` tools, echoes `tools/call` arguments, and
/// replies as SSE frames (`data: {…}`) — the shape the real one uses.
final class FakeUpstream: @unchecked Sendable {
    private(set) var port: UInt16 = 0
    private let listener: NWListener
    private let queue = DispatchQueue(label: "fake-upstream")
    private(set) var calls: [JSONValue] = []
    private(set) var hostHeaders: [String] = []
    private let sid = UUID().uuidString
    var initializeCount = 0
    var down = false

    init() throws {
        listener = try NWListener(using: .tcp, on: .any)
        let sem = DispatchSemaphore(value: 0)
        listener.stateUpdateHandler = { [weak self] st in
            if case .ready = st { self?.port = self?.listener.port?.rawValue ?? 0; sem.signal() }
            if case .failed = st { sem.signal() }
        }
        listener.newConnectionHandler = { [weak self] c in self?.accept(c) }
        listener.start(queue: queue)
        sem.wait()
    }

    func stop() { listener.cancel() }

    private func accept(_ conn: NWConnection) {
        if down { conn.cancel(); return }
        conn.stateUpdateHandler = { [weak self] st in if case .ready = st { self?.read(conn, Data()) } }
        conn.start(queue: queue)
    }

    private func read(_ conn: NWConnection, _ buf: Data) {
        conn.receive(minimumIncompleteLength: 1, maximumLength: 65536) { [weak self] data, _, done, _ in
            guard let self else { return }
            var b = buf; if let data { b.append(data) }
            switch HTTPParser.parse(b) {
            case .incomplete: if done { conn.cancel() } else { self.read(conn, b) }
            case .invalid: conn.cancel()
            case .request(let req, _):
                let resp = self.handle(req)
                conn.send(content: resp.serialize(keepAlive: false), completion: .contentProcessed { _ in conn.cancel() })
            }
        }
    }

    private func sse(_ v: JSONValue, extra: [(String, String)] = []) -> HTTPResponse {
        let body = "event: message\ndata: " + String(decoding: try! JSONCoding.encoder.encode(v), as: UTF8.self) + "\n\n"
        return HTTPResponse(status: 200, headers: [("Content-Type", "text/event-stream")] + extra, body: Data(body.utf8))
    }

    private func handle(_ req: HTTPRequest) -> HTTPResponse {
        hostHeaders.append(req.header("host") ?? "")
        let rpc = (try? JSONCoding.decoder.decode(JSONRPCRequest.self, from: req.body))
        guard let rpc else { return .text(400, "bad") }
        let id = rpc.id ?? .null
        switch rpc.method {
        case "initialize":
            initializeCount += 1
            return sse(["jsonrpc": "2.0", "id": id, "result": ["protocolVersion": "2025-06-18", "capabilities": [:], "serverInfo": ["name": "FakePlaywright", "version": "0"]]],
                       extra: [("Mcp-Session-Id", sid)])
        case "notifications/initialized":
            return .init(status: 202)
        default:
            guard req.header("mcp-session-id") == sid else { return .text(400, "no session") }
        }
        switch rpc.method {
        case "tools/list":
            return sse(["jsonrpc": "2.0", "id": id, "result": ["tools": [
                ["name": "browser_navigate", "description": "go", "inputSchema": ["type": "object", "properties": ["url": ["type": "string"]]]],
                ["name": "browser_snapshot", "description": "read", "inputSchema": ["type": "object"]],
                ["name": "browser_run_code_unsafe", "description": "danger", "inputSchema": ["type": "object"]],
                ["name": "screenshot", "description": "collides with a local tool", "inputSchema": ["type": "object"]],
            ]]])
        case "tools/call":
            calls.append(rpc.params ?? .null)
            let name = rpc.params?["name"]?.stringValue ?? "?"
            if name == "browser_snapshot" {
                return sse(["jsonrpc": "2.0", "id": id, "result": ["content": [["type": "text", "text": "- heading \"First video title\""], ["type": "image", "data": "AAAA", "mimeType": "image/png"]]]])
            }
            let text = "did \(name) with \(rpc.params?["arguments"]?["url"]?.stringValue ?? "-")"
            return sse(["jsonrpc": "2.0", "id": id, "result": ["content": [["type": "text", "text": .string(text)]], "isError": false]])
        default:
            return sse(["jsonrpc": "2.0", "id": id, "error": ["code": -32601, "message": "nope"]])
        }
    }
}

final class UpstreamMCPTests: XCTestCase {
    func makeServer(_ up: FakeUpstream, profile: ToolProfile?) -> MCPServer {
        let u = UpstreamMCP(name: "browser", url: URL(string: "http://127.0.0.1:\(up.port)/mcp")!)
        let base = MCPServer(name: "t", version: "0", tools: [EchoTool(), FakeExecTool(name: "exec"), FakeExecTool(name: "screenshot")], upstreams: [u])
        return profile.map { base.scoped(to: $0) } ?? base
    }

    func names(_ r: JSONRPCResponse?) -> [String] {
        r?.result?["tools"]?.arrayValue?.compactMap { $0["name"]?.stringValue } ?? []
    }

    func testSandboxSeesOnlyAllowlistedBrowserToolsAndLocalNamesWin() async throws {
        let up = try FakeUpstream(); defer { up.stop() }
        let s = makeServer(up, profile: .sandbox)
        let list = await s.handle(try JSONCoding.decoder.decode(JSONRPCRequest.self, from: rpc("tools/list")))
        XCTAssertEqual(names(list), ["echo", "screenshot", "browser_navigate", "browser_snapshot"],
                       "no exec, no browser_run_code_unsafe, upstream 'screenshot' shadowed by the local one")
        XCTAssertEqual(up.hostHeaders.first, "127.0.0.1", "Host must be the bare host — Playwright's allowed-hosts check")
    }

    func testOwnerSeesEverythingUpstreamOffers() async throws {
        let up = try FakeUpstream(); defer { up.stop() }
        let s = makeServer(up, profile: .owner)
        let list = await s.handle(try JSONCoding.decoder.decode(JSONRPCRequest.self, from: rpc("tools/list")))
        XCTAssertTrue(names(list).contains("browser_run_code_unsafe"))
    }

    func testCallIsForwardedAndResultPassedThroughVerbatim() async throws {
        let up = try FakeUpstream(); defer { up.stop() }
        let s = makeServer(up, profile: .sandbox)
        let nav = await s.handle(try JSONCoding.decoder.decode(JSONRPCRequest.self,
            from: rpc("tools/call", params: ["name": "browser_navigate", "arguments": ["url": "https://x"]])))
        XCTAssertEqual(nav?.result?["content"]?.arrayValue?.first?["text"]?.stringValue, "did browser_navigate with https://x")
        XCTAssertEqual(up.calls.last?["name"]?.stringValue, "browser_navigate")
        XCTAssertEqual(up.calls.last?["arguments"]?["url"]?.stringValue, "https://x")

        let snap = await s.handle(try JSONCoding.decoder.decode(JSONRPCRequest.self,
            from: rpc("tools/call", params: ["name": "browser_snapshot", "arguments": [:]])))
        let content = snap?.result?["content"]?.arrayValue
        XCTAssertEqual(content?.count, 2, "image block survives the relay")
        XCTAssertEqual(content?[1]["mimeType"]?.stringValue, "image/png")
    }

    func testDeniedUpstreamToolIsUnknownUnderSandbox() async throws {
        let up = try FakeUpstream(); defer { up.stop() }
        let s = makeServer(up, profile: .sandbox)
        let r = await s.handle(try JSONCoding.decoder.decode(JSONRPCRequest.self,
            from: rpc("tools/call", params: ["name": "browser_run_code_unsafe", "arguments": [:]])))
        XCTAssertEqual(r?.error?.code, JSONRPCError.invalidParams)
        XCTAssertTrue(up.calls.isEmpty, "the denied call never reached the upstream")
    }

    func testUpstreamDownMeansNoBrowserToolsAndLocalStillWorks() async throws {
        let up = try FakeUpstream()
        up.stop()
        let s = makeServer(up, profile: .sandbox)
        let list = await s.handle(try JSONCoding.decoder.decode(JSONRPCRequest.self, from: rpc("tools/list")))
        XCTAssertEqual(names(list), ["echo", "screenshot"])
        let echo = await s.handle(try JSONCoding.decoder.decode(JSONRPCRequest.self,
            from: rpc("tools/call", params: ["name": "echo", "arguments": ["msg": "ok"]])))
        XCTAssertEqual(echo?.result?["content"]?.arrayValue?.first?["text"]?.stringValue, "ok")
    }

    func testSessionIsReestablishedWhenUpstreamForgetsIt() async throws {
        let up = try FakeUpstream(); defer { up.stop() }
        let u = UpstreamMCP(name: "b", url: URL(string: "http://127.0.0.1:\(up.port)/mcp")!)
        _ = await u.tools()
        XCTAssertEqual(up.initializeCount, 1)
        // A second UpstreamMCP against the same fake has no session: the fake 400s,
        // the client re-inits once and retries.
        let u2 = UpstreamMCP(name: "b2", url: URL(string: "http://127.0.0.1:\(up.port)/mcp")!)
        let r = try await u2.call("browser_navigate", arguments: ["url": "https://y"])
        XCTAssertEqual(r["content"]?.arrayValue?.first?["text"]?.stringValue, "did browser_navigate with https://y")
        XCTAssertEqual(up.initializeCount, 2)
    }

    func testSSEAndPlainJSONBodiesBothParse() throws {
        let plain = try UpstreamMCP.parseBody(Data(#"{"jsonrpc":"2.0","id":1,"result":{}}"#.utf8))
        XCTAssertEqual(plain["id"]?.intValue, 1)
        let sse = try UpstreamMCP.parseBody(Data("event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n\n".utf8))
        XCTAssertEqual(sse["id"]?.intValue, 2)
        XCTAssertThrowsError(try UpstreamMCP.parseBody(Data("Access denied".utf8)))
    }
}

final class SandboxBrowserAllowlistTests: XCTestCase {
    func testAllowlistShapesMatchTheThreatModel() {
        let p = ToolProfile.sandbox
        for ok in ["browser_navigate", "browser_snapshot", "browser_click", "browser_type", "browser_evaluate", "browser_take_screenshot", "browser_tabs"] {
            XCTAssertTrue(p.allows(ok), ok)
        }
        for no in ["browser_run_code_unsafe", "browser_file_upload", "browser_pdf_save", "browser_network_requests", "browser_mouse_click_xy", "browser_close", "browser_handle_dialog", "browser_something_new"] {
            XCTAssertFalse(p.allows(no), no)
        }
        XCTAssertTrue(p.allows("screenshot"))
        XCTAssertFalse(p.allows("exec_start"))
        XCTAssertTrue(ToolProfile.owner.allows("browser_run_code_unsafe"))
    }
}
