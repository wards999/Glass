import AppKit
import CGlassCore
import Foundation
import GlassKit

/// "Share this Mac": the Mac as host, shown inside the main window's share
/// card (no window of its own). `detail` is what the card shows while
/// sharing: this Mac's address, the pairing code in large tiles, who is
/// connected, missing permissions, and the last session's results.
final class HostController: NSObject {
    /// A short line for the card's header ("Waiting for a viewer…").
    var onStatus: (String) -> Void = { _ in }

    let detail = NSStackView()
    private var statusLabel: NSTextField!
    private var pinBox: NSStackView!
    private var pinTitle: NSTextField!
    private var pinDigits: [NSTextField] = []
    private var streamLabel: NSTextField!
    private var screenRow: NSStackView!
    private var accessRow: NSStackView!
    private var permNote: NSTextField!
    private var resultsButton: NSButton!
    private var runner: HostRunner?
    private let advertiser = GlassAdvertiser()
    private var lastReport: String?
    private var stopped = false
    private var permissionTimer: Timer?
    private let testCard = TestCardController()

    override init() {
        super.init()
        build()
    }

    func start() {
        refreshPermissions()
        permissionTimer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
            self?.refreshPermissions()
        }
        if !HostPermissions.screenRecording { HostPermissions.requestScreenRecording() }
        if !HostPermissions.accessibility { HostPermissions.requestAccessibility() }
        startSharing()
    }

    func stop() {
        stopped = true
        permissionTimer?.invalidate()
        permissionTimer = nil
        advertiser.stop()
        runner?.stop()
        runner = nil
    }

    // MARK: views

    private func text(_ s: String, size: CGFloat = 12, colour: NSColor = .secondaryLabelColor,
                      weight: NSFont.Weight = .regular) -> NSTextField {
        let l = NSTextField(wrappingLabelWithString: s)
        l.font = .systemFont(ofSize: size, weight: weight)
        l.textColor = colour
        l.isSelectable = true
        return l
    }

    private func permissionRow(_ what: String, _ action: Selector) -> NSStackView {
        let icon = NSImageView(image: NSImage(systemSymbolName: "exclamationmark.triangle.fill", accessibilityDescription: nil) ?? NSImage())
        icon.contentTintColor = .systemOrange
        let label = text(what)
        label.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        let open = NSButton(title: "Open Settings\u{2026}", target: self, action: action)
        open.controlSize = .small
        let row = NSStackView(views: [icon, label, NSView(), open])
        row.spacing = 6
        row.alignment = .centerY
        return row
    }

    private func build() {
        let sep = NSBox()
        sep.boxType = .separator

        let ips = localIPv4Addresses()
        let addrTitle = text("This Mac\u{2019}s address")
        let addr = NSTextField(labelWithString: ips.first ?? "not on a network")
        addr.font = .monospacedSystemFont(ofSize: 13, weight: .medium)
        addr.isSelectable = true
        let addrRow = NSStackView(views: [addrTitle, NSView(), addr])

        // The pairing code, in tiles like the PC's.
        pinTitle = text("", size: 13, colour: .labelColor, weight: .semibold)
        pinTitle.alignment = .center
        let pinHint = text("Type it on that device. Pair only your own devices.", size: 11)
        pinHint.alignment = .center
        let tiles = NSStackView()
        tiles.spacing = 7
        for i in 0..<6 {
            let digit = NSTextField(labelWithString: "")
            digit.font = .monospacedDigitSystemFont(ofSize: 24, weight: .semibold)
            digit.alignment = .center
            let tile = LayerView(fill: Look.track, stroke: nil, radius: 9)
            digit.translatesAutoresizingMaskIntoConstraints = false
            tile.addSubview(digit)
            NSLayoutConstraint.activate([
                tile.widthAnchor.constraint(equalToConstant: 38),
                tile.heightAnchor.constraint(equalToConstant: 48),
                digit.centerXAnchor.constraint(equalTo: tile.centerXAnchor),
                digit.centerYAnchor.constraint(equalTo: tile.centerYAnchor),
            ])
            tiles.addArrangedSubview(tile)
            if i == 2 { tiles.setCustomSpacing(16, after: tile) }
            pinDigits.append(digit)
        }
        let box = NSStackView(views: [pinTitle, pinHint, tiles])
        box.orientation = .vertical
        box.alignment = .centerX
        box.spacing = 4
        box.setCustomSpacing(10, after: pinHint)
        box.isHidden = true
        pinBox = box

        statusLabel = text("Starting\u{2026}")
        streamLabel = text("", size: 11, colour: .tertiaryLabelColor)
        streamLabel.isHidden = true

        screenRow = permissionRow("Screen Recording is needed to show this screen.", #selector(openScreen))
        accessRow = permissionRow("Accessibility is needed for the other device\u{2019}s mouse and keyboard.", #selector(openAccess))
        permNote = text("After allowing Screen Recording, quit Glass (\u{2318}Q) and open it again.", size: 11)

        let results = NSButton(title: "Show Results", target: self, action: #selector(copyResults))
        results.controlSize = .small
        results.isEnabled = false
        resultsButton = results
        let card = NSButton(title: "Clarity Test Card", target: self, action: #selector(showTestCard))
        card.controlSize = .small
        let buttons = NSStackView(views: [results, card, NSView()])

        let views: [NSView] = [sep, addrRow, box, statusLabel, streamLabel, screenRow, accessRow, permNote, buttons]
        for v in views { detail.addArrangedSubview(v) }
        detail.orientation = .vertical
        detail.alignment = .leading
        detail.spacing = 8
        detail.setCustomSpacing(12, after: box)
        for v in views {
            v.widthAnchor.constraint(equalTo: detail.widthAnchor).isActive = true
        }
    }

    private func refreshPermissions() {
        let screen = HostPermissions.screenRecording, access = HostPermissions.accessibility
        screenRow.isHidden = screen
        accessRow.isHidden = access
        permNote.isHidden = screen
    }

    @objc private func openScreen() { HostPermissions.openScreenRecordingSettings() }
    @objc private func openAccess() { HostPermissions.openAccessibilitySettings() }

    // MARK: sharing

    private func startSharing() {
        guard let device = GlassDevice.shared else {
            statusLabel.stringValue = "Couldn\u{2019}t read or create this Mac\u{2019}s Glass identity."
            onStatus("Couldn\u{2019}t start")
            return
        }
        do {
            let r = try HostRunner(device: device, options: .init(), banner: banner)
            r.onEvent = { [weak self] e in self?.event(e) }
            r.onStreamChanged = { [weak self] line in
                self?.testCard.nowShowing = line
                self?.streamLabel.stringValue = "Showing: \(line)"
                self?.streamLabel.isHidden = false
            }
            r.onConnected = { [weak self] name, addr in
                self?.pinBox.isHidden = true
                self?.statusLabel.stringValue = "\u{201C}\(name)\u{201D} (\(addr)) sees this screen and controls the mouse and keyboard. Encrypted. Turn the switch off to end it."
                self?.onStatus("Connected to \u{201C}\(name)\u{201D}")
            }
            r.onSessionEnd = { [weak self] report in
                print(report)
                // Switched off during a session: show the results on their own.
                guard let self, !self.stopped else {
                    showResults(report)
                    return
                }
                self.lastReport = report
                self.resultsButton.isEnabled = true
                self.streamLabel.isHidden = true
                self.statusLabel.stringValue = "Session ended. Show Results to copy them. Waiting for the next viewer."
                self.onStatus("Waiting for a viewer\u{2026}")
            }
            runner = r
            r.start()
            advertiser.start(name: GlassDevice.computerName, port: r.host.port,
                             fingerprintPrefix: device.fingerprintHexPrefix, ip: localIPv4Addresses().first)
            let d = r.display
            statusLabel.stringValue = "Other devices see this Mac in their Glass list. Sharing \(d.width)\u{00D7}\(d.height) at \(d.fps) Hz."
            onStatus("Waiting for a viewer\u{2026}")
        } catch {
            statusLabel.stringValue = "\(error)"
            onStatus("Couldn\u{2019}t start")
        }
    }

    private func event(_ e: GlassHostEvent) {
        print(e.text)
        switch e.kind {
        case .pairingPin:
            pinTitle.stringValue = "Pairing code for \u{201C}\(e.device)\u{201D}"
            for (label, c) in zip(pinDigits, e.pin) { label.stringValue = String(c) }
            pinBox.isHidden = false
            statusLabel.stringValue = "\u{201C}\(e.device)\u{201D} (\(e.address)) wants to pair. The code is valid for 2 minutes."
            onStatus("Pairing with \u{201C}\(e.device)\u{201D}")
            NSApp.activate(ignoringOtherApps: true)
            _ = NSApp.requestUserAttention(.criticalRequest)
        case .paired:
            pinBox.isHidden = true
            statusLabel.stringValue = e.text
        default:
            pinBox.isHidden = true
            statusLabel.stringValue = e.text
        }
    }

    @objc private func copyResults() {
        guard let r = lastReport else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(r, forType: .string)
        showResults(r)
    }

    @objc private func showTestCard() { testCard.show() }
}
