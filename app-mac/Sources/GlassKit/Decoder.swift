import CGlassCore
import CoreMedia
import CoreVideo
import Foundation
import QuartzCore
import VideoToolbox

/// HEVC decode with VideoToolbox. Input is Annex-B access units (as recorded /
/// streamed); core converts them to length-prefixed samples. Output is
/// IOSurface-backed, Metal-compatible bi-planar YCbCr pixel buffers.
public final class HEVCDecoder {
    public let chroma444: Bool
    public private(set) var usingHardware = false
    /// Decode time of the most recent frame (submit -> output), seconds.
    public private(set) var lastDecodeSeconds: Double = 0

    private var session: VTDecompressionSession?
    private var format: CMVideoFormatDescription?
    private var paramSets: [Data] = []
    private var scratch: UnsafeMutableRawPointer
    private var scratchCap: Int

    public init(chroma444: Bool) {
        self.chroma444 = chroma444
        scratchCap = 1 << 20
        scratch = .allocate(byteCount: scratchCap, alignment: 16)
    }

    deinit {
        if let s = session { VTDecompressionSessionInvalidate(s) }
        scratch.deallocate()
    }

    public var pixelFormat: OSType {
        chroma444 ? kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange : kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
    }

    /// Decodes one access unit synchronously and returns the picture, or nil if
    /// the decoder produced none (e.g. frames before the first IDR).
    public func decode(_ au: UnsafeRawBufferPointer) throws -> CVPixelBuffer? {
        guard let base = au.bindMemory(to: UInt8.self).baseAddress, au.count > 0 else { return nil }
        try updateFormatIfNeeded(base, au.count)
        guard let session, let format else { return nil } // no parameter sets yet

        let need = glass_hevc_length_prefixed_len(base, au.count)
        if need > scratchCap {
            // Grows only for an unusually large frame; steady state allocates nothing here.
            scratch.deallocate()
            scratchCap = need * 2
            scratch = .allocate(byteCount: scratchCap, alignment: 16)
        }
        let n = glass_hevc_to_length_prefixed(base, au.count, scratch.assumingMemoryBound(to: UInt8.self), scratchCap)
        guard n > 0 else { return nil }

        // LATER(M5): pool CMBlockBuffer/CMSampleBuffer objects instead of per-frame CF allocations.
        var block: CMBlockBuffer?
        var st = CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault, memoryBlock: scratch, blockLength: n,
            blockAllocator: kCFAllocatorNull, customBlockSource: nil, offsetToData: 0,
            dataLength: n, flags: 0, blockBufferOut: &block)
        guard st == noErr, let block else { throw GlassError.decoder("CMBlockBufferCreateWithMemoryBlock", st) }
        var sample: CMSampleBuffer?
        var size = n
        st = CMSampleBufferCreateReady(
            allocator: kCFAllocatorDefault, dataBuffer: block, formatDescription: format,
            sampleCount: 1, sampleTimingEntryCount: 0, sampleTimingArray: nil,
            sampleSizeEntryCount: 1, sampleSizeArray: &size, sampleBufferOut: &sample)
        guard st == noErr, let sample else { throw GlassError.decoder("CMSampleBufferCreateReady", st) }

        // Synchronous decode (no async flag): the handler runs before this
        // returns, so reusing `scratch` for the next frame is safe.
        var out: CVPixelBuffer?
        var decodeStatus: OSStatus = noErr
        let t0 = CACurrentMediaTime()
        st = VTDecompressionSessionDecodeFrame(session, sampleBuffer: sample, flags: [], infoFlagsOut: nil) {
            status, _, image, _, _ in
            decodeStatus = status
            out = image
        }
        lastDecodeSeconds = CACurrentMediaTime() - t0
        guard st == noErr else { throw GlassError.decoder("VTDecompressionSessionDecodeFrame", st) }
        guard decodeStatus == noErr else { throw GlassError.decoder("decode", decodeStatus) }
        return out
    }

    private func updateFormatIfNeeded(_ base: UnsafePointer<UInt8>, _ count: Int) throws {
        var ps = GLASS_HevcParamSets()
        guard glass_hevc_param_sets(base, count, &ps) else { return }
        let sets = [ps.vps, ps.sps, ps.pps].map { Data(bytes: $0.ptr!, count: Int($0.len)) }
        if sets == paramSets, session != nil { return }

        var fmt: CMFormatDescription?
        let st: OSStatus = sets[0].withUnsafeBytes { v in
            sets[1].withUnsafeBytes { s in
                sets[2].withUnsafeBytes { p in
                    let ptrs = [v, s, p].map { $0.bindMemory(to: UInt8.self).baseAddress! }
                    let sizes = sets.map(\.count)
                    return CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                        allocator: kCFAllocatorDefault, parameterSetCount: 3,
                        parameterSetPointers: ptrs, parameterSetSizes: sizes,
                        nalUnitHeaderLength: 4, extensions: nil, formatDescriptionOut: &fmt)
                }
            }
        }
        guard st == noErr, let fmt else { throw GlassError.decoder("CMVideoFormatDescriptionCreateFromHEVCParameterSets", st) }

        if let s = session, VTDecompressionSessionCanAcceptFormatDescription(s, formatDescription: fmt) {
            format = fmt
            paramSets = sets
            return
        }
        if let s = session { VTDecompressionSessionInvalidate(s) }
        session = nil

        let spec: [CFString: Any] = [kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder: true]
        let attrs: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: pixelFormat,
            kCVPixelBufferIOSurfacePropertiesKey: [CFString: Any]() as CFDictionary,
            kCVPixelBufferMetalCompatibilityKey: true,
        ]
        var newSession: VTDecompressionSession?
        let cs = VTDecompressionSessionCreate(
            allocator: kCFAllocatorDefault, formatDescription: fmt,
            decoderSpecification: spec as CFDictionary, imageBufferAttributes: attrs as CFDictionary,
            outputCallback: nil, decompressionSessionOut: &newSession)
        guard cs == noErr, let newSession else { throw GlassError.decoder("VTDecompressionSessionCreate", cs) }
        VTSessionSetProperty(newSession, key: kVTDecompressionPropertyKey_RealTime, value: kCFBooleanTrue)

        var hw: CFBoolean?
        VTSessionCopyProperty(newSession, key: kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
                              allocator: kCFAllocatorDefault, valueOut: &hw)
        usingHardware = hw.map { CFBooleanGetValue($0) } ?? false

        session = newSession
        format = fmt
        paramSets = sets
    }
}

