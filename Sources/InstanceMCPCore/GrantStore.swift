import Foundation
import Security

/// Grants survive a daemon restart (#12).
///
/// A restart — deploy, crash, logout/login, `launchctl kickstart -k` — used to drop
/// every grant, and the human had to lend the Mac again from Connect/Remote. Live
/// grants are now persisted and re-dialled at start under their original id.
///
/// Split by sensitivity:
/// - the **record** (id, runtime, session, profile, principal, dates) is JSON in
///   `~/Library/Application Support/oab-instance-mcp/grants.json`, mode 600 in a
///   0700 directory — inspectable, no secret in it;
/// - the **attach secret** is a generic password in the login Keychain, service
///   `dev.openab.instance-mcp.grant`, account = grant id, accessible only after
///   first unlock on this device. The Keychain ACL binds it to this code-signing
///   identity, like the TCC grants.
///
/// Ended, revoked and expired grants are never written; a secret whose record is
/// gone is deleted on the next save.
public struct PersistedGrant: Codable, Sendable, Equatable {
    public var id: String
    public var runtime: URL
    public var session: String
    public var profile: String
    public var principal: String
    public var createdAt: Date
    public var expiresAt: Date
    /// Not encoded into the record file; carried to and from the secret store.
    public var secret: String

    enum CodingKeys: String, CodingKey { case id, runtime, session, profile, principal, createdAt, expiresAt }

    public init(id: String, runtime: URL, session: String, profile: String, principal: String,
                createdAt: Date, expiresAt: Date, secret: String) {
        self.id = id; self.runtime = runtime; self.session = session; self.profile = profile
        self.principal = principal; self.createdAt = createdAt; self.expiresAt = expiresAt
        self.secret = secret
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        runtime = try c.decode(URL.self, forKey: .runtime)
        session = try c.decode(String.self, forKey: .session)
        profile = try c.decode(String.self, forKey: .profile)
        principal = try c.decode(String.self, forKey: .principal)
        createdAt = try c.decode(Date.self, forKey: .createdAt)
        expiresAt = try c.decode(Date.self, forKey: .expiresAt)
        secret = ""
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(id, forKey: .id); try c.encode(runtime, forKey: .runtime)
        try c.encode(session, forKey: .session); try c.encode(profile, forKey: .profile)
        try c.encode(principal, forKey: .principal)
        try c.encode(createdAt, forKey: .createdAt); try c.encode(expiresAt, forKey: .expiresAt)
    }
}

public protocol GrantStore: Sendable {
    /// Grants still inside their deadline, with their secrets.
    func load() -> [PersistedGrant]
    /// Replace the persisted set with exactly `grants`.
    func save(_ grants: [PersistedGrant])
}

/// Where attach secrets live. The Keychain in production; a dictionary in tests
/// (CI runners have no usable login keychain).
public protocol SecretStore: Sendable {
    func set(_ secret: String, account: String) -> Bool
    func get(account: String) -> String?
    func delete(account: String)
}

public final class FileGrantStore: GrantStore, @unchecked Sendable {
    public let url: URL
    private let secrets: SecretStore
    private let log: @Sendable (String) -> Void
    private let lock = NSLock()

