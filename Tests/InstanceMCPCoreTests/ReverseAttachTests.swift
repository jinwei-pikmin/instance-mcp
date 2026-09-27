import Foundation
import Network
import XCTest
@testable import InstanceMCPCore

// MARK: - helpers

struct FakeExecTool: Tool {
    let name: String
    let description = "fake"
    let inputSchema: JSONValue = ["type": "object"]
    func call(arguments: JSONValue) async throws -> ToolResult { .text("ran \(name)") }
}

func toolNames(_ r: HTTPResponse) -> [String] {
    decodeJSON(r.body)["result"]?["tools"]?.arrayValue?.compactMap { $0["name"]?.stringValue } ?? []
}

func fullServer() -> MCPServer {
    MCPServer(name: "t", version: "0", instructions: "base", tools: [
        EchoTool(), FakeExecTool(name: "exec"), FakeExecTool(name: "exec_start"),
        FakeExecTool(name: "exec_poll"), FakeExecTool(name: "screenshot"),
    ])
}

// MARK: - Tool profiles

final class ToolProfileTests: XCTestCase {
    func testSandboxDropsEveryExecTool() async {
        let scoped = fullServer().scoped(to: .sandbox)
        XCTAssertEqual(scoped.toolNames, ["echo", "screenshot"])
        let list = await scoped.handle(try! JSONCoding.decoder.decode(JSONRPCRequest.self, from: rpc("tools/list")))
        let names = list?.result?["tools"]?.arrayValue?.compactMap { $0["name"]?.stringValue }
        XCTAssertEqual(names, ["echo", "screenshot"])
    }

    func testOwnerKeepsEverything() {
        XCTAssertEqual(fullServer().scoped(to: .owner).toolNames, ["echo", "exec", "exec_start", "exec_poll", "screenshot"])
    }

    func testCallingAnOmittedToolLooksLikeAnUnknownTool() async {
        let scoped = fullServer().scoped(to: .sandbox)
        let call = await scoped.handle(try! JSONCoding.decoder.decode(JSONRPCRequest.self,
            from: rpc("tools/call", params: ["name": "exec", "arguments": ["command": "id"]])))
        XCTAssertEqual(call?.error?.code, JSONRPCError.invalidParams)
        XCTAssertTrue(call?.error?.message.contains("unknown tool") == true)
        // Exactly the same shape as a tool that never existed.
        let ghost = await scoped.handle(try! JSONCoding.decoder.decode(JSONRPCRequest.self,
            from: rpc("tools/call", params: ["name": "nope", "arguments": [:]])))
        XCTAssertEqual(ghost?.error?.code, call?.error?.code)
    }

    func testScopedCanCarryItsOwnInstructions() async {
        let scoped = fullServer().scoped(to: .sandbox, instructions: "sandboxed")
        let init_ = await scoped.handle(try! JSONCoding.decoder.decode(JSONRPCRequest.self, from: rpc("initialize")))
        XCTAssertEqual(init_?.result?["instructions"]?.stringValue, "sandboxed")
        XCTAssertEqual(fullServer().scoped(to: .sandbox).instructions, "base")
    }
}

// MARK: - Redial policy (CLIENT-CONTRACT §9.2)

final class ReverseAttachPolicyTests: XCTestCase {
    func testTerminalCloseCodesStop() {
        XCTAssertEqual(ReverseAttachClient.disposition(closeCode: 4001), .stop(.grantExpired))
        XCTAssertEqual(ReverseAttachClient.disposition(closeCode: 4002), .stop(.replaced))
        XCTAssertEqual(ReverseAttachClient.disposition(closeCode: 4004), .stop(.sessionEnded))
        XCTAssertEqual(ReverseAttachClient.disposition(closeCode: 4010), .stop(.revoked))
    }

    func testRecoverableCloseCodesRedial() {
        for code in [1000, 1001, 1006, 4006, 4009] {
            XCTAssertEqual(ReverseAttachClient.disposition(closeCode: code), .redial, "code \(code)")
        }
    }

    func testHandshakeRejectionStopsButThrottleWaits() {
        XCTAssertEqual(ReverseAttachClient.disposition(handshakeStatus: 401), .stop(.handshakeRejected(401)))
        XCTAssertEqual(ReverseAttachClient.disposition(handshakeStatus: 404), .stop(.handshakeRejected(404)))
        XCTAssertEqual(ReverseAttachClient.disposition(handshakeStatus: 429), .redial)
        XCTAssertEqual(ReverseAttachClient.disposition(handshakeStatus: 503), .redial)
    }

