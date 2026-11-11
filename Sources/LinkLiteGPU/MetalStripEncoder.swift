// SPDX-License-Identifier: GPL-2.0-or-later

import CDLLite
import CoreVideo
import Foundation
import Metal

/// What a head's strips must be coded for. Comes from the engine (`DLLEncoderProfile`) so the
/// geometry, depth and bit dialect are never restated here.
public struct StripEncoderConfiguration: Sendable, Equatable {
    /// `DLLFamily`, passed straight to `dll_encode_strip_quantized`.
    public var family: UInt32
    public var tenBit: Bool
    /// The frame size, which is the head's mode.
    public var width: Int
    public var height: Int
    public var stripWidth: Int
    public var stripHeight: Int
    public var paddedWidth: Int
    public var paddedHeight: Int
    /// `DLLCodeTable`.
    public var codeTable: UInt32
    /// Escape ceilings at `tenBit`'s depth: DC deltas, luma AC, chroma AC.
    public var dcCmax: UInt32
    public var acCmax: UInt32
    public var chromaAcCmax: UInt32
    /// The first two bytes of every strip body.
    public var magic: [UInt8]

    public init(family: UInt32, tenBit: Bool, width: Int, height: Int, stripWidth: Int, stripHeight: Int,
                paddedWidth: Int, paddedHeight: Int, codeTable: UInt32, dcCmax: UInt32, acCmax: UInt32,
                chromaAcCmax: UInt32, magic: [UInt8]) {
        self.family = family
        self.tenBit = tenBit
        self.width = width
        self.height = height
        self.stripWidth = stripWidth
        self.stripHeight = stripHeight
        self.paddedWidth = paddedWidth
        self.paddedHeight = paddedHeight
        self.codeTable = codeTable
        self.dcCmax = dcCmax
        self.acCmax = acCmax
        self.chromaAcCmax = chromaAcCmax
        self.magic = magic
    }

    public init(profile: DLLEncoderProfile, width: Int, height: Int) {
        self.init(family: profile.family, tenBit: profile.ten_bit, width: width, height: height,
                  stripWidth: Int(profile.strip_width), stripHeight: Int(profile.strip_height),
                  paddedWidth: Int(profile.padded_width), paddedHeight: Int(profile.padded_height),
                  codeTable: profile.code_table, dcCmax: profile.dc_cmax, acCmax: profile.ac_cmax,
                  chromaAcCmax: profile.chroma_ac_cmax, magic: [profile.magic.0, profile.magic.1])
    }

    var tilesX: Int { paddedWidth / stripWidth }
    var tilesY: Int { paddedHeight / stripHeight }
    var stripCount: Int { tilesX * tilesY }
}

public enum MetalStripEncoderError: Error, CustomStringConvertible {
    case noDevice
    case unsupportedGeometry(String)
    case shaderCompilation(String)
    case allocation(String)
    case unsupportedFrame(String)
    case gpu(String)
    case packing(strip: Int)

    public var description: String {
        switch self {
        case .noDevice: "no Metal device"
        case .unsupportedGeometry(let detail): "unsupported strip geometry: \(detail)"
        case .shaderCompilation(let detail): "Metal shader compilation failed: \(detail)"
        case .allocation(let detail): "could not allocate \(detail)"
        case .unsupportedFrame(let detail): "unsupported frame: \(detail)"
        case .gpu(let detail): "GPU command buffer failed: \(detail)"
        case .packing(let strip): "coding strip \(strip) failed"
        }
    }
}

/// Coded strips ready for `dll_engine_submit_encoded`: strip `indices[i]`, content hash
/// `hashes[i]`, body `bodies[offsets[i] ..< offsets[i + 1]]`, in ascending index order.
///
/// Strips packed on the GPU stay in the encoder's shared buffers, so handing them to the engine
/// copies nothing on the way. Those are reused by the encoder's next call: use the strips before
/// encoding again.
public struct EncodedStrips: @unchecked Sendable {
    fileprivate enum Storage {
        case owned(indices: [UInt32], hashes: [UInt64], bodies: [UInt8], offsets: [UInt32])
        case borrowed(indices: MTLBuffer, hashes: MTLBuffer, bodies: MTLBuffer, offsets: MTLBuffer, bodyBytes: Int)
    }

    fileprivate var storage: Storage
    public private(set) var count: Int
    /// GPU execution time of the command buffers, 0 when nothing ran on the GPU.
    public var gpuSeconds: Double

    public init() {
        self.init(storage: .owned(indices: [], hashes: [], bodies: [], offsets: [0]), count: 0, gpuSeconds: 0)
    }

    fileprivate init(storage: Storage, count: Int, gpuSeconds: Double) {
        self.storage = storage
        self.count = count
        self.gpuSeconds = gpuSeconds
    }

