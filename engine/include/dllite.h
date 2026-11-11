// SPDX-License-Identifier: GPL-2.0-or-later
//
// C ABI between the Rust dock engine (libdllite_engine.a) and the Swift display host.
//
// Threading: callbacks run on the engine's own thread. Functions other than dll_engine_stop may
// be called concurrently from any thread while the engine is alive. Stop must be serialized
// against all other calls and must not run from a callback. Strings and buffers passed to
// callbacks are valid only for the callback's duration; copy what you keep.

#ifndef DLLITE_H
#define DLLITE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct DLLEngine DLLEngine;

typedef enum {
    /// No dock found, or the dock is owned by another process (e.g. DisplayLink Manager).
    DLL_STATE_SEARCHING = 0,
    /// Encrypted session established; monitors are reported through monitor_attached.
    DLL_STATE_ENGAGED = 1,
    /// The session failed; the engine tears down every head and retries.
    DLL_STATE_RETRYING = 2,
} DLLState;

typedef struct {
    /// Dock connector index, 0-based. Stable for the life of the attachment.
    uint8_t head;
    /// Mode the dock was programmed with. Frames for this head must be exactly this size.
    uint32_t width;
    uint32_t height;
    uint32_t refresh_hz;
    /// Physical size from the EDID, 0 when the monitor does not report one.
    uint32_t width_mm;
    uint32_t height_mm;
    /// Raw big-endian EDID manufacturer id (bytes 8..9), product code, and serial number.
    uint16_t vendor_id;
    uint16_t product_code;
    uint32_t serial;
    /// NUL-terminated UTF-8 product name, e.g. "HP Z27k G3".
    const char *name;
    const uint8_t *edid;
    size_t edid_len;
    /// The dock draws the pointer itself. Capture without the cursor and report it through
    /// dll_engine_set_cursor / dll_engine_move_cursor; otherwise draw it into the frames.
    bool hardware_cursor;
} DLLMonitor;

typedef struct {
    void *context;
    /// A monitor is lit and accepting frames at monitor->width x monitor->height.
    void (*monitor_attached)(void *context, const DLLMonitor *monitor);
    /// The head is gone (unplugged, or the session is being rebuilt). Stop submitting frames.
    void (*monitor_detached)(void *context, uint8_t head);
    /// Engine state changed. message is human-readable and NUL-terminated.
    void (*state_changed)(void *context, DLLState state, const char *message);
} DLLCallbacks;

/// Start the engine thread. It searches for the dock, engages it, and reports monitors.
/// Returns NULL only if the thread could not be spawned.
DLLEngine *dll_engine_start(DLLCallbacks callbacks);

/// Hand the engine the newest frame for a head: BGRA8888, top row first, stride in bytes.
/// `dirty` lists the areas that changed since the previous frame as dirty_count (x, y, width,
/// height) quads in frame pixels; pass NULL when that is unknown (the whole frame is read).
/// A non-NULL dirty pointer with dirty_count == 0 means no changed areas.
/// For valid nonempty geometry, bgra must contain at least stride * (height - 1) + width * 4
/// readable bytes, and dirty, unless NULL, must contain dirty_count * 4 aligned uint32_t values.
/// Both buffers must remain valid and unmodified until this returns. The changed pixels are
/// copied before returning. Frames with invalid or unrepresentable lengths, or whose size
/// differs from the head's programmed mode, are dropped.
void dll_engine_submit_frame(DLLEngine *engine, uint8_t head, const uint8_t *bgra, size_t stride,
                             uint32_t width, uint32_t height, const uint32_t *dirty,
                             size_t dirty_count);

/// Power a head's monitor down (false) or back up (true), e.g. on display sleep and wake.
void dll_engine_set_display_power(DLLEngine *engine, uint8_t head, bool on);

/// Set a head's gamma transfer table: r, g and b each point to 256 bytes mapping a captured
/// channel value to the value sent to the monitor. All three NULL (or an identity table) clears it.
/// A different table repaints the head's whole picture, even with no new frame.
void dll_engine_set_gamma(DLLEngine *engine, uint8_t head, const uint8_t *r, const uint8_t *g,
                          const uint8_t *b);

