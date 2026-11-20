import AppKit
import CGlassCore
import Foundation
import GlassKit
import UniformTypeIdentifiers

/// Finder / double-click mode: connect window -> (pairing) -> live viewer (or
/// a recording) -> results window -> back to the connect window.
/// LATER: menu-bar shell.
final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate, NSMenuDelegate {
    private var connectWindow: NSWindow?
    private var hostField: NSTextField!
    private var connectButton: NSButton!
    private var foundPopup: NSPopUpButton!
    private var found: [DiscoveredHost] = []
    private let browser = GlassBrowser()
    private let pairedMenu = NSMenu(title: "Paired Devices")
    private var player: Player?
    private var viewer: LiveViewer?
    private var hostController: HostController?
    private var openedFile = false

    func applicationDidFinishLaunching(_ note: Notification) {
        buildMenu()
        NSApp.activate(ignoringOtherApps: true)
        DispatchQueue.main.async { if !self.openedFile { self.showConnect() } }
    }

    func application(_ app: NSApplication, open urls: [URL]) {
        openedFile = true
        if let url = urls.first { play(url) }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }

    // MARK: menus

    private func buildMenu() {
        let main = NSMenu()
        func submenu(_ title: String, _ items: [NSMenuItem]) {
            let top = NSMenuItem()
            let menu = NSMenu(title: title)
            for i in items { menu.addItem(i) }
            top.submenu = menu
            main.addItem(top)
        }
        func item(_ title: String, _ action: Selector, _ key: String, _ mods: NSEvent.ModifierFlags = .command, target: AnyObject? = nil) -> NSMenuItem {
            let i = NSMenuItem(title: title, action: action, keyEquivalent: key)
            i.keyEquivalentModifierMask = mods
            i.target = target
            return i
        }
        submenu("Glass", [item("Quit Glass", #selector(NSApplication.terminate(_:)), "q")])
        // While viewing, every other shortcut goes to the PC, so Glass's own
        // live-view commands use ⌃⌥⌘ (see Live.swift).
        let chord: NSEvent.ModifierFlags = [.control, .option, .command]
        let pcStyle = item("PC-Style Modifiers (⌘ → Win, ⌃ → Ctrl)", #selector(togglePcStyle(_:)), "", target: self)
        pcStyle.state = UserDefaults.standard.bool(forKey: "pcStyleModifiers") ? .on : .off
        let paired = NSMenuItem(title: "Paired Devices", action: nil, keyEquivalent: "")
        paired.submenu = pairedMenu
        pairedMenu.delegate = self
        submenu("Connection", [
            item("Connect…", #selector(showConnectAction), "n", target: self),
            item("Share This Mac…", #selector(shareThisMac), "", target: self),
            paired,
            item("Open Recording…", #selector(openRecording), "o", target: self),
            item("Disconnect", #selector(NSWindow.performClose(_:)), "w", chord),
            item("Quit Glass (while viewing)", #selector(NSApplication.terminate(_:)), "q", chord),
            .separator(),
            pcStyle,
        ])
        submenu("Edit", [
            item("Cut", #selector(NSText.cut(_:)), "x"),
            item("Copy", #selector(NSText.copy(_:)), "c"),
            item("Paste", #selector(NSText.paste(_:)), "v"),
            item("Select All", #selector(NSText.selectAll(_:)), "a"),
        ])
        submenu("View", [
            item("Toggle Full Screen", #selector(NSWindow.toggleFullScreen(_:)), "f", chord),
            item("Show/Hide Stats", #selector(toggleStats), "s", chord, target: self),
        ])
        NSApp.mainMenu = main
    }

    @objc private func showConnectAction() { showConnect() }

    // MARK: host (M3b)

    @objc private func shareThisMac() {
        if hostController != nil { return }
        browser.stop()
        connectWindow?.orderOut(nil)
        let h = HostController()
        h.onClose = { [weak self] in
            self?.hostController = nil
            self?.showConnect()
        }
        hostController = h
        h.open()
    }

    // MARK: paired devices

    func menuNeedsUpdate(_ menu: NSMenu) {
        guard menu === pairedMenu else { return }
        menu.removeAllItems()
        let peers = GlassDevice.shared?.peers ?? []
        if peers.isEmpty {
            let none = NSMenuItem(title: "None yet: connect to a PC to pair", action: nil, keyEquivalent: "")
            none.isEnabled = false
            menu.addItem(none)
        }
        for p in peers {
            let i = NSMenuItem(title: "Forget \u{201C}\(p.name)\u{201D}  (id \(p.shortID))…", action: #selector(forgetPeer(_:)), keyEquivalent: "")
            i.target = self
            i.representedObject = p.hex
            menu.addItem(i)
        }
        if GlassDevice.shared != nil {
            menu.addItem(.separator())
            let me = NSMenuItem(title: "This Mac: \u{201C}\(GlassDevice.computerName)\u{201D}", action: nil, keyEquivalent: "")
            me.isEnabled = false
            menu.addItem(me)
        }
    }

    @objc private func forgetPeer(_ sender: NSMenuItem) {
        guard let d = GlassDevice.shared, let hex = sender.representedObject as? String,
              let p = d.peers.first(where: { $0.hex == hex }) else { return }
        let a = NSAlert()
        a.messageText = "Forget \u{201C}\(p.name)\u{201D}?"
        a.informativeText = "This Mac will no longer trust it. To connect again you'll pair with a new PIN. (To stop the PC accepting this Mac, run `glass forget` on the PC.)"
        a.addButton(withTitle: "Forget")
        a.addButton(withTitle: "Cancel")
        if a.runModal() == .alertFirstButtonReturn { d.forget(p) }
        refreshFound()
    }
    @objc private func toggleStats() { viewer?.toggleOverlay() }

    @objc private func togglePcStyle(_ sender: NSMenuItem) {
        let on = sender.state != .on
        sender.state = on ? .on : .off
        UserDefaults.standard.set(on, forKey: "pcStyleModifiers")
        viewer?.pcStyleModifiers = on
    }

    // MARK: connect window

    private func showConnect() {
        if connectWindow == nil { buildConnectWindow() }
        connectButton.isEnabled = true
        connectButton.title = "Connect"
        browser.start()
        refreshFound()
        connectWindow?.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    private func setBusy(_ title: String) {
        connectButton.isEnabled = false
        connectButton.title = title
    }

    // MARK: discovery

    private func refreshFound() {
        guard let foundPopup else { return }
        foundPopup.removeAllItems()
        if found.isEmpty {
            foundPopup.addItem(withTitle: "Searching… (or type the address below)")
            foundPopup.isEnabled = false
            return
        }
        foundPopup.isEnabled = true
        foundPopup.addItem(withTitle: "Choose a PC…")
        for h in found {
            let paired = GlassDevice.shared?.pairedHost(prefix: h.fingerprintPrefix) != nil
            let v = h.version.map { $0 == String(glass_protocol_version()) ? "" : "  (different Glass version)" } ?? ""
            foundPopup.addItem(withTitle: "\(h.name) — \(h.address)\(paired ? "  ✓ paired" : "")\(v)")
        }
    }

    @objc private func pickFound(_ sender: NSPopUpButton) {
        let i = sender.indexOfSelectedItem - 1
        guard i >= 0, i < found.count else { return }
        let h = found[i]
        hostField.stringValue = h.port == GlassClient.defaultPort ? h.address : "\(h.address):\(h.port)"
    }

    private func buildConnectWindow() {
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 520, height: 230),
                         styleMask: [.titled, .closable, .miniaturizable], backing: .buffered, defer: false)
        w.title = "Glass \(appVersion)"
        w.isReleasedWhenClosed = false
        w.delegate = self

        let foundLabel = NSTextField(labelWithString: "PCs running Glass on this network:")
        foundPopup = NSPopUpButton(frame: .zero, pullsDown: false)
        foundPopup.target = self
        foundPopup.action = #selector(pickFound(_:))
        browser.onChange = { [weak self] hosts in
            self?.found = hosts
            self?.refreshFound()
        }
        let label = NSTextField(labelWithString: "Or the PC's address (host.cmd on the PC shows it):")
        hostField = NSTextField(string: UserDefaults.standard.string(forKey: "lastHost") ?? "")
        hostField.placeholderString = "e.g. 192.168.1.20"
        hostField.font = .monospacedSystemFont(ofSize: 14, weight: .regular)
        hostField.target = self
        hostField.action = #selector(connect)
        connectButton = NSButton(title: "Connect", target: self, action: #selector(connect))
        connectButton.keyEquivalent = "\r"
        let open = NSButton(title: "Open Recording…", target: self, action: #selector(openRecording))
        let share = NSButton(title: "Share This Mac…", target: self, action: #selector(shareThisMac))
        let note = NSTextField(labelWithString: "Paired PCs only; everything is encrypted. Opens full screen; ⌃⌥⌘F window, ⌃⌥⌘W disconnect.\nTo control this Mac from a PC instead: Share This Mac.")
        note.textColor = .secondaryLabelColor
        note.font = .systemFont(ofSize: 11)

        let buttons = NSStackView(views: [open, share, NSView(), connectButton])
        buttons.orientation = .horizontal
        let stack = NSStackView(views: [foundLabel, foundPopup, label, hostField, note, buttons])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 10
        stack.edgeInsets = NSEdgeInsets(top: 20, left: 20, bottom: 20, right: 20)
        stack.translatesAutoresizingMaskIntoConstraints = false
        let content = NSView()
        content.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: content.trailingAnchor),
            stack.topAnchor.constraint(equalTo: content.topAnchor),
            stack.bottomAnchor.constraint(equalTo: content.bottomAnchor),
            hostField.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -40),
            foundPopup.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -40),
            buttons.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -40),
        ])
        w.contentView = content
        w.center()
        connectWindow = w
    }

    func windowWillClose(_ notification: Notification) {
        // Closing the connect window quits (viewer windows have their own delegate).
        if (notification.object as? NSWindow) === connectWindow { NSApp.terminate(nil) }
    }

    /// "host" or "host:port".
    private func parseAddress(_ s: String) -> (String, UInt16) {
        let t = s.trimmingCharacters(in: .whitespacesAndNewlines)
        if let i = t.lastIndex(of: ":"), let p = UInt16(t[t.index(after: i)...]) {
            return (String(t[..<i]), p)
        }
        return (t, GlassClient.defaultPort)
    }

    @objc private func connect() {
        let (host, port) = parseAddress(hostField.stringValue)
        guard !host.isEmpty, connectButton.isEnabled else { return }
        UserDefaults.standard.set(hostField.stringValue.trimmingCharacters(in: .whitespacesAndNewlines), forKey: "lastHost")
        connectTo(host, port)
    }

    private func connectTo(_ host: String, _ port: UInt16) {
        setBusy("Connecting…")
        DispatchQueue.global(qos: .userInitiated).async {
            let result = Result { try GlassClient(host: host, port: port) }
            DispatchQueue.main.async {
                switch result {
                case .success(let client):
                    self.startViewer(client)
                case .failure(let f as ConnectFailure) where f.needsPairing:
                    self.offerPairing(host, port)
                case .failure(let error):
                    self.showConnect()
                    showError("Couldn't connect", "\(error)")
                }
            }
        }
    }

    // MARK: pairing

    private func offerPairing(_ host: String, _ port: UInt16) {
        let a = NSAlert()
        a.messageText = "Pair with this PC?"
        a.informativeText = "This Mac isn't paired with \(host) yet. Glass on the PC will show a 6-digit PIN in its host.cmd window for you to type here.\n\nPair only with your own PC: a paired PC sees what you type while viewing it."
        a.addButton(withTitle: "Pair")
        a.addButton(withTitle: "Cancel")
        guard a.runModal() == .alertFirstButtonReturn else { showConnect(); return }
        beginPairing(host, port)
    }

    private func beginPairing(_ host: String, _ port: UInt16) {
        guard let device = GlassDevice.shared else {
            showConnect()
            showError("Couldn't pair", "\(ConnectFailure(code: 10, host: host))")
            return
        }
        setBusy("Pairing…")
        DispatchQueue.global(qos: .userInitiated).async {
            let result = Result { try GlassPairing(device: device, host: host, port: port) }
            DispatchQueue.main.async {
                switch result {
                case .success(let p): self.askPin(p, port)
                case .failure(let error):
                    self.showConnect()
                    showError("Couldn't pair", "\(error)")
                }
            }
        }
    }

    private func askPin(_ p: GlassPairing, _ port: UInt16) {
        let a = NSAlert()
        a.messageText = "Type the PIN shown on \u{201C}\(p.hostName)\u{201D}"
        a.informativeText = "It's in the host.cmd window on the PC, valid for 2 minutes."
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 160, height: 28))
        field.font = .monospacedDigitSystemFont(ofSize: 18, weight: .medium)
        field.placeholderString = "123 456"
        a.accessoryView = field
        a.addButton(withTitle: "Pair")
        a.addButton(withTitle: "Cancel")
        a.window.initialFirstResponder = field
        guard a.runModal() == .alertFirstButtonReturn else {
            p.cancel()
            showConnect()
            return
        }
        let pin = field.stringValue.filter(\.isNumber)
        setBusy("Pairing…")
        DispatchQueue.global(qos: .userInitiated).async {
            let result = Result { try p.finish(pin: pin) }
            DispatchQueue.main.async {
                switch result {
                case .success:
                    self.refreshFound()
                    self.connectTo(p.host, port)
                case .failure(let f as ConnectFailure) where f.wrongPin:
                    let again = NSAlert()
                    again.messageText = "Wrong PIN"
                    again.informativeText = "\(f)"
                    again.addButton(withTitle: "Try Again")
                    again.addButton(withTitle: "Cancel")
                    if again.runModal() == .alertFirstButtonReturn {
                        self.beginPairing(p.host, port)
                    } else {
                        self.showConnect()
                    }
                case .failure(let error):
                    self.showConnect()
                    showError("Couldn't pair", "\(error)")
                }
            }
        }
    }

    private func startViewer(_ client: GlassClient) {
        do {
            let v = try LiveViewer(client: client)
            v.onFinish = { [weak self] report in
                self?.viewer = nil
                showResults(report)
                self?.showConnect()
            }
            viewer = v
            browser.stop()
            connectWindow?.orderOut(nil)
            v.open()
        } catch {
            client.close()
            showConnect()
            showError("Couldn't start the viewer", "\(error)")
        }
    }

    // MARK: recordings (M1)

    @objc private func openRecording() {
        let panel = NSOpenPanel()
        panel.message = "Choose a Glass recording (.glsr) to test"
        panel.allowedContentTypes = [UTType(filenameExtension: "glsr") ?? .data]
        panel.directoryURL = FileManager.default.urls(for: .downloadsDirectory, in: .userDomainMask).first
        if panel.runModal() == .OK, let url = panel.url { play(url) }
    }

    private func play(_ url: URL) {
        var report = "\(banner)\n\(systemInfo())\n\(url.lastPathComponent): "
        do {
            let rec = try Recording(contentsOf: url)
            report += describe(rec) + "\n" + (try bench(rec))
            let player = try Player(rec)
            player.onFinish = { [weak self] playback in
                self?.player = nil
                showResults(report + "\n" + playback)
                self?.showConnect()
            }
            self.player = player
            connectWindow?.orderOut(nil)
            player.open()
        } catch {
            showResults(report + "\nerror: \(error)")
            showConnect()
        }
    }
}
