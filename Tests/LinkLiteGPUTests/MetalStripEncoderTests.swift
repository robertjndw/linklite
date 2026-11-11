// SPDX-License-Identifier: GPL-2.0-or-later

import CDLLite
import CoreVideo
import Foundation
import Metal
import Testing
@testable import LinkLiteGPU

// The GPU encoder against the engine's CPU reference (dll_reference_encode_strip and
// dll_reference_quantize_strip), strip by strip. Skipped where there is no Metal device (CI).

private let hasMetal = MTLCreateSystemDefaultDevice() != nil

private let families: [(name: String, family: DLLFamily)] = [
    ("Ella", DLL_FAMILY_ELLA), ("Ridge", DLL_FAMILY_RIDGE), ("Navarro", DLL_FAMILY_NAVARRO)
]

/// A frame as the capture hands it over: BGRA rows of `width * 4` bytes.
private struct Frame {
    var width: Int
    var height: Int
    var bgra: [UInt8]

    init(width: Int, height: Int, pixel: (Int, Int) -> (b: UInt8, g: UInt8, r: UInt8, a: UInt8)) {
        self.width = width
        self.height = height
        bgra = [UInt8](repeating: 0, count: width * height * 4)
        for y in 0..<height {
            for x in 0..<width {
                let p = pixel(x, y)
                let at = (y * width + x) * 4
                (bgra[at], bgra[at + 1], bgra[at + 2], bgra[at + 3]) = (p.b, p.g, p.r, p.a)
            }
        }
    }

    /// An IOSurface-backed copy, as ScreenCaptureKit delivers frames.
    func pixelBuffer() throws -> CVPixelBuffer {
        let attributes: [CFString: Any] = [
            kCVPixelBufferIOSurfacePropertiesKey: [CFString: Any](),
            kCVPixelBufferMetalCompatibilityKey: true
        ]
        var buffer: CVPixelBuffer?
        let status = CVPixelBufferCreate(nil, width, height, kCVPixelFormatType_32BGRA, attributes as CFDictionary, &buffer)
        let pixelBuffer = try #require(buffer, "CVPixelBufferCreate failed: \(status)")
        CVPixelBufferLockBaseAddress(pixelBuffer, [])
        defer { CVPixelBufferUnlockBaseAddress(pixelBuffer, []) }
        let base = try #require(CVPixelBufferGetBaseAddress(pixelBuffer))
        let stride = CVPixelBufferGetBytesPerRow(pixelBuffer)
        bgra.withUnsafeBytes { source in
            for y in 0..<height {
                memcpy(base + y * stride, source.baseAddress! + y * width * 4, width * 4)
            }
        }
        return pixelBuffer
    }

    /// The engine's view of the frame: packed RGB over the padded surface, padding 0, then the
    /// gamma table over all of it, padding included (apply_gamma in host.rs).
    func referenceRGB(paddedWidth: Int, paddedHeight: Int, gamma: [UInt8]?) -> [UInt8] {
        var rgb = [UInt8](repeating: 0, count: paddedWidth * paddedHeight * 3)
        for y in 0..<height {
            for x in 0..<width {
                let source = (y * width + x) * 4
                let target = (y * paddedWidth + x) * 3
                (rgb[target], rgb[target + 1], rgb[target + 2]) = (bgra[source + 2], bgra[source + 1], bgra[source])
            }
        }
        if let gamma {
            for i in stride(from: 0, to: rgb.count, by: 3) {
                rgb[i] = gamma[Int(rgb[i])]
                rgb[i + 1] = gamma[256 + Int(rgb[i + 1])]
                rgb[i + 2] = gamma[512 + Int(rgb[i + 2])]
            }
        }
        return rgb
    }
}

