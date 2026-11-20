import AppKit
import CGlassCore
import Foundation
import GlassKit

// Ways in:
//  - Finder:    open Glass.app -> connect window (live viewer, M2) or a recording (M1)
//  - Terminal:  GlassMac play FILE.glsr [--bench]           recording bench + playback
//               GlassMac connect HOST[:PORT] [--pin PIN] [--seconds S]
//                                                live viewer (pairs first with --pin), results printed

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
    Or open Glass.app and connect to the PC (run host.cmd there).
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
} else if !args.isEmpty || Bundle.main.bundleIdentifier == nil {
    // Bare binary (not inside Glass.app), or unknown arguments: print usage.
    usage()
} else {
    let delegate = AppDelegate()
    app.setActivationPolicy(.regular)
    app.delegate = delegate
    withExtendedLifetime(delegate) { app.run() }
}
