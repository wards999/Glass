import AppKit

/// The viewer's drop-down control bar (M3d), as on the Windows viewer: click
/// the floating handle near the top and it appears. One-click view and colour
/// switches, full screen, stats, key style, pin, disconnect and a status line.
/// Controls never take keyboard focus, so typing keeps going to the host.
final class ControlBar: NSVisualEffectView {
    enum Command {
        case view(UInt8), colour(Bool), fullScreen, stats, pcKeys, testLink, disconnect
    }

    var onCommand: (Command) -> Void = { _ in }
    private(set) var pinned = false
    private let views = NSSegmentedControl()
    private let colour = NSSegmentedControl()
    private let full = NSButton()
    private let stats = NSButton()
    private let keys = NSButton()
    private let pin = NSButton()
    /// The weak-link test mode (M5), orange while on.
    private let testLink = NSButton()
    private let status = NSTextField(labelWithString: "")
    /// Segment index -> view mode: Fit, More space, Larger, host's screen
    /// (only the host's screen when the host can't do the others).
    private let modes: [UInt8]

    init(hostIsMac: Bool, viewsOK: Bool, fullColourOK: Bool) {
        // Views the host can't do at all are left out (a PC host shares only
        // its own screen until it gets a virtual display, M7).
        modes = viewsOK ? [1, 2, 3, 0] : [0]
        super.init(frame: .zero)
        material = .hudWindow
        blendingMode = .withinWindow
        state = .active
        wantsLayer = true
        layer?.cornerRadius = 8
        layer?.maskedCorners = [.layerMinXMinYCorner, .layerMaxXMinYCorner]

        views.segmentStyle = .rounded
        let hostScreen = hostIsMac ? "Mac's screen" : "PC's screen"
        let labels = viewsOK ? ["Fit", "More space", "Larger", hostScreen] : [hostScreen]
        views.segmentCount = labels.count
        for (i, label) in labels.enumerated() { views.setLabel(label, forSegment: i) }
        if !viewsOK {
            views.toolTip = "This host shares only its own screen for now; more views come in a later update."
        }
        views.trackingMode = .selectOne
        views.target = self
        views.action = #selector(viewChanged)

        colour.segmentStyle = .rounded
        colour.segmentCount = 2
        colour.setLabel("Fast (4:2:0)", forSegment: 0)
        colour.setLabel("Full colour (4:4:4)", forSegment: 1)
        colour.setEnabled(fullColourOK, forSegment: 1)
        colour.trackingMode = .selectOne
        colour.target = self
        colour.action = #selector(colourChanged)

        func button(_ b: NSButton, _ title: String, _ action: Selector, toggle: Bool = false) {
            b.title = title
            b.bezelStyle = .rounded
            b.setButtonType(toggle ? .pushOnPushOff : .momentaryPushIn)
            b.target = self
            b.action = action
        }
        button(full, "Full screen", #selector(fullTapped))
        button(stats, "Stats", #selector(statsTapped), toggle: true)
        button(keys, "PC keys", #selector(keysTapped), toggle: true)
        keys.isHidden = hostIsMac // between Macs, ⌘ is ⌘
        keys.toolTip = "On: ⌘ acts as the Windows key and ⌃ as Ctrl. Off: ⌘ acts as Ctrl (⌘C copies)."
        button(testLink, "Weak Wi-Fi test", #selector(testLinkTapped), toggle: true)
        testLink.toolTip = "Test mode: what arrives goes through a pretend 15 Mbps link, to try rate control. Logged in the results."
        button(pin, "Pin", #selector(pinTapped), toggle: true)
        let disconnect = NSButton()
        button(disconnect, "Disconnect", #selector(disconnectTapped))
        status.textColor = .secondaryLabelColor
        status.font = .systemFont(ofSize: 12)
        // The status gives way first so the bar always fits the window.
        status.lineBreakMode = .byTruncatingTail
        status.setContentCompressionResistancePriority(.init(1), for: .horizontal)

        for c in [views, colour, full, stats, keys, testLink, pin, disconnect] as [NSControl] { c.refusesFirstResponder = true }
        let row = NSStackView(views: [views, colour, full, stats, keys, testLink, pin, disconnect, status])
        row.orientation = .horizontal
        row.spacing = 8
        row.edgeInsets = NSEdgeInsets(top: 6, left: 10, bottom: 6, right: 12)
        row.translatesAutoresizingMaskIntoConstraints = false
        addSubview(row)
        NSLayoutConstraint.activate([
            row.leadingAnchor.constraint(equalTo: leadingAnchor),
            row.trailingAnchor.constraint(equalTo: trailingAnchor),
            row.topAnchor.constraint(equalTo: topAnchor),
            row.bottomAnchor.constraint(equalTo: bottomAnchor),
        ])
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var acceptsFirstResponder: Bool { false }

    func update(mode: UInt8, fullColour: Bool, fullScreen: Bool, statsShown: Bool, pcKeys: Bool, testLinkMbps: UInt32, status text: String) {
        if let i = modes.firstIndex(of: mode) { views.selectedSegment = i }
        colour.selectedSegment = fullColour ? 1 : 0
        full.title = fullScreen ? "Window" : "Full screen"
        stats.state = statsShown ? .on : .off
        keys.state = pcKeys ? .on : .off
        pin.state = pinned ? .on : .off
        let testOn = testLinkMbps > 0
        testLink.state = testOn ? .on : .off
        testLink.title = testOn ? "Weak Wi-Fi test · \(testLinkMbps) Mbps" : "Weak Wi-Fi test"
        testLink.bezelColor = testOn ? .systemOrange : nil
        testLink.contentTintColor = testOn ? .white : nil
        status.stringValue = text
    }

    @objc private func viewChanged() {
        let i = views.selectedSegment
        if i >= 0, i < modes.count { onCommand(.view(modes[i])) }
    }
    @objc private func colourChanged() { onCommand(.colour(colour.selectedSegment == 1)) }
    @objc private func fullTapped() { onCommand(.fullScreen) }
    @objc private func statsTapped() { onCommand(.stats) }
    @objc private func keysTapped() { onCommand(.pcKeys) }
    @objc private func testLinkTapped() { onCommand(.testLink) }
    @objc private func pinTapped() { pinned.toggle() }
    @objc private func disconnectTapped() { onCommand(.disconnect) }
}

/// The bar's floating handle (as on the Windows viewer): a small rounded tab
/// near the top of the viewer with a chevron. A click toggles the bar; it can
/// be dragged sideways and the position is kept. Clicks on it never reach
/// the host.
final class BarHandle: NSView {
    var onClick: () -> Void = {}
    /// Dragged: the new centre, as a fraction 0...1 across the viewer.
    var onMoved: (CGFloat) -> Void = { _ in }
    /// Centre X relative to the superview's centre (set by the owner).
    var centre: NSLayoutConstraint?
    private var dragStart: (x: CGFloat, constant: CGFloat)?
    private var moved = false
    static let size = NSSize(width: 60, height: 24)

    init() {
        super.init(frame: NSRect(origin: .zero, size: Self.size))
        wantsLayer = true
        translatesAutoresizingMaskIntoConstraints = false
        alphaValue = 0.85
        let chevron = NSImageView(image: NSImage(systemSymbolName: "chevron.compact.down", accessibilityDescription: "Glass controls") ?? NSImage())
        chevron.symbolConfiguration = .init(pointSize: 15, weight: .semibold)
        chevron.contentTintColor = NSColor(srgbRed: 0.96, green: 0.95, blue: 0.93, alpha: 1)
        chevron.translatesAutoresizingMaskIntoConstraints = false
        addSubview(chevron)
        NSLayoutConstraint.activate([
            widthAnchor.constraint(equalToConstant: Self.size.width),
            heightAnchor.constraint(equalToConstant: Self.size.height),
            chevron.centerXAnchor.constraint(equalTo: centerXAnchor),
            chevron.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
        toolTip = "Glass controls (drag to move)"
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var wantsUpdateLayer: Bool { true }

    override func updateLayer() {
        layer?.backgroundColor = NSColor(srgbRed: 40 / 255, green: 46 / 255, blue: 56 / 255, alpha: 1).cgColor
        layer?.borderColor = NSColor(srgbRed: 130 / 255, green: 140 / 255, blue: 150 / 255, alpha: 1).cgColor
        layer?.borderWidth = 1
        layer?.cornerRadius = Self.size.height / 2
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        trackingAreas.forEach(removeTrackingArea)
        addTrackingArea(NSTrackingArea(rect: .zero, options: [.mouseEnteredAndExited, .activeAlways, .inVisibleRect],
                                       owner: self, userInfo: nil))
    }

    override func mouseEntered(with event: NSEvent) { alphaValue = 1 }
    override func mouseExited(with event: NSEvent) { if dragStart == nil { alphaValue = 0.85 } }
    override func resetCursorRects() { addCursorRect(bounds, cursor: .pointingHand) }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
    override var acceptsFirstResponder: Bool { false }

    override func mouseDown(with event: NSEvent) {
        guard let sv = superview else { return }
        dragStart = (sv.convert(event.locationInWindow, from: nil).x, centre?.constant ?? 0)
        moved = false
    }

    override func mouseDragged(with event: NSEvent) {
        guard let start = dragStart, let sv = superview else { return }
        let dx = sv.convert(event.locationInWindow, from: nil).x - start.x
        if abs(dx) > 3 { moved = true }
        guard moved else { return }
        let half = max(0, (sv.bounds.width - bounds.width) / 2)
        centre?.constant = max(-half, min(half, start.constant + dx))
    }

    override func mouseUp(with event: NSEvent) {
        defer { dragStart = nil }
        guard dragStart != nil else { return }
        if moved, let sv = superview {
            let span = max(1, sv.bounds.width - bounds.width)
            onMoved(min(1, max(0, 0.5 + (centre?.constant ?? 0) / span)))
        } else {
            onClick()
        }
    }
}