/// The engine's encoder profile for a family, depth and frame size, so no strip geometry or
/// coding parameter is restated here. The padding comes from the replay head, which is the
/// engine's own Accepting built the way attaching builds it (always 8-bit); the depth-dependent
/// fields come from the stateless profile at the depth asked for.
private func configuration(_ family: DLLFamily, width: Int, height: Int, tenBit: Bool) throws -> StripEncoderConfiguration {
    let bench = try #require(dll_bench_new(family.rawValue, UInt32(width), UInt32(height), 60, nil))
    defer { dll_bench_free(bench) }
    var attached = DLLEncoderProfile()
    try #require(dll_bench_encoder_profile(bench, &attached))
    var profile = DLLEncoderProfile()
    try #require(dll_strip_encoding_profile(family.rawValue, tenBit, &profile))
    profile.padded_width = attached.padded_width
    profile.padded_height = attached.padded_height
    return StripEncoderConfiguration(profile: profile, width: width, height: height)
}

private let packings: [(name: String, onGPU: Bool)] = [("CPU-packed", false), ("GPU-packed", true)]

private struct Xorshift {
    var state: UInt64
    mutating func next() -> UInt64 {
        state ^= state << 13
        state ^= state >> 7
        state ^= state << 17
        return state
    }
}

private func patterns(width: Int, height: Int) -> [(String, Frame)] {
    var random = Xorshift(state: 0x2545_f491_4f6c_dd1d)
    let noise = (0..<(width * height)).map { _ in random.next() }
    let ext = { (on: Bool) -> UInt8 in on ? 255 : 0 }
    return [
        ("noise", Frame(width: width, height: height) { x, y in
            let v = noise[y * width + x]
            return (UInt8(truncatingIfNeeded: v >> 8), UInt8(truncatingIfNeeded: v >> 24), UInt8(truncatingIfNeeded: v >> 40), 255)
        }),
        ("extremes", Frame(width: width, height: height) { x, y in
            (ext((x + y) & 4 == 0), ext(x & 2 == 0), ext((x ^ y) & 1 == 1), 255)
        }),
        ("gradients", Frame(width: width, height: height) { x, y in
            (UInt8(truncatingIfNeeded: x + y * 3), UInt8(truncatingIfNeeded: y * 8), UInt8(truncatingIfNeeded: x), 255)
        }),
        ("chroma only", Frame(width: width, height: height) { x, y in
            let d = UInt8((x * 7 + y * 13) % 128)
            return (128 - d, 128, 128 + min(d, 127), 255)
        }),
        ("block edges", Frame(width: width, height: height) { x, y in
            let on = (x / 8 + y / 8) % 2 == 0
            return (ext(on), ext(!on), ext(on), 255)
        }),
        ("one-pixel gratings", Frame(width: width, height: height) { x, y in
            (0x80, ext(y % 2 == 0), ext(x % 2 == 0), UInt8(truncatingIfNeeded: x * 3))
        }),
        // The rest drive coefficients to their largest magnitudes, where the escape codes reach
        // the ceiling categories and lose their terminator, and the codes are at their longest.
        ("checkerboard in blue", Frame(width: width, height: height) { x, y in (ext((x ^ y) & 1 == 0), 0, 0, 255) }),
        ("checkerboard in green", Frame(width: width, height: height) { x, y in (0, ext((x ^ y) & 1 == 0), 0, 255) }),
        ("checkerboard in red", Frame(width: width, height: height) { x, y in (0, 0, ext((x ^ y) & 1 == 0), 255) }),
        ("checkerboard in all", Frame(width: width, height: height) { x, y in
            let on = ext((x ^ y) & 1 == 0)
            return (on, on, on, 255)
        }),
        ("maximal chroma", Frame(width: width, height: height) { x, y in
            // Magenta against green: Cb and Cr swing the full range together.
            (x ^ y) & 1 == 0 ? (255, 0, 255, 255) : (0, 255, 0, 255)
        }),
        ("maximal chroma blocks", Frame(width: width, height: height) { x, y in
            // Whole blocks alternating, so the DC deltas take their largest steps.
            (x / 8 + y / 8) % 2 == 0 ? (255, 0, 255, 255) : (0, 255, 0, 255)
        }),
        ("white and black blocks", Frame(width: width, height: height) { x, y in
            let on = ext((x / 8 + y / 8) % 2 == 0)
            return (on, on, on, 255)
        }),
        ("noise in blocks", Frame(width: width, height: height) { x, y in
            // Busy and flat blocks side by side, so rows of every length meet at every word offset.
            let v = noise[y * width + x]
            let busy = (x / 8 * 7 + y / 8 * 3) % 5 < 2
            return busy ? (UInt8(truncatingIfNeeded: v), ext(v & 256 != 0), UInt8(truncatingIfNeeded: v >> 16), 255) : (17, 99, 201, 255)
        })
    ]
}

