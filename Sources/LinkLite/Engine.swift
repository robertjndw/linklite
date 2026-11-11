// SPDX-License-Identifier: GPL-2.0-or-later

import CDLLite
import Foundation
import LinkLiteGPU

/// A dock monitor as reported by the engine, copied out of the callback's borrowed DLLMonitor.
struct MonitorInfo: Sendable, Equatable {
    var head: UInt8
    /// The dock's programmed mode; every submitted frame must be exactly this size.
    var width: UInt32
    var height: UInt32
    var refreshHz: UInt32
    var widthMillimeters: UInt32
    var heightMillimeters: UInt32
    var vendorID: UInt16
    var productCode: UInt16
    var serial: UInt32
    var name: String
    /// The dock draws the pointer; capture must leave it out (see CursorTracker).
    var hardwareCursor: Bool

    init(_ monitor: DLLMonitor) {
        head = monitor.head
        width = monitor.width
        height = monitor.height
        refreshHz = monitor.refresh_hz
        widthMillimeters = monitor.width_mm
        heightMillimeters = monitor.height_mm
        vendorID = monitor.vendor_id
        productCode = monitor.product_code
        serial = monitor.serial
        let reported = monitor.name.map { String(cString: $0) } ?? ""
        name = reported.isEmpty ? "DisplayLink Monitor \(Int(monitor.head) + 1)" : reported
        hardwareCursor = monitor.hardware_cursor
    }

    /// Whether `other` is the same physical monitor in the same mode, so its virtual display can
    /// be reused as is.
    func sameDisplay(as other: MonitorInfo) -> Bool {
        head == other.head && vendorID == other.vendorID && productCode == other.productCode
            && serial == other.serial && width == other.width && height == other.height
            && refreshHz == other.refreshHz
    }

    /// Refresh rate to offer macOS and to pace capture at; 60 Hz if the engine reports none.
    var effectiveRefreshHz: UInt32 { refreshHz > 0 ? refreshHz : 60 }
}

enum EngineEvent: Sendable {
    case attached(MonitorInfo)
    case detached(head: UInt8)
    case stateChanged(DLLState, message: String)
}

extension DLLState: @retroactive @unchecked Sendable {}

/// Owns the engine pointer. Frame submission (capture queues) and stop (main thread) may race, so
/// every use of the pointer holds a read lock and stop takes the write lock before freeing it.
final class EngineHandle: @unchecked Sendable {
    /// Retained context handed to the C callbacks; released once dll_engine_stop guarantees no
    /// further callbacks.
    private final class Bridge {
        let deliver: @Sendable (EngineEvent) -> Void
        init(deliver: @escaping @Sendable (EngineEvent) -> Void) { self.deliver = deliver }
    }

    private let lock: UnsafeMutablePointer<pthread_rwlock_t>
    private var engine: OpaquePointer?
    private var bridge: Unmanaged<Bridge>?

    init() {
        lock = .allocate(capacity: 1)
        pthread_rwlock_init(lock, nil)
    }

    deinit {
        stop()
        pthread_rwlock_destroy(lock)
        lock.deallocate()
    }

    /// Starts the engine thread. Events arrive in order on the main queue.
    func start(onEvent: @escaping @MainActor (EngineEvent) -> Void) -> Bool {
        pthread_rwlock_wrlock(lock)
        defer { pthread_rwlock_unlock(lock) }
        guard engine == nil else { return true }

        let bridge = Unmanaged.passRetained(Bridge { event in
            // DispatchQueue.main keeps attach/detach ordering, which Task hops do not promise.
            DispatchQueue.main.async { MainActor.assumeIsolated { onEvent(event) } }
        })
        let callbacks = DLLCallbacks(
            context: bridge.toOpaque(),
            monitor_attached: { context, monitor in
                guard let context, let monitor else { return }
                let info = MonitorInfo(monitor.pointee)
                Unmanaged<Bridge>.fromOpaque(context).takeUnretainedValue().deliver(.attached(info))
            },
            monitor_detached: { context, head in
                guard let context else { return }
                Unmanaged<Bridge>.fromOpaque(context).takeUnretainedValue().deliver(.detached(head: head))
            },
            state_changed: { context, state, message in
                guard let context else { return }
                let text = message.map { String(cString: $0) } ?? ""
                Unmanaged<Bridge>.fromOpaque(context).takeUnretainedValue()
                    .deliver(.stateChanged(state, message: text))
            }
        )
        guard let started = dll_engine_start(callbacks) else {
            bridge.release()
            return false
        }
        engine = started
        self.bridge = bridge
        return true
    }

    /// Blanks every head, releases the dock, and joins the engine thread. Safe to call repeatedly.
    func stop() {
        pthread_rwlock_wrlock(lock)
        let engine = self.engine
        let bridge = self.bridge
        self.engine = nil
        self.bridge = nil
        pthread_rwlock_unlock(lock)

        guard let engine else { return }
        dll_engine_stop(engine)
        bridge?.release()
    }

