import Foundation
import XCTest
@testable import InstanceMCPCore

/// Shared conformance vectors in `conformance/` (repo root). The Swift
/// implementation is the behavioural oracle; the Linux Rust port
/// (`poc/reverse-attach-linux`) runs the same files in its `cargo test`, so a
/// change to either side that alters behaviour fails one of the two suites.
/// Edit a vector only when the behaviour change is intended on **both** sides.
final class ConformanceVectorTests: XCTestCase {
    private func load(_ name: String) throws -> [String: Any] {
        let root = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()   // InstanceMCPCoreTests
            .deletingLastPathComponent()   // Tests
            .deletingLastPathComponent()   // repo root
        let data = try Data(contentsOf: root.appendingPathComponent("conformance/\(name)"))
        return try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
    }

    func testAuthPolicyVectors() throws {
        let doc = try load("auth_vectors.json")
        let cases = try XCTUnwrap(doc["cases"] as? [[String: Any]])
        XCTAssertGreaterThanOrEqual(cases.count, 16)
        for c in cases {
            let name = c["name"] as? String ?? "?"
            let config = c["config"] as? [String: Any] ?? [:]
            let policy = AuthPolicy(
                allowedLogins: Set(config["allowedLogins"] as? [String] ?? []),
                bearerToken: config["bearerToken"] as? String,
                allowLocalUnauthenticated: config["allowLocalUnauthenticated"] as? Bool ?? false)
            if c["validate"] as? String == "error" {
                XCTAssertThrowsError(try policy.validate(), name)
                continue
            }
            var headers: [String: String] = [:]
            for (k, v) in c["headers"] as? [String: String] ?? [:] { headers[k.lowercased()] = v }
            let got = policy.decide(headers: headers,
                                    remoteIsLoopback: c["remoteIsLoopback"] as? Bool ?? false)
            let expect = c["expect"] as? [String: String] ?? [:]
            if let principal = expect["allow"] {
                XCTAssertEqual(got, .allow(principal: principal), name)
            } else if let reason = expect["deny"] {
                XCTAssertEqual(got, .deny(reason: reason), name)
            } else {
                XCTFail("\(name): vector has no expect")
            }
        }
    }

    func testReverseAttachVectors() throws {
        let doc = try load("reverse_attach_vectors.json")
        let terminals: [String: ReverseAttachClient.Terminal] = [
            "grantExpired": .grantExpired, "replaced": .replaced,
            "sessionEnded": .sessionEnded, "revoked": .revoked,
        ]
        var n = 0
        for c in try XCTUnwrap(doc["close_code"] as? [[String: Any]]) {
            n += 1
            let code = try XCTUnwrap(c["code"] as? Int)
            let expect = c["expect"] as? [String: Any] ?? [:]
            let want: ReverseAttachClient.Disposition =
                (expect["stop"] as? String).map { .stop(terminals[$0]!) } ?? .redial
            XCTAssertEqual(ReverseAttachClient.disposition(closeCode: code), want, "close \(code)")
        }
        for c in try XCTUnwrap(doc["handshake_status"] as? [[String: Any]]) {
            n += 1
            let status = try XCTUnwrap(c["status"] as? Int)
            let expect = c["expect"] as? [String: Any] ?? [:]
            let want: ReverseAttachClient.Disposition =
                (expect["stop_rejected"] as? Int).map { .stop(.handshakeRejected($0)) } ?? .redial
            XCTAssertEqual(ReverseAttachClient.disposition(handshakeStatus: status), want,
                           "handshake \(status)")
        }
        for c in try XCTUnwrap(doc["attach_url"] as? [[String: String]]) {
            n += 1
            let config = ReverseAttachClient.Config(
                runtime: URL(string: c["runtime"]!)!, session: c["session"]!, secret: "x",
                profile: .sandbox, deadline: Date())
            XCTAssertEqual(config.attachURL.absoluteString, c["expect"])
        }
        XCTAssertGreaterThanOrEqual(n, 23)
    }
}
