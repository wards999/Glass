import CGlassCore
import CoreVideo
import Metal
import XCTest
@testable import GlassKit

final class GlassCoreTests: XCTestCase {
    func testCoreLinksAndReportsVersions() {
        XCTAssertFalse(GlassCore.version.isEmpty)
        XCTAssertEqual(GlassCore.version.split(separator: ".").count, 3)
        XCTAssertEqual(GlassCore.protocolVersion, 13)
    }
}

/// Fixtures: 12 frames of FFmpeg testsrc2 at 320x192, encoded with hevc_nvenc
/// on Windows (same settings family as `glass record`) and packed with `glass pack`.
final class PipelineTests: XCTestCase {
    func fixture(_ name: String) throws -> Recording {
        let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "glsr", subdirectory: "Fixtures"))
        return try Recording(contentsOf: url)
    }

    func testParsesRecording() throws {
        for (name, is444) in [("testsrc-420", false), ("testsrc-444", true)] {
            let rec = try fixture(name)
            XCTAssertEqual(rec.header.width, 320)
            XCTAssertEqual(rec.header.height, 192)
            XCTAssertEqual(rec.is444, is444)
            XCTAssertEqual(rec.frames.count, 12)
            XCTAssertTrue(rec.frames[0].isIDR)
            XCTAssertEqual(rec.frames.map(\.meta.frame_id), Array(0..<12))
        }
    }

    func testRejectsGarbage() {
        XCTAssertThrowsError(try Recording(data: Data("not a recording".utf8)))
    }

    func decodeAll(_ rec: Recording) throws -> (HEVCDecoder, [CVPixelBuffer]) {
        let dec = HEVCDecoder(chroma444: rec.is444)
        var out: [CVPixelBuffer] = []
        for f in rec.frames {
            if let pb = try rec.withFrameBytes(f, { try dec.decode($0) }) { out.append(pb) }
        }
        return (dec, out)
    }

    func testDecodes420() throws {
        let (dec, frames) = try decodeAll(try fixture("testsrc-420"))
        XCTAssertEqual(frames.count, 12)
        XCTAssertEqual(CVPixelBufferGetWidth(frames[0]), 320)
        XCTAssertEqual(CVPixelBufferGetPixelFormatType(frames[0]), kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)
        XCTAssertNotNil(CVPixelBufferGetIOSurface(frames[0]))
        XCTAssertFalse(Renderer.isBT709(frames[0]), "NVENC RGB input is tagged BT.601")
        print("4:2:0 decoder: \(dec.usingHardware ? "hardware" : "software")")
    }

    func testDecodes444() throws {
        let rec = try fixture("testsrc-444")
        let dec: HEVCDecoder, frames: [CVPixelBuffer]
        do {
            (dec, frames) = try decodeAll(rec)
        } catch {
            throw XCTSkip("HEVC 4:4:4 decode unavailable on this machine: \(error)")
        }
        XCTAssertEqual(frames.count, 12)
        XCTAssertEqual(CVPixelBufferGetPixelFormatType(frames[0]), kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange)
        XCTAssertEqual(CVPixelBufferGetWidthOfPlane(frames[0], 1), 320, "4:4:4 chroma plane is full width")
        print("4:4:4 decoder: \(dec.usingHardware ? "hardware" : "software")")
    }

    /// Renders the first 4:2:0 frame offscreen and checks the shader produced
    /// real colour (testsrc2 is mostly saturated colour bars, never all black).
    func testRendersOffscreen() throws {
        guard let device = MTLCreateSystemDefaultDevice() else { throw XCTSkip("no Metal device") }
        let (_, frames) = try decodeAll(try fixture("testsrc-420"))
        let renderer = try Renderer(device: device)
        let desc = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .bgra8Unorm, width: 320, height: 192, mipmapped: false)
        desc.usage = [.renderTarget, .shaderRead]
        desc.storageMode = .shared
        let target = try XCTUnwrap(device.makeTexture(descriptor: desc))
        let cb = try XCTUnwrap(renderer.queue.makeCommandBuffer())
        XCTAssertTrue(renderer.encode(frames[0], to: target, in: cb))
        cb.commit()
        cb.waitUntilCompleted()
        XCTAssertNil(cb.error)
        var px = [UInt8](repeating: 0, count: 320 * 192 * 4)
        target.getBytes(&px, bytesPerRow: 320 * 4, from: MTLRegionMake2D(0, 0, 320, 192), mipmapLevel: 0)
        var distinct = Set<UInt32>()
        for i in stride(from: 0, to: px.count, by: 4 * 97) {
            let b = UInt32(px[i]), g = UInt32(px[i + 1]), r = UInt32(px[i + 2])
            distinct.insert((r << 16) | (g << 8) | b)
        }
        XCTAssertGreaterThan(distinct.count, 8, "expected a colourful test pattern")
    }
}

