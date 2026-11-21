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
    private var listStack: NSStackView!
    private var machineButtons: [NSButton] = []
    private var found: [DiscoveredHost] = []
    private let browser = GlassBrowser()
    private let pairedMenu = NSMenu(title: "Paired Devices")
    private var player: Player?
    private var viewer: LiveViewer?
    private var hostController: HostController?
    private var openedFile = false

    func applicationDidFinishLaunching(_ note: Notification) {
        buildMenu()
        applyTheme()
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
        machineButtons.forEach { $0.isEnabled = false }
    }

    // MARK: theme (System / Light / Dark)

    private func applyTheme() {
        switch UserDefaults.standard.string(forKey: "theme") {
        case "light": NSApp.appearance = NSAppearance(named: .aqua)
        case "dark": NSApp.appearance = NSAppearance(named: .darkAqua)
        default: NSApp.appearance = nil
        }
    }

    @objc private func pickTheme(_ sender: NSPopUpButton) {
        let t = ["system", "light", "dark"][max(0, sender.indexOfSelectedItem)]
        UserDefaults.standard.set(t, forKey: "theme")
        applyTheme()
    }

    @objc private func showPaired(_ sender: NSButton) {
        pairedMenu.popUp(positioning: nil, at: NSPoint(x: 0, y: sender.bounds.height + 4), in: sender)
    }

    // MARK: discovery

    private func refreshFound() {
        guard let listStack else { return }
        listStack.arrangedSubviews.forEach { $0.removeFromSuperview() }
        machineButtons = []
        let busy = !(connectButton?.isEnabled ?? true)
        if found.isEmpty {
            let l = NSTextField(wrappingLabelWithString: "Looking for Macs and PCs with Glass\u{2026}  Start sharing on the other machine, or type its address below.")
            l.textColor = .secondaryLabelColor
            add(card(l))
            return
        }
        for (i, h) in found.enumerated() {
            let paired = GlassDevice.shared?.pairedHost(prefix: h.fingerprintPrefix) != nil
            let versionOK = h.version.map { $0 == String(glass_protocol_version()) } ?? true
            let name = NSTextField(labelWithString: h.name)
            name.font = .systemFont(ofSize: 15, weight: .semibold)
            name.lineBreakMode = .byTruncatingTail
            name.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
            let status = !versionOK ? tag("different Glass version", .systemOrange)
                : paired ? tag("\u{2713} paired", .systemGreen) : tag("needs PIN", .systemOrange)
            let top = NSStackView(views: [name, tag(h.kind, .secondaryLabelColor), status])
            top.spacing = 6
            let addr = NSTextField(labelWithString: h.address)
            addr.textColor = .secondaryLabelColor
            let left = NSStackView(views: [top, addr])
            left.orientation = .vertical
            left.alignment = .leading
            left.spacing = 3
            let b = NSButton(title: "Connect", target: self, action: #selector(connectFound(_:)))
            b.tag = i
            b.bezelColor = .controlAccentColor
            b.controlSize = .large
            b.isEnabled = versionOK && !busy
            machineButtons.append(b)
            let row = NSStackView(views: [left, NSView(), b])
            row.orientation = .horizontal
            add(card(row))
        }
    }

    private func add(_ v: NSView) {
        listStack.addArrangedSubview(v)
        v.widthAnchor.constraint(equalTo: listStack.widthAnchor).isActive = true
    }

    @objc private func connectFound(_ sender: NSButton) {
        guard sender.tag < found.count, connectButton.isEnabled else { return }
        let h = found[sender.tag]
        hostField.stringValue = h.port == GlassClient.defaultPort ? h.address : "\(h.address):\(h.port)"
        connect()
    }

    private func buildConnectWindow() {
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 580, height: 600),
                         styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
        w.title = "Glass \(appVersion)"
        w.isReleasedWhenClosed = false
        w.delegate = self
        w.contentMinSize = NSSize(width: 480, height: 460)

        // Header: title and theme.
        let title = NSTextField(labelWithString: "Glass")
        title.font = .systemFont(ofSize: 26, weight: .bold)
        let theme = NSPopUpButton(frame: .zero, pullsDown: false)
        theme.addItems(withTitles: ["Theme: System", "Theme: Light", "Theme: Dark"])
        let saved = UserDefaults.standard.string(forKey: "theme") ?? "system"
        theme.selectItem(at: ["system", "light", "dark"].firstIndex(of: saved) ?? 0)
        theme.target = self
        theme.action = #selector(pickTheme(_:))
        let header = NSStackView(views: [title, NSView(), theme])
        let id = GlassDevice.shared.map { "  \u{00B7}  id \($0.fingerprintHexPrefix)" } ?? ""
        let me = NSTextField(labelWithString: "This Mac: \u{201C}\(GlassDevice.computerName)\u{201D}\(id)")
        me.textColor = .secondaryLabelColor

        // Sharing.
        let shareTitle = NSTextField(labelWithString: "Share this Mac")
        shareTitle.font = .systemFont(ofSize: 16, weight: .semibold)
        let shareNote = NSTextField(labelWithString: "Let another Mac or PC view and control this Mac.")
        shareNote.textColor = .secondaryLabelColor
        let shareText = NSStackView(views: [shareTitle, shareNote])
        shareText.orientation = .vertical
        shareText.alignment = .leading
        shareText.spacing = 3
        let share = NSButton(title: "Start Sharing\u{2026}", target: self, action: #selector(shareThisMac))
        share.bezelColor = .controlAccentColor
        share.controlSize = .large
        let shareCard = card(NSStackView(views: [shareText, NSView(), share]))

        // Machines.
        let machines = NSTextField(labelWithString: "Machines on this network")
        machines.font = .systemFont(ofSize: 16, weight: .semibold)
        let paired = NSButton(title: "Paired Devices", target: self, action: #selector(showPaired(_:)))
        let machinesHeader = NSStackView(views: [machines, NSView(), paired])
        listStack = NSStackView()
        listStack.orientation = .vertical
        listStack.alignment = .leading
        listStack.spacing = 8
        listStack.translatesAutoresizingMaskIntoConstraints = false
        let doc = FlippedView()
        doc.translatesAutoresizingMaskIntoConstraints = false
        doc.addSubview(listStack)
        let scroll = NSScrollView()
        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.documentView = doc
        scroll.setContentHuggingPriority(.defaultLow, for: .vertical)
        NSLayoutConstraint.activate([
            listStack.topAnchor.constraint(equalTo: doc.topAnchor),
            listStack.leadingAnchor.constraint(equalTo: doc.leadingAnchor),
            listStack.trailingAnchor.constraint(equalTo: doc.trailingAnchor),
            doc.bottomAnchor.constraint(equalTo: listStack.bottomAnchor),
            doc.topAnchor.constraint(equalTo: scroll.contentView.topAnchor),
            doc.leadingAnchor.constraint(equalTo: scroll.contentView.leadingAnchor),
            doc.widthAnchor.constraint(equalTo: scroll.contentView.widthAnchor),
            scroll.heightAnchor.constraint(greaterThanOrEqualToConstant: 140),
        ])
        browser.onChange = { [weak self] hosts in
            // Not this Mac itself.
            let mine = GlassDevice.shared?.fingerprintHexPrefix ?? "-"
            self?.found = hosts.filter { $0.fingerprintPrefix?.lowercased() != mine }
            self?.refreshFound()
        }

        // A typed address.
        let label = NSTextField(labelWithString: "Or an address:")
        hostField = NSTextField(string: UserDefaults.standard.string(forKey: "lastHost") ?? "")
        hostField.placeholderString = "e.g. 192.168.1.20"
        hostField.target = self
        hostField.action = #selector(connect)
        connectButton = NSButton(title: "Connect", target: self, action: #selector(connect))
        connectButton.keyEquivalent = "\r"
        let addressRow = NSStackView(views: [label, hostField, connectButton, NSView()])
        let open = NSButton(title: "Open Recording\u{2026}", target: self, action: #selector(openRecording))
        open.controlSize = .small
        let note = NSTextField(labelWithString: "Paired devices only; everything is encrypted. While viewing, move to the top edge for the bar.")
        note.textColor = .secondaryLabelColor
        note.font = .systemFont(ofSize: 11)
        note.lineBreakMode = .byTruncatingTail
        note.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        let footer = NSStackView(views: [note, NSView(), open])

        let stack = NSStackView(views: [header, me, shareCard, machinesHeader, scroll, addressRow, footer])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 10
        stack.setCustomSpacing(18, after: shareCard)
        stack.edgeInsets = NSEdgeInsets(top: 18, left: 20, bottom: 16, right: 20)
        stack.translatesAutoresizingMaskIntoConstraints = false
        let content = NSView()
        content.addSubview(stack)
        var cs = [
            stack.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: content.trailingAnchor),
            stack.topAnchor.constraint(equalTo: content.topAnchor),
            stack.bottomAnchor.constraint(equalTo: content.bottomAnchor),
            hostField.widthAnchor.constraint(equalToConstant: 220),
        ]
        for v in [header, shareCard, machinesHeader, scroll, addressRow, footer] {
            cs.append(v.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -40))
        }
        NSLayoutConstraint.activate(cs)
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
            // The view last used with this host (default: fit), and the colour choice.
            let saved = UserDefaults.standard.object(forKey: "viewMode-\(host)") as? Int
            let mode = UInt8(saved ?? Int(GLASS_MODE_FIT))
            let full = UserDefaults.standard.bool(forKey: "fullColour")
            let result = Result { try GlassClient(host: host, port: port, mode: mode, fullColour: full) }
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
        a.messageText = "Pair with \(host)?"
        a.informativeText = "This Mac isn't paired with it yet. Glass there will show a 6-digit PIN for you to type here.\n\nPair only your own devices: a paired host sees what you type while viewing it."
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
        a.informativeText = "It's shown in Glass on that machine, valid for 2 minutes."
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

/// Top-down layout for the scrolling machine list.
private final class FlippedView: NSView {
    override var isFlipped: Bool { true }
}

/// A rounded card that follows light/dark.
private func card(_ content: NSView) -> NSBox {
    let b = NSBox()
    b.boxType = .custom
    b.titlePosition = .noTitle
    b.cornerRadius = 12
    b.borderWidth = 1
    b.borderColor = .separatorColor
    b.fillColor = .controlBackgroundColor
    b.contentViewMargins = NSSize(width: 14, height: 12)
    b.contentView = content
    return b
}

/// A small rounded tag ("Mac", "✓ paired").
private func tag(_ text: String, _ colour: NSColor) -> NSView {
    let l = NSTextField(labelWithString: text)
    l.font = .systemFont(ofSize: 11, weight: .medium)
    l.textColor = colour
    let b = NSBox()
    b.boxType = .custom
    b.titlePosition = .noTitle
    b.cornerRadius = 8
    b.borderWidth = 0
    b.fillColor = colour.withAlphaComponent(0.16)
    b.contentViewMargins = NSSize(width: 7, height: 2)
    b.contentView = l
    return b
}