/// Set the pointer image the dock draws: BGRA8888 (premultiplied), top row first, tightly packed,
/// at most 64x64. hot_x/hot_y is the hotspot within the image, in image pixels.
void dll_engine_set_cursor(DLLEngine *engine, const uint8_t *bgra, uint32_t width, uint32_t height,
                           int32_t hot_x, int32_t hot_y);

/// Report where the pointer's hotspot is: on `head` at (x, y) in that head's mode pixels, or on
/// none of the dock's heads when head < 0. Cheap; call on every pointer move.
void dll_engine_move_cursor(DLLEngine *engine, int32_t head, int32_t x, int32_t y);

/// Most heads one dll_engine_stats call reports.
#define DLL_MAX_HEADS 4

/// Engine-wide performance counters since the previous dll_engine_stats call.
typedef struct {
    /// Engine loop wakeups that went on to visit the heads.
    uint64_t loop_passes;
    /// Time the engine loop slept because the shared video budget was exhausted.
    uint64_t budget_wait_us;
} DLLEngineStats;

/// One attached head's performance counters since the previous dll_engine_stats call.
/// Times are summed microseconds.
typedef struct {
    uint8_t head;
    /// Frames dll_engine_submit_frame or dll_engine_submit_encoded accepted, changed or not.
    uint64_t frames_submitted;
    /// Presentations attempted: new frames and owed retransmissions, including ones that sent
    /// nothing. Stage times below except submit_us accumulate over these.
    uint64_t present_calls;
    /// Presentations that put video bytes on the wire.
    uint64_t presents;
    /// Head visits with a new frame or a due retransmission held back by the video budget.
    uint64_t skipped;
    /// Presents whose change was too scattered for exact damage and went as a coarse superset.
    uint64_t coarse_fallbacks;
    /// Video bytes submitted to the dock.
    uint64_t bytes_sent;
    /// dll_engine_submit_frame: hashing and BGRA to RGB conversion, including its lock wait.
    /// dll_engine_submit_encoded: checking and copying the bodies.
    uint64_t submit_us;
    /// Copying changed strips out of the head's mailbox, on every in-budget visit.
    uint64_t take_us;
    /// Selecting the strips to send.
    uint64_t plan_us;
    /// Encoding them.
    uint64_t encode_us;
    /// Building the frame reports and USB transfers.
    uint64_t pack_us;
    /// Submitting the transfers to the video endpoint.
    uint64_t usb_us;
    /// The status poll after each presentation.
    uint64_t poll_us;
    /// GPU time reported through dll_engine_add_gpu_time.
    uint64_t gpu_us;
} DLLHeadStats;

/// Copy the performance counters accumulated since the previous call, and reset them.
/// engine_stats, unless NULL, receives the engine-wide counters. heads, unless NULL, receives
/// one entry per attached head, at most max_heads of them; returns how many were written.
/// Every head's counters are reset whether or not they fit. Pass DLL_MAX_HEADS to see them all.
size_t dll_engine_stats(DLLEngine *engine, DLLEngineStats *engine_stats, DLLHeadStats *heads,
                        size_t max_heads);

// ---- Strips coded outside the engine (the GPU encoder) ----
//
// Instead of dll_engine_submit_frame, a head can be fed strips its producer hashed and coded
// itself. Whichever kind of submit arrives first after the head is attached picks the feed;
// switching later starts the head's mailbox over, as attaching does.

/// Dock families, as dll_encode_strip_quantized and the reference calls take them.
typedef enum {
    DLL_FAMILY_ELLA = 0,
    DLL_FAMILY_RIDGE = 1,
    DLL_FAMILY_NAVARRO = 2,
} DLLFamily;

/// How the dock spells the unary code. Narrow interleaves each unary one with a payload bit,
/// LSB first; Wide puts the MSB-first payload after the terminator.
typedef enum {
    DLL_CODE_TABLE_WIDE = 0,
    DLL_CODE_TABLE_NARROW = 1,
} DLLCodeTable;

/// Blocks in one strip, in raster order (the top 8-pixel half first on 64x16 strips).
#define DLL_STRIP_BLOCKS 16
/// No coded strip body is longer than this, at either depth.
#define DLL_STRIP_BODY_MAX 16384

