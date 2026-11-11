// SPDX-License-Identifier: GPL-2.0-or-later

import CoreGraphics
import CoreMedia
import CoreVideo
import Foundation
@preconcurrency import ScreenCaptureKit

// Records a display into the clip format docs/BENCHMARK.md describes, and replays clips through
// the engine's pixel path offline, fed by the CPU or the Metal strip encoder. Neither needs the
// dock, so both run with DisplayLink Manager holding it.

func usage() -> String {
    """
    usage: linklite-bench record <dir> [--display main|<displayID>] [--frames 300] [--fps 60]
           linklite-bench replay <clip>... --cpu|--gpu [--family ridge|ella|navarro|all] [--runs 3]
                                 [--gamma <file|none>]... [--cpu-pack] [--out file]

    --cpu-pack keeps the GPU transform but codes the bits on the CPU (the Phase 1 path).
    record needs the Screen Recording permission for the terminal that runs it.
    """
}

struct Options {
    var directory: URL
    var displayID = CGMainDisplayID()
    var frames = 300
    var fps = 60
}

enum UsageError: Error, CustomStringConvertible {
    case message(String)
    var description: String {
        switch self {
        case .message(let text): text
        }
    }
}

func parseOptions(_ args: [String]) throws -> Options {
    guard args.first == "record", args.count >= 2 else { throw UsageError.message(usage()) }
    var options = Options(directory: URL(fileURLWithPath: args[1]))
    var index = 2
    func value(_ flag: String) throws -> String {
        index += 1
        guard index < args.count else { throw UsageError.message("\(flag) needs a value") }
        return args[index]
    }
    while index < args.count {
        switch args[index] {
        case "--display":
            let text = try value("--display")
            if text != "main" {
                guard let id = UInt32(text) else { throw UsageError.message("--display needs main or a display ID") }
                options.displayID = id
            }
        case "--frames":
            guard let count = Int(try value("--frames")), count > 0 else { throw UsageError.message("--frames needs a positive number") }
            options.frames = count
        case "--fps":
            guard let fps = Int(try value("--fps")), fps > 0 else { throw UsageError.message("--fps needs a positive number") }
            options.fps = fps
        default:
            throw UsageError.message("unknown option \(args[index])\n\(usage())")
        }
        index += 1
    }
    return options
}

/// Writes every complete frame as it arrives. The callback runs on one serial queue, so the
/// counters need no locking; `finished` is resumed once, from that queue.
final class Recorder: NSObject, SCStreamOutput, SCStreamDelegate, @unchecked Sendable {
    private let directory: URL
    private let width: Int
    private let height: Int
    private let frames: Int
    private let fps: Int
    private var written = 0
    private var failure: Error?
    private var continuation: CheckedContinuation<Void, Never>?
    private var dirtyBytes: [UInt32] = []

    init(directory: URL, width: Int, height: Int, frames: Int, fps: Int) {
        self.directory = directory
        self.width = width
        self.height = height
        self.frames = frames
        self.fps = fps
    }