/// A table that moves every value, including 0, so padding codes as lut[c][0] != 0.
private let invertingGamma: [UInt8] = (0..<768).map { i in
    let channel = i / 256, value = i % 256
    return UInt8(255 - (value + channel * 17) % 256)
}

/// Compares every strip in `packed` with the CPU reference body; returns how many it compared.
/// Reports the first difference per configuration with family, depth, strip and position.
private func compareBodies(_ packed: EncodedStrips, configuration c: StripEncoderConfiguration, rgb: [UInt8],
                           label: String) throws -> Int {
    var reference = [UInt8](repeating: 0, count: Int(DLL_STRIP_BODY_MAX))
    let indices = packed.indices, bodies = packed.bodies, offsets = packed.offsets
    try #require(offsets.count == packed.count + 1 && offsets.first == 0 && Int(offsets.last!) == bodies.count,
                 "\(label): offsets do not cover the bodies")
    var compared = 0
    for i in 0..<packed.count {
        let index = Int(indices[i])
        if i > 0 { #expect(indices[i - 1] < indices[i], "\(label): strips out of order") }
        let sx = UInt32((index % c.tilesX) * c.stripWidth)
        let sy = UInt32((index / c.tilesX) * c.stripHeight)
        let place = "\(label) strip \(index) at (\(sx), \(sy))"
        let length = rgb.withUnsafeBufferPointer { rgb in
            dll_reference_encode_strip(c.family, c.tenBit, rgb.baseAddress, rgb.count, UInt32(c.paddedWidth), sx, sy,
                                       &reference, reference.count)
        }
        try #require(length > 0, "reference encode refused \(place)")
        let body = bodies[Int(offsets[i])..<Int(offsets[i + 1])]
        if body.count != length || !body.elementsEqual(reference[0..<length]) {
            let first = zip(body, reference[0..<length]).enumerated().first { $0.element.0 != $0.element.1 }?.offset
                ?? min(body.count, length)
            Issue.record("\(place): body differs at byte \(first) (GPU \(body.count) bytes, CPU \(length))")
            return compared
        }
        compared += 1
    }
    return compared
}

/// With CPU packing, also checks the quantised blocks against the reference quantiser before the
/// bodies; returns how many strips it compared.
private func compareQuantized(encoder: MetalStripEncoder, quantized: MetalStripEncoder.Quantized, rgb: [UInt8],
                              label: String) throws -> Int {
    let c = encoder.configuration
    var referenceBlocks = [DLLQuantBlock](repeating: DLLQuantBlock(), count: Int(DLL_STRIP_BLOCKS))
    for strip in quantized.strips {
        let sx = UInt32((strip.index % c.tilesX) * c.stripWidth)
        let sy = UInt32((strip.index / c.tilesX) * c.stripHeight)
        let place = "\(label) strip \(strip.index) at (\(sx), \(sy))"
        let ok = rgb.withUnsafeBufferPointer { rgb in
            referenceBlocks.withUnsafeMutableBufferPointer {
                dll_reference_quantize_strip(c.family, c.tenBit, rgb.baseAddress, rgb.count, UInt32(c.paddedWidth), sx, sy, $0.baseAddress)
            }
        }
        try #require(ok, "reference quantise refused \(place)")
        let gpu = UnsafeRawBufferPointer(start: encoder.blocks(slot: strip.slot), count: Int(DLL_STRIP_BLOCKS) * 384)
            .bindMemory(to: Int16.self)
        let cpu = referenceBlocks.withUnsafeBytes { Array($0.bindMemory(to: Int16.self)) }
        if let at = (0..<cpu.count).first(where: { gpu[$0] != cpu[$0] }) {
            let block = at / 192, plane = ["Cr", "Cb", "Y"][at % 192 / 64], position = at % 64
            Issue.record("\(place): coefficient differs at block \(block) \(plane)[\(position)]: GPU \(gpu[at]), CPU \(cpu[at])")
            return 0
        }
    }
    return try compareBodies(encoder.pack(quantized), configuration: c, rgb: rgb, label: label)
}