    /// The four arrays in place: indices and hashes of `count`, the bodies back to back, and
    /// `count + 1` offsets.
    public func withUnsafeBuffers<R>(
        _ body: (_ indices: UnsafeBufferPointer<UInt32>, _ hashes: UnsafeBufferPointer<UInt64>,
                 _ bodies: UnsafeBufferPointer<UInt8>, _ offsets: UnsafeBufferPointer<UInt32>) throws -> R
    ) rethrows -> R {
        switch storage {
        case let .owned(indices, hashes, bodies, offsets):
            return try indices.withUnsafeBufferPointer { i in
                try hashes.withUnsafeBufferPointer { h in
                    try bodies.withUnsafeBufferPointer { b in
                        try offsets.withUnsafeBufferPointer { o in try body(i, h, b, o) }
                    }
                }
            }
        case let .borrowed(indices, hashes, bodies, offsets, bodyBytes):
            return try body(
                UnsafeBufferPointer(start: indices.contents().assumingMemoryBound(to: UInt32.self), count: count),
                UnsafeBufferPointer(start: hashes.contents().assumingMemoryBound(to: UInt64.self), count: count),
                UnsafeBufferPointer(start: bodies.contents().assumingMemoryBound(to: UInt8.self), count: bodyBytes),
                UnsafeBufferPointer(start: offsets.contents().assumingMemoryBound(to: UInt32.self), count: count + 1))
        }
    }

    public var indices: [UInt32] { withUnsafeBuffers { indices, _, _, _ in Array(indices) } }
    public var hashes: [UInt64] { withUnsafeBuffers { _, hashes, _, _ in Array(hashes) } }
    public var bodies: [UInt8] { withUnsafeBuffers { _, _, bodies, _ in Array(bodies) } }
    public var offsets: [UInt32] { withUnsafeBuffers { _, _, _, offsets in Array(offsets) } }
}

/// Mirrors StripParams in the Metal source.
private struct StripParams {
    var frameWidth: UInt32
    var frameHeight: UInt32
    var paddedWidth: UInt32
    var stripWidth: UInt32
    var stripHeight: UInt32
    var stripWidthShift: UInt32
    var tilesX: UInt32
    var blockShift: UInt32
    var tenBit: UInt32
    var force: UInt32 = 0
    var fromShadow: UInt32 = 0
    var narrow: UInt32
    var dcCmax: UInt32
    var acCmax: UInt32
    var chromaAcCmax: UInt32
    var magic: UInt32
    var candidateCount: UInt32 = 0
    var outputCapacity: UInt32 = 0
    var withLengths: UInt32 = 0
    var fromCoefficients: UInt32 = 0
}

/// One process-wide library per device: compiling the source takes tens of milliseconds, and a
/// capture restart must not pay it again.
private enum ShaderCache {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var libraries: [UInt64: MTLLibrary] = [:]

    static func library(for device: MTLDevice) throws -> MTLLibrary {
        lock.lock()
        defer { lock.unlock() }
        if let library = libraries[device.registryID] { return library }
        let options = MTLCompileOptions()
        // Only integer arithmetic and the exact unorm8 round trip run here; keep the compiler
        // from relaxing the float rounding that round trip depends on.
        if #available(macOS 15.0, *) {
            options.mathMode = .safe
        } else {
            options.fastMathEnabled = false
        }
        do {
            let library = try device.makeLibrary(source: stripEncoderMetalSource, options: options)
            libraries[device.registryID] = library
            return library
        } catch {
            throw MetalStripEncoderError.shaderCompilation(error.localizedDescription)
        }
    }
}

/// Hashes, transforms, quantises and codes a head's strips on the GPU. With `packOnGPU` false the
/// GPU stops at the quantised blocks and the CPU codes them with the engine's own coder
/// (`dll_encode_strip_quantized`), which is slower but keeps the reference coder one switch away.
///
/// Not thread-safe: use one instance from one serial queue. Each call waits for its own command
/// buffers, so two frames never touch the per-head state at once.
public final class MetalStripEncoder: @unchecked Sendable {
    public let configuration: StripEncoderConfiguration
    public let device: MTLDevice
    /// Bit coding on the GPU (the default) or on the CPU from GPU-quantised blocks.
    public let packOnGPU: Bool

    private static let threadsPerStrip = 256
    private static let blockBytes = MemoryLayout<DLLQuantBlock>.stride
    private static let stripCoefficientBytes = Int(DLL_STRIP_BLOCKS) * blockBytes

    private let queue: MTLCommandQueue
    private let hashPipeline: MTLComputePipelineState
    private let scanPipeline: MTLComputePipelineState
    private let encodePipeline: MTLComputePipelineState?
    private let sizePipeline: MTLComputePipelineState?
    private let packPipeline: MTLComputePipelineState?
    private let scanThreads: Int
    private let textureCache: CVMetalTextureCache
    /// Stands in for the frame when strips are coded from the shadow, since the kernel always
    /// binds a texture.
    private let placeholder: MTLTexture

