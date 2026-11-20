import AppKit
import CGlassCore
import Foundation
import GlassKit
import UniformTypeIdentifiers

/// Finder / double-click mode: connect window -> live viewer (or a recording)
/// -> results window -> back to the connect window.
/// LATER(M6): menu-bar shell, device list (mDNS), pairing UI.
final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    private var connectWindow: NSWindow?
    private var hostField: NSTextField!
    private var connectButton: NSButton!
    private var player: Player?
    private var viewer: LiveViewer?
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
        submenu("Connection", [
            item("Connect…", #selector(showConnectAction), "n", target: self),
            item("Open Recording…", #selector(openRecording), "o", target: self),
            item("Disconnect", #selector(NSWindow.performClose(_:)), "w", chord),
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
        connectWindow?.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    private func buildConnectWindow() {
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 460, height: 170),
                         styleMask: [.titled, .closable, .miniaturizable], backing: .buffered, defer: false)
        w.title = "Glass \(appVersion)"
        w.isReleasedWhenClosed = false
        w.delegate = self

        let label = NSTextField(labelWithString: "PC address (host.cmd on the PC shows it):")
        hostField = NSTextField(string: UserDefaults.standard.string(forKey: "lastHost") ?? "")
        hostField.placeholderString = "e.g. 192.168.1.20"
        hostField.font = .monospacedSystemFont(ofSize: 14, weight: .regular)
        hostField.target = self
        hostField.action = #selector(connect)
        connectButton = NSButton(title: "Connect", target: self, action: #selector(connect))
        connectButton.keyEquivalent = "\r"
        let open = NSButton(title: "Open Recording…", target: self, action: #selector(openRecording))
        let note = NSTextField(labelWithString: "Opens full screen; mouse and keyboard go to the PC. ⌃⌥⌘F window, ⌃⌥⌘W disconnect.")
        note.textColor = .secondaryLabelColor
        note.font = .systemFont(ofSize: 11)

        let buttons = NSStackView(views: [open, NSView(), connectButton])
        buttons.orientation = .horizontal
        let stack = NSStackView(views: [label, hostField, note, buttons])
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
        connectButton.isEnabled = false
        connectButton.title = "Connecting…"
        DispatchQueue.global(qos: .userInitiated).async {
            let result = Result { try GlassClient(host: host, port: port) }
            DispatchQueue.main.async {
                switch result {
                case .success(let client):
                    self.startViewer(client)
                case .failure(let error):
                    self.showConnect()
                    showError("Couldn't connect", "\(error)")
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