    func testAttachURLIsBuiltFromTheRuntimeBase() {
        let c = ReverseAttachClient.Config(runtime: URL(string: "ws://100.1.2.3:8090")!, session: "laptop",
                                           secret: "s", profile: .sandbox, deadline: .distantFuture)
        XCTAssertEqual(c.attachURL.absoluteString, "ws://100.1.2.3:8090/tools/attach/laptop")
        let tls = ReverseAttachClient.Config(runtime: URL(string: "wss://pod.tail.ts.net/")!, session: "x",
                                             secret: "s", profile: .owner, deadline: .distantFuture)
        XCTAssertEqual(tls.attachURL.absoluteString, "wss://pod.tail.ts.net/tools/attach/x")
    }

    /// The socket answers frames exactly as the HTTP endpoint would, minus auth
    /// and sessions: the grant *is* the auth, no header on the socket is trusted.
    func testFrameAnsweringMatchesDispatch() async {
        let c = ReverseAttachClient.Config(runtime: URL(string: "ws://h:1")!, session: "s", secret: "x",
                                           profile: .sandbox, deadline: .distantFuture)
        let client = ReverseAttachClient(config: c, server: fullServer().scoped(to: .sandbox))
        let list = await client.answer(String(decoding: rpc("tools/list", id: 7), as: UTF8.self))
        let parsed = decodeJSON(Data(list!.utf8))
        XCTAssertEqual(parsed["id"]?.intValue, 7)
        XCTAssertEqual(parsed["result"]?["tools"]?.arrayValue?.count, 2)
        let note = await client.answer(String(decoding: rpc("notifications/initialized", id: nil), as: UTF8.self))
        XCTAssertNil(note)
        let garbage = await client.answer("not json")
        XCTAssertEqual(decodeJSON(Data(garbage!.utf8))["error"]?["code"]?.intValue, JSONRPCError.parseError)
    }
}

// MARK: - /attach endpoint

final class AttachEndpointTests: XCTestCase {
    func makeEndpoint(mint: (@Sendable (URL, String, String, TimeInterval) async throws -> (secret: String, expiresIn: TimeInterval))? = nil) -> (MCPHTTPEndpoint, AttachManager) {
        let mgr = AttachManager(server: fullServer(), mint: mint)
        let ep = MCPHTTPEndpoint(server: fullServer(), auth: AuthPolicy(allowedLogins: ["a@b"]), attach: mgr)
        return (ep, mgr)
    }
    func req(_ method: String, _ path: String, body: JSONValue? = nil, auth: Bool = true) -> HTTPRequest {
        var h = ["content-type": "application/json"]
        if auth { h["tailscale-user-login"] = "a@b"; h["x-forwarded-for"] = "100.1.1.1" }
        let data = body.map { try! JSONCoding.encoder.encode($0) } ?? Data()
        return HTTPRequest(method: method, path: path, query: nil, headers: h, body: data)
    }

    func testAttachNeedsTheHumanCredential() async {
        let (ep, _) = makeEndpoint()
        let r = await ep.handle(req("GET", "/attach", auth: false), remoteIsLoopback: true)
        XCTAssertEqual(r.status, 401)
        let r2 = await ep.handle(req("POST", "/attach", body: ["runtime": "ws://h:1", "session": "s", "secret": "x"], auth: false), remoteIsLoopback: true)
        XCTAssertEqual(r2.status, 401)
    }

    func testAttachIs404WhenDisabled() async {
        let ep = MCPHTTPEndpoint(server: fullServer(), auth: AuthPolicy(allowedLogins: ["a@b"]))
        let r = await ep.handle(req("GET", "/attach"), remoteIsLoopback: true)
        XCTAssertEqual(r.status, 404)
    }

    func testBadRequestsAre400() async {
        let (ep, _) = makeEndpoint()
        let cases: [(JSONValue, String)] = [
            (["session": "s", "secret": "x"], "runtime"),
            (["runtime": "http://h:1", "session": "s", "secret": "x"], "ws://"),
            (["runtime": "ws://h:1", "session": "Bad_Name", "secret": "x"], "session"),
            (["runtime": "ws://h:1", "session": "s", "secret": "x", "profile": "root"], "profile"),
            (["runtime": "ws://h:1", "session": "s", "secret": "x", "ttl_secs": 0], "ttl"),
            (["runtime": "ws://h:1", "session": "s", "secret": "x", "ttl_secs": 86401], "ttl"),
            (["runtime": "ws://h:1", "session": "s"], "exactly one"),
            (["runtime": "ws://h:1", "session": "s", "secret": "x", "admin_credential": "y"], "exactly one"),
        ]
        for (body, needle) in cases {
            let r = await ep.handle(req("POST", "/attach", body: body), remoteIsLoopback: true)
            XCTAssertEqual(r.status, 400, "\(body)")
            XCTAssertTrue(decodeJSON(r.body)["error"]?.stringValue?.contains(needle) == true, "\(body) → \(String(decoding: r.body, as: UTF8.self))")
        }
    }

