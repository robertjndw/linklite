// SPDX-License-Identifier: GPL-2.0-or-later

import CDLLite
import CoreVideo
import Foundation
import LinkLiteGPU

// `replay`: runs recorded clips through the engine's pixel path (dll_bench_*, the same Replay the
// Rust `replay run` tool drives) with the frames fed either as BGRA (--cpu) or as strips coded by
// MetalStripEncoder from an IOSurface (--gpu). The JSON matches `replay run`, plus GPU time, so
// docs/benchmark/summarize-gpu.py compares the two directly.

struct ReplayOptions {
    var clips: [URL] = []
    var gpu = false
    var packOnGPU = true
    var family = "all"
    var runs = 3
    var gammas: [String] = []
    var out: URL?
}

func parseReplayOptions(_ args: [String]) throws -> ReplayOptions {
    var options = ReplayOptions()
    var path: Bool?
    var index = 0
    func value(_ flag: String) throws -> String {
        index += 1
        guard index < args.count else { throw UsageError.message("\(flag) needs a value") }
        return args[index]
    }
    while index < args.count {
        switch args[index] {
        case "--cpu": path = false
        case "--gpu": path = true
        case "--cpu-pack": options.packOnGPU = false
        case "--family": options.family = try value("--family")
        case "--runs":
            guard let runs = Int(try value("--runs")), runs > 0 else { throw UsageError.message("--runs needs a positive number") }
            options.runs = runs
        case "--gamma": options.gammas.append(try value("--gamma"))
        case "--out": options.out = URL(fileURLWithPath: try value("--out"))
        case let flag where flag.hasPrefix("--"): throw UsageError.message("unknown option \(flag)\n\(usage())")
        case let clip: options.clips.append(URL(fileURLWithPath: clip))
        }
        index += 1
    }
    guard let path, !options.clips.isEmpty else { throw UsageError.message(usage()) }
    options.gpu = path
    if options.gammas.isEmpty { options.gammas = ["none"] }
    return options
}

private struct ClipMeta {
    var width: Int
    var height: Int
    var stride: Int
    var frames: Int
    var fps: Double
}

private struct ClipFrame {
    /// Index into Clip.images; a frame without its own .bgra repeats the previous image.
    var image: Int
    /// (x, y, width, height) quads; nil is a full frame, empty is no damage.
    var dirty: [UInt32]?
}

private struct Clip {
    var name: String
    var meta: ClipMeta
    var images: [[UInt8]]
    var frames: [ClipFrame]
}

private func loadClip(_ directory: URL) throws -> Clip {
    let metaData = try Data(contentsOf: directory.appendingPathComponent("meta.json"))
    guard let json = try JSONSerialization.jsonObject(with: metaData) as? [String: Any] else {
        throw UsageError.message("\(directory.path): malformed meta.json")
    }
    func number(_ key: String) throws -> Double {
        guard let value = json[key] as? NSNumber else { throw UsageError.message("\(directory.path): meta.json has no \"\(key)\"") }
        return value.doubleValue
    }
    let meta = ClipMeta(width: Int(try number("width")), height: Int(try number("height")), stride: Int(try number("stride")),
                        frames: Int(try number("frames")), fps: try number("fps"))
    var images: [[UInt8]] = []
    var frames: [ClipFrame] = []
    for index in 0..<meta.frames {
        let name = String(format: "%05d", index)
        let bgra = directory.appendingPathComponent("\(name).bgra")
        if FileManager.default.fileExists(atPath: bgra.path) {
            let data = try Data(contentsOf: bgra)
            guard data.count == meta.stride * meta.height else {
                throw UsageError.message("\(bgra.path): \(data.count) bytes, expected stride*height")
            }
            images.append([UInt8](data))
        } else if images.isEmpty {
            throw UsageError.message("\(bgra.path): the first frame must have pixels")
        }
        var dirty: [UInt32]?
        if let bytes = try? Data(contentsOf: directory.appendingPathComponent("\(name).dirty")) {
            guard bytes.count % 16 == 0 else { throw UsageError.message("frame \(index): .dirty is not whole x,y,w,h quads") }
            dirty = bytes.withUnsafeBytes { raw in
                (0..<(bytes.count / 4)).map { UInt32(littleEndian: raw.loadUnaligned(fromByteOffset: $0 * 4, as: UInt32.self)) }
            }
        }
        frames.append(ClipFrame(image: images.count - 1, dirty: dirty))
    }
    return Clip(name: directory.lastPathComponent, meta: meta, images: images, frames: frames)
}

