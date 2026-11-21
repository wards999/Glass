import AppKit
import CGlassCore
import Foundation
import GlassKit

/// "Share This Mac": the Mac as host. Shows what the PC needs (address,
/// PIN when pairing), the permissions macOS needs, who is connected, and
/// the last session's results.
final class HostController: NSObject, NSWindowDelegate {
    var onClose: () -> Void = {}

    private var window: NSWindow!
    private var statusLabel: NSTextField!
    private var pinLabel: NSTextField!
    private var addressLabel: NSTextField!
    private var screenRow: NSTextField!
    private var screenButton: NSButton!
    private var accessRow: NSTextField!
    private var accessButton: NSButton!
    private var copyButton: NSButton!
    private var runner: HostRunner?
    private let advertiser = GlassAdvertiser()
    private var lastReport: String?
    private var closed = false
    private var permissionTimer: Timer?
    private var streamLabel: NSTextField!
    private let testCard = TestCardController()

    func open() {
        buildWindow()
        refreshPermissions()
        permissionTimer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
            self?.refreshPermissions()
        }
        window.center()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        if !HostPermissions.screenRecording { HostPermissions.requestScreenRecording() }
        if !HostPermissions.accessibility { HostPermissions.requestAccessibility() }
        startSharing()
    }

    private func label(_ s: String, size: CGFloat = 13, weight: NSFont.Weight = .regular) -> NSTextField {
        let l = NSTextField(wrappingLabelWithString: s)
        l.font = .systemFont(ofSize: size, weight: weight)
        l.isSelectable = true
        return l
    }

    private func buildWindow() {
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 560, height: 380),
                         styleMask: [.titled, .closable, .miniaturizable], backing: .buffered, defer: false)
        w.title = "Glass — Share This Mac"
        w.isReleasedWhenClosed = false
        w.delegate = self
        window = w

        let device = GlassDevice.shared
        let id = device.map { d in
            d.fingerprintShort
        } ?? "?"
        let title = label("Sharing this Mac: \u{201C}\(GlassDevice.computerName)\u{201D}  (id \(id))", size: 14, weight: .semibold)
        let ips = localIPv4Addresses()
        addressLabel = label(ips.isEmpty
            ? "Not on a network?"
            : "On the PC, run connect.cmd and type this Mac's address:  \(ips[0])", size: 14)
        addressLabel.font = .monospacedSystemFont(ofSize: 14, weight: .medium)
        pinLabel = label("", size: 34, weight: .bold)
        pinLabel.font = .monospacedDigitSystemFont(ofSize: 34, weight: .bold)
        pinLabel.isHidden = true
        statusLabel = label("Starting…")
        streamLabel = label("", size: 12)
        streamLabel.textColor = .secondaryLabelColor

        screenRow = label("")
        screenButton = NSButton(title: "Open Settings…", target: self, action: #selector(openScreen))
        accessRow = label("")
        accessButton = NSButton(title: "Open Settings…", target: self, action: #selector(openAccess))
        let screenStack = NSStackView(views: [screenRow, NSView(), screenButton])
        let accessStack = NSStackView(views: [accessRow, NSView(), accessButton])
        let permNote = label("After allowing Screen Recording, quit Glass (⌘Q) and open it again.", size: 11)
        permNote.textColor = .secondaryLabelColor

        copyButton = NSButton(title: "Show Results", target: self, action: #selector(copyResults))
        copyButton.isEnabled = false
        let stop = NSButton(title: "Stop Sharing", target: self, action: #selector(stopSharing))
        let card = NSButton(title: "Clarity Test Card", target: self, action: #selector(showTestCard))
        let buttons = NSStackView(views: [copyButton, card, NSView(), stop])

        let sep = NSBox()
        sep.boxType = .separator
        let stack = NSStackView(views: [title, addressLabel, pinLabel, statusLabel, streamLabel, sep, screenStack, accessStack, permNote, buttons])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 10
        stack.edgeInsets = NSEdgeInsets(top: 20, left: 20, bottom: 20, right: 20)
        stack.translatesAutoresizingMaskIntoConstraints = false
        let content = NSView()
        content.addSubview(stack)
        var constraints = [
            stack.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: content.trailingAnchor),
            stack.topAnchor.constraint(equalTo: content.topAnchor),
            stack.bottomAnchor.constraint(lessThanOrEqualTo: content.bottomAnchor),
        ]
        for v in [title, addressLabel, statusLabel, streamLabel, sep, screenStack, accessStack, permNote, buttons] as [NSView] {
            constraints.append(v.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -40))
        }
        NSLayoutConstraint.activate(constraints)
        w.contentView = content
    }

    private func refreshPermissions() {
        let screen = HostPermissions.screenRecording, access = HostPermissions.accessibility
        screenRow.stringValue = screen
            ? "✓ Screen Recording: allowed"
            : "✗ Screen Recording: needed to show this screen (Privacy & Security ▸ Screen & System Audio Recording)"
        screenButton.isHidden = screen
        accessRow.stringValue = access
            ? "✓ Accessibility: allowed"
            : "✗ Accessibility: needed for the PC's mouse and keyboard (Privacy & Security ▸ Accessibility)"
        accessButton.isHidden = access
    }

    @objc private func openScreen() { HostPermissions.openScreenRecordingSettings() }
    @objc private func openAccess() { HostPermissions.openAccessibilitySettings() }

    private func startSharing() {
        guard let device = GlassDevice.shared else {
            statusLabel.stringValue = "Couldn't read or create this Mac's Glass identity."
            return
        }
        do {
            let r = try HostRunner(device: device, options: .init(), banner: banner)
            r.onEvent = { [weak self] e in self?.event(e) }
            r.onStreamChanged = { [weak self] line in
                self?.testCard.nowShowing = line
                self?.streamLabel.stringValue = "Showing: \(line). The PC picks this with Ctrl+Alt+Shift+R."
            }
            r.onConnected = { [weak self] name, addr in
                self?.pinLabel.isHidden = true
                self?.statusLabel.stringValue = "Connected: \u{201C}\(name)\u{201D} (\(addr)), encrypted. It sees this screen and controls the mouse and keyboard. Stop Sharing ends it."
            }
            r.onSessionEnd = { [weak self] report in
                print(report)
                // Stop Sharing during a session: the window is gone, so show
                // the results on their own (with Copy).
                guard let self, !self.closed else {
                    showResults(report)
                    return
                }
                self.lastReport = report
                self.copyButton.isEnabled = true
                self.statusLabel.stringValue = "Session ended. Show Results, then Copy results and paste them to Claude. Waiting for the PC…"
            }
            runner = r
            r.start()
            advertiser.start(name: GlassDevice.computerName, port: r.host.port,
                             fingerprintPrefix: device.fingerprintHexPrefix, ip: localIPv4Addresses().first)
            let d = r.display
            statusLabel.stringValue = "Waiting for the PC… (\(d.width)x\(d.height) @ \(d.fps) Hz, port \(r.host.port))"
        } catch {
            statusLabel.stringValue = "\(error)"
        }
    }

    private func event(_ e: GlassHostEvent) {
        print(e.text)
        switch e.kind {
        case .pairingPin:
            pinLabel.stringValue = "PIN  \(e.pin.prefix(3)) \(e.pin.suffix(3))"
            pinLabel.isHidden = false
            statusLabel.stringValue = "\u{201C}\(e.device)\u{201D} (\(e.address)) wants to pair. Type this PIN on the PC (valid 2 minutes). Pair only your own devices."
            _ = NSApp.requestUserAttention(.criticalRequest)
        case .paired:
            pinLabel.isHidden = true
            statusLabel.stringValue = e.text
        default:
            statusLabel.stringValue = e.text
        }
    }

    @objc private func copyResults() {
        guard let r = lastReport else { return }
        // Show them too: a silent copy looked like nothing happened.
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(r, forType: .string)
        showResults(r)
    }

    @objc private func stopSharing() { window.close() }

    @objc private func showTestCard() { testCard.show() }

    func windowWillClose(_ notification: Notification) {
        closed = true
        permissionTimer?.invalidate()
        permissionTimer = nil
        advertiser.stop()
        runner?.stop()
        runner = nil
        onClose()
    }
}
