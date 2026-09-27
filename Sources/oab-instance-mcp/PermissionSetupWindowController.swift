import AppKit
import InstanceMCPCore

/// First-run guidance for the three optional macOS TCC capabilities. This window
/// can open settings and test; it cannot and must not grant a permission itself.
///
/// No timer polls TCC and no screenshot is taken automatically. That is
/// deliberate: Connect's old screenshot polling caused one macOS permission
/// dialog per call. Here every re-test is either the user's explicit click or the
/// single refresh when they return from System Settings.
@MainActor
final class PermissionSetupWindowController: NSWindowController, NSWindowDelegate {
    private var statusLabels: [PermissionKind: NSTextField] = [:]
    private var actionButtons: [PermissionKind: NSButton] = [:]
    private let summaryLabel = NSTextField(labelWithString: "")
    private let finishButton = NSButton(title: "Not Now", target: nil, action: nil)
    private let testButton = NSButton(title: "Test Again", target: nil, action: nil)
    private let onFinish: () -> Void
    private var activationObserver: NSObjectProtocol?
    private(set) var snapshot = PermissionProbe.current()

    init(onFinish: @escaping () -> Void) {
        self.onFinish = onFinish
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 650, height: 510),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false
        )
        window.title = "Set Up Mac Permissions"
        window.isReleasedWhenClosed = false
        window.center()
        super.init(window: window)
        window.delegate = self
        buildUI(in: window)
        activationObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.didBecomeActiveNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.refresh() }
        }
        refresh()
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    deinit {
        if let activationObserver { NotificationCenter.default.removeObserver(activationObserver) }
    }

    func show() {
        refresh()
        NSApp.activate(ignoringOtherApps: true)
        window?.center()
        window?.makeKeyAndOrderFront(nil)
    }

    func refresh() {
        snapshot = PermissionProbe.current()
        for kind in PermissionKind.allCases {
            let state = snapshot[kind]
            let label = statusLabels[kind]
            label?.stringValue = state.statusText
            label?.textColor = state.statusColor
            let button = actionButtons[kind]
            button?.title = state.isGranted ? "Granted" : "Open Settings"
            button?.isEnabled = !state.isGranted
        }
        summaryLabel.stringValue = snapshot.allGranted
            ? "All three permissions are ready. Agents can see, control, and access protected files on this Mac."
            : "\(snapshot.grantedCount) of 3 ready. Grant only the capabilities you want this Mac to lend."
        summaryLabel.textColor = snapshot.allGranted ? .systemGreen : .secondaryLabelColor
        finishButton.title = snapshot.allGranted ? "Done" : "Not Now"
    }

    func windowWillClose(_ notification: Notification) { onFinish() }

    // MARK: UI

    private func buildUI(in window: NSWindow) {
        guard let content = window.contentView else { return }
        let stack = NSStackView()
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 14
        stack.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 28),
            stack.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -28),
            stack.topAnchor.constraint(equalTo: content.topAnchor, constant: 24),
            stack.bottomAnchor.constraint(lessThanOrEqualTo: content.bottomAnchor, constant: -22),
        ])

        let title = NSTextField(labelWithString: "Give agents only the capabilities you choose")
        title.font = .systemFont(ofSize: 20, weight: .semibold)
        stack.addArrangedSubview(title)

        let intro = wrappingLabel("oab-instance-mcp runs in your logged-in desktop session. macOS requires you to approve each sensitive capability in System Settings. The app cannot approve them for you, and it never bypasses TCC.")
        stack.addArrangedSubview(intro)

        summaryLabel.font = .systemFont(ofSize: 12, weight: .medium)
        summaryLabel.maximumNumberOfLines = 2
        stack.addArrangedSubview(summaryLabel)

        for kind in PermissionKind.allCases { stack.addArrangedSubview(row(for: kind)) }

        let optional = wrappingLabel("Browser tools (navigate, read the DOM, click, type) work without these permissions. Choose Not Now if this Mac will only lend its browser.")
        optional.font = .systemFont(ofSize: 11)
        optional.textColor = .secondaryLabelColor
        stack.addArrangedSubview(optional)

        let footer = NSStackView()
        footer.orientation = .horizontal
        footer.alignment = .centerY
        footer.spacing = 10
        let spacer = NSView()
        footer.addArrangedSubview(spacer)
        testButton.target = self
        testButton.action = #selector(testAgain)
        footer.addArrangedSubview(testButton)
        finishButton.target = self
        finishButton.action = #selector(finish)
        finishButton.keyEquivalent = "\r"
        footer.addArrangedSubview(finishButton)
        footer.translatesAutoresizingMaskIntoConstraints = false
        stack.addArrangedSubview(footer)
        footer.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true
    }

    private func row(for kind: PermissionKind) -> NSView {
        let box = NSBox()
        box.boxType = .custom
        box.cornerRadius = 8
        box.borderColor = .separatorColor
        box.borderWidth = 1
        box.fillColor = .controlBackgroundColor
        box.contentViewMargins = NSSize(width: 14, height: 10)
        box.translatesAutoresizingMaskIntoConstraints = false
        // NSBox does not derive an intrinsic height from an assigned contentView;
        // without this an NSStackView collapses all three cards onto one row.
        box.widthAnchor.constraint(equalToConstant: 594).isActive = true
        box.heightAnchor.constraint(equalToConstant: 72).isActive = true

        let icon = NSImageView()
        icon.image = NSImage(systemSymbolName: kind.symbolName, accessibilityDescription: kind.title)
        icon.symbolConfiguration = .init(pointSize: 20, weight: .regular)
        icon.contentTintColor = .secondaryLabelColor
        icon.widthAnchor.constraint(equalToConstant: 30).isActive = true

        let title = NSTextField(labelWithString: kind.title)
        title.font = .systemFont(ofSize: 13, weight: .semibold)
        let detail = wrappingLabel(kind.detail)
        detail.font = .systemFont(ofSize: 11)
        detail.textColor = .secondaryLabelColor
        let labels = NSStackView(views: [title, detail])
        labels.orientation = .vertical
        labels.alignment = .leading
        labels.spacing = 2

        let status = NSTextField(labelWithString: "Testing…")
        status.font = .systemFont(ofSize: 12, weight: .medium)
        status.alignment = .right
        status.widthAnchor.constraint(equalToConstant: 82).isActive = true
        statusLabels[kind] = status

        let action = NSButton(title: "Open Settings", target: self, action: #selector(openSettings(_:)))
        action.tag = PermissionKind.allCases.firstIndex(of: kind) ?? 0
        action.widthAnchor.constraint(equalToConstant: 105).isActive = true
        actionButtons[kind] = action

        let row = NSStackView(views: [icon, labels, status, action])
        row.orientation = .horizontal
        row.alignment = .centerY
        row.spacing = 10
        labels.setContentHuggingPriority(.defaultLow, for: .horizontal)
        labels.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        row.translatesAutoresizingMaskIntoConstraints = false
        row.widthAnchor.constraint(equalToConstant: 566).isActive = true
        box.contentView = row
        return box
    }

    private func wrappingLabel(_ text: String) -> NSTextField {
        let label = NSTextField(wrappingLabelWithString: text)
        label.maximumNumberOfLines = 0
        label.lineBreakMode = .byWordWrapping
        return label
    }

    // MARK: actions

    @objc private func testAgain() { refresh() }

    @objc private func finish() {
        onFinish()
        // Avoid a duplicate callback from windowWillClose. `onFinish` only sets
        // one boolean today, so duplicate would be harmless, but the ownership is
        // clearer if the close delegate is temporarily detached.
        window?.delegate = nil
        close()
        window?.delegate = self
    }

    @objc private func openSettings(_ sender: NSButton) {
        guard PermissionKind.allCases.indices.contains(sender.tag) else { return }
        let kind = PermissionKind.allCases[sender.tag]
        NSWorkspace.shared.open(kind.settingsURL)
    }
}