    /// The latest pre-gamma pixels of the whole padded surface, as 0x00BBGGRR; padding stays 0.
    private let shadow: MTLBuffer
    /// The post-gamma hash of each strip as last coded, 0 when not yet coded.
    private let hashes: MTLBuffer
    private let lut: MTLBuffer
    private let candidates: MTLBuffer
    /// Every strip index in order, for full encodes.
    private let allStrips: MTLBuffer
    /// Per candidate: whether it changed, its body length, its slot in the compact lists.
    private let flags: MTLBuffer
    private let lengths: MTLBuffer
    private let slots: MTLBuffer
    /// Per slot, in ascending strip order: the strip index, its hash, its body offset (one more
    /// entry for the end).
    private let changed: MTLBuffer
    private let changedHashes: MTLBuffer
    private let offsets: MTLBuffer
    /// The changed count, then the total body bytes.
    private let totals: MTLBuffer
    /// The packed bodies, back to back. Grows when a frame needs more and never shrinks.
    private var output: MTLBuffer?
    /// CPU packing only: `DLL_STRIP_BLOCKS` DLLQuantBlocks per changed strip, by slot.
    private let coefficients: MTLBuffer?

    private var params: StripParams
    private var lutBytes: [UInt8]
    /// The shadow holds a whole frame, so a full encode can run without a new one.
    public private(set) var hasFrame = false
    /// Every strip must be coded again at the next opportunity: nothing coded yet, or the gamma
    /// table changed, or the host asked for it.
    public private(set) var needsFullEncode = true
    private var candidateMarks: [Bool]

    public static let identityGamma: [UInt8] = (0..<768).map { UInt8($0 & 255) }

    public init(configuration: StripEncoderConfiguration, device: MTLDevice? = nil, packOnGPU: Bool = true) throws {
        guard let device = device ?? MTLCreateSystemDefaultDevice() else { throw MetalStripEncoderError.noDevice }
        let c = configuration
        guard c.width > 0, c.height > 0, c.stripWidth > 0, c.stripHeight > 0,
              c.stripWidth.nonzeroBitCount == 1, c.stripHeight.nonzeroBitCount == 1,
              c.stripWidth * c.stripHeight == Int(DLL_STRIP_BLOCKS) * 64,
              c.stripWidth % 8 == 0, c.stripHeight % 8 == 0,
              c.paddedWidth % c.stripWidth == 0, c.paddedHeight % c.stripHeight == 0,
              c.paddedWidth >= c.width, c.paddedHeight >= c.height,
              c.paddedWidth - c.width < c.stripWidth, c.paddedHeight - c.height < c.stripHeight,
              c.paddedWidth <= 65536, c.paddedHeight <= 65536
        else {
            throw MetalStripEncoderError.unsupportedGeometry(
                "\(c.width)x\(c.height) in \(c.stripWidth)x\(c.stripHeight) strips padded to \(c.paddedWidth)x\(c.paddedHeight)")
        }
        // The kernels' code fields hold at most 16 unary ones (Bits::unary) and the 10-bit DC
        // ceiling is 12; anything else is a profile this coder was not written for.
        guard c.codeTable <= 1, (1...16).contains(c.dcCmax), (1...16).contains(c.acCmax),
              (1...16).contains(c.chromaAcCmax), c.magic.count == 2 else {
            throw MetalStripEncoderError.unsupportedGeometry(
                "code table \(c.codeTable), ceilings \(c.dcCmax)/\(c.acCmax)/\(c.chromaAcCmax)")
        }
        guard MemoryLayout<DLLQuantBlock>.size == 3 * 64 * 2, blockSizeIsPacked() else {
            throw MetalStripEncoderError.unsupportedGeometry("DLLQuantBlock is not 384 packed bytes")
        }
        self.configuration = configuration
        self.device = device
        self.packOnGPU = packOnGPU

        let library = try ShaderCache.library(for: device)
        func pipeline(_ name: String, threads: Int) throws -> MTLComputePipelineState {
            guard let function = library.makeFunction(name: name) else {
                throw MetalStripEncoderError.shaderCompilation("missing kernel \(name)")
            }
            let state = try device.makeComputePipelineState(function: function)
            guard state.maxTotalThreadsPerThreadgroup >= threads else {
                throw MetalStripEncoderError.unsupportedGeometry("\(name) allows only \(state.maxTotalThreadsPerThreadgroup) threads per group")
            }
            // The coder's reductions count eight 32-lane SIMD-groups to a strip.
            guard state.threadExecutionWidth == 32 else {
                throw MetalStripEncoderError.unsupportedGeometry("\(name) runs \(state.threadExecutionWidth)-wide SIMD-groups, not 32")
            }
            return state
        }
        do {
            hashPipeline = try pipeline("hash_strips", threads: Self.threadsPerStrip)
            scanPipeline = try pipeline("scan_strips", threads: 32)
            if packOnGPU {
                sizePipeline = try pipeline("size_strips", threads: Self.threadsPerStrip)
                packPipeline = try pipeline("pack_strips", threads: Self.threadsPerStrip)
                encodePipeline = nil
            } else {
                encodePipeline = try pipeline("encode_strips", threads: Self.threadsPerStrip)
                sizePipeline = nil
                packPipeline = nil
            }
        } catch let error as MetalStripEncoderError {
            throw error
        } catch {
            throw MetalStripEncoderError.shaderCompilation(error.localizedDescription)
        }
        scanThreads = min(1024, scanPipeline.maxTotalThreadsPerThreadgroup) / 32 * 32
        guard let queue = device.makeCommandQueue() else { throw MetalStripEncoderError.allocation("a command queue") }
        self.queue = queue

        var cache: CVMetalTextureCache?
        guard CVMetalTextureCacheCreate(nil, nil, device, nil, &cache) == kCVReturnSuccess, let cache else {
            throw MetalStripEncoderError.allocation("a Metal texture cache")
        }
        textureCache = cache
        let descriptor = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .bgra8Unorm, width: 1, height: 1, mipmapped: false)
        descriptor.usage = .shaderRead
        guard let placeholder = device.makeTexture(descriptor: descriptor) else {
            throw MetalStripEncoderError.allocation("a placeholder texture")
        }
        self.placeholder = placeholder

