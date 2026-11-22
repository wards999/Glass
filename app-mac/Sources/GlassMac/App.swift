import AppKit
import CGlassCore
import Foundation
import QuartzCore
import GlassKit
import UniformTypeIdentifiers

/// Finder / double-click mode: connect window -> (pairing) -> live viewer (or
/// a recording) -> results window -> back to the connect window.
/// LATER: menu-bar shell.
final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate, NSMenuDelegate, NSTextFieldDelegate {
    private var connectWindow: NSWindow?
    private var hostField: NSTextField!
    private var connectButton: NSButton!
    private var listStack: NSStackView!
    private var countLabel: NSTextField?
    private var connectingHost: String?
    /// The address of the machine being viewed (its row says Viewing).
    private var viewingHost: String?
    /// Someone is viewing this Mac (sharing, in a session), and the line
    /// that says who.
    private var beingViewed = false
    private var hostStatus = ""
    /// The name of the machine being viewed (for the session bar).
    private var viewingName = ""
    private var viewingPort: UInt16 = 0
    /// Reports of a session that dropped and is being reconnected.
    private var droppedReports = ""
    private var sessionCard: NSView?
    private var sessionTitle: NSTextField?
    private var sessionButton: NSButton?
    private var addressRow: NSStackView!
    private var addressLink: NSButton!
    private var shareSwitch: NSSwitch?
    private var themeButton: NSButton?
    private var settingsPanel: NSPanel?
    private var settingsMonitor: Any?
    private var gearButton: NSButton?
    private var shareNote: NSTextField?
    private var shareIcon: NSImageView?
    private var shareTile: LayerView?
    private var shareStack: NSStackView?
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

    func applicationWillTerminate(_ note: Notification) {
        hostController?.stop()
    }

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
            item("Allow Connections", #selector(shareThisMac), "", target: self),
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

    /// Sharing runs inside the main window's share card.
    @objc private func shareThisMac() {
        showConnect()
        guard hostController == nil else { return }
        let h = HostController()
        h.onStatus = { [weak self] s in
            guard let self else { return }
            self.shareNote?.stringValue = s.hasPrefix("Pairing") || s.hasPrefix("Couldn")
                ? s : "Open to your paired devices"
            self.hostStatus = s
            let viewed = s.hasPrefix("Connected")
            if viewed != self.beingViewed {
                self.beingViewed = viewed
                self.refreshFound()
            }
        }
        hostController = h
        shareSwitch?.state = .on
        shareIcon?.contentTintColor = .systemGreen
        shareTile?.fill = NSColor.systemGreen.withAlphaComponent(0.14)
        if let shareStack {
            shareStack.addArrangedSubview(h.detail)
            h.detail.widthAnchor.constraint(equalTo: shareStack.widthAnchor).isActive = true
        }
        h.start()
    }

    private func stopSharing() {
        guard let h = hostController else { return }
        h.stop()
        h.detail.removeFromSuperview()
        hostController = nil
        beingViewed = false
        refreshFound()
        shareSwitch?.state = .off
        shareIcon?.contentTintColor = .secondaryLabelColor
        shareTile?.fill = Look.track
        shareNote?.stringValue = "Closed \u{00B7} nobody can connect"
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
        connectingHost = nil
        shareSwitch?.state = hostController == nil ? .off : .on
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
        refreshFound()
    }

    // MARK: theme (System / Light / Dark)

    private func applyTheme() {
        switch UserDefaults.standard.string(forKey: "theme") {
        case "light": NSApp.appearance = NSAppearance(named: .aqua)
        case "dark": NSApp.appearance = NSAppearance(named: .darkAqua)
        default: NSApp.appearance = nil
        }
    }

    /// The header's quick switch: light <-> dark (Settings also offers System).
    @objc private func quickTheme(_ sender: NSButton) {
        UserDefaults.standard.set(isDark ? "light" : "dark", forKey: "theme")
        applyTheme()
        updateThemeButton()
    }

    private var isDark: Bool { NSApp.effectiveAppearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua }

    private func updateThemeButton() {
        themeButton?.image = NSImage(systemSymbolName: isDark ? "sun.max" : "moon", accessibilityDescription: isDark ? "Switch to light" : "Switch to dark")
        themeButton?.toolTip = isDark ? "Switch to light" : "Switch to dark"
    }

    // MARK: settings (a panel under the gear, as on Windows)

    @objc private func showSettings(_ sender: NSButton) {
        if settingsPanel != nil {
            closeSettings()
            return
        }
        guard let w = connectWindow else { return }
        let card = Card(settingsView(), inset: 0)
        let container = NSView()
        container.addSubview(card)
        NSLayoutConstraint.activate([
            card.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            card.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            card.topAnchor.constraint(equalTo: container.topAnchor),
            card.bottomAnchor.constraint(equalTo: container.bottomAnchor),
        ])
        let size = container.fittingSize
        let panel = NSPanel(contentRect: NSRect(origin: .zero, size: size), styleMask: [.borderless],
                            backing: .buffered, defer: false)
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = true
        panel.contentView = container
        // Right-aligned under the gear, kept on screen.
        let gear = w.convertToScreen(sender.convert(sender.bounds, to: nil))
        var origin = NSPoint(x: gear.maxX - size.width, y: gear.minY - 6 - size.height)
        if let vf = w.screen?.visibleFrame {
            origin.x = min(max(origin.x, vf.minX + 8), vf.maxX - size.width - 8)
            origin.y = max(origin.y, vf.minY + 8)
        }
        panel.setFrameOrigin(origin)
        w.addChildWindow(panel, ordered: .above)
        settingsPanel = panel
        // A click outside it or Esc closes it (the gear toggles it itself).
        settingsMonitor = NSEvent.addLocalMonitorForEvents(matching: [.leftMouseDown, .rightMouseDown, .keyDown]) { [weak self] e in
            guard let self, let p = self.settingsPanel else { return e }
            if e.type == .keyDown {
                if e.keyCode == 53 { // Esc
                    self.closeSettings()
                    return nil
                }
                return e
            }
            if e.window === p { return e }
            if e.window === self.connectWindow, let g = self.gearButton,
               g.bounds.contains(g.convert(e.locationInWindow, from: nil)) {
                return e
            }
            self.closeSettings()
            return e
        }
    }

    private func closeSettings() {
        if let m = settingsMonitor { NSEvent.removeMonitor(m) }
        settingsMonitor = nil
        if let p = settingsPanel {
            p.parent?.removeChildWindow(p)
            p.orderOut(nil)
        }
        settingsPanel = nil
    }

    /// The views, as on Windows (generic: any host that can make them).
    private static let startViews: [(UInt8, String)] = [
        (1, "View 1 of 4: Fit (fills this screen, sharp text)"),
        (2, "View 2 of 4: More space (smaller text)"),
        (3, "View 3 of 4: Larger (bigger text)"),
        (0, "View 4 of 4: Host\u{2019}s own screen (may show black bars)"),
    ]

    private func settingsView() -> NSView {
        func heading(_ t: String, _ size: CGFloat = 13) -> NSTextField {
            let l = NSTextField(labelWithString: t)
            l.font = .systemFont(ofSize: size, weight: .semibold)
            return l
        }
        let theme = NSSegmentedControl(labels: ["System", "Light", "Dark"], trackingMode: .selectOne,
                                       target: self, action: #selector(pickThemeSegment(_:)))
        theme.selectedSegment = ["system", "light", "dark"].firstIndex(of: UserDefaults.standard.string(forKey: "theme") ?? "system") ?? 0
        theme.segmentDistribution = .fillEqually
        let current = (UserDefaults.standard.object(forKey: "startView") as? Int) ?? Int(GLASS_MODE_FIT)
        let radios = NSStackView()
        radios.orientation = .vertical
        radios.alignment = .leading
        radios.spacing = 6
        for (mode, title) in Self.startViews {
            let r = NSButton(radioButtonWithTitle: title, target: self, action: #selector(pickStartView(_:)))
            r.tag = Int(mode)
            r.state = Int(mode) == current ? .on : .off
            radios.addArrangedSubview(r)
        }
        let colour = NSSegmentedControl(labels: ["Fast \u{00B7} 4:2:0", "Full colour \u{00B7} 4:4:4"], trackingMode: .selectOne,
                                        target: self, action: #selector(pickColour(_:)))
        colour.selectedSegment = UserDefaults.standard.bool(forKey: "fullColour") ? 1 : 0
        colour.segmentDistribution = .fillEqually
        let note = NSTextField(wrappingLabelWithString: "Fast has the lowest lag. Full colour is exact but adds some lag. Both can be switched while viewing, from the handle at the top of the viewer.")
        note.font = .systemFont(ofSize: 11)
        note.textColor = .secondaryLabelColor
        let clipboard = NSSegmentedControl(labels: ["Shared", "Off"], trackingMode: .selectOne,
                                           target: self, action: #selector(pickClipboard(_:)))
        clipboard.selectedSegment = ClipboardSync.enabled ? 0 : 1
        clipboard.segmentDistribution = .fillEqually
        let clipNote = NSTextField(wrappingLabelWithString: "Shared: text, pictures and files copied on one computer can be pasted on the other, both ways, while a session is on (files up to 200 MB a copy). Changes apply from the next session.")
        clipNote.font = .systemFont(ofSize: 11)
        clipNote.textColor = .secondaryLabelColor
        // A visible way out (Esc and a click outside work too).
        let closeX = NSButton(image: NSImage(systemSymbolName: "xmark.circle.fill", accessibilityDescription: "Close") ?? NSImage(),
                              target: self, action: #selector(closeSettingsTapped))
        closeX.isBordered = false
        closeX.contentTintColor = .tertiaryLabelColor
        closeX.symbolConfiguration = .init(pointSize: 16, weight: .regular)
        let titleRow = NSStackView(views: [heading("Settings", 17), NSView(), closeX])
        var views: [NSView] = [titleRow, heading("Appearance"), theme,
                               heading("When viewing, start in"), radios,
                               heading("Colour"), colour, note,
                               heading("Clipboard"), clipboard, clipNote, heading("Paired devices")]
        let peers = GlassDevice.shared?.peers ?? []
        if peers.isEmpty {
            let none = NSTextField(labelWithString: "None yet.")
            none.textColor = .secondaryLabelColor
            views.append(none)
        }
        for p in peers {
            let name = NSTextField(labelWithString: p.name)
            name.lineBreakMode = .byTruncatingTail
            name.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
            var roles: [String] = []
            if p.roles & 2 != 0 { roles.append("can view this Mac") }
            if p.roles & 1 != 0 { roles.append("this Mac can view it") }
            let detail = NSTextField(labelWithString: roles.joined(separator: " \u{00B7} "))
            detail.font = .systemFont(ofSize: 11)
            detail.textColor = .secondaryLabelColor
            let texts = NSStackView(views: [name, detail])
            texts.orientation = .vertical
            texts.alignment = .leading
            texts.spacing = 1
            let forget = NSButton(title: "Forget", target: self, action: #selector(forgetFromSettings(_:)))
            forget.isBordered = false
            forget.contentTintColor = .systemRed
            forget.identifier = NSUserInterfaceItemIdentifier(p.hex)
            views.append(NSStackView(views: [texts, NSView(), forget]))
        }
        let id = GlassDevice.shared.map { " \u{00B7} id \($0.fingerprintHexPrefix)" } ?? ""
        let about = NSTextField(labelWithString: "This Mac \u{201C}\(GlassDevice.computerName)\u{201D}\(id) \u{00B7} protocol v\(glass_protocol_version())")
        about.font = .systemFont(ofSize: 11)
        about.textColor = .tertiaryLabelColor
        views.append(about)
        let stack = NSStackView(views: views)
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 8
        stack.setCustomSpacing(14, after: theme)
        stack.setCustomSpacing(14, after: radios)
        stack.setCustomSpacing(14, after: note)
        stack.setCustomSpacing(14, after: clipNote)
        stack.setCustomSpacing(14, after: views[views.count - 2])
        stack.edgeInsets = NSEdgeInsets(top: 18, left: 18, bottom: 18, right: 18)
        stack.translatesAutoresizingMaskIntoConstraints = false
        stack.widthAnchor.constraint(equalToConstant: 380).isActive = true
        for v in views where !(v is NSTextField) || v === note || v === clipNote {
            v.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -36).isActive = true
        }
        return stack
    }

    @objc private func closeSettingsTapped() { closeSettings() }

    @objc private func pickStartView(_ sender: NSButton) {
        UserDefaults.standard.set(sender.tag, forKey: "startView")
    }

    @objc private func pickThemeSegment(_ sender: NSSegmentedControl) {
        UserDefaults.standard.set(["system", "light", "dark"][max(0, sender.selectedSegment)], forKey: "theme")
        applyTheme()
        updateThemeButton()
    }

    @objc private func pickColour(_ sender: NSSegmentedControl) {
        UserDefaults.standard.set(sender.selectedSegment == 1, forKey: "fullColour")
    }

    @objc private func pickClipboard(_ sender: NSSegmentedControl) {
        UserDefaults.standard.set(sender.selectedSegment == 0, forKey: "shareClipboard")
    }

    @objc private func forgetFromSettings(_ sender: NSButton) {
        guard let d = GlassDevice.shared, let hex = sender.identifier?.rawValue,
              let p = d.peers.first(where: { $0.hex == hex }) else { return }
        closeSettings()
        let a = NSAlert()
        a.messageText = "Forget \u{201C}\(p.name)\u{201D}?"
        a.informativeText = "This Mac will no longer trust it. To connect again you'll pair with a new code."
        a.addButton(withTitle: "Forget")
        a.addButton(withTitle: "Cancel")
        if a.runModal() == .alertFirstButtonReturn { d.forget(p) }
        refreshFound()
    }

    // MARK: discovery

    /// The session this Mac is in, if any: what to say and the button.
    private func session() -> (String, String)? {
        if viewer != nil {
            return ("Viewing \u{201C}\(viewingName)\u{201D}", "Disconnect")
        }
        if beingViewed {
            // "Connected to “NAME”" -> NAME
            let name = hostStatus.split(separator: "\u{201C}").dropFirst().first?
                .split(separator: "\u{201D}").first.map(String.init) ?? "A device"
            return ("\u{201C}\(name)\u{201D} is viewing this Mac", "Stop Sharing")
        }
        return nil
    }

    /// The session bar at the top, and nothing else connects meanwhile.
    private func updateSession() {
        let s = session()
        sessionCard?.isHidden = s == nil
        sessionTitle?.stringValue = s?.0 ?? ""
        sessionButton?.title = s?.1 ?? ""
        connectButton?.isEnabled = s == nil && connectingHost == nil
        // Connect by address is off during a session, and its box closes.
        addressLink?.isEnabled = s == nil
        hostField?.isEnabled = s == nil
        if s != nil, addressRow?.isHidden == false { toggleAddress() }
        // Sharing can't start while viewing (one session at a time).
        shareSwitch?.isEnabled = !(viewer != nil && hostController == nil)
    }

    @objc private func endSessionTapped() { endSession() }

    private func refreshFound() {
        updateSession()
        guard let listStack else { return }
        listStack.arrangedSubviews.forEach { $0.removeFromSuperview() }
        countLabel?.stringValue = found.isEmpty ? "" : "\(found.count)"
        if found.isEmpty {
            // During a session it's still (no searching animation) and says so.
            let session: String? = beingViewed ? hostStatus : viewer != nil ? "Viewing another device." : nil
            let empty = EmptyState(session: session)
            empty.onEnd = { [weak self] in self?.endSession() }
            add(empty)
            return
        }
        let me = String(glass_protocol_version())
        let hosts = found.map { h -> (DiscoveredHost, Bool, Bool) in
            (h, GlassDevice.shared?.pairedHost(prefix: h.fingerprintPrefix) != nil, h.version.map { $0 == me } ?? true)
        }.sorted { a, b in
            // Ready ones first, then by name.
            (a.2 ? 0 : 1, a.1 ? 0 : 1, a.0.name.lowercased()) < (b.2 ? 0 : 1, b.1 ? 0 : 1, b.0.name.lowercased())
        }
        for (h, paired, versionOK) in hosts {
            let viewing = viewer != nil && viewingHost == h.address
            let (status, colour): (String, NSColor) = viewing ? ("Viewing", .systemGreen)
                : !versionOK ? ("Update Glass", .tertiaryLabelColor)
                : paired ? ("Ready", .systemGreen) : ("Pair", .systemOrange)
            let kind = h.os == "mac" ? "Mac" : h.os == "windows" ? "Windows PC" : "Glass"
            // One session at a time: while viewing, only that row is live,
            // and it disconnects.
            let row = DeviceRow(name: h.name, detail: "\(kind) \u{00B7} \(h.address)", mac: h.os == "mac",
                                status: status, statusColour: colour,
                                enabled: viewing || (versionOK && connectingHost == nil && session() == nil),
                                busy: connectingHost == h.address,
                                action: viewing ? "Disconnect" : "Connect")
            if !versionOK { row.toolTip = "This device runs a different version of Glass. Install the same build on both." }
            row.onClick = { [weak self] in
                if viewing { self?.viewer?.stop("disconnected from the Glass window") } else { self?.connectFound(h) }
            }
            add(row)
        }
    }

    private func add(_ v: NSView) {
        listStack.addArrangedSubview(v)
        v.widthAnchor.constraint(equalTo: listStack.widthAnchor).isActive = true
    }

    /// Ends the session this Mac is in: the one being viewed here, or the
    /// one viewing this Mac (sharing stays on).
    private func endSession() {
        if let v = viewer {
            v.stop("ended from the Glass window")
        } else if hostController != nil {
            stopSharing()
        }
    }

    private func connectFound(_ h: DiscoveredHost) {
        viewingName = h.name
        guard connectButton.isEnabled else { return }
        hostField.stringValue = h.port == GlassClient.defaultPort ? h.address : "\(h.address):\(h.port)"
        connect()
    }

    @objc private func toggleAddress() {
        addressRow.isHidden.toggle()
        addressLink.title = addressRow.isHidden ? "+  Connect by address" : "\u{2212}  Connect by address"
        if !addressRow.isHidden { connectWindow?.makeFirstResponder(hostField) }
    }

    /// Esc in the address field closes it (as on the PC).
    func control(_ control: NSControl, textView: NSTextView, doCommandBy commandSelector: Selector) -> Bool {
        guard control === hostField, commandSelector == #selector(NSResponder.cancelOperation(_:)) else { return false }
        if !addressRow.isHidden { toggleAddress() }
        return true
    }

    @objc private func shareSwitched(_ sender: NSSwitch) {
        if sender.state == .on { shareThisMac() } else { stopSharing() }
    }

    @objc private func lookAgain() {
        browser.stop()
        browser.start()
    }

    private func buildConnectWindow() {
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 440, height: 700),
                         styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
                         backing: .buffered, defer: false)
        w.title = "Glass"
        w.titlebarAppearsTransparent = true
        w.titleVisibility = .hidden
        w.isReleasedWhenClosed = false
        w.delegate = self
        w.backgroundColor = Look.bg
        w.contentMinSize = NSSize(width: 380, height: 500)

        // Header: icon, name, this Mac; theme and settings.
        let icon = NSImageView(image: NSApp.applicationIconImage ?? NSImage())
        icon.translatesAutoresizingMaskIntoConstraints = false
        let title = NSTextField(labelWithString: "Glass")
        title.font = .systemFont(ofSize: 22, weight: .semibold)
        let me = NSTextField(labelWithString: "This Mac \u{00B7} \(GlassDevice.computerName)")
        me.font = .systemFont(ofSize: 12)
        me.textColor = .secondaryLabelColor
        let names = NSStackView(views: [title, me])
        names.orientation = .vertical
        names.alignment = .leading
        names.spacing = 0
        let themeToggle = symbolButton("moon", "Switch to dark", #selector(quickTheme(_:)))
        themeButton = themeToggle
        updateThemeButton()
        let gear = symbolButton("gearshape", "Settings", #selector(showSettings(_:)))
        gearButton = gear
        let header = NSStackView(views: [icon, names, NSView(), themeToggle, gear])

        // The session bar: who's connected, and the way out (hidden otherwise).
        let dot = LayerView(fill: .systemGreen, stroke: nil, radius: 5)
        dot.widthAnchor.constraint(equalToConstant: 10).isActive = true
        dot.heightAnchor.constraint(equalToConstant: 10).isActive = true
        let sTitle = NSTextField(labelWithString: "")
        sTitle.font = .systemFont(ofSize: 14, weight: .semibold)
        sTitle.lineBreakMode = .byTruncatingTail
        sTitle.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        let sNote = NSTextField(labelWithString: "Encrypted \u{00B7} one session at a time")
        sNote.font = .systemFont(ofSize: 12)
        sNote.textColor = .secondaryLabelColor
        let sTexts = NSStackView(views: [sTitle, sNote])
        sTexts.orientation = .vertical
        sTexts.alignment = .leading
        sTexts.spacing = 1
        let sButton = NSButton(title: "Stop Sharing", target: self, action: #selector(endSessionTapped))
        sButton.bezelColor = .systemRed
        sButton.controlSize = .large
        let sRow = NSStackView(views: [dot, sTexts, NSView(), sButton])
        sRow.spacing = 10
        sRow.alignment = .centerY
        let sCard = Card(sRow, inset: 12)
        sCard.fill = NSColor.systemGreen.withAlphaComponent(0.12)
        sCard.stroke = NSColor.systemGreen.withAlphaComponent(0.5)
        sCard.isHidden = true
        sessionCard = sCard
        sessionTitle = sTitle
        sessionButton = sButton
        header.spacing = 10
        header.alignment = .centerY

        // Devices.
        let devices = NSTextField(labelWithString: "CONNECT TO")
        devices.font = .systemFont(ofSize: 11, weight: .semibold)
        devices.textColor = .secondaryLabelColor
        let count = NSTextField(labelWithString: "")
        count.font = .systemFont(ofSize: 11)
        count.textColor = .secondaryLabelColor
        countLabel = count
        let refresh = symbolButton("arrow.clockwise", "Look again", #selector(lookAgain))
        let devicesHeader = NSStackView(views: [devices, count, NSView(), refresh])
        devicesHeader.spacing = 6
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
        scroll.setContentHuggingPriority(.defaultHigh, for: .vertical)
        NSLayoutConstraint.activate([
            listStack.topAnchor.constraint(equalTo: doc.topAnchor),
            listStack.leadingAnchor.constraint(equalTo: doc.leadingAnchor),
            listStack.trailingAnchor.constraint(equalTo: doc.trailingAnchor),
            doc.bottomAnchor.constraint(equalTo: listStack.bottomAnchor),
            doc.topAnchor.constraint(equalTo: scroll.contentView.topAnchor),
            doc.leadingAnchor.constraint(equalTo: scroll.contentView.leadingAnchor),
            doc.widthAnchor.constraint(equalTo: scroll.contentView.widthAnchor),
            scroll.heightAnchor.constraint(greaterThanOrEqualToConstant: 80),
        ])
        browser.onChange = { [weak self] hosts in
            // Not this Mac itself.
            let mine = GlassDevice.shared?.fingerprintHexPrefix ?? "-"
            self?.found = hosts.filter { $0.fingerprintPrefix?.lowercased() != mine }
            self?.refreshFound()
        }

        // A typed address, tucked away.
        addressLink = NSButton(title: "+  Connect by address", target: self, action: #selector(toggleAddress))
        addressLink.isBordered = false
        addressLink.contentTintColor = .controlAccentColor
        addressLink.font = .systemFont(ofSize: 13)
        hostField = NSTextField(string: UserDefaults.standard.string(forKey: "lastHost") ?? "")
        hostField.placeholderString = "Name or address, e.g. 192.168.1.20"
        hostField.controlSize = .large
        hostField.target = self
        hostField.action = #selector(connect)
        hostField.delegate = self
        connectButton = NSButton(title: "Connect", target: self, action: #selector(connect))
        connectButton.keyEquivalent = "\r"
        connectButton.controlSize = .large
        addressRow = NSStackView(views: [hostField, connectButton])
        addressRow.isHidden = true

        // Sharing, at the bottom.
        let shareTile = LayerView(fill: Look.track, stroke: nil, radius: 11)
        self.shareTile = shareTile
        let shareIcon = NSImageView(image: NSImage(systemSymbolName: "laptopcomputer", accessibilityDescription: nil) ?? NSImage())
        shareIcon.symbolConfiguration = .init(pointSize: 20, weight: .regular)
        shareIcon.contentTintColor = .secondaryLabelColor
        center(shareIcon, in: shareTile, size: 44)
        self.shareIcon = shareIcon
        let shareTitle = NSTextField(labelWithString: "Allow Connections")
        shareTitle.font = .systemFont(ofSize: 15, weight: .semibold)
        let shareNote = NSTextField(labelWithString: "Closed \u{00B7} nobody can connect")
        shareNote.font = .systemFont(ofSize: 12)
        shareNote.textColor = .secondaryLabelColor
        shareNote.lineBreakMode = .byTruncatingTail
        shareNote.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        self.shareNote = shareNote
        let shareText = NSStackView(views: [shareTitle, shareNote])
        shareText.orientation = .vertical
        shareText.alignment = .leading
        shareText.spacing = 2
        let toggle = NSSwitch()
        toggle.target = self
        toggle.action = #selector(shareSwitched(_:))
        shareSwitch = toggle
        let shareRow = NSStackView(views: [shareTile, shareText, NSView(), toggle])
        shareRow.spacing = 12
        shareRow.alignment = .centerY
        let shareBody = NSStackView(views: [shareRow])
        shareBody.orientation = .vertical
        shareBody.alignment = .leading
        shareBody.spacing = 12
        shareRow.widthAnchor.constraint(equalTo: shareBody.widthAnchor).isActive = true
        shareStack = shareBody
        let shareCard = Card(shareBody)

        // The list is as tall as its rows (up to the space there is), so the
        // address link sits right under it; the spare space goes below.
        let fit = scroll.heightAnchor.constraint(equalTo: doc.heightAnchor)
        fit.priority = .defaultHigh
        fit.isActive = true
        let spacer = NSView()
        spacer.setContentHuggingPriority(.init(1), for: .vertical)
        spacer.setContentCompressionResistancePriority(.init(1), for: .vertical)
        let stack = NSStackView(views: [header, sCard, devicesHeader, scroll, addressLink!, addressRow!, spacer, shareCard])
        stack.distribution = .fill
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 8
        stack.setCustomSpacing(20, after: header)
        stack.setCustomSpacing(12, after: addressRow)
        stack.edgeInsets = NSEdgeInsets(top: 40, left: 22, bottom: 20, right: 22)
        stack.translatesAutoresizingMaskIntoConstraints = false
        let content = NSView()
        content.addSubview(stack)
        var cs = [
            stack.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: content.trailingAnchor),
            stack.topAnchor.constraint(equalTo: content.topAnchor),
            stack.bottomAnchor.constraint(equalTo: content.bottomAnchor),
            icon.widthAnchor.constraint(equalToConstant: 44),
            icon.heightAnchor.constraint(equalToConstant: 44),
        ]
        for v in [header, sCard, devicesHeader, scroll, addressRow!, shareCard] as [NSView] {
            cs.append(v.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -44))
        }
        NSLayoutConstraint.activate(cs)
        w.contentView = content
        w.setContentSize(NSSize(width: 440, height: 700))
        w.center()
        connectWindow = w
    }

    private func symbolButton(_ symbol: String, _ tip: String, _ action: Selector) -> NSButton {
        let b = NSButton(image: NSImage(systemSymbolName: symbol, accessibilityDescription: tip) ?? NSImage(),
                         target: self, action: action)
        b.isBordered = false
        b.bezelStyle = .regularSquare
        b.symbolConfiguration = .init(pointSize: 15, weight: .regular)
        b.contentTintColor = .secondaryLabelColor
        b.toolTip = tip
        b.widthAnchor.constraint(equalToConstant: 30).isActive = true
        b.heightAnchor.constraint(equalToConstant: 30).isActive = true
        return b
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
        var (host, port) = parseAddress(hostField.stringValue)
        // A machine's name works too (addresses change; names don't): use
        // the address it has right now.
        if let h = found.first(where: { $0.name.caseInsensitiveCompare(host) == .orderedSame }) {
            (host, port) = (h.address, h.port)
        }
        viewingName = found.first(where: { $0.address == host })?.name ?? host
        guard !host.isEmpty, connectButton.isEnabled else { return }
        UserDefaults.standard.set(hostField.stringValue.trimmingCharacters(in: .whitespacesAndNewlines), forKey: "lastHost")
        connectTo(host, port)
    }

    /// Tries to connect again every second until `until`, then shows what
    /// happened.
    private func reconnect(_ host: String, _ port: UInt16, until: Date) {
        DispatchQueue.global(qos: .userInitiated).asyncAfter(deadline: .now() + 1) {
            let saved = UserDefaults.standard.object(forKey: "viewMode-\(host)") as? Int
                ?? UserDefaults.standard.object(forKey: "startView") as? Int
            let mode = UInt8(saved ?? Int(GLASS_MODE_FIT))
            let full = UserDefaults.standard.bool(forKey: "fullColour")
            let result = try? GlassClient(host: host, port: port, mode: mode, fullColour: full)
            DispatchQueue.main.async {
                if let client = result {
                    self.startViewer(client)
                } else if Date() < until {
                    self.reconnect(host, port, until: until)
                } else {
                    self.viewingHost = nil
                    self.showConnect()
                    showResults(self.droppedReports + "(couldn\u{2019}t reconnect within 15 s)")
                    self.droppedReports = ""
                }
            }
        }
    }

    private func connectTo(_ host: String, _ port: UInt16) {
        connectingHost = host
        setBusy("Connecting…")
        DispatchQueue.global(qos: .userInitiated).async {
            // The view last used with this host (default: fit), and the colour choice.
            let saved = UserDefaults.standard.object(forKey: "startView") as? Int
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
                guard let self else { return }
                let host = self.viewingHost
                self.viewer = nil
                // Dropped without the host ending it (its agent restarted, a
                // Wi-Fi blip): reconnect quietly for up to 15 s.
                if report.contains("(ended: the connection to the host dropped)"), let host {
                    self.droppedReports += report + "\n(reconnecting after the connection dropped)\n\n"
                    self.reconnect(host, self.viewingPort, until: Date().addingTimeInterval(15))
                    return
                }
                self.viewingHost = nil
                self.showConnect()
                showResults(self.droppedReports + report)
                self.droppedReports = ""
            }
            viewer = v
            viewingHost = client.host
            viewingPort = client.port
            connectingHost = nil
            // The Glass window stays (it shows Viewing, with Disconnect);
            // the viewer opens full screen in its own space. One session at
            // a time, so typed connects wait too.
            connectButton.title = "Connect"
            connectButton.isEnabled = false
            refreshFound()
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

// MARK: - Look (same palette as the Windows app)

enum Look {
    private static func rgb(_ r: CGFloat, _ g: CGFloat, _ b: CGFloat) -> NSColor {
        NSColor(srgbRed: r / 255, green: g / 255, blue: b / 255, alpha: 1)
    }
    private static func dyn(_ light: NSColor, _ dark: NSColor) -> NSColor {
        NSColor(name: nil) { $0.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua ? dark : light }
    }
    static let bg = dyn(rgb(243, 245, 248), rgb(17, 19, 23))
    static let card = dyn(.white, rgb(27, 31, 37))
    static let border = dyn(rgb(225, 229, 235), rgb(42, 48, 57))
    static let hover = dyn(rgb(241, 246, 253), rgb(33, 39, 48))
    static let track = dyn(rgb(234, 237, 242), rgb(37, 42, 50))
}

/// Top-down layout for the scrolling device list.
private final class FlippedView: NSView {
    override var isFlipped: Bool { true }
}

/// A rounded, layer-backed view whose colours follow light/dark.
class LayerView: NSView {
    var fill: NSColor { didSet { needsDisplay = true } }
    var stroke: NSColor? { didSet { needsDisplay = true } }
    var radius: CGFloat

    init(fill: NSColor = Look.card, stroke: NSColor? = Look.border, radius: CGFloat = 14) {
        self.fill = fill
        self.stroke = stroke
        self.radius = radius
        super.init(frame: .zero)
        wantsLayer = true
        translatesAutoresizingMaskIntoConstraints = false
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var wantsUpdateLayer: Bool { true }

    override func updateLayer() {
        effectiveAppearance.performAsCurrentDrawingAppearance {
            layer?.backgroundColor = fill.cgColor
            layer?.borderColor = stroke?.cgColor
        }
        layer?.cornerRadius = radius
        layer?.borderWidth = stroke == nil ? 0 : 1
    }
}

/// A card: `content` with 16 pt of padding.
final class Card: LayerView {
    init(_ content: NSView, inset: CGFloat = 16) {
        super.init()
        content.translatesAutoresizingMaskIntoConstraints = false
        addSubview(content)
        NSLayoutConstraint.activate([
            content.leadingAnchor.constraint(equalTo: leadingAnchor, constant: inset),
            content.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -inset),
            content.topAnchor.constraint(equalTo: topAnchor, constant: inset),
            content.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -inset),
        ])
    }

    required init?(coder: NSCoder) { fatalError("not used") }
}

func center(_ v: NSView, in box: NSView, size: CGFloat) {
    v.translatesAutoresizingMaskIntoConstraints = false
    box.addSubview(v)
    NSLayoutConstraint.activate([
        box.widthAnchor.constraint(equalToConstant: size),
        box.heightAnchor.constraint(equalToConstant: size),
        v.centerXAnchor.constraint(equalTo: box.centerXAnchor),
        v.centerYAnchor.constraint(equalTo: box.centerYAnchor),
    ])
}

/// One device: click anywhere to connect. Hover shows a Connect pill.
final class DeviceRow: LayerView {
    var onClick: (() -> Void)?
    private let enabled: Bool
    private let statusStack: NSStackView
    private let pill = LayerView(fill: .controlAccentColor, stroke: nil, radius: 13)
    private var hovering = false {
        didSet {
            let on = hovering && enabled
            fill = on ? Look.hover : Look.card
            stroke = on ? NSColor.controlAccentColor.withAlphaComponent(0.6) : Look.border
            pill.isHidden = !on
            statusStack.isHidden = on
        }
    }

    init(name: String, detail: String, mac: Bool, status: String, statusColour: NSColor, enabled: Bool, busy: Bool,
         action: String = "Connect") {
        self.enabled = enabled
        let dot = LayerView(fill: statusColour, stroke: nil, radius: 4)
        dot.widthAnchor.constraint(equalToConstant: 8).isActive = true
        dot.heightAnchor.constraint(equalToConstant: 8).isActive = true
        let statusLabel = NSTextField(labelWithString: status)
        statusLabel.font = .systemFont(ofSize: 12)
        statusLabel.textColor = .secondaryLabelColor
        statusStack = NSStackView(views: [dot, statusLabel])
        statusStack.spacing = 6
        statusStack.alignment = .centerY
        super.init()

        let tile = LayerView(fill: NSColor.controlAccentColor.withAlphaComponent(0.13), stroke: nil, radius: 11)
        let img = NSImageView(image: NSImage(systemSymbolName: mac ? "laptopcomputer" : "desktopcomputer",
                                             accessibilityDescription: nil) ?? NSImage())
        img.symbolConfiguration = .init(pointSize: 20, weight: .regular)
        img.contentTintColor = .controlAccentColor
        center(img, in: tile, size: 44)
        let title = NSTextField(labelWithString: name)
        title.font = .systemFont(ofSize: 15, weight: .semibold)
        title.lineBreakMode = .byTruncatingTail
        title.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        let sub = NSTextField(labelWithString: detail)
        sub.font = .systemFont(ofSize: 12)
        sub.textColor = .secondaryLabelColor
        let texts = NSStackView(views: [title, sub])
        texts.orientation = .vertical
        texts.alignment = .leading
        texts.spacing = 2
        if !enabled && !busy {
            tile.alphaValue = 0.5
            title.alphaValue = 0.5
        }

        let pillLabel = NSTextField(labelWithString: action)
        if action != "Connect" { pill.fill = .systemRed }
        pillLabel.font = .systemFont(ofSize: 12, weight: .semibold)
        pillLabel.textColor = .white
        pillLabel.translatesAutoresizingMaskIntoConstraints = false
        pill.addSubview(pillLabel)
        NSLayoutConstraint.activate([
            pillLabel.leadingAnchor.constraint(equalTo: pill.leadingAnchor, constant: 14),
            pillLabel.trailingAnchor.constraint(equalTo: pill.trailingAnchor, constant: -14),
            pillLabel.centerYAnchor.constraint(equalTo: pill.centerYAnchor),
            pill.heightAnchor.constraint(equalToConstant: 26),
        ])
        pill.isHidden = true

        var right: [NSView] = [statusStack, pill]
        if busy {
            let spin = NSProgressIndicator()
            spin.style = .spinning
            spin.controlSize = .small
            spin.startAnimation(nil)
            right = [spin]
        }
        let row = NSStackView(views: [tile, texts, NSView()] + right)
        row.spacing = 14
        row.alignment = .centerY
        row.translatesAutoresizingMaskIntoConstraints = false
        addSubview(row)
        NSLayoutConstraint.activate([
            heightAnchor.constraint(equalToConstant: 70),
            row.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 13),
            row.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -16),
            row.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        trackingAreas.forEach(removeTrackingArea)
        addTrackingArea(NSTrackingArea(rect: .zero, options: [.mouseEnteredAndExited, .activeAlways, .inVisibleRect],
                                       owner: self, userInfo: nil))
    }

    override func mouseEntered(with event: NSEvent) { hovering = true }
    override func mouseExited(with event: NSEvent) { hovering = false }
    override func mouseDown(with event: NSEvent) {}
    override func mouseUp(with event: NSEvent) {
        guard enabled, bounds.contains(convert(event.locationInWindow, from: nil)) else { return }
        onClick?()
    }
    override func resetCursorRects() {
        if enabled { addCursorRect(bounds, cursor: .pointingHand) }
    }
}

/// Shown while no device has been found: the same radar as the Windows app
/// (three rings growing from 24 to 68 pt and fading, a 2.2 s cycle) around
/// a monitor in a soft accent circle.
final class EmptyState: LayerView {
    var onEnd: () -> Void = {}
    private var endButton: NSButton!

    @objc private func endTapped() { onEnd() }

    /// `session`: in a session (says who), so no searching animation.
    init(session: String? = nil) {
        super.init()
        endButton = NSButton(title: "End session", target: nil, action: nil)
        let radar = RadarView(animated: session == nil)
        let title = NSTextField(labelWithString: session == nil ? "Looking for devices\u{2026}" : "In a session")
        title.font = .systemFont(ofSize: 15, weight: .semibold)
        let note = NSTextField(labelWithString: session ?? "Turn on sharing on your Mac or PC and it appears here.")
        note.font = .systemFont(ofSize: 12)
        note.textColor = .secondaryLabelColor
        note.lineBreakMode = .byTruncatingTail
        endButton.bezelColor = .systemRed
        endButton.controlSize = .large
        endButton.isHidden = session == nil
        endButton.target = self
        endButton.action = #selector(endTapped)
        let stack = NSStackView(views: [radar, title, note, endButton])
        stack.orientation = .vertical
        stack.alignment = .centerX
        stack.spacing = 8
        stack.setCustomSpacing(4, after: radar)
        stack.translatesAutoresizingMaskIntoConstraints = false
        addSubview(stack)
        NSLayoutConstraint.activate([
            heightAnchor.constraint(equalToConstant: session == nil ? 210 : 250),
            stack.centerXAnchor.constraint(equalTo: centerXAnchor),
            stack.topAnchor.constraint(equalTo: topAnchor, constant: 8),
        ])
    }

    required init?(coder: NSCoder) { fatalError("not used") }
}

/// Expanding, fading rings (Core Animation, so no timers or redraws).
final class RadarView: NSView {
    private static let side: CGFloat = 140
    private static let period: CFTimeInterval = 1 / 0.45
    private let rings = (0..<3).map { _ in CAShapeLayer() }
    private let dot = CAShapeLayer()
    private let animated: Bool

    init(animated: Bool = true) {
        self.animated = animated
        super.init(frame: NSRect(x: 0, y: 0, width: Self.side, height: Self.side))
        wantsLayer = true
        translatesAutoresizingMaskIntoConstraints = false
        widthAnchor.constraint(equalToConstant: Self.side).isActive = true
        heightAnchor.constraint(equalToConstant: Self.side).isActive = true
        layer?.addSublayer(dot)
        for r in rings {
            r.fillColor = nil
            r.lineWidth = 1.5
            r.opacity = 0
            layer?.addSublayer(r)
        }
        let img = NSImageView(image: NSImage(systemSymbolName: "desktopcomputer", accessibilityDescription: nil) ?? NSImage())
        img.symbolConfiguration = .init(pointSize: 18, weight: .regular)
        img.contentTintColor = animated ? .controlAccentColor : .systemGreen
        img.translatesAutoresizingMaskIntoConstraints = false
        addSubview(img)
        img.centerXAnchor.constraint(equalTo: centerXAnchor).isActive = true
        img.centerYAnchor.constraint(equalTo: centerYAnchor).isActive = true
        applyColours()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    private func circle(_ radius: CGFloat) -> CGPath {
        let c = CGPoint(x: bounds.midX, y: bounds.midY)
        return CGPath(ellipseIn: CGRect(x: c.x - radius, y: c.y - radius, width: radius * 2, height: radius * 2), transform: nil)
    }

    private var laidOut = CGRect.null

    override func layout() {
        super.layout()
        guard bounds != laidOut else { return }
        laidOut = bounds
        dot.frame = bounds
        dot.path = circle(24)
        for r in rings { r.frame = bounds }
        startAnimating()
    }

    private func startAnimating() {
        guard animated else { return }
        let now = CACurrentMediaTime()
        for (i, r) in rings.enumerated() {
            r.removeAllAnimations()
            let grow = CABasicAnimation(keyPath: "path")
            grow.fromValue = circle(24)
            grow.toValue = circle(68)
            let fade = CABasicAnimation(keyPath: "opacity")
            fade.fromValue = 0.55
            fade.toValue = 0
            let group = CAAnimationGroup()
            group.animations = [grow, fade]
            group.duration = Self.period
            group.repeatCount = .infinity
            group.timingFunction = CAMediaTimingFunction(name: .linear)
            // Staggered by a third so the rings are evenly spaced from the start.
            group.beginTime = now - Self.period * Double(i) / 3
            r.add(group, forKey: "radar")
        }
    }

    private func applyColours() {
        effectiveAppearance.performAsCurrentDrawingAppearance {
            let accent: NSColor = animated ? .controlAccentColor : .systemGreen
            dot.fillColor = accent.withAlphaComponent(0.14).cgColor
            for r in rings { r.strokeColor = accent.cgColor }
        }
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        applyColours()
    }
}