    /// Hands the engine a BGRA frame; the engine copies the changed pixels before returning.
    /// `dirty` holds (x, y, width, height) quads in frame pixels, or is nil for the whole frame.
    func submitFrame(head: UInt8, bgra: UnsafePointer<UInt8>, stride: Int, width: UInt32, height: UInt32,
                     dirty: [UInt32]?) {
        pthread_rwlock_rdlock(lock)
        defer { pthread_rwlock_unlock(lock) }
        guard let engine else { return }
        if let dirty {
            if dirty.isEmpty {
                // NULL means unknown damage, not an empty report. Keep a non-NULL sentinel
                // even when Swift chooses a NULL base address for an empty array.
                var empty: UInt32 = 0
                withUnsafePointer(to: &empty) {
                    dll_engine_submit_frame(engine, head, bgra, stride, width, height, $0, 0)
                }
            } else {
                dirty.withUnsafeBufferPointer {
                    dll_engine_submit_frame(engine, head, bgra, stride, width, height, $0.baseAddress, dirty.count / 4)
                }
            }
        } else {
            dll_engine_submit_frame(engine, head, bgra, stride, width, height, nil, 0)
        }
    }

    /// What a strip producer for `head` must match; nil while the head is not attached.
    func encoderProfile(head: UInt8) -> DLLEncoderProfile? {
        pthread_rwlock_rdlock(lock)
        defer { pthread_rwlock_unlock(lock) }
        guard let engine else { return nil }
        var profile = DLLEncoderProfile()
        return dll_engine_encoder_profile(engine, head, &profile) ? profile : nil
    }

    /// Hands the engine strips the GPU encoder coded; false when the engine refused them all.
    func submitEncoded(head: UInt8, width: UInt32, height: UInt32, tenBit: Bool, strips: EncodedStrips) -> Bool {
        pthread_rwlock_rdlock(lock)
        defer { pthread_rwlock_unlock(lock) }
        guard let engine else { return false }
        return strips.withUnsafeBuffers { indices, hashes, bodies, offsets in
            dll_engine_submit_encoded(engine, head, width, height, tenBit, strips.count, indices.baseAddress,
                                      hashes.baseAddress, bodies.baseAddress, bodies.count, offsets.baseAddress)
        }
    }

    /// Whether the engine wants every strip of `head` coded again; clears the request.
    func takeFullRequest(head: UInt8) -> Bool {
        pthread_rwlock_rdlock(lock)
        defer { pthread_rwlock_unlock(lock) }
        guard let engine else { return false }
        return dll_engine_take_full_request(engine, head)
    }

    func addGPUTime(head: UInt8, seconds: Double) {
        pthread_rwlock_rdlock(lock)
        defer { pthread_rwlock_unlock(lock) }
        guard let engine, seconds > 0 else { return }
        dll_engine_add_gpu_time(engine, head, UInt64((seconds * 1e6).rounded()))
    }

    func setDisplayPower(head: UInt8, on: Bool) {
        pthread_rwlock_rdlock(lock)
        defer { pthread_rwlock_unlock(lock) }
        guard let engine else { return }
        dll_engine_set_display_power(engine, head, on)
    }

    /// Sets a head's gamma table: 768 bytes, 256 each for R, G, B. nil clears it.
    func setGamma(head: UInt8, lut: [UInt8]?) {
        pthread_rwlock_rdlock(lock)
        defer { pthread_rwlock_unlock(lock) }
        guard let engine else { return }
        guard let lut, lut.count == 768 else {
            dll_engine_set_gamma(engine, head, nil, nil, nil)
            return
        }
        lut.withUnsafeBufferPointer {
            let base = $0.baseAddress!
            dll_engine_set_gamma(engine, head, base, base + 256, base + 512)
        }
    }

    /// Sets the dock cursor image: premultiplied BGRA, `width * height * 4` bytes.
    func setCursor(bgra: [UInt8], width: UInt32, height: UInt32, hotX: Int32, hotY: Int32) {
        pthread_rwlock_rdlock(lock)
        defer { pthread_rwlock_unlock(lock) }
        guard let engine else { return }
        bgra.withUnsafeBufferPointer { dll_engine_set_cursor(engine, $0.baseAddress, width, height, hotX, hotY) }
    }

    /// Reports the pointer's hotspot on `head` (dock mode pixels), or on no head when `head < 0`.
    func moveCursor(head: Int32, x: Int32, y: Int32) {
        pthread_rwlock_rdlock(lock)
        defer { pthread_rwlock_unlock(lock) }
        guard let engine else { return }
        dll_engine_move_cursor(engine, head, x, y)
    }

    /// Performance counters accumulated since the previous call, which resets them: engine-wide,
    /// and one entry per attached head. Nil while the engine is not running.
    func stats() -> (engine: DLLEngineStats, heads: [DLLHeadStats])? {
        pthread_rwlock_rdlock(lock)
        defer { pthread_rwlock_unlock(lock) }
        guard let engine else { return nil }
        var totals = DLLEngineStats()
        var heads = [DLLHeadStats](repeating: DLLHeadStats(), count: Int(DLL_MAX_HEADS))
        let count = heads.withUnsafeMutableBufferPointer {
            dll_engine_stats(engine, &totals, $0.baseAddress, $0.count)
        }
        return (totals, Array(heads.prefix(count)))
    }
}