        let strips = c.stripCount
        shadow = try Self.buffer(device, c.paddedWidth * c.paddedHeight * 4, "the source shadow")
        hashes = try Self.buffer(device, strips * 8, "the strip hashes")
        lut = try Self.buffer(device, 768, "the gamma table")
        candidates = try Self.buffer(device, strips * 4, "the candidate list")
        allStrips = try Self.buffer(device, strips * 4, "the strip list")
        flags = try Self.buffer(device, strips * 4, "the changed flags")
        lengths = try Self.buffer(device, strips * 4, "the body lengths")
        slots = try Self.buffer(device, strips * 4, "the strip slots")
        changed = try Self.buffer(device, strips * 4, "the changed list")
        changedHashes = try Self.buffer(device, strips * 8, "the changed hashes")
        offsets = try Self.buffer(device, (strips + 1) * 4, "the body offsets")
        totals = try Self.buffer(device, 16, "the totals")
        if packOnGPU {
            // About 500 bytes a strip covers a busy 4K desktop; a frame that needs more grows it.
            output = try Self.buffer(device, strips * 512, "the strip bodies")
            coefficients = nil
        } else {
            coefficients = try Self.buffer(device, strips * Self.stripCoefficientBytes, "the coefficient buffer")
        }
        let all = allStrips.contents().bindMemory(to: UInt32.self, capacity: strips)
        for i in 0..<strips { all[i] = UInt32(i) }