    public static var defaultURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/oab-instance-mcp/grants.json")
    }

    public init(url: URL = FileGrantStore.defaultURL, secrets: SecretStore = KeychainSecretStore(),
                log: @escaping @Sendable (String) -> Void = { _ in }) {
        self.url = url; self.secrets = secrets; self.log = log
    }

    private struct File: Codable { var version: Int; var grants: [PersistedGrant] }

    private func readRecords() -> [PersistedGrant] {
        guard let data = try? Data(contentsOf: url) else { return [] }
        guard let file = try? Self.decoder.decode(File.self, from: data), file.version == 1 else {
            log("grants: ignoring unreadable \(url.path)")
            return []
        }
        return file.grants
    }

    public func load() -> [PersistedGrant] {
        lock.lock(); defer { lock.unlock() }
        narrowPermissions()
        let now = Date()
        return readRecords().compactMap { record in
            guard record.expiresAt > now else { return nil }
            guard let secret = secrets.get(account: record.id) else {
                log("grants: \(record.id.prefix(8)) has no secret in the Keychain; dropping it")
                return nil
            }
            var g = record; g.secret = secret
            return g
        }
    }

    public func save(_ grants: [PersistedGrant]) {
        lock.lock(); defer { lock.unlock() }
        let previous = Set(readRecords().map(\.id))
        let current = Set(grants.map(\.id))
        for g in grants where !secrets.set(g.secret, account: g.id) {
            log("grants: could not store the secret for \(g.id.prefix(8)) in the Keychain")
        }
        do {
            let dir = url.deletingLastPathComponent()
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true,
                                                    attributes: [.posixPermissions: 0o700])
            let data = try Self.encoder.encode(File(version: 1, grants: grants))
            let tmp = dir.appendingPathComponent(url.lastPathComponent + ".tmp")
            try? FileManager.default.removeItem(at: tmp)
            guard FileManager.default.createFile(atPath: tmp.path, contents: data,
                                                 attributes: [.posixPermissions: 0o600]) else {
                throw CocoaError(.fileWriteUnknown)
            }
            _ = try FileManager.default.replaceItemAt(url, withItemAt: tmp)
        } catch {
            log("grants: could not persist to \(url.path): \(error)")
            return
        }
        for gone in previous.subtracting(current) { secrets.delete(account: gone) }
    }

    private func narrowPermissions() {
        guard let attrs = try? FileManager.default.attributesOfItem(atPath: url.path),
              let mode = (attrs[.posixPermissions] as? NSNumber)?.intValue, mode & 0o077 != 0 else { return }
        log("grants: \(url.path) was mode \(String(mode, radix: 8)); resetting to 600")
        try? FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: url.path)
    }

    private static let encoder: JSONEncoder = {
        let e = JSONEncoder(); e.dateEncodingStrategy = .iso8601; e.outputFormatting = [.sortedKeys]; return e
    }()
    private static let decoder: JSONDecoder = {
        let d = JSONDecoder(); d.dateDecodingStrategy = .iso8601; return d
    }()
}

public struct KeychainSecretStore: SecretStore {
    public static let service = "dev.openab.instance-mcp.grant"
    public init() {}

    private func query(_ account: String) -> [String: Any] {
        [kSecClass as String: kSecClassGenericPassword,
         kSecAttrService as String: Self.service,
         kSecAttrAccount as String: account]
    }

    public func set(_ secret: String, account: String) -> Bool {
        SecItemDelete(query(account) as CFDictionary)
        var add = query(account)
        add[kSecValueData as String] = Data(secret.utf8)
        add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        add[kSecAttrLabel as String] = "oab-instance-mcp attach grant"
        return SecItemAdd(add as CFDictionary, nil) == errSecSuccess
    }

    public func get(account: String) -> String? {
        var q = query(account)
        q[kSecReturnData as String] = true
        q[kSecMatchLimit as String] = kSecMatchLimitOne
        var out: CFTypeRef?
        guard SecItemCopyMatching(q as CFDictionary, &out) == errSecSuccess, let data = out as? Data else { return nil }
        return String(decoding: data, as: UTF8.self)
    }

    public func delete(account: String) {
        SecItemDelete(query(account) as CFDictionary)
    }
}

/// Test double, and a fallback nothing in production uses.
public final class InMemorySecretStore: SecretStore, @unchecked Sendable {
    private var values: [String: String] = [:]
    private let lock = NSLock()
    public init() {}
    public func set(_ secret: String, account: String) -> Bool { lock.lock(); values[account] = secret; lock.unlock(); return true }
    public func get(account: String) -> String? { lock.lock(); defer { lock.unlock() }; return values[account] }
    public func delete(account: String) { lock.lock(); values[account] = nil; lock.unlock() }
    public var accounts: [String] { lock.lock(); defer { lock.unlock() }; return values.keys.sorted() }
}
