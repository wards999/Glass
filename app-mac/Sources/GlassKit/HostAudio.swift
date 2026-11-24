import CGlassCore
import CoreAudio
import CoreMedia
import Foundation
import ScreenCaptureKit

/// The Mac host's sound (M4b step 3): everything the Mac plays, captured
/// with ScreenCaptureKit (Glass's own sound left out), cut into 10 ms
/// frames and sent through core (Opus, the same path as a PC host's). Its
/// own stream, so it carries on across view changes. While it runs, the
/// Mac's output is muted so the sound plays only on the viewer;
/// ScreenCaptureKit takes it from the apps, before the speakers.
public final class HostAudio: NSObject, SCStreamOutput, SCStreamDelegate {
    private let out: OpaquePointer
    private let queue = DispatchQueue(label: "glass.host-audio", qos: .userInteractive)
    private var stream: SCStream?
    /// Interleaved stereo waiting to fill a frame (only on `queue`).
    private var pending: [Float] = []
    private var frame = [Float](repeating: 0, count: Int(GLASS_AUDIO_FRAME_LEN))
    private let lock = NSLock()
    private var _frames: UInt64 = 0
    private var _silent: UInt64 = 0
    private var _problem: String?
    private var mute: OutputMute?

    /// `out` from `GlassHostSession.audioOut()`; freed here.
    public init(out: OpaquePointer) {
        self.out = out
        pending.reserveCapacity(Int(GLASS_AUDIO_FRAME_LEN) * 4)
    }

    deinit { glass_audio_out_free(out) }

    public var frames: UInt64 { lock.withLock { _frames } }
    public var silent: UInt64 { lock.withLock { _silent } }
    public var problem: String? { lock.withLock { _problem } }
    /// The Mac's output was muted for the session.
    public var muted: Bool { mute?.mutedOnce ?? false }