private extension PermissionKind {
    var title: String {
        switch self {
        case .screenRecording: return "Screen & System Audio Recording"
        case .accessibility: return "Accessibility"
        case .fullDiskAccess: return "Full Disk Access"
        }
    }

    var detail: String {
        switch self {
        case .screenRecording: return "Lets screenshot and OpenAB Connect's Screens pane see this desktop."
        case .accessibility: return "Lets mouse and key tools control the pointer and keyboard."
        case .fullDiskAccess: return "Lets exec/osascript open protected Mail, Messages, Safari, and other user data."
        }
    }

    var symbolName: String {
        switch self {
        case .screenRecording: return "rectangle.inset.filled.and.person.filled"
        case .accessibility: return "accessibility"
        case .fullDiskAccess: return "internaldrive"
        }
    }

    var settingsURL: URL {
        let pane: String
        switch self {
        case .screenRecording: pane = "Privacy_ScreenCapture"
        case .accessibility: pane = "Privacy_Accessibility"
        case .fullDiskAccess: pane = "Privacy_AllFiles"
        }
        return URL(string: "x-apple.systempreferences:com.apple.preference.security?\(pane)")!
    }
}

private extension PermissionState {
    var statusText: String {
        switch self {
        case .granted: return "✓ Granted"
        case .denied: return "✗ Not granted"
        case .unknown: return "? Can't test"
        }
    }

    var statusColor: NSColor {
        switch self {
        case .granted: return .systemGreen
        case .denied: return .systemRed
        case .unknown: return .systemOrange
        }
    }
}