        lutBytes = Self.identityGamma
        lut.contents().copyMemory(from: lutBytes, byteCount: 768)
        candidateMarks = [Bool](repeating: false, count: strips)
        params = StripParams(
            frameWidth: UInt32(c.width), frameHeight: UInt32(c.height), paddedWidth: UInt32(c.paddedWidth),
            stripWidth: UInt32(c.stripWidth), stripHeight: UInt32(c.stripHeight),
            stripWidthShift: UInt32(c.stripWidth.trailingZeroBitCount), tilesX: UInt32(c.tilesX),
            blockShift: UInt32((c.stripWidth / 8).trailingZeroBitCount), tenBit: c.tenBit ? 1 : 0,
            narrow: c.codeTable == DLL_CODE_TABLE_NARROW.rawValue ? 1 : 0, dcCmax: c.dcCmax, acCmax: c.acCmax,
            chromaAcCmax: c.chromaAcCmax, magic: UInt32(c.magic[0]) | UInt32(c.magic[1]) << 8,
            withLengths: packOnGPU ? 1 : 0)
    }

    private static func buffer(_ device: MTLDevice, _ length: Int, _ what: String) throws -> MTLBuffer {
        guard let buffer = device.makeBuffer(length: max(length, 16), options: .storageModeShared) else {
            throw MetalStripEncoderError.allocation(what)
        }
        memset(buffer.contents(), 0, buffer.length)
        return buffer
    }

    /// Bytes of GPU memory this encoder holds now; the body buffer only grows.
    public var allocatedBytes: Int {
        [shadow, hashes, lut, candidates, allStrips, flags, lengths, slots, changed, changedHashes, offsets, totals]
            .reduce(0) { $0 + $1.allocatedSize } + (output?.allocatedSize ?? 0) + (coefficients?.allocatedSize ?? 0)
    }

    /// Set the gamma table: 768 bytes, R, G, B tables of 256 each; nil is the identity. A
    /// different table codes every strip again, since every coded strip went through the old one.
    public func setGamma(_ table: [UInt8]?) {
        let table = (table?.count == 768 ? table : nil) ?? Self.identityGamma
        guard table != lutBytes else { return }
        lutBytes = table
        lut.contents().copyMemory(from: table, byteCount: 768)
        needsFullEncode = true
    }

    /// Code every strip again at the next call, whatever the hashes say.
    public func requestFullEncode() {
        needsFullEncode = true
    }

    /// Code the strips of `frame` that changed. `dirty` holds (x, y, width, height) quads in frame
    /// pixels, as ScreenCaptureKit reports them; nil means the whole frame may have changed.
    /// With `full`, or a pending full encode, every strip is coded from this frame. The frame must
    /// be IOSurface-backed 32BGRA at the configured size, and stays referenced until the GPU is done.
    public func encode(frame: CVPixelBuffer, dirty: [UInt32]?, full: Bool = false) throws -> EncodedStrips {
        guard let run = try dispatch(frame: frame, dirty: dirty, full: full) else { return EncodedStrips() }
        return try strips(run)
    }

    /// Code every strip from the shadow, for a full encode with no new frame (a gamma change or an
    /// engine request while the screen is still). Nil until a frame has been seen.
    public func encodeFromShadow() throws -> EncodedStrips? {
        guard let run = try dispatchFromShadow() else { return nil }
        return try strips(run)
    }

    /// The shadow as a BGRA frame of the configured size (rows of width * 4 bytes, alpha 255),
    /// for handing the CPU path the picture the GPU last saw. Nil until a frame has been seen.
    public func shadowBGRA() -> [UInt8]? {
        guard hasFrame else { return nil }
        let c = configuration
        let pixels = shadow.contents().bindMemory(to: UInt32.self, capacity: c.paddedWidth * c.paddedHeight)
        var bgra = [UInt8](repeating: 255, count: c.width * c.height * 4)
        bgra.withUnsafeMutableBufferPointer { out in
            for y in 0..<c.height {
                for x in 0..<c.width {
                    let px = pixels[y * c.paddedWidth + x]
                    let at = (y * c.width + x) * 4
                    out[at] = UInt8(truncatingIfNeeded: px >> 16)
                    out[at + 1] = UInt8(truncatingIfNeeded: px >> 8)
                    out[at + 2] = UInt8(truncatingIfNeeded: px)
                }
            }
        }
        return bgra
    }

    // MARK: - GPU stage

    /// What one call left in the GPU buffers: `changed` strips in slot order, and with GPU
    /// packing `bodyBytes` of bodies.
    struct Run {
        var changed: Int
        var bodyBytes: Int
        var gpuSeconds: Double
    }

    /// The GPU's output for one call with CPU packing: changed strips sorted by index, each with
    /// the slot its coefficients went to.
    struct Quantized {
        var strips: [(index: Int, slot: Int, hash: UInt64)] = []
        var gpuSeconds: Double = 0
    }

    private func strips(_ run: Run) throws -> EncodedStrips {
        guard packOnGPU else { return try pack(quantized(run)) }
        guard let output else { throw MetalStripEncoderError.allocation("the strip bodies") }
        return EncodedStrips(
            storage: .borrowed(indices: changed, hashes: changedHashes, bodies: output, offsets: offsets, bodyBytes: run.bodyBytes),
            count: run.changed, gpuSeconds: run.gpuSeconds)
    }

    private func quantized(_ run: Run) -> Quantized {
        let list = changed.contents().bindMemory(to: UInt32.self, capacity: configuration.stripCount)
        let stripHashes = changedHashes.contents().bindMemory(to: UInt64.self, capacity: configuration.stripCount)
        let strips = (0..<run.changed).map { slot in (index: Int(list[slot]), slot: slot, hash: stripHashes[slot]) }
        return Quantized(strips: strips, gpuSeconds: run.gpuSeconds)
    }

    /// CPU packing only: the frame's changed strips, quantised but not coded.
    func encodeQuantized(frame: CVPixelBuffer, dirty: [UInt32]?, full: Bool) throws -> Quantized {
        precondition(!packOnGPU, "quantised blocks are only kept with CPU packing")
        guard let run = try dispatch(frame: frame, dirty: dirty, full: full) else { return Quantized() }
        return quantized(run)
    }

    /// CPU packing only: every strip of the shadow, quantised but not coded.
    func encodeQuantizedFromShadow() throws -> Quantized? {
        precondition(!packOnGPU, "quantised blocks are only kept with CPU packing")
        return try dispatchFromShadow().map(quantized)
    }

    private func dispatch(frame: CVPixelBuffer, dirty: [UInt32]?, full: Bool) throws -> Run? {
        let c = configuration
        guard CVPixelBufferGetPixelFormatType(frame) == kCVPixelFormatType_32BGRA else {
            throw MetalStripEncoderError.unsupportedFrame("pixel format is not 32BGRA")
        }
        guard CVPixelBufferGetWidth(frame) == c.width, CVPixelBufferGetHeight(frame) == c.height else {
            throw MetalStripEncoderError.unsupportedFrame(
                "\(CVPixelBufferGetWidth(frame))x\(CVPixelBufferGetHeight(frame)), expected \(c.width)x\(c.height)")
        }
        let everything = full || needsFullEncode || !hasFrame
        let count: Int
        let list: MTLBuffer
        if everything {
            count = c.stripCount
            list = allStrips
        } else {
            count = markCandidates(dirty)
            list = candidates
        }
        if count == 0 { return nil }

        var cvTexture: CVMetalTexture?
        guard CVMetalTextureCacheCreateTextureFromImage(nil, textureCache, frame, nil, .bgra8Unorm, c.width, c.height, 0,
                                                        &cvTexture) == kCVReturnSuccess,
              let cvTexture, let texture = CVMetalTextureGetTexture(cvTexture)
        else {
            throw MetalStripEncoderError.unsupportedFrame("no Metal texture for the frame (is it IOSurface-backed?)")
        }
        let result = try run(texture: texture, list: list, count: count, force: everything, fromShadow: false,
                             keepAlive: [frame, cvTexture])
        if everything {
            hasFrame = true
            needsFullEncode = false
        }
        return result
    }

    private func dispatchFromShadow() throws -> Run? {
        guard hasFrame else { return nil }
        let result = try run(texture: placeholder, list: allStrips, count: configuration.stripCount, force: true,
                             fromShadow: true, keepAlive: [])
        needsFullEncode = false
        return result
    }

    /// The candidate strips for `dirty`, marked the way the engine's convert_frame marks them,
    /// written to `candidates` in ascending order. Returns how many.
    private func markCandidates(_ dirty: [UInt32]?) -> Int {
        let c = configuration
        guard let dirty else {
            let all = allStrips.contents()
            candidates.contents().copyMemory(from: all, byteCount: c.stripCount * 4)
            return c.stripCount
        }
        var any = false
        var quad = 0
        while quad + 3 < dirty.count {
            let x0 = Int(dirty[quad]), y0 = Int(dirty[quad + 1])
            let x1 = min(x0 + Int(dirty[quad + 2]), c.width), y1 = min(y0 + Int(dirty[quad + 3]), c.height)
            quad += 4
            guard x0 < x1, y0 < y1 else { continue }
            for ty in (y0 / c.stripHeight)..<((y1 + c.stripHeight - 1) / c.stripHeight) {
                for tx in (x0 / c.stripWidth)..<((x1 + c.stripWidth - 1) / c.stripWidth) {
                    candidateMarks[ty * c.tilesX + tx] = true
                    any = true
                }
            }
        }
        guard any else { return 0 }
        let out = candidates.contents().bindMemory(to: UInt32.self, capacity: c.stripCount)
        var count = 0
        for index in 0..<c.stripCount where candidateMarks[index] {
            candidateMarks[index] = false
            out[count] = UInt32(index)
            count += 1
        }
        return count
    }

    /// Tests only: code `blocks` (`DLL_STRIP_BLOCKS` per strip) as the strips `strips`, in
    /// ascending order, with the GPU coder alone, so coefficients no picture produces reach it.
    func packForTesting(blocks: [DLLQuantBlock], strips: [Int]) throws -> EncodedStrips {
        precondition(packOnGPU && blocks.count == strips.count * Int(DLL_STRIP_BLOCKS) && !strips.isEmpty)
        precondition(zip(strips, strips.dropFirst()).allSatisfy(<) && strips.last! < configuration.stripCount)
        let given = try blocks.withUnsafeBytes { raw in
            guard let buffer = device.makeBuffer(bytes: raw.baseAddress!, length: raw.count, options: .storageModeShared) else {
                throw MetalStripEncoderError.allocation("the test coefficients")
            }
            return buffer
        }
        let list = candidates.contents().bindMemory(to: UInt32.self, capacity: strips.count)
        let flagged = flags.contents().bindMemory(to: UInt32.self, capacity: strips.count)
        for (i, strip) in strips.enumerated() {
            list[i] = UInt32(strip)
            flagged[i] = 1
        }
        return try self.strips(run(texture: placeholder, list: candidates, count: strips.count, force: true,
                                   fromShadow: true, keepAlive: [], given: given))
    }

    private func run(texture: MTLTexture, list: MTLBuffer, count: Int, force: Bool, fromShadow: Bool,
                     keepAlive: [AnyObject], given: MTLBuffer? = nil) throws -> Run {
        var params = self.params
        params.force = force ? 1 : 0
        params.fromShadow = fromShadow ? 1 : 0
        params.fromCoefficients = given == nil ? 0 : 1
        // The kernels only read `given` when told to, but every binding must be a buffer.
        let given = given ?? totals
        params.candidateCount = UInt32(count)
        params.outputCapacity = UInt32(clamping: output?.length ?? 0)

        guard let commands = queue.makeCommandBuffer() else { throw MetalStripEncoderError.gpu("no command buffer") }
        let groups = MTLSize(width: count, height: 1, depth: 1)
        let threads = MTLSize(width: Self.threadsPerStrip, height: 1, depth: 1)
        func pass(_ pipeline: MTLComputePipelineState, groups: MTLSize = groups, threads: MTLSize = threads,
                  _ bind: (MTLComputeCommandEncoder) -> Void) throws {
            // One encoder per kernel: each reads what the one before wrote, and buffers in one
            // command buffer are hazard-tracked across encoders.
            guard let encoder = commands.makeComputeCommandEncoder() else { throw MetalStripEncoderError.gpu("no encoder") }
            encoder.setComputePipelineState(pipeline)
            bind(encoder)
            encoder.dispatchThreadgroups(groups, threadsPerThreadgroup: threads)
            encoder.endEncoding()
        }
        let paramsLength = MemoryLayout<StripParams>.stride

        if params.fromCoefficients == 0 {
            try pass(hashPipeline) { e in
                e.setTexture(texture, index: 0)
                e.setBuffers([shadow, hashes, lut, list, flags], offsets: [0, 0, 0, 0, 0], range: 0..<5)
                e.setBytes(&params, length: paramsLength, index: 5)
            }
        }
        if let sizePipeline {
            try pass(sizePipeline) { e in
                e.setBuffers([shadow, lut, list, flags, lengths], offsets: [0, 0, 0, 0, 0], range: 0..<5)
                e.setBytes(&params, length: paramsLength, index: 5)
                e.setBuffer(given, offset: 0, index: 6)
            }
        }
        try pass(scanPipeline, groups: MTLSize(width: 1, height: 1, depth: 1),
                 threads: MTLSize(width: scanThreads, height: 1, depth: 1)) { e in
            e.setBuffers([list, flags, lengths, hashes, changed, slots, offsets, changedHashes, totals],
                         offsets: [0, 0, 0, 0, 0, 0, 0, 0, 0], range: 0..<9)
            e.setBytes(&params, length: paramsLength, index: 9)
        }
        if let packPipeline, let output {
            try encodePack(packPipeline, into: commands, list: list, output: output, given: given, groups: groups,
                           params: &params)
        }
        if let encodePipeline, let coefficients {
            // The changed count is only known on the GPU, so this is sized for every candidate
            // and the surplus groups return at once.
            try pass(encodePipeline) { e in
                e.setBuffers([shadow, lut, changed, totals, coefficients], offsets: [0, 0, 0, 0, 0], range: 0..<5)
                e.setBytes(&params, length: paramsLength, index: 5)
            }
        }

        var gpuSeconds = try finish(commands, keepAlive: keepAlive)
        let changedTotal = Int(totals.contents().load(as: UInt32.self))
        guard changedTotal <= count else {
            needsFullEncode = true
            throw MetalStripEncoderError.gpu("changed count \(changedTotal) exceeds \(count) candidates")
        }
        var bodyBytes = 0
        if let packPipeline, let current = output {
            bodyBytes = Int(totals.contents().load(fromByteOffset: 4, as: UInt32.self))
            if bodyBytes > current.length {
                // Every body past the old end was skipped; the hashes, flags and offsets are all
                // still right, so packing again into a larger buffer completes the frame.
                let grown = try Self.buffer(device, (bodyBytes + bodyBytes / 4 + 0xFFFF) & ~0xFFFF, "the strip bodies")
                output = grown
                params.outputCapacity = UInt32(clamping: grown.length)
                guard let again = queue.makeCommandBuffer() else { throw MetalStripEncoderError.gpu("no command buffer") }
                try encodePack(packPipeline, into: again, list: list, output: grown, given: given, groups: groups,
                               params: &params)
                gpuSeconds += try finish(again, keepAlive: [])
            }
        }
        return Run(changed: changedTotal, bodyBytes: bodyBytes, gpuSeconds: gpuSeconds)
    }

    private func encodePack(_ pipeline: MTLComputePipelineState, into commands: MTLCommandBuffer, list: MTLBuffer,
                            output: MTLBuffer, given: MTLBuffer, groups: MTLSize, params: inout StripParams) throws {
        guard let e = commands.makeComputeCommandEncoder() else { throw MetalStripEncoderError.gpu("no encoder") }
        e.setComputePipelineState(pipeline)
        e.setBuffers([shadow, lut, list, flags, slots, offsets, output], offsets: [0, 0, 0, 0, 0, 0, 0], range: 0..<7)
        e.setBytes(&params, length: MemoryLayout<StripParams>.stride, index: 7)
        e.setBuffer(given, offset: 0, index: 8)
        e.dispatchThreadgroups(groups, threadsPerThreadgroup: MTLSize(width: Self.threadsPerStrip, height: 1, depth: 1))
        e.endEncoding()
    }

    /// Commit and wait; returns the GPU time.
    private func finish(_ commands: MTLCommandBuffer, keepAlive: [AnyObject]) throws -> Double {
        // The frame's IOSurface must outlive the GPU's reads of it.
        nonisolated(unsafe) let retained = keepAlive
        commands.addCompletedHandler { _ in withExtendedLifetime(retained) {} }
        commands.commit()
        commands.waitUntilCompleted()
        if commands.status != .completed {
            // The hashes may be half updated; nothing but a full encode is trustworthy now.
            needsFullEncode = true
            throw MetalStripEncoderError.gpu(commands.error?.localizedDescription ?? "status \(commands.status.rawValue)")
        }
        return max(0, commands.gpuEndTime - commands.gpuStartTime)
    }

    /// CPU packing only: the 16 quantised blocks of a strip in a `Quantized`, valid until the
    /// next encode.
    func blocks(slot: Int) -> UnsafePointer<DLLQuantBlock> {
        UnsafePointer((coefficients!.contents() + slot * Self.stripCoefficientBytes)
            .bindMemory(to: DLLQuantBlock.self, capacity: Int(DLL_STRIP_BLOCKS)))
    }

    /// The shadow's pixels, 0x00BBGGRR, row by row over the padded width. For tests.
    func shadowPixels() -> [UInt32] {
        let n = configuration.paddedWidth * configuration.paddedHeight
        return Array(UnsafeBufferPointer(start: shadow.contents().bindMemory(to: UInt32.self, capacity: n), count: n))
    }

    // MARK: - CPU stage

    /// Code each strip's blocks with the engine's coder, spread over every core.
    func pack(_ quantized: Quantized) throws -> EncodedStrips {
        let n = quantized.strips.count
        guard n > 0, let coefficients else {
            var empty = EncodedStrips()
            empty.gpuSeconds = quantized.gpuSeconds
            return empty
        }
        let c = configuration
        let chunks = min(n, ProcessInfo.processInfo.activeProcessorCount * 4)
        let bodyMax = Int(DLL_STRIP_BODY_MAX)
        // Each chunk writes only its own slots, so these are shared without a lock.
        let work = PackWork(strips: quantized.strips, chunks: chunks, coefficients: coefficients.contents())
        defer { work.release() }
        DispatchQueue.concurrentPerform(iterations: chunks) { chunk in
            let range = (chunk * n / chunks)..<((chunk + 1) * n / chunks)
            var local = [UInt8]()
            local.reserveCapacity(range.count * 4096)
            withUnsafeTemporaryAllocation(of: UInt8.self, capacity: bodyMax) { scratch in
                for i in range {
                    let strip = work.strips[i]
                    let sx = UInt32((strip.index % c.tilesX) * c.stripWidth)
                    let sy = UInt32((strip.index / c.tilesX) * c.stripHeight)
                    let blocks = (work.coefficients + strip.slot * Self.stripCoefficientBytes)
                        .assumingMemoryBound(to: DLLQuantBlock.self)
                    let length = dll_encode_strip_quantized(c.family, c.tenBit, sx, sy, blocks, scratch.baseAddress, bodyMax)
                    guard length > 0, length <= bodyMax else {
                        work.failed[chunk] = strip.index
                        return
                    }
                    local.append(contentsOf: UnsafeBufferPointer(start: scratch.baseAddress, count: length))
                    work.lengths[i] = length
                }
            }
            work.parts[chunk] = local
        }
        let failed = Array(UnsafeBufferPointer(start: work.failed, count: chunks))
        let lengths = UnsafeBufferPointer(start: work.lengths, count: n)
        let parts = UnsafeBufferPointer(start: work.parts, count: chunks)
        if let bad = failed.first(where: { $0 >= 0 }) { throw MetalStripEncoderError.packing(strip: bad) }

        var offsets = [UInt32]()
        offsets.reserveCapacity(n + 1)
        offsets.append(0)
        var total = 0
        for length in lengths {
            total += length
            offsets.append(UInt32(total))
        }
        var bodies = [UInt8]()
        bodies.reserveCapacity(total)
        for part in parts { bodies.append(contentsOf: part) }
        return EncodedStrips(
            storage: .owned(indices: quantized.strips.map { UInt32($0.index) }, hashes: quantized.strips.map(\.hash),
                            bodies: bodies, offsets: offsets),
            count: n, gpuSeconds: quantized.gpuSeconds)
    }
}