/// Codes `frame` (or, with `frame` nil, the shadow) in full and compares every strip with the
/// reference, through whichever packing the encoder uses.
private func encodeAndCompare(_ encoder: MetalStripEncoder, frame: Frame?, reference: Frame, gamma: [UInt8]?,
                              label: String) throws -> Int {
    let c = encoder.configuration
    let rgb = reference.referenceRGB(paddedWidth: c.paddedWidth, paddedHeight: c.paddedHeight, gamma: gamma)
    if encoder.packOnGPU {
        let packed: EncodedStrips
        if let frame {
            packed = try encoder.encode(frame: frame.pixelBuffer(), dirty: nil, full: true)
        } else {
            packed = try #require(try encoder.encodeFromShadow())
        }
        #expect(packed.count == c.stripCount, "\(label): a full encode codes every strip")
        return try compareBodies(packed, configuration: c, rgb: rgb, label: label)
    }
    let quantized: MetalStripEncoder.Quantized
    if let frame {
        quantized = try encoder.encodeQuantized(frame: frame.pixelBuffer(), dirty: nil, full: true)
    } else {
        quantized = try #require(try encoder.encodeQuantizedFromShadow())
    }
    #expect(quantized.strips.count == c.stripCount, "\(label): a full encode codes every strip")
    return try compareQuantized(encoder: encoder, quantized: quantized, rgb: rgb, label: label)
}

@Suite(.enabled(if: hasMetal, "no Metal device"), .serialized)
struct MetalStripEncoderTests {
    /// bgra8Unorm reads back as c / 255; the kernel's round(v * 255) has to give every byte back.
    @Test func everyByteValueSurvivesTheTextureRead() throws {
        let frame = Frame(width: 256, height: 3) { x, y in
            switch y {
            case 0: (UInt8(x), UInt8(255 - x), UInt8((x * 7) & 255), UInt8(x))
            case 1: (UInt8((x * 3) & 255), UInt8(x), UInt8(255 - x), 0)
            default: (UInt8(255 - x), UInt8((x * 11) & 255), UInt8(x), 255)
            }
        }
        let encoder = try MetalStripEncoder(configuration: configuration(DLL_FAMILY_RIDGE, width: 256, height: 3, tenBit: false))
        _ = try encoder.encode(frame: frame.pixelBuffer(), dirty: nil)
        let shadow = encoder.shadowPixels()
        let pw = encoder.configuration.paddedWidth
        var seen = [Set<UInt8>](repeating: [], count: 3)
        for y in 0..<frame.height {
            for x in 0..<frame.width {
                let at = (y * frame.width + x) * 4
                let (b, g, r) = (frame.bgra[at], frame.bgra[at + 1], frame.bgra[at + 2])
                let expected = UInt32(r) | UInt32(g) << 8 | UInt32(b) << 16
                #expect(shadow[y * pw + x] == expected, "pixel (\(x), \(y))")
                seen[0].insert(r)
                seen[1].insert(g)
                seen[2].insert(b)
            }
        }
        #expect(seen.allSatisfy { $0.count == 256 })
        // Padding stays zero, which the gamma table then maps like the engine's padding.
        #expect(shadow[(frame.height) * pw] == 0)
    }

