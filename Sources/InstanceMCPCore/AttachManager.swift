import Foundation

/// Grants: "lend this Mac to that session, with that profile, for that long."
///
/// A grant is created by a human through `POST /attach` (from OpenAB Connect or
/// Remote, or a curl) and lives **here**, on the Mac. The granting device does not
/// need to stay online; the Mac dials, redials and gives up on its own. Design:
/// instance-mcp `docs/requirements/reverse-attach.md`, "How a grant works".
///
/// One grant per (runtime, session): a new one for the same target replaces the
/// old, which is also how renewal works. The profile is fixed for a grant's life.
public actor AttachManager {
    public struct Grant: Sendable {
        public let id: String
        public let runtime: URL
        public let session: String
        public let profile: ToolProfile
        public let principal: String
        public let createdAt: Date
        public let expiresAt: Date
        public var state: ReverseAttachClient.State
    }

    public struct Request: Sendable {
        public var runtime: URL
        public var session: String
        public var profile: ToolProfile
        public var ttl: TimeInterval
        /// Exactly one of the two: the caller already minted at the runtime and
        /// hands us the secret, or hands us the runtime's admin credential so we
        /// mint ourselves (used for that one call, never stored).
        public var secret: String?
        public var adminCredential: String?
    }

    public enum Failure: Error, Equatable, CustomStringConvertible {
        case badRequest(String)
        case mintFailed(status: Int, body: String)
        case notFound

        public var description: String {
            switch self {
            case .badRequest(let s): return s
            case .mintFailed(let status, let body): return "runtime refused to mint (HTTP \(status)): \(body)"
            case .notFound: return "no such grant"
            }
        }
    }

    public static let maxTTL: TimeInterval = 24 * 3600
    public static let defaultTTL: TimeInterval = 3600

    private let baseServer: MCPServer
    private let log: @Sendable (String) -> Void
    private var grants: [String: Grant] = [:]
    private var clients: [String: ReverseAttachClient] = [:]
    /// Attach secrets by grant id, held only so live grants can be persisted (#12).
    private var secrets: [String: String] = [:]
    /// nil ⇒ grants live in memory only (tests, `--no-grant-persistence`).
    private let store: GrantStore?
    /// Seam for tests: mint against the runtime's admin plane.
    private let mint: @Sendable (URL, String, String, TimeInterval) async throws -> (secret: String, expiresIn: TimeInterval)

    public init(server: MCPServer,
                mint: (@Sendable (URL, String, String, TimeInterval) async throws -> (secret: String, expiresIn: TimeInterval))? = nil,
                store: GrantStore? = nil,
                log: @escaping @Sendable (String) -> Void = { _ in }) {
        self.baseServer = server
        self.log = log
        self.mint = mint ?? AttachManager.mintAtRuntime
        self.store = store
    }

    /// Re-dial every persisted grant still inside its deadline, under its original
    /// id. Call once at start. A runtime that has forgotten the grant (pod replaced)
    /// answers the handshake with 401 and the grant ends through the normal
    /// disposition. Returns how many were resumed.
    @discardableResult
    public func resume() async -> Int {
        guard let store else { return 0 }
        let persisted = store.load()
        var resumed = 0
        for p in persisted where grants[p.id] == nil {
            guard let profile = ToolProfile(rawValue: p.profile) else {
                log("grant \(p.id.prefix(8)) has unknown profile \(p.profile); dropping it")
                continue
            }
            let grant = Grant(id: p.id, runtime: p.runtime, session: p.session, profile: profile,
                              principal: p.principal, createdAt: p.createdAt, expiresAt: p.expiresAt, state: .idle)
            log("grant \(p.id.prefix(8)) resumed: \(profile.rawValue) → \(p.runtime.host ?? "?")/\(p.session), \(Int(p.expiresAt.timeIntervalSinceNow))s left")
            await startClient(grant, secret: p.secret)
            resumed += 1
        }
        // Rewrite without whatever expired or failed to load while we were down.
        persist()
        return resumed
    }

    // MARK: API

    public func list() -> [Grant] {
        grants.values.sorted { $0.createdAt < $1.createdAt }
    }

    public func get(_ id: String) -> Grant? { grants[id] }

    @discardableResult
    public func create(_ req: Request, principal: String) async throws -> Grant {
        guard let scheme = req.runtime.scheme?.lowercased(), scheme == "ws" || scheme == "wss",
              req.runtime.host != nil else {
            throw Failure.badRequest("runtime must be a ws:// or wss:// URL")
        }
        guard !req.session.isEmpty, req.session.count <= 32,
              req.session.allSatisfy({ $0.isASCII && ($0.isLowercase || $0.isNumber || $0 == "-") }) else {
            throw Failure.badRequest("session must match [a-z0-9-]{1,32}")
        }
        guard req.ttl > 0, req.ttl <= Self.maxTTL else {
            throw Failure.badRequest("ttl_secs must be 1...\(Int(Self.maxTTL))")
        }
        let secret: String
        var expiresAt = Date().addingTimeInterval(req.ttl)
        switch (req.secret, req.adminCredential) {
        case (let s?, nil) where !s.isEmpty:
            secret = s
        case (nil, let cred?) where !cred.isEmpty:
            let minted: (secret: String, expiresIn: TimeInterval)
            do {
                minted = try await mint(req.runtime, req.session, cred, req.ttl)
            } catch let f as Failure {
                throw f
            } catch {
                // Transport failure toward the runtime (unreachable, ATS, TLS…) is the
                // runtime's side of the fence as far as the caller is concerned: 502.
                throw Failure.mintFailed(status: 0, body: error.localizedDescription)
            }
            secret = minted.secret
            // The runtime's TTL wins; ours is a request, theirs is the grant.
            expiresAt = min(expiresAt, Date().addingTimeInterval(minted.expiresIn))
        default:
            throw Failure.badRequest("provide exactly one of secret or admin_credential")
        }

        // Replace any grant for the same target.
        for (id, g) in grants where g.runtime == req.runtime && g.session == req.session {
            await revoke(id, reason: "replaced by a new grant")
        }

        let id = UUID().uuidString.lowercased()
        let grant = Grant(id: id, runtime: req.runtime, session: req.session, profile: req.profile,
                          principal: principal, createdAt: Date(), expiresAt: expiresAt, state: .idle)
        log("grant \(id.prefix(8)) by \(principal): \(req.profile.rawValue) → \(req.runtime.host ?? "?")/\(req.session) for \(Int(req.ttl))s")
        let started = await startClient(grant, secret: secret)
        persist()
        return started
    }

    /// Register `grant` and start dialling. Shared by `create` and `resume`.
    @discardableResult
    private func startClient(_ grant: Grant, secret: String) async -> Grant {
        let id = grant.id
        let instructions = Self.sandboxInstructions(profile: grant.profile, base: baseServer.instructions)
        let scoped = baseServer.scoped(to: grant.profile, instructions: instructions)
        let config = ReverseAttachClient.Config(runtime: grant.runtime, session: grant.session, secret: secret,
                                                profile: grant.profile, deadline: grant.expiresAt)
        let client = ReverseAttachClient(
            config: config, server: scoped,
            onStateChange: { [weak self] s in Task { await self?.update(id, state: s) } },
            log: log)
        grants[id] = grant
        clients[id] = client
        secrets[id] = secret
        await client.start()
        var started = grant
        started.state = await client.state
        return started
    }

    public func revoke(_ id: String, reason: String = "revoked") async {
        guard let g = grants.removeValue(forKey: id) else { return }
        secrets.removeValue(forKey: id)
        if let c = clients.removeValue(forKey: id) { await c.cancel() }
        log("grant \(id.prefix(8)) \(reason) (\(g.session))")
        persist()
    }

    /// Drop grants whose client has reached a terminal state or whose deadline
    /// passed. Called from the HTTP layer opportunistically; nothing depends on it.
    public func sweep() async {
        var changed = false
        for (id, g) in grants {
            if case .ended = g.state {
                grants.removeValue(forKey: id); clients.removeValue(forKey: id); secrets.removeValue(forKey: id)
                changed = true
                continue
            }
            if g.expiresAt < Date() { await revoke(id, reason: "expired") }
        }
        if changed { persist() }
    }

    private func update(_ id: String, state: ReverseAttachClient.State) {
        grants[id]?.state = state
        // A terminal grant must not be resumed after a restart.
        if case .ended = state { persist() }
    }

    /// Write the live grants (not ended, not expired) to the store, if any.
    private func persist() {
        guard let store else { return }
        let now = Date()
        let live: [PersistedGrant] = grants.values.compactMap { g in
            if case .ended = g.state { return nil }
            guard g.expiresAt > now, let secret = secrets[g.id] else { return nil }
            return PersistedGrant(id: g.id, runtime: g.runtime, session: g.session, profile: g.profile.rawValue,
                                  principal: g.principal, createdAt: g.createdAt, expiresAt: g.expiresAt,
                                  secret: secret)
        }
        store.save(live.sorted { $0.createdAt < $1.createdAt })
    }

    // MARK: helpers

    static func sandboxInstructions(profile: ToolProfile, base: String?) -> String? {
        guard profile == .sandbox else { return base }
        let head = base.map { $0 + "\n\n" } ?? ""
        return head + """
            You reached this Mac through OpenAB Connect: a human lent it to your sandbox session for a \
            limited time and is likely watching the screen. This is the `sandbox` profile — there is no \
            `exec` tool here (you already have a shell in your own session); drive the Mac through \
            `screenshot`, `mouse`, `key` and `osascript`, and — when `browser_*` tools are listed — through \
            the browser directly: `browser_navigate` then `browser_snapshot` gives you the page as text, \
            no screenshot needed. If a tool starts failing with "not attached", the grant ended; ask the \
            human to lend the Mac again.
            """
    }

    /// Build the exact runtime mint request. Keeping the body shape in a pure
    /// function gives tests a seam for the bug where Connect requested 12h but
    /// this hop sent an empty POST, silently reducing every lease to one hour.
    static func runtimeMintRequest(
        runtime: URL,
        session: String,
        credential: String,
        ttl: TimeInterval
    ) -> URLRequest {
        var c = URLComponents(url: runtime, resolvingAgainstBaseURL: false)!
        c.scheme = (c.scheme?.lowercased() == "wss") ? "https" : "http"
        let base = c.path.hasSuffix("/") ? String(c.path.dropLast()) : c.path
        c.path = base + "/admin/sessions/\(session)/tools-attach"
        var req = URLRequest(url: c.url!)
        req.httpMethod = "POST"
        req.setValue("Bearer \(credential)", forHTTPHeaderField: "Authorization")
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try! JSONCoding.encoder.encode(JSONValue.object([
            "ttl_secs": .number(ttl.rounded(.down)),
        ]))
        req.timeoutInterval = 15
        return req
    }

    /// `POST {runtime as http(s)}/admin/sessions/{session}/tools-attach` with the
    /// admin credential and the lease requested by Connect/Remote. The credential
    /// is used for this request and discarded.
    static let mintAtRuntime: @Sendable (URL, String, String, TimeInterval) async throws -> (secret: String, expiresIn: TimeInterval) = { runtime, session, credential, ttl in
        let req = runtimeMintRequest(
            runtime: runtime,
            session: session,
            credential: credential,
            ttl: ttl
        )
        let (data, resp) = try await URLSession(configuration: .ephemeral).data(for: req)
        let status = (resp as? HTTPURLResponse)?.statusCode ?? 0
        guard status == 201,
              let body = try? JSONCoding.decoder.decode(JSONValue.self, from: data),
              let secret = body["secret"]?.stringValue else {
            throw Failure.mintFailed(status: status, body: String(decoding: data.prefix(200), as: UTF8.self))
        }
        return (secret, TimeInterval(body["expires_in_secs"]?.intValue ?? 3600))
    }
}