/// Live path over loopback: core's replay host streams a fixture, GlassClient
/// receives it (same code the viewer uses), VideoToolbox decodes it.
final class LiveTests: XCTestCase {
    static let pin = "135790"

    /// A fresh identity in a temporary folder (never the user's real one).
    func tempDevice() throws -> GlassDevice {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("glass-test-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: dir) }
        return try XCTUnwrap(GlassDevice(directory: dir.path, name: "Test Mac"))
    }

    /// Pairs a new identity with `host` (checking the refusals on the way) and connects.
    func pairedClient(_ host: ReplayHost) throws -> GlassClient {
        let device = try tempDevice()
        XCTAssertThrowsError(try GlassClient(host: "127.0.0.1", port: host.port, device: device)) { e in
            XCTAssertTrue((e as? ConnectFailure)?.needsPairing ?? false, "\(e)")
        }
        let wrong = try GlassPairing(device: device, host: "127.0.0.1", port: host.port)
        XCTAssertThrowsError(try wrong.finish(pin: "000000")) { e in
            XCTAssertTrue((e as? ConnectFailure)?.wrongPin ?? false, "\(e)")
        }
        let p = try GlassPairing(device: device, host: "127.0.0.1", port: host.port)
        XCTAssertEqual(p.hostName, "Glass test host")
        try p.finish(pin: LiveTests.pin)
        XCTAssertEqual(device.peers.count, 1)
        XCTAssertTrue(device.peers[0].isHost)
        let c = try GlassClient(host: "127.0.0.1", port: host.port, device: device)
        XCTAssertEqual(c.hostName, "Glass test host")
        return c
    }

    func testLoopbackStreamDecodes() throws {
        for name in ["testsrc-420", "testsrc-444"] {
            let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "glsr", subdirectory: "Fixtures"))
            let data = try Data(contentsOf: url)
            let rec = try Recording(data: data)
            let expected = Set(rec.frames.map { f in rec.withFrameBytes(f) { Data($0) } })
            let host = try ReplayHost(data: data, pin: LiveTests.pin)
            let client = try pairedClient(host)
            XCTAssertEqual(client.info.width, 320)
            XCTAssertEqual(client.info.height, 192)
            XCTAssertEqual(client.is444, rec.is444)
            let dec = HEVCDecoder(chroma444: client.is444)
            var received = 0, decoded = 0, mismatched = 0
            let deadline = Date().addingTimeInterval(10)
            do {
                while decoded < 24, Date() < deadline {
                    let r = try client.waitFrame(timeoutMs: 200) { _, bytes in
                        received += 1
                        if !expected.contains(Data(bytes)) { mismatched += 1 }
                        if try dec.decode(bytes) != nil { decoded += 1 }
                    }
                    XCTAssertNotEqual(r, .disconnected)
                }
            } catch let error where rec.is444 {
                throw XCTSkip("HEVC 4:4:4 decode unavailable on this machine: \(error)")
            }
            XCTAssertGreaterThanOrEqual(decoded, 24, name)
            XCTAssertEqual(mismatched, 0, name)
            XCTAssertEqual(client.stats.rx.frames_lost, 0, name)
            XCTAssertNotNil(client.hostToLocal(GlassClock.nowUs()), "clock sync should have a sample by now")
            print("\(name): received \(received), decoded \(decoded), RTT \(client.stats.last_rtt_us) us")
            client.close()
            host.stop()
        }
    }

    func testInputAndCursorOverLoopback() throws {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "testsrc-420", withExtension: "glsr", subdirectory: "Fixtures"))
        let host = try ReplayHost(data: try Data(contentsOf: url), pin: LiveTests.pin)
        let client = try pairedClient(host)
        _ = client.waitFrame(timeoutMs: 2000) { _, _ in } // streaming
        client.move(x: 1000, y: 2000)
        client.key(GlassClient.hid(forMacKey: 0x00, iso: false, pcStyle: false), down: true) // A
        client.key(0x04, down: false)
        client.button(.left, down: true, x: 1000, y: 2000)
        client.button(.left, down: false, x: 1000, y: 2000)
        client.scroll(dy: 120, dx: 0)
        var info = client.cursor(seen: 0)
        let deadline = Date().addingTimeInterval(5)
        while (info.rgba == nil || host.inputActions < 5), Date() < deadline {
            Thread.sleep(forTimeInterval: 0.02)
            info = client.cursor(seen: 0)
        }
        XCTAssertEqual(info.info.width, 16)
        XCTAssertTrue(info.info.visible)
        XCTAssertEqual(info.rgba?.count, 16 * 16 * 4)
        // move, A down, A up, (move+) click down, click up, scroll
        XCTAssertGreaterThanOrEqual(host.inputActions, 5)
        XCTAssertGreaterThanOrEqual(client.stats.input_events, 5)
        client.close()
        host.stop()
    }

    func testKeyMapping() {
        XCTAssertEqual(GlassClient.hid(forMacKey: 0x00, iso: false, pcStyle: false), 0x04) // A
        XCTAssertEqual(GlassClient.hid(forMacKey: 0x37, iso: false, pcStyle: false), 0xE0) // ⌘ -> Ctrl
        XCTAssertEqual(GlassClient.hid(forMacKey: 0x37, iso: false, pcStyle: true), 0xE3) // ⌘ -> Win
        XCTAssertEqual(GlassClient.hid(forMacKey: 0x0A, iso: true, pcStyle: false), 0x35) // ISO § -> `
        XCTAssertEqual(GlassClient.hid(forMacKey: 0x3F, iso: false, pcStyle: false), 0) // fn
    }

    func testVideoPointMapping() {
        // 16:9 picture centred in a 16:10 drawable: bars top and bottom.
        let vp = Renderer.fitViewport(src: CGSize(width: 2560, height: 1440), dst: CGSize(width: 2940, height: 1912))
        let top = Renderer.videoPoint(CGPoint(x: 0, y: vp.originY), in: vp)
        XCTAssertEqual(top.x, 0)
        XCTAssertEqual(top.y, 0)
        let br = Renderer.videoPoint(CGPoint(x: 2939, y: vp.originY + vp.height - 1), in: vp)
        XCTAssertEqual(br.x, 65535)
        XCTAssertEqual(br.y, 65535)
        // In the bar above the picture: clamped to the top edge.
        XCTAssertEqual(Renderer.videoPoint(CGPoint(x: 1470, y: 5), in: vp).y, 0)
        let mid = Renderer.videoPoint(CGPoint(x: 1469.5, y: vp.originY + (vp.height - 1) / 2), in: vp)
        XCTAssertEqual(Int(mid.x), 32768, accuracy: 2)
        XCTAssertEqual(Int(mid.y), 32768, accuracy: 2)
    }

    func testConnectRefused() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("glass-test-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        let device = try XCTUnwrap(GlassDevice(directory: dir.path, name: "Test Mac"))
        XCTAssertThrowsError(try GlassClient(host: "127.0.0.1", port: 9, device: device)) { error in
            XCTAssertEqual((error as? ConnectFailure)?.code, 2)
        }
    }

    func testFitViewport() {
        // 16:9 into 16:10 -> bars top and bottom.
        let v = Renderer.fitViewport(src: CGSize(width: 2560, height: 1440), dst: CGSize(width: 2940, height: 1912))
        XCTAssertEqual(v.width, 2940)
        XCTAssertEqual(v.height, 1654)
        XCTAssertEqual(v.originX, 0)
        XCTAssertEqual(v.originY, 129)
        // Exact fit stays 1:1.
        let e = Renderer.fitViewport(src: CGSize(width: 2560, height: 1440), dst: CGSize(width: 2560, height: 1440))
        XCTAssertEqual(e.width, 2560)
        XCTAssertEqual(e.originY, 0)
    }
}