    /// Every strip of every pattern on each family, depth, gamma and packing, at sizes that are
    /// not a whole number of strips: the coded bytes (and with CPU packing the coefficients) equal
    /// the CPU path's.
    @Test(arguments: [(330, 75), (128, 16), (200, 37)], [false, true])
    func gpuStripsMatchTheCpuReference(size: (Int, Int), packOnGPU: Bool) throws {
        let (width, height) = size
        var total = 0
        for (name, family) in families {
            for tenBit in [false, true] {
                let config = try configuration(family, width: width, height: height, tenBit: tenBit)
                for gamma in [nil, invertingGamma] {
                    let encoder = try MetalStripEncoder(configuration: config, packOnGPU: packOnGPU)
                    encoder.setGamma(gamma)
                    for (pattern, frame) in patterns(width: width, height: height) {
                        let label = "\(packOnGPU ? "GPU" : "CPU")-packed \(name) \(tenBit ? 10 : 8)-bit gamma \(gamma == nil ? "identity" : "inverting") \(pattern) \(width)x\(height)"
                        let compared = try encodeAndCompare(encoder, frame: frame, reference: frame, gamma: gamma, label: label)
                        #expect(compared == config.stripCount, "\(label)")
                        total += compared
                    }
                }
            }
        }
        print("gpuStripsMatchTheCpuReference \(width)x\(height) \(packOnGPU ? "GPU" : "CPU")-packed: \(total) strips compared byte for byte")
        #expect(total > 0)
    }

    /// The GPU coder alone, fed blocks no picture produces: magnitudes past every ceiling (the
    /// escape saturates), DC steps past the DC ceiling, single coefficients at every position,
    /// and random mixes whose codes land at every bit offset of the staging words. Compared with
    /// the engine's coder on the same blocks.
    @Test(arguments: [false, true])
    func theGpuCoderMatchesTheEngineCoderOnArbitraryBlocks(tenBit: Bool) throws {
        var total = 0
        for (name, family) in families {
            let config = try configuration(family, width: 1024, height: 256, tenBit: tenBit)
            let encoder = try MetalStripEncoder(configuration: config)
            var random = Xorshift(state: 0x9e37_79b9_7f4a_7c15 ^ UInt64(family.rawValue))
            var strips: [[DLLQuantBlock]] = []
            func strip(_ value: (Int, Int, Int) -> Int16) -> [DLLQuantBlock] {
                (0..<Int(DLL_STRIP_BLOCKS)).map { block in
                    var b = DLLQuantBlock()
                    withUnsafeMutableBytes(of: &b) { raw in
                        let v = raw.bindMemory(to: Int16.self)
                        for plane in 0..<3 {
                            for pos in 0..<64 { v[plane * 64 + pos] = value(block, plane, pos) }
                        }
                    }
                    return b
                }
            }
            let extremes: [Int16] = [0, 1, -1, 2, -2, 511, -512, 1023, -1024, 2047, -2048, 4092, -4092, 8191, -8192, .max, .min]
            // Every block at the extremes, signs alternating, and DCs swinging end to end.
            for e in extremes {
                strips.append(strip { block, _, pos in pos == 0 ? (block % 2 == 0 ? e : 0 &- e) : (pos % 2 == 0 ? e : 0 &- e) })
            }
            // One significant coefficient per plane, walking every position.
            for pos in 0..<64 {
                strips.append(strip { block, plane, p in p == (pos + block + plane * 7) % 64 ? Int16(pos * 64 - 2048) : 0 })
            }
            // Flat strips: all zero, and DC only.
            strips.append(strip { _, _, _ in 0 })
            strips.append(strip { block, _, pos in pos == 0 ? Int16(block * 300 - 2400) : 0 })
            // Random magnitudes of every size, sparse and dense.
            for density in [2, 5, 16, 64] {
                for _ in 0..<8 {
                    strips.append(strip { _, _, _ in
                        let r = random.next()
                        guard Int(r % 64) < density else { return 0 }
                        let bits = Int((r >> 8) % 17)
                        let magnitude = Int16(truncatingIfNeeded: (r >> 16) & ((1 << bits) - 1))
                        return r & (1 << 40) != 0 ? magnitude : 0 &- magnitude
                    })
                }
            }
            try #require(strips.count <= config.stripCount)
            let indices = Array(0..<strips.count).map { $0 * config.stripCount / strips.count }
            let packed = try encoder.packForTesting(blocks: strips.flatMap { $0 }, strips: indices)
            try #require(packed.count == strips.count)
            let bodies = packed.bodies, offsets = packed.offsets
            var reference = [UInt8](repeating: 0, count: Int(DLL_STRIP_BODY_MAX))
            for (i, blocks) in strips.enumerated() {
                let sx = UInt32((indices[i] % config.tilesX) * config.stripWidth)
                let sy = UInt32((indices[i] / config.tilesX) * config.stripHeight)
                let length = blocks.withUnsafeBufferPointer {
                    dll_encode_strip_quantized(config.family, tenBit, sx, sy, $0.baseAddress, &reference, reference.count)
                }
                try #require(length > 0)
                let body = bodies[Int(offsets[i])..<Int(offsets[i + 1])]
                if body.count != length || !body.elementsEqual(reference[0..<length]) {
                    let first = zip(body, reference[0..<length]).enumerated().first { $0.element.0 != $0.element.1 }?.offset
                        ?? min(body.count, length)
                    Issue.record("\(name) \(tenBit ? 10 : 8)-bit block set \(i): body differs at byte \(first) (GPU \(body.count) bytes, CPU \(length))")
                    continue
                }
                total += 1
            }
        }
        print("theGpuCoderMatchesTheEngineCoderOnArbitraryBlocks \(tenBit ? 10 : 8)-bit: \(total) strips compared byte for byte")
        #expect(total > 0)
    }

