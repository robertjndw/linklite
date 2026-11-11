// SPDX-License-Identifier: GPL-2.0-or-later

import CoreGraphics
import CoreMedia
import CoreVideo
import Foundation
import LinkLiteGPU
import os
@preconcurrency import ScreenCaptureKit

private let log = Logger(subsystem: "com.ball.linklite", category: "capture")

/// Captures one virtual display with ScreenCaptureKit at the dock's native mode and hands every
/// complete frame to the engine.
final class DisplayCapture: NSObject, SCStreamOutput, SCStreamDelegate, @unchecked Sendable {
    enum CaptureError: Error, CustomStringConvertible {
        case displayNotShareable(CGDirectDisplayID)

        var description: String {
            switch self {
            case .displayNotShareable(let id):
                "Display \(id) never appeared in ScreenCaptureKit's shareable content"
            }
        }
    }

    let head: UInt8
    let width: UInt32
    let height: UInt32
    private let engine: EngineHandle
    private let queue: DispatchQueue
    private let onStop: @Sendable (Error) -> Void
    /// Set once in start() before capture begins, read afterwards; never mutated concurrently.
    private var stream: SCStream?
    private var loggedSizeMismatch = false
    /// Set once the stream's first frame has been sent whole; touched only on the capture queue.
    private var sentFirstFrame = false
    /// Reused on the serial capture queue; the engine only borrows it during submitFrame.
    private var dirtyRects: [UInt32] = []
    /// `defaults write com.ball.linklite GPUEncoder -bool false` codes strips on the CPU, for A/B
    /// runs. Read when capture starts.
    private static let gpuEncoderKey = "GPUEncoder"
    /// `defaults write com.ball.linklite GPUPacking -bool false` keeps the GPU's transform but codes
    /// the bits on the CPU with the engine's coder, for A/B runs.
    private static let gpuPackingKey = "GPUPacking"
    /// Codes this head's strips on the GPU; nil means frames go to the engine as BGRA. Set before
    /// the stream starts, afterwards touched only on the capture queue, and dropped for the rest
    /// of the session on the first GPU failure.
    private var encoder: MetalStripEncoder?
    private var encoderTenBit = false
    /// Submissions the engine refused in a row; a few mean the encoder no longer fits the head.
    private var refusedSubmissions = 0
    /// Polls the engine's full-encode request while the screen is still, when no frame would.
    private var fullRequestTimer: DispatchSourceTimer?

    private init(head: UInt8, width: UInt32, height: UInt32, engine: EngineHandle,
                 onStop: @escaping @Sendable (Error) -> Void) {
        self.head = head
        self.width = width
        self.height = height
        self.engine = engine
        self.onStop = onStop
        queue = DispatchQueue(label: "com.ball.linklite.capture.head\(head)", qos: .userInteractive)
    }

    /// Finds `displayID` in the shareable content (a freshly created virtual display can take a
    /// moment to show up) and starts streaming it at the monitor's native size.
    /// `onStop` runs if the stream later dies on its own.
    static func start(displayID: CGDirectDisplayID, monitor: MonitorInfo, engine: EngineHandle,
                      onStop: @escaping @Sendable (Error) -> Void) async throws -> DisplayCapture {
        let display = try await shareableDisplay(displayID)
        let capture = DisplayCapture(head: monitor.head, width: monitor.width, height: monitor.height,
                                     engine: engine, onStop: onStop)
        capture.setUpEncoder()
        try await capture.begin(display: display, refreshHz: monitor.effectiveRefreshHz,
                                drawsCursor: !monitor.hardwareCursor)
        capture.queue.async { capture.startFullRequestTimer(refreshHz: monitor.effectiveRefreshHz) }
        return capture
    }

    private static func shareableDisplay(_ displayID: CGDirectDisplayID) async throws -> SCDisplay {
        for attempt in 1...20 {
            let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
            if let display = content.displays.first(where: { $0.displayID == displayID }) {
                if attempt > 1 { log.info("Display \(displayID) shareable after \(attempt) attempts") }
                return display
            }
            try await Task.sleep(for: .milliseconds(250))
        }
        throw CaptureError.displayNotShareable(displayID)
    }

