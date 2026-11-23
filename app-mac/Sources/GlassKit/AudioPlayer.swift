import AVFoundation
import AudioToolbox
import CGlassCore

/// Plays the host's sound (M4b, ADR 0025): core's receiver keeps a
/// lock-free ring of 48 kHz stereo topped up, and the output's render
/// callback reads it. The callback never locks, waits or allocates (its
/// scratch buffer is made here). A small output buffer (256 frames,
/// ~5 ms) keeps the sound close to the picture.
public final class AudioPlayer {
    private let engine = AVAudioEngine()
    private let ring: OpaquePointer
    private let scratch: UnsafeMutablePointer<Float>
    private let scratchFrames = 4096
    /// 0...1, read by the render callback.
    private let gain: UnsafeMutablePointer<Float>

    /// `ring` from `glass_client_audio_ring`: this player releases it.
    public init?(ring: OpaquePointer) {
        self.ring = ring
        scratch = .allocate(capacity: scratchFrames * 2)
        gain = .allocate(capacity: 1)
        gain.pointee = 1
        guard let format = AVAudioFormat(standardFormatWithSampleRate: 48_000, channels: 2) else {
            cleanUp()
            return nil
        }
        let (ring, scratch, cap, gain) = (ring, scratch, scratchFrames, gain)
        let source = AVAudioSourceNode(format: format) { _, _, frameCount, list -> OSStatus in
            let buffers = UnsafeMutableAudioBufferListPointer(list)
            guard buffers.count >= 2,
                  let l = buffers[0].mData?.assumingMemoryBound(to: Float.self),
                  let r = buffers[1].mData?.assumingMemoryBound(to: Float.self) else { return noErr }
            var done = 0
            let total = Int(frameCount)
            while done < total {
                let n = min(cap, total - done)
                _ = glass_audio_ring_read(ring, scratch, n * 2, gain.pointee)
                for i in 0..<n {
                    l[done + i] = scratch[i * 2]
                    r[done + i] = scratch[i * 2 + 1]
                }
                done += n
            }
            return noErr
        }
        engine.attach(source)
        engine.connect(source, to: engine.mainMixerNode, format: format)
        // A short output buffer: the sound's delay matters as much as the
        // picture's.
        if let unit = engine.outputNode.audioUnit {
            var frames: UInt32 = 256
            AudioUnitSetProperty(unit, kAudioDevicePropertyBufferFrameSize, kAudioUnitScope_Global, 0,
                                 &frames, UInt32(MemoryLayout<UInt32>.size))
        }
        do {
            try engine.start()
        } catch {
            cleanUp()
            return nil
        }
    }

    /// 0 (silent) ... 1.
    public var volume: Float {
        get { gain.pointee }
        set { gain.pointee = max(0, min(1, newValue)) }
    }

    public func stop() {
        engine.stop()
    }

    private func cleanUp() {
        glass_audio_ring_free(ring)
        scratch.deallocate()
        gain.deallocate()
    }

    deinit {
        engine.stop()
        cleanUp()
    }
}
