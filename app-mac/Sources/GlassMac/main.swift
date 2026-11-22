import AppKit
import CGlassCore
import Foundation
import GlassKit

// Ways in:
//  - Finder:    open Glass.app -> connect window (live viewer, M2) or a recording (M1)
//  - Terminal:  GlassMac play FILE.glsr [--bench]           recording bench + playback
//               GlassMac connect HOST[:PORT] [--pin PIN] [--seconds S]
//                                                live viewer (pairs first with --pin), results printed
//               GlassMac host [--test-pattern] [--pin PIN] [--port P] [--mbps M] [--seconds S]
//                                                share this Mac (M3b); results printed per session
//               GlassMac check-444               can this Mac hardware-encode HEVC 4:4:4?

let args = Array(CommandLine.arguments.dropFirst()).filter { !$0.hasPrefix("-psn_") }
let app = NSApplication.shared

func usage() {
    print("""
    \(banner)

    usage:
      GlassMac play FILE.glsr            decode bench, then playback in a window
      GlassMac play FILE.glsr --bench    decode bench only (no window)
      GlassMac connect HOST[:PORT] [--pin PIN] [--seconds S]
                                         live viewer; prints results when it ends.
                                         Not paired yet: --pin with the PIN the host shows.
      GlassMac host [--test-pattern] [--pin PIN] [--port P] [--mbps M] [--seconds S] [--no-444] [--measure]
                                         share this Mac; with --seconds, exits after one session.
                                         --measure: sharpness (PSNR) of what is sent.
                                         --test-pattern: synthetic picture, input not injected.
      GlassMac check-444                 can this Mac hardware-encode HEVC 4:4:4?
    Or open Glass.app and connect to the PC (run host.cmd there), or Share This Mac.
    """)
}

if args.first == "play", args.count >= 2 {
    do {
        let rec = try Recording(contentsOf: URL(fileURLWithPath: args[1]))
        print("\(banner)\n\(systemInfo())\n\(args[1]): \(describe(rec))")
        print(try bench(rec))
        if !args.contains("--bench") {
            let player = try Player(rec)
            player.onFinish = { print($0); exit(0) }
            app.setActivationPolicy(.regular)
            player.open()
            withExtendedLifetime(player) { app.run() }
        }
    } catch {
        print("error: \(error)")
        exit(1)
    }
} else if args.first == "connect", args.count >= 2 {
    var host = args[1], port = GlassClient.defaultPort
    if let i = host.lastIndex(of: ":"), let p = UInt16(host[host.index(after: i)...]) {
        port = p
        host = String(host[..<i])
    }
    func value(_ flag: String) -> String? { args.firstIndex(of: flag).flatMap { args.dropFirst($0 + 1).first } }
    let seconds = value("--seconds").flatMap { Double($0) }
    do {
        let client: GlassClient
        do {
            client = try GlassClient(host: host, port: port)
        } catch let f as ConnectFailure where f.needsPairing {
            guard let pin = value("--pin"), let device = GlassDevice.shared else { throw f }
            let pairing = try GlassPairing(device: device, host: host, port: port)
            print("Pairing with \"\(pairing.hostName)\" …")
            try pairing.finish(pin: pin)
            client = try GlassClient(host: host, port: port)
        }
        let viewer = try LiveViewer(client: client)
        viewer.onFinish = { print($0); exit(0) }
        app.setActivationPolicy(.regular)
        viewer.open()
        if let seconds {
            DispatchQueue.main.asyncAfter(deadline: .now() + seconds) { viewer.stop("--seconds elapsed") }
        }
        withExtendedLifetime(viewer) { app.run() }
    } catch {
        print("error: \(error)")
        exit(1)
    }
} else if args.first == "check-444" {
    print("\(banner)\n\(systemInfo())")
    print(checkHEVC444Encode())
    exit(0)
} else if args.first == "host" {
    func value(_ flag: String) -> String? { args.firstIndex(of: flag).flatMap { args.dropFirst($0 + 1).first } }
    var o = HostRunner.Options()
    o.testPattern = args.contains("--test-pattern")
    o.allow444 = !args.contains("--no-444")
    o.clipboard = !args.contains("--no-clipboard")
    HostRunner.measureQuality = args.contains("--measure")
    o.fixedPin = value("--pin")
    if let p = value("--port").flatMap({ UInt16($0) }) { o.port = p }
    if let m = value("--mbps").flatMap({ Int($0) }) { o.mbps = m }
    o.seconds = value("--seconds").flatMap { Double($0) }
    guard let device = GlassDevice.shared else {
        print("error: couldn't read or create this Mac's Glass identity")
        exit(1)
    }
    if !o.testPattern {
        if !HostPermissions.screenRecording { print("Screen Recording isn't allowed for this program yet (System Settings ▸ Privacy & Security).") }
        if !HostPermissions.accessibility { print("Accessibility isn't allowed for this program yet: the viewer's input will do nothing.") }
    }
    do {
        let runner = try HostRunner(device: device, options: o, banner: banner)
        runner.onEvent = { print($0.text) }
        runner.onStreamChanged = { print("Streaming: \($0)") }
        runner.onConnected = { name, addr in print("Viewer connected: \u{201C}\(name)\u{201D} from \(addr) (encrypted)") }
        runner.onSessionEnd = { report in
            print(report)
            if o.seconds != nil { exit(0) }
        }
        let d = runner.display
        print("\(banner)\nThis Mac: \u{201C}\(GlassDevice.computerName)\u{201D}, id \(device.fingerprintShort) | \(d.width)x\(d.height) @ \(d.fps) Hz\(o.testPattern ? " (test pattern)" : "") | port \(runner.host.port) | addresses \(localIPv4Addresses().joined(separator: ", "))")
        print(checkHEVC444Encode())
        runner.start()
        app.setActivationPolicy(.accessory)
        withExtendedLifetime(runner) { app.run() }
    } catch {
        print("error: \(error)")
        exit(1)
    }
} else if !args.isEmpty || Bundle.main.bundleIdentifier == nil {
    // Bare binary (not inside Glass.app), or unknown arguments: print usage.
    usage()
} else {
    let delegate = AppDelegate()
    app.setActivationPolicy(.regular)
    app.delegate = delegate
    withExtendedLifetime(delegate) { app.run() }
}