/// Per-call scratch for the parallel packer. Raw pointers so the concurrent closure can write
/// disjoint slots; every chunk owns its own `parts` and `failed` entry and its strips' `lengths`.
private struct PackWork: @unchecked Sendable {
    let strips: [(index: Int, slot: Int, hash: UInt64)]
    let coefficients: UnsafeMutableRawPointer
    let parts: UnsafeMutablePointer<[UInt8]>
    let lengths: UnsafeMutablePointer<Int>
    let failed: UnsafeMutablePointer<Int>
    let chunks: Int

    init(strips: [(index: Int, slot: Int, hash: UInt64)], chunks: Int, coefficients: UnsafeMutableRawPointer) {
        self.strips = strips
        self.coefficients = coefficients
        self.chunks = chunks
        parts = .allocate(capacity: chunks)
        parts.initialize(repeating: [], count: chunks)
        lengths = .allocate(capacity: strips.count)
        lengths.initialize(repeating: 0, count: strips.count)
        failed = .allocate(capacity: chunks)
        failed.initialize(repeating: -1, count: chunks)
    }

    func release() {
        parts.deinitialize(count: chunks)
        parts.deallocate()
        lengths.deallocate()
        failed.deallocate()
    }
}

/// DLLQuantBlock's three planes are back to back with no padding, which the kernel assumes.
private func blockSizeIsPacked() -> Bool {
    MemoryLayout<DLLQuantBlock>.offset(of: \DLLQuantBlock.cb) == 128
        && MemoryLayout<DLLQuantBlock>.offset(of: \DLLQuantBlock.y) == 256
}
