import Foundation
import XCTest
@testable import InstanceMCPCore

/// #12: grants survive a daemon restart. These use `FileGrantStore` against a temp
/// directory with an in-memory `SecretStore` — CI has no usable login Keychain,
/// and the Keychain backend is exercised live on the Mac instead.
final class GrantPersistenceTests: XCTestCase {
    private var dir: URL!

    override func setUp() {
        dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("grants-\(UUID().uuidString)", isDirectory: true)
    }

    override func tearDown() {
        try? FileManager.default.removeItem(at: dir)
    }

    private func store(_ secrets: InMemorySecretStore) -> FileGrantStore {
        FileGrantStore(url: dir.appendingPathComponent("sub/grants.json"), secrets: secrets)
    }

    private func grant(_ id: String, expiresIn: TimeInterval) -> PersistedGrant {
        PersistedGrant(id: id, runtime: URL(string: "ws://127.0.0.1:9")!, session: "s", profile: "sandbox",
                       principal: "a@b", createdAt: Date(), expiresAt: Date().addingTimeInterval(expiresIn),
                       secret: "secret-\(id)")
    }

    private func mode(_ path: String) -> Int {
        ((try? FileManager.default.attributesOfItem(atPath: path))?[.posixPermissions] as? NSNumber)?.intValue ?? -1
    }

    // MARK: store

    func testRoundTripKeepsTheSecretOutOfTheFile() throws {
        let secrets = InMemorySecretStore()
        let s = store(secrets)
        s.save([grant("a", expiresIn: 600), grant("b", expiresIn: 600)])

        XCTAssertEqual(mode(s.url.path) & 0o777, 0o600)
        XCTAssertEqual(mode(s.url.deletingLastPathComponent().path) & 0o777, 0o700)
        let onDisk = try String(contentsOf: s.url, encoding: .utf8)
        XCTAssertFalse(onDisk.contains("secret-a"), "secret must not be in the record file")
        XCTAssertEqual(secrets.accounts, ["a", "b"])

        let loaded = s.load()
        XCTAssertEqual(loaded.map(\.id), ["a", "b"])
        XCTAssertEqual(loaded.map(\.secret), ["secret-a", "secret-b"])
    }

    func testRemovedGrantsLoseTheirSecrets() {
        let secrets = InMemorySecretStore()
        let s = store(secrets)
        s.save([grant("a", expiresIn: 600), grant("b", expiresIn: 600)])
        s.save([grant("b", expiresIn: 600)])
        XCTAssertEqual(secrets.accounts, ["b"])
        XCTAssertEqual(s.load().map(\.id), ["b"])
    }

    func testExpiredOrSecretlessRecordsAreDroppedOnLoad() {
        let secrets = InMemorySecretStore()
        let s = store(secrets)
        s.save([grant("live", expiresIn: 600), grant("stale", expiresIn: -1), grant("orphan", expiresIn: 600)])
        secrets.delete(account: "orphan")
        XCTAssertEqual(s.load().map(\.id), ["live"])
    }

    func testAWidenedFileIsNarrowedBeforeItIsRead() {
        let s = store(InMemorySecretStore())
        s.save([grant("a", expiresIn: 600)])
        try? FileManager.default.setAttributes([.posixPermissions: 0o644], ofItemAtPath: s.url.path)
        XCTAssertEqual(s.load().count, 1)
        XCTAssertEqual(mode(s.url.path) & 0o777, 0o600)
    }

    func testMissingOrCorruptFileIsEmpty() throws {
        let s = store(InMemorySecretStore())
        XCTAssertTrue(s.load().isEmpty)
        try FileManager.default.createDirectory(at: s.url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try Data("not json".utf8).write(to: s.url)
        XCTAssertTrue(s.load().isEmpty)
    }

    // MARK: manager

    private func request(_ session: String, secret: String = "abc", ttl: TimeInterval = 60) -> AttachManager.Request {
        // Unreachable runtime: the grant exists and its client keeps redialling.
        AttachManager.Request(runtime: URL(string: "ws://127.0.0.1:9")!, session: session, profile: .sandbox,
                              ttl: ttl, secret: secret, adminCredential: nil)
    }

    func testAGrantIsResumedUnderTheSameIdAfterARestart() async throws {
        let secrets = InMemorySecretStore()
        let first = AttachManager(server: fullServer(), store: store(secrets))
        let created = try await first.create(request("laptop", secret: "k1"), principal: "a@b")
        XCTAssertEqual(secrets.get(account: created.id), "k1")

        // "Restart": a fresh manager over the same store. The old one is simply
        // abandoned, as a killed process would be.
        let second = AttachManager(server: fullServer(), store: store(secrets))
        let resumed = await second.resume()
        XCTAssertEqual(resumed, 1)
        let grants = await second.list()
        XCTAssertEqual(grants.map(\.id), [created.id])
        XCTAssertEqual(grants.first?.session, "laptop")
        XCTAssertEqual(grants.first?.profile, .sandbox)
        XCTAssertEqual(grants.first?.principal, "a@b")
        XCTAssertEqual(grants.first?.expiresAt.timeIntervalSince1970 ?? 0,
                       created.expiresAt.timeIntervalSince1970, accuracy: 1)

        await second.revoke(created.id)
        await first.revoke(created.id)
    }

    func testARevokedGrantIsNotResumed() async throws {
        let secrets = InMemorySecretStore()
        let mgr = AttachManager(server: fullServer(), store: store(secrets))
        let g = try await mgr.create(request("laptop"), principal: "a@b")
        await mgr.revoke(g.id)
        XCTAssertTrue(secrets.accounts.isEmpty, "revoke deletes the Keychain secret")
        let next = AttachManager(server: fullServer(), store: store(secrets))
        let resumed = await next.resume()
        XCTAssertEqual(resumed, 0)
        let remaining = await next.list()
        XCTAssertTrue(remaining.isEmpty)
    }

    func testAReplacedGrantLeavesOnlyTheNewOnePersisted() async throws {
        let secrets = InMemorySecretStore()
        let mgr = AttachManager(server: fullServer(), store: store(secrets))
        let old = try await mgr.create(request("laptop", secret: "old"), principal: "a@b")
        let new = try await mgr.create(request("laptop", secret: "new"), principal: "a@b")
        XCTAssertEqual(secrets.accounts, [new.id])
        XCTAssertNil(secrets.get(account: old.id))
        await mgr.revoke(new.id)
    }

    func testWithoutAStoreNothingIsPersisted() async throws {
        let mgr = AttachManager(server: fullServer())
        let g = try await mgr.create(request("laptop"), principal: "a@b")
        let resumed = await mgr.resume()
        XCTAssertEqual(resumed, 0)
        await mgr.revoke(g.id)
    }
}