    /// A body buffer too small for the frame grows, and the frame still comes out whole.
    @Test func theBodyBufferGrowsForABusyFrame() throws {
        let (width, height) = (1024, 512)
        let config = try configuration(DLL_FAMILY_RIDGE, width: width, height: height, tenBit: true)
        let encoder = try MetalStripEncoder(configuration: config)
        let before = encoder.allocatedBytes
        let frame = patterns(width: width, height: height)[0].1
        let compared = try encodeAndCompare(encoder, frame: frame, reference: frame, gamma: nil, label: "busy 10-bit noise")
        #expect(compared == config.stripCount)
        #expect(encoder.allocatedBytes > before, "noise at 10 bits needs more than 512 bytes a strip")
        // And it stays grown: the next busy frame packs in one pass.
        #expect(try encodeAndCompare(encoder, frame: frame, reference: frame, gamma: nil, label: "again") == config.stripCount)
    }

    /// A full encode with no new frame codes the shadow, which must hold the last frame exactly.
    @Test(arguments: [false, true])
    func aFullEncodeFromTheShadowMatchesTheReference(packOnGPU: Bool) throws {
        let (width, height) = (200, 37)
        for (name, family) in families {
            let config = try configuration(family, width: width, height: height, tenBit: false)
            let encoder = try MetalStripEncoder(configuration: config, packOnGPU: packOnGPU)
            #expect(try encoder.encodeFromShadow() == nil, "no frame yet")
            #expect(encoder.shadowBGRA() == nil)
            let frames = patterns(width: width, height: height)
            _ = try encoder.encode(frame: frames[0].1.pixelBuffer(), dirty: nil)
            // Only part of the second frame is reported dirty; the shadow takes just those strips.
            let second = frames[2].1
            _ = try encoder.encode(frame: second.pixelBuffer(), dirty: [0, 0, UInt32(width), UInt32(height)])
            encoder.setGamma(invertingGamma)
            #expect(encoder.needsFullEncode)
            let compared = try encodeAndCompare(encoder, frame: nil, reference: second, gamma: invertingGamma,
                                                label: "\(name) from the shadow")
            #expect(!encoder.needsFullEncode)
            #expect(compared == config.stripCount)
            // What the CPU path gets after a fallback is the same picture, alpha aside.
            let bgra = try #require(encoder.shadowBGRA())
            var opaque = second.bgra
            for i in stride(from: 3, to: opaque.count, by: 4) { opaque[i] = 255 }
            #expect(bgra == opaque)
        }
    }