    public func start(display: CGDirectDisplayID) {
        mute = OutputMute()
        SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: true) { [self] content, error in
            guard let content, let d = content.displays.first(where: { $0.displayID == display }) ?? content.displays.first else {
                note("no display for sound capture: \(error?.localizedDescription ?? "none")")
                return
            }
            let cfg = SCStreamConfiguration()
            cfg.capturesAudio = true
            cfg.excludesCurrentProcessAudio = true
            cfg.sampleRate = 48_000
            cfg.channelCount = 2
            // Sound only: the smallest picture, as rarely as it allows.
            cfg.width = 2
            cfg.height = 2
            cfg.minimumFrameInterval = CMTime(value: 1, timescale: 1)
            cfg.showsCursor = false
            let s = SCStream(filter: SCContentFilter(display: d, excludingWindows: []), configuration: cfg, delegate: self)
            do {
                try s.addStreamOutput(self, type: .audio, sampleHandlerQueue: queue)
            } catch {
                note("sound capture: \(error.localizedDescription)")
                return
            }
            stream = s
            s.startCapture { [self] error in
                if let error { note("sound capture: \(error.localizedDescription)") }
            }
        }
    }

    public func stop() {
        stream?.stopCapture { _ in }
        stream = nil
        mute?.restore()
    }

    public func stream(_ stream: SCStream, didStopWithError error: Error) {
        note("sound capture stopped: \(error.localizedDescription)")
    }

    private func note(_ s: String) { lock.withLock { if _problem == nil { _problem = s } } }

    public func stream(_ stream: SCStream, didOutputSampleBuffer sb: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .audio, sb.isValid,
              let fmt = CMSampleBufferGetFormatDescription(sb),
              let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(fmt)?.pointee,
              asbd.mFormatID == kAudioFormatLinearPCM, asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0,
              asbd.mBitsPerChannel == 32 else { return }
        if asbd.mSampleRate != 48_000 {
            note("sound at \(Int(asbd.mSampleRate)) Hz, not 48 kHz: not sent")
            return
        }
        let n = CMSampleBufferGetNumSamples(sb)
        guard n > 0 else { return }
        var block: CMBlockBuffer?
        var size = 0
        CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(sb, bufferListSizeNeededOut: &size, bufferListOut: nil,
                                                                bufferListSize: 0, blockBufferAllocator: nil,
                                                                blockBufferMemoryAllocator: nil, flags: 0, blockBufferOut: nil)
        let raw = UnsafeMutableRawPointer.allocate(byteCount: size, alignment: 16)
        defer { raw.deallocate() }
        let abl = raw.bindMemory(to: AudioBufferList.self, capacity: 1)
        guard CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sb, bufferListSizeNeededOut: nil, bufferListOut: abl, bufferListSize: size,
            blockBufferAllocator: nil, blockBufferMemoryAllocator: nil,
            flags: kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment, blockBufferOut: &block) == noErr else { return }
        let bufs = UnsafeMutableAudioBufferListPointer(abl)
        let interleaved = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved == 0
        let ch = Int(asbd.mChannelsPerFrame)
        // To interleaved stereo (mono doubled, extra channels dropped).
        for i in 0..<n {
            let l: Float, r: Float
            if interleaved, let p = bufs.first?.mData?.assumingMemoryBound(to: Float.self) {
                l = p[i * ch]
                r = ch > 1 ? p[i * ch + 1] : l
            } else if !interleaved, let pl = bufs.first?.mData?.assumingMemoryBound(to: Float.self) {
                l = pl[i]
                r = bufs.count > 1 ? (bufs[1].mData?.assumingMemoryBound(to: Float.self)[i] ?? l) : l
            } else { return }
            pending.append(l)
            pending.append(r)
        }
        // The capture time of the first sample still waiting.
        let pts = CMSampleBufferGetPresentationTimeStamp(sb)
        let endUs = (pts.isValid ? UInt64(max(0, pts.seconds * 1e6)) : GlassClock.nowUs()) + UInt64(n) * 1_000_000 / 48_000
        let len = Int(GLASS_AUDIO_FRAME_LEN)
        var used = 0
        while pending.count - used >= len {
            let waitingAfter = (pending.count - used - len) / 2
            let ts = endUs &- UInt64(waitingAfter + len / 2) * 1_000_000 / 48_000
            var loud = false
            for k in 0..<len {
                let v = pending[used + k]
                frame[k] = v
                if v > 1e-4 || v < -1e-4 { loud = true }
            }
            used += len
            let sent = frame.withUnsafeBufferPointer { glass_audio_out_send(out, $0.baseAddress, len, ts) }
            if sent {
                lock.withLock {
                    _frames += 1
                    if !loud { _silent += 1 }
                }
            }
        }
        if used > 0 { pending.removeFirst(used) }
    }
}

/// Mutes the Mac's default output while the Mac is viewed (M4b: the sound
/// plays only on the viewer), and puts it back as it was.
final class OutputMute {
    private var device = AudioObjectID(kAudioObjectUnknown)
    private var was: UInt32 = 0
    private var done = false
    private(set) var mutedOnce = false

    init() {
        var addr = AudioObjectPropertyAddress(mSelector: kAudioHardwarePropertyDefaultOutputDevice,
                                              mScope: kAudioObjectPropertyScopeGlobal,
                                              mElement: kAudioObjectPropertyElementMain)
        var size = UInt32(MemoryLayout<AudioObjectID>.size)
        guard AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &addr, 0, nil, &size, &device) == noErr else { return }
        var mute = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyMute,
                                              mScope: kAudioDevicePropertyScopeOutput,
                                              mElement: kAudioObjectPropertyElementMain)
        guard AudioObjectHasProperty(device, &mute) else { return }
        size = UInt32(MemoryLayout<UInt32>.size)
        guard AudioObjectGetPropertyData(device, &mute, 0, nil, &size, &was) == noErr else { return }
        var on: UInt32 = 1
        done = AudioObjectSetPropertyData(device, &mute, 0, nil, size, &on) == noErr
        mutedOnce = done
    }

    func restore() {
        guard done else { return }
        var mute = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyMute,
                                              mScope: kAudioDevicePropertyScopeOutput,
                                              mElement: kAudioObjectPropertyElementMain)
        var v = was
        _ = AudioObjectSetPropertyData(device, &mute, 0, nil, UInt32(MemoryLayout<UInt32>.size), &v)
        done = false
    }
}