// MARK: - JSON shapes

extension AttachManager.Grant {
    public var json: JSONValue {
        var o: [String: JSONValue] = [
            "id": .string(id),
            "runtime": .string(runtime.absoluteString),
            "session": .string(session),
            "profile": .string(profile.rawValue),
            "principal": .string(principal),
            "created_at": .string(ISO8601DateFormatter().string(from: createdAt)),
            "expires_at": .string(ISO8601DateFormatter().string(from: expiresAt)),
            "expires_in_secs": .number(max(0, expiresAt.timeIntervalSinceNow).rounded()),
        ]
        switch state {
        case .idle: o["state"] = "idle"
        case .dialing: o["state"] = "dialing"
        case .attached: o["state"] = "attached"
        case .waitingToRedial(let s): o["state"] = "redialing"; o["redial_in_secs"] = .number(s.rounded())
        case .ended(let t): o["state"] = "ended"; o["ended"] = .string(Self.label(t))
        }
        return .object(o)
    }

    static func label(_ t: ReverseAttachClient.Terminal) -> String {
        switch t {
        case .grantExpired: return "grant_expired"
        case .replaced: return "replaced"
        case .sessionEnded: return "session_ended"
        case .revoked: return "revoked"
        case .handshakeRejected(let s): return "handshake_rejected_\(s)"
        case .cancelled: return "cancelled"
        case .deadline: return "deadline"
        }
    }
}