/// One block's quantised coefficients per plane, in the wire's Mallat order. Every value the
/// quantiser produces fits: magnitudes stay at or below 4092 even at 10 bits.
typedef struct {
    int16_t cr[64];
    int16_t cb[64];
    int16_t y[64];
} DLLQuantBlock;

/// What a strip producer for one head must match.
typedef struct {
    /// DLLFamily, to pass to dll_encode_strip_quantized.
    uint32_t family;
    /// DLLCodeTable.
    uint32_t code_table;
    /// Strip size in pixels: 64x16 on Ella and Ridge, 128x8 on Navarro.
    uint32_t strip_width;
    uint32_t strip_height;
    /// The mode rounded up to whole strips. Strip indices count strip_width-wide strips across
    /// padded_width, row by row.
    uint32_t padded_width;
    uint32_t padded_height;
    /// Escape ceilings: DC deltas, luma AC, chroma AC.
    uint32_t dc_cmax;
    uint32_t ac_cmax;
    uint32_t chroma_ac_cmax;
    /// The first two bytes of every strip body, ahead of its x and y.
    uint8_t magic[2];
    /// Each 8-bit channel (after gamma) is widened to (v << 2) | (v >> 6).
    bool ten_bit;
} DLLEncoderProfile;

/// Describe what a strip producer for the head must match. Returns false, leaving out untouched,
/// while the head is not attached.
bool dll_engine_encoder_profile(DLLEngine *engine, uint8_t head, DLLEncoderProfile *out);

/// Hand the engine coded strips for a head: count strips, strip i at index strip_index[i] with
/// content hash hash[i] (the producer's own, never 0, compared only with its other hashes) and
/// body bodies[body_offsets[i] .. body_offsets[i + 1]). body_offsets has count + 1 entries,
/// ascending, the last at most bodies_len. width and height are the frame size, which must be
/// the head's mode, and ten_bit the depth the strips were coded at. A strip submitted again
/// before the engine takes it replaces the older copy. The bodies are copied before returning.
/// Returns false, queuing nothing, when anything does not fit the head (see the profile); a
/// depth mismatch also raises the full-encode request.
bool dll_engine_submit_encoded(DLLEngine *engine, uint8_t head, uint32_t width, uint32_t height,
                               bool ten_bit, size_t count, const uint32_t *strip_index,
                               const uint64_t *hash, const uint8_t *bodies, size_t bodies_len,
                               const uint32_t *body_offsets);

/// Whether the producer must code every strip of the head again, ignoring its own hashes; the
/// request is cleared. Raised when the head is attached, when its gamma table changes (the
/// producer applies it), and when the engine needs a strip it holds no current body for.
/// Poll before each frame, and also while the screen is still: nothing else repeats the request.
bool dll_engine_take_full_request(DLLEngine *engine, uint8_t head);

/// Add GPU time spent on a head's strips to its gpu_us counter.
void dll_engine_add_gpu_time(DLLEngine *engine, uint8_t head, uint64_t micros);

/// The strip coding of a family at a depth, as dll_engine_encoder_profile reports it for an
/// attached head of that family, with padded_width and padded_height 0. Stateless. Returns false,
/// leaving out untouched, for an unknown family.
bool dll_strip_encoding_profile(uint32_t family, bool ten_bit, DLLEncoderProfile *out);

/// Code one strip at pixel (sx, sy) from its DLL_STRIP_BLOCKS quantised blocks, for a family and
/// depth. Stateless; safe to call from many threads at once. Writes the body to out when it fits
/// in out_cap bytes (DLL_STRIP_BODY_MAX always does) and returns its length either way; returns
/// 0 for an unknown family, null blocks, or coordinates beyond 65535.
size_t dll_encode_strip_quantized(uint32_t family, bool ten_bit, uint32_t sx, uint32_t sy,
                                  const DLLQuantBlock *blocks, uint8_t *out, size_t out_cap);