    private func begin(display: SCDisplay, refreshHz: UInt32, drawsCursor: Bool) async throws {
        let filter = SCContentFilter(display: display, excludingWindows: [])
        let config = SCStreamConfiguration()
        // The dock's mode, not the macOS mode: a HiDPI backing store gets scaled to the panel.
        config.width = Int(width)
        config.height = Int(height)
        config.pixelFormat = kCVPixelFormatType_32BGRA
        // A dock with a hardware cursor draws the pointer itself (CursorTracker); leaving it out of
        // the frames means pointer movement sends no video at all.
        config.showsCursor = drawsCursor
        config.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(refreshHz))
        config.queueDepth = 3
        config.colorSpaceName = CGColorSpace.sRGB

        let stream = SCStream(filter: filter, configuration: config, delegate: self)
        try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: queue)
        self.stream = stream
        try await stream.startCapture()
        log.info("Capturing display \(display.displayID) for head \(self.head) at \(self.width)x\(self.height)@\(refreshHz)")
    }

    /// Before the stream starts, so no frame can race it: Metal setup happens off the main thread.
    private func setUpEncoder() {
        guard UserDefaults.standard.object(forKey: Self.gpuEncoderKey) as? Bool ?? true else {
            log.info("Head \(self.head): GPU encoder turned off by the GPUEncoder default; coding on the CPU")
            return
        }
        guard let profile = engine.encoderProfile(head: head) else {
            log.info("Head \(self.head): the engine offers no strip profile; coding on the CPU")
            return
        }
        do {
            let configuration = StripEncoderConfiguration(profile: profile, width: Int(width), height: Int(height))
            let packOnGPU = UserDefaults.standard.object(forKey: Self.gpuPackingKey) as? Bool ?? true
            let encoder = try MetalStripEncoder(configuration: configuration, packOnGPU: packOnGPU)
            self.encoder = encoder
            encoderTenBit = profile.ten_bit
            log.info("Head \(self.head): coding strips on the GPU (\(encoder.device.name, privacy: .public)), bits packed on the \(packOnGPU ? "GPU" : "CPU", privacy: .public)")
        } catch {
            log.error("Head \(self.head): GPU encoder unavailable, coding on the CPU: \(String(describing: error), privacy: .public)")
        }
    }

    /// On the capture queue, once the stream runs: a stream that failed to start leaves no timer.
    private func startFullRequestTimer(refreshHz: UInt32) {
        guard encoder != nil, fullRequestTimer == nil else { return }
        // Every refresh: the engine raises the request on its own (a gamma change, a missing body)
        // and nothing else would notice while capture delivers no frames.
        let timer = DispatchSource.makeTimerSource(queue: queue)
        let period = DispatchTimeInterval.nanoseconds(Int(1_000_000_000 / max(refreshHz, 1)))
        timer.schedule(deadline: .now() + period, repeating: period, leeway: period)
        timer.setEventHandler { [weak self] in self?.serviceFullRequest() }
        timer.resume()
        fullRequestTimer = timer
    }

    /// The display's gamma table (768 bytes, R, G, B) for the GPU encoder; nil is the identity.
    /// The engine applies it itself to BGRA frames, so the CPU path needs nothing here.
    func setGamma(_ lut: [UInt8]?) {
        queue.async { [weak self] in self?.encoder?.setGamma(lut) }
    }

    private func serviceFullRequest() {
        guard let encoder else { return }
        if engine.takeFullRequest(head: head) {
            encoder.requestFullEncode()
        }
        guard encoder.needsFullEncode, encoder.hasFrame else { return }
        do {
            if let strips = try encoder.encodeFromShadow() {
                try submit(strips)
            }
        } catch {
            dropEncoder(error, resubmitShadow: true)
        }
    }

    private func submit(_ strips: EncodedStrips) throws {
        engine.addGPUTime(head: head, seconds: strips.gpuSeconds)
        if engine.submitEncoded(head: head, width: width, height: height, tenBit: encoderTenBit, strips: strips) {
            refusedSubmissions = 0
            return
        }
        // The GPU already counts these strips as sent, so only a full encode can resync it.
        encoder?.requestFullEncode()
        refusedSubmissions += 1
        if refusedSubmissions >= 3 {
            throw MetalStripEncoderError.gpu("the engine refused \(refusedSubmissions) submissions in a row")
        }
    }

    /// With `resubmitShadow`, also hands the engine the last frame the GPU saw, as BGRA. Without
    /// it a still screen would leave the engine's CPU feed with no surface at all until the next
    /// capture frame, which may never come; the frame callback passes false because it submits
    /// its own frame right after.
    private func dropEncoder(_ error: Error, resubmitShadow: Bool = false) {
        log.error("Head \(self.head): GPU encoder failed, coding on the CPU for the rest of the session: \(String(describing: error), privacy: .public)")
        let last = resubmitShadow ? encoder?.shadowBGRA() : nil
        encoder = nil
        fullRequestTimer?.cancel()
        fullRequestTimer = nil
        guard let last else { return }
        // Possibly a mix of two frames if the GPU died mid-copy. The next capture frame replaces it.
        last.withUnsafeBufferPointer { bgra in
            engine.submitFrame(head: head, bgra: bgra.baseAddress!, stride: Int(width) * 4, width: width, height: height,
                               dirty: nil)
        }
        sentFirstFrame = true
    }

    func stop() async {
        queue.async { [weak self] in
            self?.fullRequestTimer?.cancel()
            self?.fullRequestTimer = nil
        }
        guard let stream else { return }
        do {
            try await stream.stopCapture()
        } catch {
            log.debug("stopCapture for head \(self.head): \(error.localizedDescription, privacy: .public)")
        }
    }

    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .screen,
              let attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, createIfNecessary: false)
                as? [[SCStreamFrameInfo: Any]],
              let rawStatus = attachments.first?[.status] as? Int,
              SCFrameStatus(rawValue: rawStatus) == .complete,
              let pixelBuffer = CMSampleBufferGetImageBuffer(sampleBuffer)
        else { return }

        let frameWidth = UInt32(CVPixelBufferGetWidth(pixelBuffer))
        let frameHeight = UInt32(CVPixelBufferGetHeight(pixelBuffer))
        if frameWidth != width || frameHeight != height {
            if !loggedSizeMismatch {
                loggedSizeMismatch = true
                log.error("Head \(self.head) got \(frameWidth)x\(frameHeight) frames, expected \(self.width)x\(self.height)")
            }
            return
        }

        // ScreenCaptureKit reports what changed, in output-frame pixels (measured: a 62x60 menu bar
        // item on a 3840x2160 stream of a 1920x1080-point display). That holds only while the
        // display is captured 1:1; when macOS renders it at another size and the stream scales it
        // ("looks like" 2560x1440), the whole frame is sent. The first frame of a stream carries
        // no useful report, so it is sent whole too.
        let info = attachments.first ?? [:]
        let unscaled = (info[.contentScale] as? CGFloat).map { abs($0 - 1) < 0.001 } ?? false
        var dirty: [UInt32]?
        if sentFirstFrame, unscaled, let reported = info[.dirtyRects] as? [NSDictionary] {
            dirtyRects.removeAll(keepingCapacity: true)
            dirtyRects.reserveCapacity(reported.count * 4)
            for entry in reported {
                guard let rect = CGRect(dictionaryRepresentation: entry)?.integral, !rect.isEmpty else { continue }
                dirtyRects.append(UInt32(max(0, rect.minX)))
                dirtyRects.append(UInt32(max(0, rect.minY)))
                dirtyRects.append(UInt32(rect.width))
                dirtyRects.append(UInt32(rect.height))
            }
            dirty = dirtyRects
        }

        if let encoder {
            do {
                if engine.takeFullRequest(head: head) {
                    encoder.requestFullEncode()
                }
                // The encoder waits for its command buffer, so the sample buffer, which owns the
                // IOSurface the GPU reads, outlives every use of it.
                let strips = try withExtendedLifetime(sampleBuffer) {
                    try encoder.encode(frame: pixelBuffer, dirty: dirty)
                }
                try submit(strips)
                sentFirstFrame = true
                return
            } catch {
                // The engine starts its mailbox over when the BGRA frame below arrives, so that
                // frame has to be whole: its dirty rects only describe the change since the last.
                dropEncoder(error)
                dirty = nil
            }
        }

        guard CVPixelBufferLockBaseAddress(pixelBuffer, .readOnly) == kCVReturnSuccess else { return }
        defer { CVPixelBufferUnlockBaseAddress(pixelBuffer, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(pixelBuffer) else { return }
        engine.submitFrame(head: head, bgra: base.assumingMemoryBound(to: UInt8.self),
                           stride: CVPixelBufferGetBytesPerRow(pixelBuffer),
                           width: frameWidth, height: frameHeight, dirty: dirty)
        sentFirstFrame = true
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        log.error("Capture for head \(self.head) stopped: \(error.localizedDescription, privacy: .public)")
        onStop(error)
    }
}