    func testCreateListRevoke() async {
        let (ep, _) = makeEndpoint()
        // Unreachable runtime: the grant exists and the client is dialing/redialing.
        let r = await ep.handle(req("POST", "/attach", body: ["runtime": "ws://127.0.0.1:9", "session": "laptop", "secret": "abc", "ttl_secs": 60]), remoteIsLoopback: true)
        XCTAssertEqual(r.status, 202, String(decoding: r.body, as: UTF8.self))
        let g = decodeJSON(r.body)
        let id = g["id"]!.stringValue!
        XCTAssertEqual(g["profile"]?.stringValue, "sandbox")
        XCTAssertEqual(g["principal"]?.stringValue, "a@b")
        XCTAssertEqual(g["session"]?.stringValue, "laptop")

        let list = await ep.handle(req("GET", "/attach"), remoteIsLoopback: true)
        XCTAssertEqual(decodeJSON(list.body)["grants"]?.arrayValue?.count, 1)

        let one = await ep.handle(req("GET", "/attach/\(id)"), remoteIsLoopback: true)
        XCTAssertEqual(one.status, 200)

        // Same target again replaces, not duplicates.
        let again = await ep.handle(req("POST", "/attach", body: ["runtime": "ws://127.0.0.1:9", "session": "laptop", "secret": "def", "ttl_secs": 60, "profile": "owner"]), remoteIsLoopback: true)
        XCTAssertEqual(again.status, 202)
        let list2 = await ep.handle(req("GET", "/attach"), remoteIsLoopback: true)
        let grants = decodeJSON(list2.body)["grants"]!.arrayValue!
        XCTAssertEqual(grants.count, 1)
        XCTAssertEqual(grants[0]["profile"]?.stringValue, "owner")
        let id2 = grants[0]["id"]!.stringValue!
        XCTAssertNotEqual(id, id2)

        let del = await ep.handle(req("DELETE", "/attach/\(id2)"), remoteIsLoopback: true)
        XCTAssertEqual(del.status, 204)
        let gone = await ep.handle(req("DELETE", "/attach/\(id2)"), remoteIsLoopback: true)
        XCTAssertEqual(gone.status, 404)
        let empty = await ep.handle(req("GET", "/attach"), remoteIsLoopback: true)
        XCTAssertEqual(decodeJSON(empty.body)["grants"]?.arrayValue?.count, 0)
    }

    func testAdminCredentialPathMintsAndDoesNotStoreIt() async {
        final class Box: @unchecked Sendable { var seen: [(URL, String, String, TimeInterval)] = [] }
        let box = Box()
        let (ep, mgr) = makeEndpoint(mint: { rt, s, cred, ttl in
            box.seen.append((rt, s, cred, ttl)); return ("minted-secret", 120)
        })
        let r = await ep.handle(req("POST", "/attach", body: ["runtime": "ws://127.0.0.1:9", "session": "s", "admin_credential": "hunter2", "ttl_secs": 3600]), remoteIsLoopback: true)
        XCTAssertEqual(r.status, 202, String(decoding: r.body, as: UTF8.self))
        XCTAssertEqual(box.seen.count, 1)
        XCTAssertEqual(box.seen[0].2, "hunter2")
        // The runtime said 120 s; that wins over our 3600.
        let expires = decodeJSON(r.body)["expires_in_secs"]!.doubleValue!
        XCTAssertLessThanOrEqual(expires, 120)
        let grants = await mgr.list()
        XCTAssertEqual(grants.count, 1)
        // Nothing in the grant record carries either credential.
        let dumped = String(decoding: try! JSONCoding.encoder.encode(grants[0].json), as: UTF8.self)
        XCTAssertFalse(dumped.contains("hunter2"))
        XCTAssertFalse(dumped.contains("minted-secret"))
    }

    func testMintFailureIs502() async {
        let (ep, _) = makeEndpoint(mint: { _, _, _, _ in throw AttachManager.Failure.mintFailed(status: 401, body: "") })
        let r = await ep.handle(req("POST", "/attach", body: ["runtime": "ws://127.0.0.1:9", "session": "s", "admin_credential": "bad"]), remoteIsLoopback: true)
        XCTAssertEqual(r.status, 502)
    }

