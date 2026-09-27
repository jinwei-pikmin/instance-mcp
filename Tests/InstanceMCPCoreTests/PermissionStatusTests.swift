import XCTest
@testable import InstanceMCPCore

final class PermissionProbeTests: XCTestCase {
    func testFullDiskAccessGrantedWhenAnExistingProtectedFileOpens() {
        var opened: [String] = []
        let state = PermissionProbe.fullDiskAccess(
            paths: ["missing", "denied", "readable", "after"],
            exists: { $0 != "missing" },
            openForRead: { path in opened.append(path); return path == "readable" }
        )
        XCTAssertEqual(state, .granted)
        XCTAssertEqual(opened, ["denied", "readable"], "stop after the first successful open")
    }

    func testFullDiskAccessDeniedWhenProbeFilesExistButNoneOpen() {
        let state = PermissionProbe.fullDiskAccess(
            paths: ["tcc", "safari", "messages"],
            exists: { _ in true },
            openForRead: { _ in false }
        )
        XCTAssertEqual(state, .denied)
    }

    func testFullDiskAccessUnknownWhenNoProbeFileExists() {
        var opens = 0
        let state = PermissionProbe.fullDiskAccess(
            paths: ["tcc", "safari"],
            exists: { _ in false },
            openForRead: { _ in opens += 1; return true }
        )
        XCTAssertEqual(state, .unknown)
        XCTAssertEqual(opens, 0)
    }

    func testCandidatePathsStayInsideTheHomeAndUseProtectedDatabases() {
        let paths = PermissionProbe.fullDiskAccessCandidatePaths(homeDirectory: "/Users/tester")
        XCTAssertEqual(paths, [
            "/Users/tester/Library/Application Support/com.apple.TCC/TCC.db",
            "/Users/tester/Library/Safari/History.db",
            "/Users/tester/Library/Messages/chat.db",
        ])
    }

    func testSnapshotSummary() {
        let partial = PermissionSnapshot(screenRecording: .granted, accessibility: .denied,
                                         fullDiskAccess: .unknown)
        XCTAssertFalse(partial.allGranted)
        XCTAssertEqual(partial.grantedCount, 1)
        XCTAssertEqual(partial[.accessibility], .denied)
        let all = PermissionSnapshot(screenRecording: .granted, accessibility: .granted,
                                     fullDiskAccess: .granted)
        XCTAssertTrue(all.allGranted)
        XCTAssertEqual(all.grantedCount, 3)
    }
}

final class PermissionSetupPolicyTests: XCTestCase {
    let complete = PermissionSnapshot(screenRecording: .granted, accessibility: .granted,
                                      fullDiskAccess: .granted)
    let missing = PermissionSnapshot(screenRecording: .denied, accessibility: .granted,
                                     fullDiskAccess: .granted)

    func testFirstRunWithMissingPermissionAutoShows() {
        XCTAssertTrue(PermissionSetupPolicy.shouldAutoShow(hasShown: false, snapshot: missing))
    }

    func testNotNowIsRespectedAcrossLaterLaunches() {
        XCTAssertFalse(PermissionSetupPolicy.shouldAutoShow(hasShown: true, snapshot: missing))
    }

    func testFullyGrantedMachineNeverAutoShows() {
        XCTAssertFalse(PermissionSetupPolicy.shouldAutoShow(hasShown: false, snapshot: complete))
        XCTAssertFalse(PermissionSetupPolicy.shouldAutoShow(hasShown: true, snapshot: complete))
    }

    func testUnknownFDAIsNotReportedAsComplete() {
        let unknown = PermissionSnapshot(screenRecording: .granted, accessibility: .granted,
                                         fullDiskAccess: .unknown)
        XCTAssertTrue(PermissionSetupPolicy.shouldAutoShow(hasShown: false, snapshot: unknown))
    }
}