/// User plus system CPU seconds of the whole process, every thread included.
private func cpuSeconds() -> Double {
    var usage = rusage()
    getrusage(RUSAGE_SELF, &usage)
    let seconds = { (t: timeval) in Double(t.tv_sec) + Double(t.tv_usec) / 1e6 }
    return seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

private struct RunResult {
    var cpuMs: [Double] = []
    var wallMs: [Double] = []
    var gpuMs: [Double] = []
    /// submit, take, plan, encode, pack, then the encoder's own wall time (GPU wait and packing).
    var stages = [Double](repeating: 0, count: 6)
    var totalWallMs = 0.0
    var totals = DLLBenchTotals()
    /// What the encoder held at the end of the run, its body buffer at its largest.
    var gpuBytes = 0
}

private func mean(_ values: [Double]) -> Double {
    values.isEmpty ? 0 : values.reduce(0, +) / Double(values.count)
}

/// The same nearest-rank percentile `replay run` uses.
private func percentile(_ values: [Double], _ p: Double) -> Double {
    guard !values.isEmpty else { return 0 }
    let sorted = values.sorted()
    let rank = min(max(Int((p * Double(sorted.count)).rounded(.up)), 1), sorted.count)
    return sorted[rank - 1]
}

/// An IOSurface-backed frame, as ScreenCaptureKit delivers them, so the GPU path reads through the
/// same zero-copy texture it does live.
private func makeSurface(width: Int, height: Int) throws -> CVPixelBuffer {
    let attributes: [CFString: Any] = [
        kCVPixelBufferIOSurfacePropertiesKey: [CFString: Any](),
        kCVPixelBufferMetalCompatibilityKey: true
    ]
    var buffer: CVPixelBuffer?
    guard CVPixelBufferCreate(nil, width, height, kCVPixelFormatType_32BGRA, attributes as CFDictionary, &buffer) == kCVReturnSuccess,
          let buffer else { throw UsageError.message("could not create an IOSurface-backed pixel buffer") }
    return buffer
}

private func upload(_ image: [UInt8], stride: Int, into surface: CVPixelBuffer) {
    CVPixelBufferLockBaseAddress(surface, [])
    defer { CVPixelBufferUnlockBaseAddress(surface, []) }
    let base = CVPixelBufferGetBaseAddress(surface)!
    let target = CVPixelBufferGetBytesPerRow(surface)
    let row = CVPixelBufferGetWidth(surface) * 4
    image.withUnsafeBytes { source in
        for y in 0..<CVPixelBufferGetHeight(surface) {
            memcpy(base + y * target, source.baseAddress! + y * stride, row)
        }
    }
}

private func runOnce(_ clip: Clip, family: DLLFamily, gamma: [UInt8]?, gpu: Bool, packOnGPU: Bool) throws -> RunResult {
    let m = clip.meta
    let created = gamma.map { table in table.withUnsafeBufferPointer { dll_bench_new(family.rawValue, UInt32(m.width), UInt32(m.height), m.fps, $0.baseAddress) } }
        ?? dll_bench_new(family.rawValue, UInt32(m.width), UInt32(m.height), m.fps, nil)
    guard let bench = created else { throw UsageError.message("the engine refused a \(m.width)x\(m.height) replay head") }
    defer { dll_bench_free(bench) }

    var encoder: MetalStripEncoder?
    var surface: CVPixelBuffer?
    if gpu {
        var profile = DLLEncoderProfile()
        guard dll_bench_encoder_profile(bench, &profile) else { throw UsageError.message("no encoder profile") }
        let gpuEncoder = try MetalStripEncoder(configuration: StripEncoderConfiguration(profile: profile, width: m.width, height: m.height),
                                               packOnGPU: packOnGPU)
        gpuEncoder.setGamma(gamma)
        encoder = gpuEncoder
        surface = try makeSurface(width: m.width, height: m.height)
    }

    var result = RunResult()
    var uploaded = -1
    var empty: UInt32 = 0
    for (index, frame) in clip.frames.enumerated() {
        // Getting the pixels into an IOSurface is the capture's work, not the encoder's.
        if let surface, frame.image != uploaded {
            upload(clip.images[frame.image], stride: m.stride, into: surface)
            uploaded = frame.image
        }
        var step = DLLBenchStep()
        let cpu0 = cpuSeconds()
        let wall0 = DispatchTime.now().uptimeNanoseconds
        var accepted: Bool
        if let encoder, let surface {
            if dll_bench_take_full_request(bench) {
                encoder.requestFullEncode()
            }
            let encodeStart = DispatchTime.now().uptimeNanoseconds
            let strips = try encoder.encode(frame: surface, dirty: frame.dirty)
            result.stages[5] += Double(DispatchTime.now().uptimeNanoseconds - encodeStart) / 1e6
            result.gpuMs.append(strips.gpuSeconds * 1e3)
            accepted = strips.withUnsafeBuffers { indices, hashes, bodies, offsets in
                dll_bench_step_encoded(bench, encoder.configuration.tenBit, strips.count, indices.baseAddress,
                                       hashes.baseAddress, bodies.baseAddress, bodies.count, offsets.baseAddress, &step)
            }
        } else {
            let image = clip.images[frame.image]
            accepted = image.withUnsafeBufferPointer { bgra in
                switch frame.dirty {
                case nil:
                    return dll_bench_step_surface(bench, bgra.baseAddress, bgra.count, m.stride, nil, 0, &step)
                case let rects? where rects.isEmpty:
                    // A non-NULL pointer with no quads: nothing changed, unlike NULL (everything).
                    return dll_bench_step_surface(bench, bgra.baseAddress, bgra.count, m.stride, &empty, 0, &step)
                case let rects?:
                    return rects.withUnsafeBufferPointer {
                        dll_bench_step_surface(bench, bgra.baseAddress, bgra.count, m.stride, $0.baseAddress, rects.count / 4, &step)
                    }
                }
            }
        }
        let wall = Double(DispatchTime.now().uptimeNanoseconds - wall0) / 1e6
        let cpu1 = cpuSeconds()
        guard accepted else { throw UsageError.message("\(clip.name) frame \(index): the engine refused the frame") }
        // Checksumming is the harness's own cost, so it stays outside the measured window.
        dll_bench_account(bench)
        result.cpuMs.append((cpu1 - cpu0) * 1e3)
        result.wallMs.append(wall)
        result.totalWallMs += wall
        let stageNs = [step.submit_ns, step.take_ns, step.plan_ns, step.encode_ns, step.pack_ns]
        for (i, ns) in stageNs.enumerated() { result.stages[i] += Double(ns) / 1e6 }
    }
    dll_bench_totals(bench, &result.totals)
    result.gpuBytes = encoder?.allocatedBytes ?? 0
    return result
}

private func loadGamma(_ path: String) throws -> [UInt8] {
    let bytes = [UInt8](try Data(contentsOf: URL(fileURLWithPath: path)))
    guard bytes.count == 768 else { throw UsageError.message("\(path): \(bytes.count) bytes, expected 768 (R, G, B tables of 256)") }
    return bytes
}

func replay(_ options: ReplayOptions) throws {
    let all: [(String, DLLFamily)] = [("ridge", DLL_FAMILY_RIDGE), ("ella", DLL_FAMILY_ELLA), ("navarro", DLL_FAMILY_NAVARRO)]
    let families = all.filter { options.family == "all" || $0.0 == options.family }
    guard !families.isEmpty else { throw UsageError.message("unknown family '\(options.family)' (ridge, ella, navarro, all)") }
    let path = options.gpu ? "gpu" : "cpu"
    let packing = options.gpu && options.packOnGPU ? "gpu" : "cpu"

    var entries: [String] = []
    for directory in options.clips {
        let clip = try loadClip(directory)
        for gammaArg in options.gammas {
            let gamma = gammaArg == "none" ? nil : try loadGamma(gammaArg)
            let gammaLabel = gammaArg == "none" ? "none" : URL(fileURLWithPath: gammaArg).lastPathComponent
            for (familyName, family) in families {
                var results: [RunResult] = []
                for _ in 0..<options.runs {
                    results.append(try runOnce(clip, family: family, gamma: gamma, gpu: options.gpu, packOnGPU: options.packOnGPU))
                }
                let checksum = results[0].totals.checksum
                guard results.allSatisfy({ $0.totals.checksum == checksum }) else {
                    throw UsageError.message("\(clip.name) \(familyName): checksum differs between runs, replay is not deterministic")
                }
                // The median run by mean CPU is reported whole, as `replay run` does.
                let means = results.map { mean($0.cpuMs) }
                let order = means.indices.sorted { means[$0] < means[$1] }
                let r = results[order[options.runs / 2]]
                let n = Double(clip.frames.count)
                let f = { (v: Double) in String(format: "%.4f", v) }
                let gpuPart = options.gpu
                    ? "\"gpu_ms_per_frame\": {\"mean\": \(f(mean(r.gpuMs))), \"p50\": \(f(percentile(r.gpuMs, 0.5))), \"p95\": \(f(percentile(r.gpuMs, 0.95)))},\n     "
                        + "\"packing\": \"\(packing)\", \"gpu_memory_bytes\": \(results.map(\.gpuBytes).max() ?? 0),\n     "
                    : ""
                let t = r.totals
                entries.append("""
                    {"clip": "\(clip.name)", "family": "\(familyName)", "gamma": "\(gammaLabel)", "path": "\(path)", \
                "frames": \(clip.frames.count), "width": \(clip.meta.width), "height": \(clip.meta.height), \
                "fps": \(clip.meta.fps.rounded() == clip.meta.fps ? String(Int(clip.meta.fps)) : String(clip.meta.fps)),
                     "checksum": "\(String(format: "%016llx", t.checksum))", "record_bytes": \(t.record_bytes), \
                "sent_bytes": \(t.sent_bytes), "presents": \(t.presents), "skipped": \(t.skipped),
                     "wall_ms_total": \(String(format: "%.3f", r.totalWallMs)), \
                "wall_ms_per_frame": {"mean": \(f(mean(r.wallMs))), "p50": \(f(percentile(r.wallMs, 0.5))), "p95": \(f(percentile(r.wallMs, 0.95)))},
                     "cpu_ms_per_frame": {"mean": \(f(mean(r.cpuMs))), "p50": \(f(percentile(r.cpuMs, 0.5))), "p95": \(f(percentile(r.cpuMs, 0.95)))},
                     \(gpuPart)"stage_ms_per_frame": {"submit": \(f(r.stages[0] / n)), "take": \(f(r.stages[1] / n)), \
                "plan": \(f(r.stages[2] / n)), "encode": \(f(r.stages[3] / n)), "pack": \(f(r.stages[4] / n)), \
                "gpu_encoder": \(f(r.stages[5] / n))},
                     "run_cpu_means": [\(means.map(f).joined(separator: ", "))]}
                """)
                let gpuNote = options.gpu
                    ? String(format: ", gpu %.3f ms/frame, %@-packed, %.1f MB on the GPU", mean(r.gpuMs), packing,
                             Double(results.map(\.gpuBytes).max() ?? 0) / 1e6)
                    : ""
                FileHandle.standardError.write(Data(String(
                    format: "%@ %@ gamma=%@ %@: cpu %.3f ms/frame%@, checksum %016llx\n",
                    clip.name, familyName, gammaLabel, path, mean(r.cpuMs), gpuNote, t.checksum).utf8))
            }
        }
    }
    let tool = "linklite-bench replay --\(path)" + (options.gpu && !options.packOnGPU ? " --cpu-pack" : "")
    let json = "{\n  \"schema\": 1,\n  \"tool\": \"\(tool)\",\n  \"runs\": \(options.runs),\n  \"results\": [\n"
        + entries.joined(separator: ",\n") + "\n  ]\n}\n"
    if let out = options.out {
        try json.write(to: out, atomically: true, encoding: .utf8)
    } else {
        print(json, terminator: "")
    }
}