/// Mac as host (M3b): core's host through GlassKit, with a GlassClient as
/// the viewer. Frames are sent by hand, so no encoder is needed on CI.
final class HostTests: XCTestCase {
    final class Recorder: GlassInputHandler {
        private let lock = NSLock()
        private var _keys: [UInt8] = []
        var keys: [UInt8] { lock.withLock { _keys } }
        func handle(_ a: GLASS_InputActionC) {
            if Int32(a.kind) == GLASS_INPUT_KEY, a.down { lock.withLock { _keys.append(a.hid) } }
        }
    }

    func tempDevice(_ name: String) throws -> GlassDevice {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("glass-test-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: dir) }
        return try XCTUnwrap(GlassDevice(directory: dir.path, name: name))
    }

    func testHostPairsStreamsAndTakesInput() throws {
        let hostDevice = try tempDevice("Host Mac")
        let pinLock = NSLock()
        var pins: [String] = []
        let host = try GlassHost(device: hostDevice, port: 0, fixedPin: "246810") { e in
            if e.kind == .pairingPin { pinLock.withLock { pins.append(e.pin) } }
        }
        let info = GLASS_StreamInfo(codec: UInt8(GLASS_CODEC_HEVC), chroma: UInt8(GLASS_CHROMA_420), width: 64, height: 32, fps: 60,
                                    mode: UInt8(GLASS_MODE_HOST_SCREEN))
        let rec = Recorder()
        let ready = expectation(description: "session")
        let sessionLock = NSLock()
        var session: GlassHostSession?
        let acceptor = Thread {
            while true {
                switch host.accept(info: info, input: rec) {
                case .session(let s):
                    sessionLock.withLock { session = s }
                    ready.fulfill()
                    return
                case .stopped:
                    return
                case .handled, .failed:
                    continue
                }
            }
        }
        acceptor.start()

        let viewer = try tempDevice("PC")
        let p = try GlassPairing(device: viewer, host: "127.0.0.1", port: host.port)
        XCTAssertEqual(p.hostName, "Host Mac")
        try p.finish(pin: "246810")
        XCTAssertEqual(pinLock.withLock { pins }, ["246810"])
        let client = try GlassClient(host: "127.0.0.1", port: host.port, device: viewer)
        wait(for: [ready], timeout: 10)
        let s = try XCTUnwrap(sessionLock.withLock { session })
        XCTAssertEqual(s.peerName, "PC")
        XCTAssertEqual(s.peerAddress, "127.0.0.1")
        // The Mac viewer says it decodes 4:4:4 and wants the host's own screen.
        XCTAssertEqual(s.viewer.caps & UInt8(GLASS_CAP_HEVC_444), UInt8(GLASS_CAP_HEVC_444))
        XCTAssertEqual(s.viewer.mode, UInt8(GLASS_MODE_FIT)) // the default view
        XCTAssertNotEqual(s.viewer.caps & UInt8(GLASS_CAP_PREFER_420), 0) // 4:2:0 to start
        XCTAssertTrue(client.hostIsMac)
        XCTAssertNotEqual(Int32(client.hostCaps) & GLASS_HOST_CAP_444, 0)
        XCTAssertNil(s.takeModeRequest())

        // Frames reach the viewer intact once its UDP punch has arrived.
        let bytes: [UInt8] = [0, 0, 0, 1, 0x26, 1, 0xAA, 0x55]
        var got = 0, id: UInt32 = 0
        let deadline = Date().addingTimeInterval(10)
        while got < 3, Date() < deadline {
            id += 1
            let f = GLASS_OutFrame(frame_id: id, flags: 1, capture_ts_us: GlassClock.nowUs(), encode_us: 100, input_seq: 0)
            bytes.withUnsafeBytes { _ = s.send(f, $0) }
            let r = client.waitFrame(timeoutMs: 50) { meta, data in
                XCTAssertEqual(Array(data), bytes)
                XCTAssertEqual(meta.encode_us, 100)
            }
            if r == .frame { got += 1 }
        }
        XCTAssertGreaterThanOrEqual(got, 3)
        XCTAssertGreaterThanOrEqual(s.counters.frames_sent, 3)

        // The viewer's keys arrive as HID usages, which map to Mac key codes.
        client.key(0x04, down: true)
        client.key(0x04, down: false)
        let keyDeadline = Date().addingTimeInterval(5)
        while rec.keys.isEmpty, Date() < keyDeadline { Thread.sleep(forTimeInterval: 0.01) }
        XCTAssertEqual(rec.keys.first, 0x04)
        XCTAssertEqual(glassMacKey(hid: 0x04, iso: false), 0x00) // A
        XCTAssertEqual(glassMacKey(hid: 0xE3, iso: false), 0x37) // GUI -> ⌘
        XCTAssertNil(glassMacKey(hid: 0x46, iso: false)) // Print Screen

        client.close()
        s.close()
        host.stop()
    }

    func testAnnexBFromEncoderOutput() {
        // VideoToolbox's length-prefixed NALs become Annex-B behind the parameter sets.
        let vps: [UInt8] = [0x40, 1, 0xAA]
        let sample: [UInt8] = [0, 0, 0, 3, 0x26, 1, 0xDD]
        var out = [UInt8](repeating: 0, count: 64)
        let n = vps.withUnsafeBufferPointer { v in
            var pre = [GLASS_Bytes(ptr: v.baseAddress, len: v.count)]
            return sample.withUnsafeBufferPointer { s in
                glass_hevc_from_length_prefixed(&pre, 1, s.baseAddress, s.count, &out, out.count)
            }
        }
        XCTAssertEqual(Array(out[0..<max(n, 0)]), [0, 0, 0, 1, 0x40, 1, 0xAA, 0, 0, 0, 1, 0x26, 1, 0xDD])
    }

    func testLocalAddressesAreIPv4() {
        for a in localIPv4Addresses() {
            XCTAssertEqual(a.split(separator: ".").count, 4, a)
        }
    }
}