    func testRuntimeMintRequestForwardsTheRequestedLeaseTTL() throws {
        let req = AttachManager.runtimeMintRequest(
            runtime: URL(string: "wss://pod.tail.example:8090/base/")!,
            session: "mac",
            credential: "admin-secret",
            ttl: 24 * 3600
        )
        XCTAssertEqual(req.url?.absoluteString,
                       "https://pod.tail.example:8090/base/admin/sessions/mac/tools-attach")
        XCTAssertEqual(req.httpMethod, "POST")
        XCTAssertEqual(req.value(forHTTPHeaderField: "Authorization"), "Bearer admin-secret")
        XCTAssertEqual(req.value(forHTTPHeaderField: "Content-Type"), "application/json")
        let body = try JSONCoding.decoder.decode(JSONValue.self, from: XCTUnwrap(req.httpBody))
        XCTAssertEqual(body["ttl_secs"]?.intValue, 24 * 3600,
                       "the Mac→runtime hop must not silently fall back to the runtime's 1h default")
        XCTAssertEqual(body.objectValue?.count, 1)
    }
}

// MARK: - End to end against a fake openab-pty runtime

/// A minimal WebSocket server that plays the runtime: accepts the upgrade if the
/// bearer matches, sends `initialize`, then relays two CLI requests and closes
/// with a chosen code. Built on Network.framework's WebSocket support so the
/// test needs no dependency.
final class FakeRuntime: @unchecked Sendable {
    private(set) var port: UInt16 = 0
    private let listener: NWListener
    private let queue = DispatchQueue(label: "fake-runtime")
    let expectedSecret: String
    let closeWith: NWProtocolWebSocket.CloseCode
    /// Frames received from the Mac, in order (JSON).
    private(set) var received: [JSONValue] = []
    private(set) var upgradeAttempts = 0
    let done = XCTestExpectation(description: "runtime finished a connection")

    init(secret: String, closeWith: NWProtocolWebSocket.CloseCode) throws {
        expectedSecret = secret
        self.closeWith = closeWith
        let params = NWParameters.tcp
        let ws = NWProtocolWebSocket.Options()
        ws.autoReplyPing = true
        params.defaultProtocolStack.applicationProtocols.insert(ws, at: 0)
        listener = try NWListener(using: params, on: .any)
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
        upgradeAttempts += 1
        // Network.framework does not expose the upgrade request headers directly
        // on the server side in a portable way; the secret check is modelled by
        // the *runtime* tests in openab-pty. Here we always accept and drive the
        // MCP conversation, which is what this side is responsible for.
        conn.stateUpdateHandler = { [weak self] st in
            if case .ready = st { self?.drive(conn) }
        }
        conn.start(queue: queue)
    }

    private func send(_ conn: NWConnection, _ v: JSONValue, _ then: @escaping () -> Void) {
        let meta = NWProtocolWebSocket.Metadata(opcode: .text)
        let ctx = NWConnection.ContentContext(identifier: "t", metadata: [meta])
        conn.send(content: try! JSONCoding.encoder.encode(v), contentContext: ctx, isComplete: true,
                  completion: .contentProcessed { _ in then() })
    }

    private func recv(_ conn: NWConnection, _ then: @escaping (JSONValue) -> Void) {
        conn.receiveMessage { [weak self] data, _, _, _ in
            guard let data, let v = try? JSONCoding.decoder.decode(JSONValue.self, from: data) else { return }
            self?.received.append(v)
            then(v)
        }
    }

