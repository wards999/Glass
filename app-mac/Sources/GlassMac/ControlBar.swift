import AppKit

/// The viewer's drop-down control bar (M3d), as on the Windows viewer: move
/// the pointer to the top edge and it appears. One-click view and colour
/// switches, full screen, stats, key style, pin, disconnect and a status line.
/// Controls never take keyboard focus, so typing keeps going to the host.
final class ControlBar: NSVisualEffectView {
    enum Command {
        case view(UInt8), colour(Bool), fullScreen, stats, pcKeys, disconnect
    }

    var onCommand: (Command) -> Void = { _ in }
    private(set) var pinned = false
    private let views = NSSegmentedControl()
    private let colour = NSSegmentedControl()
    private let full = NSButton()
    private let stats = NSButton()
    private let keys = NSButton()
    private let pin = NSButton()
    private let status = NSTextField(labelWithString: "")
    /// Segment index -> view mode: Fit, More space, Larger, host's screen.
    private static let modes: [UInt8] = [1, 2, 3, 0]

    init(hostIsMac: Bool, viewsOK: Bool, fullColourOK: Bool) {
        super.init(frame: .zero)
        material = .hudWindow
        blendingMode = .withinWindow
        state = .active
        wantsLayer = true
        layer?.cornerRadius = 8
        layer?.maskedCorners = [.layerMinXMinYCorner, .layerMaxXMinYCorner]

        views.segmentStyle = .rounded
        views.segmentCount = 4
        for (i, label) in ["Fit", "More space", "Larger", hostIsMac ? "Mac's screen" : "PC's screen"].enumerated() {
            views.setLabel(label, forSegment: i)
            views.setEnabled(viewsOK || i == 3, forSegment: i)
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
        button(pin, "Pin", #selector(pinTapped), toggle: true)
        let disconnect = NSButton()
        button(disconnect, "Disconnect", #selector(disconnectTapped))
        status.textColor = .secondaryLabelColor
        status.font = .systemFont(ofSize: 12)

        for c in [views, colour, full, stats, keys, pin, disconnect] as [NSControl] { c.refusesFirstResponder = true }
        let row = NSStackView(views: [views, colour, full, stats, keys, pin, disconnect, status])
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

    func update(mode: UInt8, fullColour: Bool, fullScreen: Bool, statsShown: Bool, pcKeys: Bool, status text: String) {
        if let i = Self.modes.firstIndex(of: mode) { views.selectedSegment = i }
        colour.selectedSegment = fullColour ? 1 : 0
        full.title = fullScreen ? "Window" : "Full screen"
        stats.state = statsShown ? .on : .off
        keys.state = pcKeys ? .on : .off
        pin.state = pinned ? .on : .off
        status.stringValue = text
    }

    @objc private func viewChanged() {
        let i = views.selectedSegment
        if i >= 0, i < Self.modes.count { onCommand(.view(Self.modes[i])) }
    }
    @objc private func colourChanged() { onCommand(.colour(colour.selectedSegment == 1)) }
    @objc private func fullTapped() { onCommand(.fullScreen) }
    @objc private func statsTapped() { onCommand(.stats) }
    @objc private func keysTapped() { onCommand(.pcKeys) }
    @objc private func pinTapped() { pinned.toggle() }
    @objc private func disconnectTapped() { onCommand(.disconnect) }
}