    func run(display: SCDisplay) async throws {
        let config = SCStreamConfiguration()
        config.width = width
        config.height = height
        config.pixelFormat = kCVPixelFormatType_32BGRA
        config.showsCursor = false
        config.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(fps))
        config.queueDepth = 3
        config.colorSpaceName = CGColorSpace.sRGB
        let stream = SCStream(filter: SCContentFilter(display: display, excludingWindows: []),
                              configuration: config, delegate: self)
        let queue = DispatchQueue(label: "linklite-bench.capture", qos: .userInteractive)
        try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: queue)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        await withCheckedContinuation { (done: CheckedContinuation<Void, Never>) in
            queue.sync { continuation = done }
            Task {
                do { try await stream.startCapture() } catch {
                    queue.async { self.finish(error) }
                }
            }
        }
        try? await stream.stopCapture()
        if let failure { throw failure }
        let meta = "{\"width\": \(width), \"height\": \(height), \"stride\": \(width * 4), \"frames\": \(written), \"fps\": \(fps)}\n"
        try meta.write(to: directory.appendingPathComponent("meta.json"), atomically: true, encoding: .utf8)
    }

    private func finish(_ error: Error?) {
        failure = failure ?? error
        continuation?.resume()
        continuation = nil
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        finish(error)
    }

    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of type: SCStreamOutputType) {
        guard continuation != nil, type == .screen,
              let attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, createIfNecessary: false)
                as? [[SCStreamFrameInfo: Any]],
              let rawStatus = attachments.first?[.status] as? Int,
              SCFrameStatus(rawValue: rawStatus) == .complete,
              let pixelBuffer = CMSampleBufferGetImageBuffer(sampleBuffer),
              CVPixelBufferGetWidth(pixelBuffer) == width, CVPixelBufferGetHeight(pixelBuffer) == height
        else { return }

        // Same rule as DisplayCapture: the report is in output pixels only while the stream is 1:1,
        // and the first frame carries no useful one, so those frames get no .dirty file (a full frame).
        let info = attachments.first ?? [:]
        let unscaled = (info[.contentScale] as? CGFloat).map { abs($0 - 1) < 0.001 } ?? false
        var dirty: [UInt32]?
        if written > 0, unscaled, let reported = info[.dirtyRects] as? [NSDictionary] {
            dirtyBytes.removeAll(keepingCapacity: true)
            for entry in reported {
                guard let rect = CGRect(dictionaryRepresentation: entry)?.integral, !rect.isEmpty else { continue }
                dirtyBytes += [UInt32(max(0, rect.minX)), UInt32(max(0, rect.minY)),
                               UInt32(rect.width), UInt32(rect.height)]
            }
            dirty = dirtyBytes
        }

        guard CVPixelBufferLockBaseAddress(pixelBuffer, .readOnly) == kCVReturnSuccess else { return }
        defer { CVPixelBufferUnlockBaseAddress(pixelBuffer, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(pixelBuffer) else { return }
        // Rows go out tightly packed: the buffer's own row padding is a capture detail the clip
        // format has no use for, and its last row may not even be padded.
        let rowBytes = width * 4
        let sourceStride = CVPixelBufferGetBytesPerRow(pixelBuffer)
        var image = Data(count: rowBytes * height)
        image.withUnsafeMutableBytes { target in
            for y in 0..<height {
                memcpy(target.baseAddress! + y * rowBytes, base + y * sourceStride, rowBytes)
            }
        }
        let name = String(format: "%05d", written)
        do {
            try image.write(to: directory.appendingPathComponent("\(name).bgra"))
            if let dirty {
                try dirty.withUnsafeBufferPointer { Data(buffer: $0) }
                    .write(to: directory.appendingPathComponent("\(name).dirty"))
            }
        } catch {
            finish(error)
            return
        }
        written += 1
        if written >= frames { finish(nil) }
    }
}

func record(_ options: Options) async throws {
    // A pixel-size stream of the display, like DisplayCapture's dock-native size, so contentScale is 1.
    guard let mode = CGDisplayCopyDisplayMode(options.displayID) else {
        throw UsageError.message("display \(options.displayID) not found")
    }
    let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
    guard let display = content.displays.first(where: { $0.displayID == options.displayID }) else {
        throw UsageError.message("display \(options.displayID) is not shareable (is Screen Recording allowed?)")
    }
    let recorder = Recorder(directory: options.directory, width: mode.pixelWidth, height: mode.pixelHeight,
                            frames: options.frames, fps: options.fps)
    try await recorder.run(display: display)
    print("recorded \(options.frames) frames of display \(options.displayID) to \(options.directory.path)")
}

do {
    let arguments = Array(CommandLine.arguments.dropFirst())
    if arguments.first == "replay" {
        try replay(try parseReplayOptions(Array(arguments.dropFirst())))
    } else {
        try await record(try parseOptions(arguments))
    }
} catch {
    FileHandle.standardError.write(Data("linklite-bench: \(error)\n".utf8))
    exit(1)
}