    private func drive(_ conn: NWConnection) {
        // 1. initialize, as the runtime (MCP client) does on attach.
        send(conn, ["jsonrpc": "2.0", "id": 1, "method": "initialize", "params": ["protocolVersion": "2025-06-18", "capabilities": [:], "clientInfo": ["name": "openab-pty", "version": "t"]]]) {
            self.recv(conn) { _ in
                // 2. tools/list on behalf of the CLI.
                self.send(conn, ["jsonrpc": "2.0", "id": 2, "method": "tools/list"]) {
                    self.recv(conn) { _ in
                        // 3. a tools/call for exec — must be refused under sandbox.
                        self.send(conn, ["jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": ["name": "exec", "arguments": ["command": "id"]]]) {
                            self.recv(conn) { _ in
                                // 4. a permitted call.
                                self.send(conn, ["jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": ["name": "echo", "arguments": ["msg": "hi"]]]) {
                                    self.recv(conn) { _ in
                                        let meta = NWProtocolWebSocket.Metadata(opcode: .close)
                                        meta.closeCode = self.closeWith
                                        let ctx = NWConnection.ContentContext(identifier: "close", metadata: [meta])
                                        conn.send(content: nil, contentContext: ctx, isComplete: true, completion: .contentProcessed { _ in
                                            self.done.fulfill()
                                            conn.cancel()
                                        })
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

final class ReverseAttachEndToEndTests: XCTestCase {
    func testMacServesTheRuntimeWithTheSandboxProfileAndStopsOnRevoke() async throws {
        let runtime = try FakeRuntime(secret: "s3", closeWith: .privateCode(4010))
        defer { runtime.stop() }
        let states = StateSink()
        let cfg = ReverseAttachClient.Config(runtime: URL(string: "ws://127.0.0.1:\(runtime.port)")!, session: "laptop",
                                             secret: "s3", profile: .sandbox, deadline: Date().addingTimeInterval(30))
        let client = ReverseAttachClient(config: cfg, server: fullServer().scoped(to: .sandbox),
                                         onStateChange: { states.push($0) })
        await client.start()

        await fulfillment(of: [runtime.done], timeout: 10)
        // Give the client a beat to observe the close.
        try await Task.sleep(nanoseconds: 300_000_000)

        // The runtime saw four replies with its own ids.
        XCTAssertEqual(runtime.received.count, 4, "\(runtime.received)")
        XCTAssertEqual(runtime.received[0]["id"]?.intValue, 1)
        XCTAssertEqual(runtime.received[0]["result"]?["serverInfo"]?["name"]?.stringValue, "t")
        let names = runtime.received[1]["result"]?["tools"]?.arrayValue?.compactMap { $0["name"]?.stringValue }
        XCTAssertEqual(names, ["echo", "screenshot"], "sandbox profile: no exec*")
        XCTAssertNotNil(runtime.received[2]["error"], "exec under sandbox is refused")
        XCTAssertEqual(runtime.received[3]["result"]?["content"]?.arrayValue?.first?["text"]?.stringValue, "hi")

        // 4010 is terminal: no redial.
        let final_ = await client.state
        XCTAssertEqual(final_, .ended(.revoked), "\(states.all)")
        XCTAssertEqual(runtime.upgradeAttempts, 1)
        XCTAssertTrue(states.all.contains(.attached))
    }

    func testRuntimeReplacedRedialsWithBackoff() async throws {
        let runtime = try FakeRuntime(secret: "s3", closeWith: .privateCode(4006))
        defer { runtime.stop() }
        let states = StateSink()
        var cfg = ReverseAttachClient.Config(runtime: URL(string: "ws://127.0.0.1:\(runtime.port)")!, session: "laptop",
                                             secret: "s3", profile: .owner, deadline: Date().addingTimeInterval(2.5))
        cfg.initialBackoff = 0.2
        let client = ReverseAttachClient(config: cfg, server: fullServer(), onStateChange: { states.push($0) })
        await client.start()
        try await Task.sleep(nanoseconds: 3_500_000_000)
        XCTAssertGreaterThanOrEqual(runtime.upgradeAttempts, 2, "4006 must redial")
        let final_ = await client.state
        XCTAssertEqual(final_, .ended(.deadline), "gives up at the grant deadline: \(states.all)")
        XCTAssertTrue(states.all.contains { if case .waitingToRedial = $0 { return true }; return false })
    }

    func testHandshakeRefusalStops() async throws {
        // Nothing listens here → connection refused → redial, then deadline.
        // (A real 401 is exercised by openab-pty's own suite; here we prove the
        // client does not spin: it backs off and stops at the deadline.)
        var cfg = ReverseAttachClient.Config(runtime: URL(string: "ws://127.0.0.1:1")!, session: "x",
                                             secret: "s", profile: .sandbox, deadline: Date().addingTimeInterval(1.2))
        cfg.initialBackoff = 0.3
        let states = StateSink()
        let client = ReverseAttachClient(config: cfg, server: fullServer(), onStateChange: { states.push($0) })
        await client.start()
        try await Task.sleep(nanoseconds: 2_500_000_000)
        let final_ = await client.state
        XCTAssertEqual(final_, .ended(.deadline), "\(states.all)")
        let dials = states.all.filter { $0 == .dialing }.count
        XCTAssertLessThanOrEqual(dials, 5, "backoff must bound the dial rate: \(states.all)")
    }
}

final class StateSink: @unchecked Sendable {
    private let lock = NSLock()
    private var states: [ReverseAttachClient.State] = []
    func push(_ s: ReverseAttachClient.State) { lock.lock(); states.append(s); lock.unlock() }
    var all: [ReverseAttachClient.State] { lock.lock(); defer { lock.unlock() }; return states }
}