    @Test(arguments: [false, true], [false, true])
    func onlyStripsWhosePostGammaPixelsMovedAreCoded(withGamma: Bool, packOnGPU: Bool) throws {
        let (width, height) = (200, 37)
        let gamma = withGamma ? invertingGamma : nil
        for (name, family) in families {
            let config = try configuration(family, width: width, height: height, tenBit: false)
            let encoder = try MetalStripEncoder(configuration: config, packOnGPU: packOnGPU)
            encoder.setGamma(gamma)
            let base = patterns(width: width, height: height)[0].1
            let first = try encoder.encode(frame: base.pixelBuffer(), dirty: nil)
            #expect(first.count == config.stripCount, "\(name): the first frame codes everything")
            #expect(first.hashes.allSatisfy { $0 != 0 })
            #expect(first.indices == (0..<config.stripCount).map(UInt32.init))

            // The same pixels, reported as all dirty: nothing to send.
            #expect(try encoder.encode(frame: base.pixelBuffer(), dirty: nil).count == 0, "\(name): unchanged")

            // One pixel: exactly its strip, whether reported precisely or as the whole frame.
            var changed = base
            let (x, y) = (131, 29)
            changed.bgra[(y * width + x) * 4 + 1] ^= 0x40
            let strip = UInt32((y / config.stripHeight) * config.tilesX + x / config.stripWidth)
            let precise = try encoder.encode(frame: changed.pixelBuffer(), dirty: [UInt32(x), UInt32(y), 1, 1])
            #expect(precise.indices == [strip], "\(name): one pixel")
            let back = try encoder.encode(frame: base.pixelBuffer(), dirty: nil)
            #expect(back.indices == [strip], "\(name): and back")
            let rgb = base.referenceRGB(paddedWidth: config.paddedWidth, paddedHeight: config.paddedHeight, gamma: gamma)
            #expect(try compareBodies(back, configuration: config, rgb: rgb, label: "\(name) one-pixel change") == 1)

            // A dirty report that misses the change codes nothing, as on the CPU path.
            #expect(try encoder.encode(frame: changed.pixelBuffer(), dirty: [0, 0, 8, 8]).count == 0, "\(name): undamaged")
            _ = try encoder.encode(frame: changed.pixelBuffer(), dirty: nil)

            // Alpha alone never reaches the dock.
            var alpha = changed
            for i in stride(from: 3, to: alpha.bgra.count, by: 4) { alpha.bgra[i] ^= 0x5a }
            #expect(try encoder.encode(frame: alpha.pixelBuffer(), dirty: nil).count == 0, "\(name): alpha only")

            // A full request codes everything again, even unchanged.
            encoder.requestFullEncode()
            #expect(try encoder.encode(frame: alpha.pixelBuffer(), dirty: []).count == config.stripCount)
        }
    }

    /// Two captured values the gamma table maps to the same output are the same picture.
    @Test(arguments: [false, true])
    func aChangeTheGammaTableHidesCodesNothing(packOnGPU: Bool) throws {
        var gamma = MetalStripEncoder.identityGamma
        gamma[256 + 201] = 200 // green 200 and 201 both send 200
        let config = try configuration(DLL_FAMILY_RIDGE, width: 64, height: 16, tenBit: false)
        let encoder = try MetalStripEncoder(configuration: config, packOnGPU: packOnGPU)
        encoder.setGamma(gamma)
        let before = Frame(width: 64, height: 16) { _, _ in (10, 200, 30, 255) }
        let after = Frame(width: 64, height: 16) { x, _ in (10, x == 5 ? 201 : 200, 30, 255) }
        #expect(try encoder.encode(frame: before.pixelBuffer(), dirty: nil).count == 1)
        #expect(try encoder.encode(frame: after.pixelBuffer(), dirty: nil).count == 0)
        // The shadow still took the new pixels, so a later table that tells them apart is right.
        encoder.setGamma(nil)
        #expect(try encodeAndCompare(encoder, frame: nil, reference: after, gamma: nil, label: "after") == 1)
    }

    @Test func framesOfTheWrongShapeAreRefused() throws {
        let encoder = try MetalStripEncoder(configuration: configuration(DLL_FAMILY_RIDGE, width: 64, height: 16, tenBit: false))
        let wrong = Frame(width: 32, height: 16) { _, _ in (0, 0, 0, 255) }
        #expect(throws: MetalStripEncoderError.self) { try encoder.encode(frame: try wrong.pixelBuffer(), dirty: nil) }
    }
}