/// Reference for tests: the strip at (sx, sy) of a packed RGB888 surface w_pad pixels wide, coded
/// by the engine's CPU path. Same return convention as dll_encode_strip_quantized; also 0 when the
/// strip is not strip-aligned or does not lie inside rgb_len bytes.
size_t dll_reference_encode_strip(uint32_t family, bool ten_bit, const uint8_t *rgb, size_t rgb_len,
                                  uint32_t w_pad, uint32_t sx, uint32_t sy, uint8_t *out,
                                  size_t out_cap);

/// Reference for tests: the same strip's quantised blocks, written to out (DLL_STRIP_BLOCKS of
/// them). Returns false on the same inputs dll_reference_encode_strip rejects.
bool dll_reference_quantize_strip(uint32_t family, bool ten_bit, const uint8_t *rgb,
                                  size_t rgb_len, uint32_t w_pad, uint32_t sx, uint32_t sy,
                                  DLLQuantBlock *out);

// ---- Offline replay benchmark ----
//
// One head of a dock family driven with no dock attached, through the engine's own submit, take,
// plan, encode and pack, the way the `replay` tool runs it (docs/BENCHMARK.md): the video budget
// runs on a virtual clock that advances 1/fps per step, and USB is a null sink. Lets a host-side
// strip producer be measured on exactly the path the CPU feed takes. Not thread-safe.

typedef struct DLLBench DLLBench;

/// What one step did. Stage times are wall nanoseconds.
typedef struct {
    uint64_t submit_ns;
    uint64_t take_ns;
    uint64_t plan_ns;
    uint64_t encode_ns;
    uint64_t pack_ns;
    /// How many times the step's records went out; 0 when nothing was sent.
    uint32_t presentations;
    /// Bytes packing would have put on USB.
    uint64_t sent_bytes;
    /// A new frame was waiting but the budget was exhausted.
    bool skipped;
} DLLBenchStep;

/// Totals since dll_bench_new. checksum is FNV-1a over each accounted present's presentation
/// count and length-prefixed records, the same as `replay run` reports.
typedef struct {
    uint64_t checksum;
    uint64_t record_bytes;
    uint64_t sent_bytes;
    uint64_t presents;
    uint64_t skipped;
} DLLBenchTotals;

/// A head of family (DLLFamily) accepting width x height frames at fps. gamma is NULL or 768 bytes
/// (R, G, B tables of 256); an identity table is refused. NULL on bad arguments.
DLLBench *dll_bench_new(uint32_t family, uint32_t width, uint32_t height, double fps,
                        const uint8_t *gamma);

/// The head's encoder profile, as dll_engine_encoder_profile reports it (always 8-bit).
bool dll_bench_encoder_profile(const DLLBench *bench, DLLEncoderProfile *out);

/// As dll_engine_take_full_request. Poll before each encoded step.
bool dll_bench_take_full_request(DLLBench *bench);

/// One frame fed as BGRA, as dll_engine_submit_frame takes it (dirty NULL means the whole frame),
/// then one engine pass. Returns false on invalid arguments.
bool dll_bench_step_surface(DLLBench *bench, const uint8_t *bgra, size_t bgra_len, size_t stride,
                            const uint32_t *dirty, size_t dirty_count, DLLBenchStep *out);

/// One frame fed as coded strips, as dll_engine_submit_encoded takes them, then one engine pass.
/// Returns false, after advancing the clock, when the engine refuses the strips.
bool dll_bench_step_encoded(DLLBench *bench, bool ten_bit, size_t count,
                            const uint32_t *strip_index, const uint64_t *hash,
                            const uint8_t *bodies, size_t bodies_len,
                            const uint32_t *body_offsets, DLLBenchStep *out);

/// Fold the last step's records into the checksum. Separate from the step so its cost can be
/// kept out of a measurement; call it after every step.
void dll_bench_account(DLLBench *bench);

void dll_bench_totals(const DLLBench *bench, DLLBenchTotals *out);

void dll_bench_free(DLLBench *bench);

/// Blank every head, release the dock, join the engine thread, and free the engine.
/// No callback runs after this returns. Must not be called from a callback. Before calling,
/// exclude all other calls using this engine, including another stop; never use it afterwards.
void dll_engine_stop(DLLEngine *engine);

#ifdef __cplusplus
}
#endif

#endif
