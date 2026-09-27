import ApplicationServices
import CoreGraphics
import Darwin
import Foundation

/// The three macOS TCC capabilities this daemon can use. Browser-only operation
/// needs none; each row in the setup window explains which tools it unlocks.
public enum PermissionKind: String, CaseIterable, Sendable {
    case screenRecording
    case accessibility
    case fullDiskAccess
}

/// `unknown` is meaningful for FDA: macOS has no public preflight API, and a Mac
/// with none of our protected probe files gives us no honest measurement.
public enum PermissionState: Equatable, Sendable {
    case granted
    case denied
    case unknown

    public var isGranted: Bool { self == .granted }
}

public struct PermissionSnapshot: Equatable, Sendable {
    public var screenRecording: PermissionState
    public var accessibility: PermissionState
    public var fullDiskAccess: PermissionState

    public init(screenRecording: PermissionState, accessibility: PermissionState,
                fullDiskAccess: PermissionState) {
        self.screenRecording = screenRecording
        self.accessibility = accessibility
        self.fullDiskAccess = fullDiskAccess
    }

    public subscript(_ kind: PermissionKind) -> PermissionState {
        switch kind {
        case .screenRecording: return screenRecording
        case .accessibility: return accessibility
        case .fullDiskAccess: return fullDiskAccess
        }
    }

    public var allGranted: Bool {
        PermissionKind.allCases.allSatisfy { self[$0].isGranted }
    }

    public var grantedCount: Int {
        PermissionKind.allCases.filter { self[$0].isGranted }.count
    }
}

/// Testable probes for the shipping permission rules.
public enum PermissionProbe {
    /// Snapshot using Apple's public preflight APIs plus an actual protected-file
    /// open for Full Disk Access.
    public static func current(homeDirectory: String = NSHomeDirectory()) -> PermissionSnapshot {
        PermissionSnapshot(
            screenRecording: CGPreflightScreenCaptureAccess() ? .granted : .denied,
            accessibility: AXIsProcessTrusted() ? .granted : .denied,
            fullDiskAccess: fullDiskAccess(homeDirectory: homeDirectory)
        )
    }

    /// Paths protected by `kTCCServiceSystemPolicyAllFiles`. We do not read or
    /// inspect any content: a successful `open` is immediately followed by
    /// `close`. The user's own TCC.db exists on every normal desktop account;
    /// Safari/Messages are fallbacks for unusual layouts.
    public static func fullDiskAccessCandidatePaths(homeDirectory: String) -> [String] {
        let home = URL(fileURLWithPath: homeDirectory, isDirectory: true)
        return [
            "Library/Application Support/com.apple.TCC/TCC.db",
            "Library/Safari/History.db",
            "Library/Messages/chat.db",
        ].map { home.appendingPathComponent($0).path }
    }

    public static func fullDiskAccess(homeDirectory: String = NSHomeDirectory()) -> PermissionState {
        fullDiskAccess(
            paths: fullDiskAccessCandidatePaths(homeDirectory: homeDirectory),
            exists: { FileManager.default.fileExists(atPath: $0) },
            openForRead: { path in
                let fd = Darwin.open(path, O_RDONLY | O_CLOEXEC)
                guard fd >= 0 else { return false }
                Darwin.close(fd)
                return true
            }
        )
    }

    /// Injection seam: tests cover granted, denied and no-probe-file without
    /// depending on the CI runner's own TCC database.
    public static func fullDiskAccess(
        paths: [String],
        exists: (String) -> Bool,
        openForRead: (String) -> Bool
    ) -> PermissionState {
        var found = false
        for path in paths where exists(path) {
            found = true
            if openForRead(path) { return .granted }
        }
        return found ? .denied : .unknown
    }
}

/// One-time auto-show policy. "Not Now" is respected across launches and
/// versions; the menu item remains available forever. A fully granted machine
/// never gets an onboarding window just because it upgraded to 0.6.3.
public enum PermissionSetupPolicy {
    public static func shouldAutoShow(hasShown: Bool, snapshot: PermissionSnapshot) -> Bool {
        !hasShown && !snapshot.allGranted
    }
}
