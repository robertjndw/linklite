// SPDX-License-Identifier: GPL-2.0-or-later

// The Metal source is compiled at runtime with makeLibrary(source:): SwiftPM cannot build a
// .metallib without Xcode's Metal toolchain, and the app bundle carries no SwiftPM resource
// bundle, so the source travels inside the binary.
//
// Every strip is 16 blocks of 8x8 (1024 pixels) on every dock, laid 8x2 (64x16) or 16x1 (128x8),
// so the per-strip kernels run 256 threads per strip, four pixels or twelve coefficients each.
//
// One frame is a chain of kernels in one command buffer:
//   hash_strips   flags the candidate strips whose post-gamma pixels moved, and updates the shadow.
//   size_strips   quantises each flagged strip and works out its exact coded length.
//   scan_strips   compacts the flagged strips in candidate order and gives each body its offset.
//   pack_strips   quantises each flagged strip again and writes its whole body at that offset.
// With CPU packing, encode_strips replaces size and pack and writes the quantised blocks instead.

let stripEncoderMetalSource = #"""
#include <metal_stdlib>
using namespace metal;

// Mirrors StripParams in MetalStripEncoder.swift, field for field.
struct StripParams {
    uint frame_width;
    uint frame_height;
    uint padded_width;
    uint strip_width;
    uint strip_height;
    uint strip_width_shift;
    uint tiles_x;
    uint block_shift;
    uint ten_bit;
    uint force;
    uint from_shadow;
    uint narrow;
    uint dc_cmax;
    uint ac_cmax;
    uint chroma_ac_cmax;
    uint magic;
    uint candidate_count;
    uint output_capacity;
    uint with_lengths;
    // Tests only: size_strips and pack_strips take each strip's blocks from `coefficients` (by
    // candidate) instead of quantising the shadow, so arbitrary coefficients reach the coder.
    uint from_coefficients;
};

constant uint kThreads = 256;
constant uint kStripPixels = 1024;
constant uint kBlockCoefficients = 3 * 64;
constant uint kStripCoefficients = 16 * kBlockCoefficients;
constant uint kItemsPerThread = kStripCoefficients / kThreads;

// The rows of the largest strip need 8822 bytes (16 blocks x 63 positions x (24 + 24 + 22) bits at
// the 10-bit ceilings, plus padding), so main goes after that, and both fit in the 12 KiB the
// pixel planes leave free once the first Haar level has read them.
constant uint kMainWord = 2240;
constant uint kStagingWords = kMainWord + 64;

// The level-one bands go out in 2x2 Morton order: wire position 16 + k holds the band's row-major
// coefficient SCAN4_MORTON[k] (transform.rs). This is that table inverted, row-major to k.
constant uint kMortonInverse[16] = {0, 4, 1, 5, 8, 12, 9, 13, 2, 6, 3, 7, 10, 14, 11, 15};

// Floor division by 2^n, as Rust's >> on i32. Spelled with shifts of non-negative values only,
// because C++ leaves >> of a negative number to the implementation.
static inline int asr(int v, uint n) {
    return v >= 0 ? (v >> n) : ~((~v) >> n);
}

static inline ulong mix64(ulong z) {
    z += 0x9E3779B97F4A7C15UL;
    z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9UL;
    z = (z ^ (z >> 27)) * 0x94D049BB133111EBUL;
    return z ^ (z >> 31);
}

// A frame pixel as 0x00BBGGRR. The texture is bgra8Unorm, so a channel reads back as c / 255;
// rounding v * 255 recovers c exactly (MetalStripEncoderTests checks all 256 values).
static inline uint frame_pixel(texture2d<float, access::read> frame, uint x, uint y,
                               constant StripParams &p) {
    if (x >= p.frame_width || y >= p.frame_height) {
        return 0;
    }
    uint4 c = uint4(rint(frame.read(uint2(x, y)) * 255.0f));
    return c.r | (c.g << 8) | (c.b << 16);
}

// The post-gamma channels of a 0x00BBGGRR pixel.
static inline uint3 gamma_rgb(uint px, constant uchar *lut) {
    return uint3(lut[px & 255], lut[256 + ((px >> 8) & 255)], lut[512 + ((px >> 16) & 255)]);
}

// K1 (with K2 folded in): one threadgroup per candidate strip. Hashes the strip's post-gamma RGB,
// padding included, so a change the gamma table hides, or one only in alpha, is no change. Sets
// flags[group] when the hash moved (always when forced). Read from the frame, every candidate
// strip is also copied into the shadow, so the shadow always holds the latest pre-gamma pixels,
// whether or not the hash moved.
//
// Flags rather than an atomic append keep the changed list in candidate order, which scan_strips
// relies on and which makes every run lay the bodies out the same way.
kernel void hash_strips(texture2d<float, access::read> frame [[texture(0)]],
                        device uint *shadow [[buffer(0)]],
                        device ulong *hashes [[buffer(1)]],
                        constant uchar *lut [[buffer(2)]],
                        device const uint *candidates [[buffer(3)]],
                        device uint *flags [[buffer(4)]],
                        constant StripParams &p [[buffer(5)]],
                        uint group [[threadgroup_position_in_grid]],
                        uint tid [[thread_index_in_threadgroup]]) {
    threadgroup ulong partial[kThreads];
    uint strip = candidates[group];
    uint sx = (strip % p.tiles_x) * p.strip_width;
    uint sy = (strip / p.tiles_x) * p.strip_height;
    ulong sum = 0;
    for (uint k = 0; k < kStripPixels / kThreads; k++) {
        uint i = tid + k * kThreads;
        uint x = sx + (i & (p.strip_width - 1));
        uint y = sy + (i >> p.strip_width_shift);
        uint at = y * p.padded_width + x;
        uint px;
        if (p.from_shadow != 0) {
            px = shadow[at];
        } else {
            px = frame_pixel(frame, x, y, p);
            shadow[at] = px;
        }
        uint3 rgb = gamma_rgb(px, lut);
        // A sum of per-pixel mixes is order-free, so the reduction below can run in any shape,
        // and the position in the word keeps two strips that differ by a permutation apart.
        sum += mix64((ulong(i) << 24) | ulong((rgb.r << 16) | (rgb.g << 8) | rgb.b));
    }
    partial[tid] = sum;
    threadgroup_barrier(mem_flags::mem_threadgroup);
    for (uint s = kThreads / 2; s > 0; s >>= 1) {
        if (tid < s) {
            partial[tid] += partial[tid + s];
        }
        threadgroup_barrier(mem_flags::mem_threadgroup);
    }
    if (tid == 0) {
        // Never 0: the engine and this encoder both read 0 as "content unknown".
        ulong hash = partial[0] | 1;
        bool moved = p.force != 0 || hash != hashes[strip];
        if (moved) {
            hashes[strip] = hash;
        }
        flags[group] = moved ? 1 : 0;
    }
}

// The quantiser of strip.rs (quant_chroma_plane, quant_luma_plane): plane 0 Cr, 1 Cb, 2 Y.
static inline short quantize(uint plane, uint pos, int t) {
    if (plane < 2) {
        uint s = pos == 0 ? 6 : pos < 3 ? 4 : pos == 3 ? 5 : pos < 12 ? 4 : pos < 48 ? 5 : 6;
        return short(asr(t + (1 << (s - 1)), s));
    }
    if (pos == 0) {
        return short(asr(t + 8, 4));
    }
    int v;
    if (pos < 3) {
        v = asr(t + 8, 4);
    } else if (pos == 3) {
        v = asr(t + 16, 5);
    } else if (pos < 12) {
        v = asr(t + 2, 2);
    } else if (pos < 16) {
        v = asr(t + 4, 3);
    } else if (pos < 48) {
        // The one band that truncates toward zero instead of flooring.
        int m = abs(t) >> 1;
        v = t < 0 ? -m : m;
    } else {
        v = asr(t + 2, 2);
    }
    return short(clamp(v, -2048, 2047));
}

// Reads one strip from the shadow, applies gamma and the 10-bit widening, colour(), the three
// Haar levels of transform_scalar and the quantiser, and leaves the strip's 16 blocks in q (Cr,
// Cb, Y, 64 each, in raster block order: DLLQuantBlock's layout). Every thread of the group must
// call it. planes is free again on return; q is complete.
//
// Haar steps are exact integer sums, so each 2x2 butterfly can be evaluated in any order: every
// level is a set of independent butterflies, spread over the threadgroup.
static void quantise_strip(device const uint *shadow, constant uchar *lut, constant StripParams &p,
                           uint strip, uint tid, threadgroup int *planes, threadgroup int *ll1,
                           threadgroup int *ll2, threadgroup short *q) {
    uint sx = (strip % p.tiles_x) * p.strip_width;
    uint sy = (strip / p.tiles_x) * p.strip_height;
    uint across_mask = (1u << p.block_shift) - 1;

    // planes is [plane][block][row * 8 + column], in the codec's x64 fixed point.
    for (uint k = 0; k < kStripPixels / kThreads; k++) {
        uint i = tid + k * kThreads;
        uint block = i >> 6;
        uint x = sx + (block & across_mask) * 8 + (i & 7);
        uint y = sy + (block >> p.block_shift) * 8 + ((i >> 3) & 7);
        int3 c = int3(gamma_rgb(shadow[y * p.padded_width + x], lut));
        if (p.ten_bit != 0) {
            c = (c << 2) | (c >> 6);
        }
        int cb = c.r - c.g;
        int cr = c.b - c.g;
        planes[i] = 64 * cr;
        planes[kStripPixels + i] = 64 * cb;
        planes[2 * kStripPixels + i] = 64 * c.g + 64 * asr(cb + cr, 2);
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);

    // Level one: 48 planes x 16 butterflies over the 8x8 block.
    for (uint k = 0; k < 3; k++) {
        uint item = tid + k * kThreads;
        uint bp = item >> 4;
        uint g = item & 15;
        uint plane = bp >> 4;
        uint block = bp & 15;
        uint r = (g >> 2) * 2;
        uint col = (g & 3) * 2;
        threadgroup const int *src = planes + plane * kStripPixels + block * 64;
        int a = src[r * 8 + col], b = src[r * 8 + col + 1];
        int c = src[(r + 1) * 8 + col], d = src[(r + 1) * 8 + col + 1];
        ll1[bp * 16 + g] = (a + b) + (c + d);
        uint m = kMortonInverse[g];
        threadgroup short *o = q + block * kBlockCoefficients + plane * 64;
        o[16 + m] = quantize(plane, 16 + m, asr((a - b) + (c - d), 6));
        o[32 + m] = quantize(plane, 32 + m, asr((a + b) - (c + d), 6));
        o[48 + m] = quantize(plane, 48 + m, asr((a - b) - (c - d), 6));
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);

    // Level two: 48 x 4 butterflies over each 4x4 LL1.
    if (tid < 192) {
        uint bp = tid >> 2;
        uint g = tid & 3;
        uint plane = bp >> 4;
        uint block = bp & 15;
        uint r = (g >> 1) * 2;
        uint col = (g & 1) * 2;
        threadgroup const int *src = ll1 + bp * 16;
        int a = src[r * 4 + col], b = src[r * 4 + col + 1];
        int c = src[(r + 1) * 4 + col], d = src[(r + 1) * 4 + col + 1];
        ll2[bp * 4 + g] = (a + b) + (c + d);
        threadgroup short *o = q + block * kBlockCoefficients + plane * 64;
        o[4 + g] = quantize(plane, 4 + g, asr((a - b) + (c - d), 6));
        o[8 + g] = quantize(plane, 8 + g, asr((a + b) - (c + d), 6));
        o[12 + g] = quantize(plane, 12 + g, asr((a - b) - (c - d), 6));
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);

    // Level three: one butterfly per plane and block, giving the DC and the coarsest bands.
    if (tid < 48) {
        uint plane = tid >> 4;
        uint block = tid & 15;
        threadgroup const int *src = ll2 + tid * 4;
        int a = src[0], b = src[1], c = src[2], d = src[3];
        threadgroup short *o = q + block * kBlockCoefficients + plane * 64;
        o[0] = quantize(plane, 0, asr((a + b) + (c + d), 6));
        o[1] = quantize(plane, 1, asr((a - b) + (c - d), 6));
        o[2] = quantize(plane, 2, asr((a + b) - (c + d), 6));
        o[3] = quantize(plane, 3, asr((a - b) - (c - d), 6));
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);
}

// Either quantise the strip or, for tests, load the given blocks; q is complete on return.
static void strip_coefficients(device const uint *shadow, constant uchar *lut, constant StripParams &p,
                               device const short *given, uint group, uint strip, uint tid,
                               threadgroup int *planes, threadgroup int *ll1, threadgroup int *ll2,
                               threadgroup short *q) {
    if (p.from_coefficients != 0) {
        for (uint k = 0; k < kItemsPerThread; k++) {
            q[tid + k * kThreads] = given[group * kStripCoefficients + tid + k * kThreads];
        }
        threadgroup_barrier(mem_flags::mem_threadgroup);
    } else {
        quantise_strip(shadow, lut, p, strip, tid, planes, ll1, ll2, q);
    }
}

// CPU packing: one threadgroup per changed strip (threadgroups past the count exit at once).
// Writes the strip's 16 DLLQuantBlocks to slot `group` of `coefficients`.
kernel void encode_strips(device const uint *shadow [[buffer(0)]],
                          constant uchar *lut [[buffer(1)]],
                          device const uint *changed [[buffer(2)]],
                          device const uint *totals [[buffer(3)]],
                          device short *coefficients [[buffer(4)]],
                          constant StripParams &p [[buffer(5)]],
                          uint group [[threadgroup_position_in_grid]],
                          uint tid [[thread_index_in_threadgroup]]) {
    if (group >= totals[0]) {
        return;
    }
    threadgroup int planes[3 * kStripPixels];
    threadgroup int ll1[48 * 16];
    threadgroup int ll2[48 * 4];
    threadgroup short q[kStripCoefficients];
    quantise_strip(shadow, lut, p, changed[group], tid, planes, ll1, ll2, q);
    device short *out = coefficients + group * kStripCoefficients;
    for (uint k = 0; k < kItemsPerThread; k++) {
        out[tid + k * kThreads] = q[tid + k * kThreads];
    }
}

// ---- The entropy coder of transform.rs (Bits) ----
//
// A code is a word of at most 39 bits, LSB first, and its length. Every field of a strip is built
// from Bits::unary, whose length is 2 * count + term in both dialects, so a length never needs
// the word.

struct Code {
    ulong word;
    uint len;
};

// spread_bits: bit i of the low 16 to bit 2i.
static inline ulong spread_bits(uint v) {
    ulong x = ulong(v & 0xffff);
    x = (x | (x << 8)) & 0x00ff00ffUL;
    x = (x | (x << 4)) & 0x0f0f0f0fUL;
    x = (x | (x << 2)) & 0x33333333UL;
    return (x | (x << 1)) & 0x55555555UL;
}

// Bits::unary: `count` ones, a 0 terminator when `term`, and the low `count` bits of payload.
// Wide puts the payload MSB first after the terminator; Narrow pairs each one with a payload
// bit, LSB first, terminator last.
static inline Code unary(uint count, uint term, uint payload, bool narrow) {
    ulong ones = (1UL << count) - 1;
    ulong word;
    if (narrow) {
        word = spread_bits(uint(ones)) | (spread_bits(payload & uint(ones)) << 1);
    } else {
        // The 64-bit shift keeps count == 0 defined (a 32-bit shift by 32 is not).
        ulong msb_first = ulong(reverse_bits(payload)) >> (32 - count);
        word = ones | (msb_first << (count + term));
    }
    return Code{word, 2 * count + term};
}

// Bits::esc, saturation included: the category is capped at cmax and the magnitude at the
// largest value that category holds, and only a category below cmax is terminated.
static inline uint esc_category(int v, uint cmax) {
    return min(32 - clz(uint(abs(v))), cmax);
}

static inline uint esc_length(int v, uint cmax) {
    uint c = esc_category(v, cmax);
    return 2 * c + (c < cmax ? 1 : 0);
}

static inline Code esc(int v, uint cmax, bool narrow) {
    uint c = esc_category(v, cmax);
    uint top = 1u << c;
    uint off = min(uint(abs(v)), top - 1) - (top >> 1);
    return unary(c, c < cmax ? 1 : 0, (off << 1) | (v > 0 ? 1u : 0u), narrow);
}

static inline void append(thread Code &to, Code code) {
    to.word |= code.word << to.len;
    to.len += code.len;
}

// Bits::chroma_base.
static inline Code chroma_base(uint last, bool narrow) {
    uint c = 31 - clz(last + 1);
    return unary(c, 1, last - ((1u << c) - 1), narrow);
}

// Bits::colour_sync_unit for one block, as one code: a present Cr replaces the first of luma's
// two zero root branches with its node, a present Cb the second.
static inline Code sync_unit(uint lcr, uint lcb, uint ly, bool narrow) {
    Code code = {0, 0};
    uint skip = 0;
    if (lcr != 0) {
        append(code, chroma_base(lcr, narrow));
        skip = 1;
    }
    if (lcb != 0) {
        if (lcr == 0) {
            code.len += 1;
        }
        append(code, chroma_base(lcb, narrow));
        skip = 2;
    }
    code.len += 2 - skip;
    if (ly == 0) {
        append(code, unary(6, 1, 0, narrow));
    } else {
        uint k = 31 - clz(64 - ly);
        append(code, unary(k, 1, (64 - (1u << k)) - ly, narrow));
    }
    return code;
}

static inline uint round_even(uint n) {
    return n + (n & 1);
}

// Where everything of one strip goes, as StripCoder::finish lays it out: a 16-byte header, main
// (16 sync units, then the 48 DC deltas) zero-padded to w18 = 16 + round_even(main) + 2, then
// row0 (AC of blocks 0..8) padded even, then row1 (blocks 8..16) padded even.
struct Layout {
    // This thread's first row bit, counted from the start of row0.
    uint row_bit;
    // This thread's main bit (threads 0..63 each own one main code).
    uint main_bit;
    uint row0_len;
    uint main_bytes;
    uint w18;
    uint body_len;
};

// The code of main item `item`: 0..15 the sync units, then the DC deltas, Cr, Cb, Y per block,
// each predicted from the block before (from 0 for block 0) and capped at dc_cmax.
static inline Code main_code(uint item, threadgroup const short *q, threadgroup const uint *lasts,
                             constant StripParams &p) {
    bool narrow = p.narrow != 0;
    if (item < 16) {
        return sync_unit(lasts[item * 3], lasts[item * 3 + 1], lasts[item * 3 + 2], narrow);
    }
    uint k = item - 16;
    uint block = k / 3;
    uint plane = k % 3;
    int dc = q[block * kBlockCoefficients + plane * 64];
    int previous = block > 0 ? int(q[(block - 1) * kBlockCoefficients + plane * 64]) : 0;
    return esc(dc - previous, p.dc_cmax, narrow);
}

// The row code of coefficient `i` (block, plane, position as DLLQuantBlock lays them out): an
// escape for positions 1..last of its plane, nothing otherwise. The DC goes in main.
static inline bool row_coded(uint i, threadgroup const uint *lasts) {
    uint pos = i & 63;
    return pos != 0 && pos <= lasts[i >> 6];
}

static inline uint row_cmax(uint i, constant StripParams &p) {
    return ((i >> 6) % 3) < 2 ? p.chroma_ac_cmax : p.ac_cmax;
}

// Computes the strip's layout from q. Every thread of the group must call it, with the q that
// quantise_strip left. Thread t owns coefficients 12t..12t+11, which keeps each thread's codes in
// one contiguous run of bits: threads 0..127 are exactly row0 and 128..255 row1.
static Layout layout_strip(threadgroup const short *q, threadgroup uint *lasts, threadgroup uint *sums,
                           constant StripParams &p, uint tid, uint lane, uint sg) {
    // last_significant per (block, plane): 48 runs of 64, two positions per lane.
    for (uint run = sg; run < 48; run += kThreads / 32) {
        threadgroup const short *c = q + run * 64;
        uint low = (lane != 0 && c[lane] != 0) ? lane : 0u;
        uint high = c[lane + 32] != 0 ? lane + 32 : 0u;
        uint last = simd_max(max(low, high));
        if (lane == 0) {
            lasts[run] = last;
        }
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);

    uint row_bits = 0;
    for (uint k = 0; k < kItemsPerThread; k++) {
        uint i = tid * kItemsPerThread + k;
        if (row_coded(i, lasts)) {
            row_bits += esc_length(q[i], row_cmax(i, p));
        }
    }
    uint main_bits = tid < 64 ? main_code(tid, q, lasts, p).len : 0;

    uint row_prefix = simd_prefix_exclusive_sum(row_bits);
    uint main_prefix = simd_prefix_exclusive_sum(main_bits);
    uint row_total = simd_sum(row_bits);
    uint main_total = simd_sum(main_bits);
    if (lane == 0) {
        sums[sg] = row_total;
        sums[8 + sg] = main_total;
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);

    uint row0_bits = sums[0] + sums[1] + sums[2] + sums[3];
    uint row1_bits = sums[4] + sums[5] + sums[6] + sums[7];
    uint all_main = sums[8] + sums[9];
    Layout layout;
    layout.row0_len = round_even((row0_bits + 7) / 8);
    uint rows_len = layout.row0_len + round_even((row1_bits + 7) / 8);
    layout.main_bytes = (all_main + 7) / 8;
    layout.w18 = 16 + round_even(layout.main_bytes) + 2;
    layout.body_len = layout.w18 + rows_len;

    // Row1 starts byte-aligned at row0_len, so its bits count from there.
    uint row_base = sg < 4 ? 0 : layout.row0_len * 8;
    for (uint s = sg < 4 ? 0 : 4; s < sg; s++) {
        row_base += sums[s];
    }
    layout.row_bit = row_base + row_prefix;
    layout.main_bit = (sg == 1 ? sums[8] : 0) + main_prefix;
    return layout;
}

// K3 + K4: one threadgroup per candidate; unflagged ones exit at once. The coefficients never
// leave threadgroup memory: pack_strips works them out again rather than reading 6 KiB a strip
// back, which also saves a coefficient buffer sized for every strip.
kernel void size_strips(device const uint *shadow [[buffer(0)]],
                        constant uchar *lut [[buffer(1)]],
                        device const uint *candidates [[buffer(2)]],
                        device const uint *flags [[buffer(3)]],
                        device uint *lengths [[buffer(4)]],
                        constant StripParams &p [[buffer(5)]],
                        device const short *given [[buffer(6)]],
                        uint group [[threadgroup_position_in_grid]],
                        uint tid [[thread_index_in_threadgroup]],
                        uint lane [[thread_index_in_simdgroup]],
                        uint sg [[simdgroup_index_in_threadgroup]]) {
    if (flags[group] == 0) {
        return;
    }
    threadgroup int planes[3 * kStripPixels];
    threadgroup int ll1[48 * 16];
    threadgroup int ll2[48 * 4];
    threadgroup short q[kStripCoefficients];
    threadgroup uint lasts[48];
    threadgroup uint sums[16];
    strip_coefficients(shadow, lut, p, given, group, candidates[group], tid, planes, ll1, ll2, q);
    Layout layout = layout_strip(q, lasts, sums, p, tid, lane, sg);
    if (tid == 0) {
        lengths[group] = layout.body_len;
    }
}

// Exclusive scan over the candidates, in one threadgroup: each flagged candidate gets its slot
// in the compact lists and (with lengths) its body's byte offset. Writes changed (strip index by
// slot), changed_hashes, offsets (count + 1 entries), slots (by candidate), and totals: the
// changed count, then the total body bytes.
kernel void scan_strips(device const uint *candidates [[buffer(0)]],
                        device const uint *flags [[buffer(1)]],
                        device const uint *lengths [[buffer(2)]],
                        device const ulong *hashes [[buffer(3)]],
                        device uint *changed [[buffer(4)]],
                        device uint *slots [[buffer(5)]],
                        device uint *offsets [[buffer(6)]],
                        device ulong *changed_hashes [[buffer(7)]],
                        device uint *totals [[buffer(8)]],
                        constant StripParams &p [[buffer(9)]],
                        uint tid [[thread_index_in_threadgroup]],
                        uint threads [[threads_per_threadgroup]],
                        uint lane [[thread_index_in_simdgroup]],
                        uint sg [[simdgroup_index_in_threadgroup]],
                        uint simdgroups [[simdgroups_per_threadgroup]]) {
    threadgroup uint count_base[32];
    threadgroup uint byte_base[32];
    uint n = p.candidate_count;
    bool with_lengths = p.with_lengths != 0;
    // Contiguous chunks, so a thread's slots and offsets are consecutive.
    uint chunk = (n + threads - 1) / threads;
    uint begin = min(tid * chunk, n);
    uint end = min(begin + chunk, n);
    uint count = 0;
    uint bytes = 0;
    for (uint i = begin; i < end; i++) {
        if (flags[i] != 0) {
            count += 1;
            bytes += with_lengths ? lengths[i] : 0;
        }
    }
    uint count_prefix = simd_prefix_exclusive_sum(count);
    uint byte_prefix = simd_prefix_exclusive_sum(bytes);
    uint count_total = simd_sum(count);
    uint byte_total = simd_sum(bytes);
    if (lane == 0) {
        count_base[sg] = count_total;
        byte_base[sg] = byte_total;
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);
    if (sg == 0) {
        uint c = lane < simdgroups ? count_base[lane] : 0;
        uint b = lane < simdgroups ? byte_base[lane] : 0;
        uint c_prefix = simd_prefix_exclusive_sum(c);
        uint b_prefix = simd_prefix_exclusive_sum(b);
        uint all_count = simd_sum(c);
        uint all_bytes = simd_sum(b);
        if (lane < simdgroups) {
            count_base[lane] = c_prefix;
            byte_base[lane] = b_prefix;
        }
        if (lane == 0) {
            totals[0] = all_count;
            totals[1] = all_bytes;
            offsets[all_count] = all_bytes;
        }
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);
    uint slot = count_base[sg] + count_prefix;
    uint offset = byte_base[sg] + byte_prefix;
    for (uint i = begin; i < end; i++) {
        if (flags[i] != 0) {
            uint strip = candidates[i];
            changed[slot] = strip;
            changed_hashes[slot] = hashes[strip];
            offsets[slot] = offset;
            slots[i] = slot;
            slot += 1;
            offset += with_lengths ? lengths[i] : 0;
        }
    }
}

// OR a code into the staging bits at bit `at`. A code is at most 39 bits and starts at most 31
// bits into its first word, so it touches at most three words. Neighbouring codes share words,
// hence the atomics; zero words, which most run bits are, are skipped.
static inline void stage(threadgroup atomic_uint *bits, uint at, ulong word) {
    if (word == 0) {
        return;
    }
    uint w = at >> 5;
    uint s = at & 31;
    uint first = uint(word << s);
    ulong rest = word >> (32 - s);
    if (first != 0) {
        atomic_fetch_or_explicit(&bits[w], first, memory_order_relaxed);
    }
    if (uint(rest) != 0) {
        atomic_fetch_or_explicit(&bits[w + 1], uint(rest), memory_order_relaxed);
    }
    if (uint(rest >> 32) != 0) {
        atomic_fetch_or_explicit(&bits[w + 2], uint(rest >> 32), memory_order_relaxed);
    }
}

static inline uchar staged_byte(threadgroup atomic_uint *bits, uint byte) {
    return uchar(atomic_load_explicit(&bits[byte >> 2], memory_order_relaxed) >> ((byte & 3) * 8));
}

// K5: one threadgroup per candidate; unflagged ones exit at once. Codes the strip into
// threadgroup memory, every thread its own run of codes at the offsets layout_strip found, then
// copies the whole body to its offset in `bodies`. Bodies are byte-aligned and back to back, so
// each byte of the output belongs to exactly one strip and is written once, by plain stores.
// A body that would run past output_capacity is not written at all; the host sees the total,
// grows the buffer and runs this kernel again.
kernel void pack_strips(device const uint *shadow [[buffer(0)]],
                        constant uchar *lut [[buffer(1)]],
                        device const uint *candidates [[buffer(2)]],
                        device const uint *flags [[buffer(3)]],
                        device const uint *slots [[buffer(4)]],
                        device const uint *offsets [[buffer(5)]],
                        device uchar *bodies [[buffer(6)]],
                        constant StripParams &p [[buffer(7)]],
                        device const short *given [[buffer(8)]],
                        uint group [[threadgroup_position_in_grid]],
                        uint tid [[thread_index_in_threadgroup]],
                        uint lane [[thread_index_in_simdgroup]],
                        uint sg [[simdgroup_index_in_threadgroup]]) {
    if (flags[group] == 0) {
        return;
    }
    threadgroup int planes[3 * kStripPixels];
    threadgroup int ll1[48 * 16];
    threadgroup int ll2[48 * 4];
    threadgroup short q[kStripCoefficients];
    threadgroup uint lasts[48];
    threadgroup uint sums[16];
    uint strip = candidates[group];
    strip_coefficients(shadow, lut, p, given, group, strip, tid, planes, ll1, ll2, q);

    // The pixel planes are spent once quantise_strip returns; their 12 KiB stage the bits.
    threadgroup atomic_uint *bits = reinterpret_cast<threadgroup atomic_uint *>(planes);
    for (uint w = tid; w < kStagingWords; w += kThreads) {
        atomic_store_explicit(&bits[w], 0u, memory_order_relaxed);
    }
    Layout layout = layout_strip(q, lasts, sums, p, tid, lane, sg);
    uint offset = offsets[slots[group]];
    if (offset + layout.body_len > p.output_capacity) {
        return;
    }

    bool narrow = p.narrow != 0;
    uint at = layout.row_bit;
    for (uint k = 0; k < kItemsPerThread; k++) {
        uint i = tid * kItemsPerThread + k;
        if (row_coded(i, lasts)) {
            Code code = esc(q[i], row_cmax(i, p), narrow);
            stage(bits, at, code.word);
            at += code.len;
        }
    }
    if (tid < 64) {
        stage(bits + kMainWord, layout.main_bit, main_code(tid, q, lasts, p).word);
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);

    uint sx = (strip % p.tiles_x) * p.strip_width;
    uint sy = (strip / p.tiles_x) * p.strip_height;
    device uchar *out = bodies + offset;
    for (uint j = tid; j < layout.body_len; j += kThreads) {
        uchar b;
        if (j >= layout.w18) {
            b = staged_byte(bits, j - layout.w18);
        } else if (j >= 16) {
            uint m = j - 16;
            b = m < layout.main_bytes ? staged_byte(bits + kMainWord, m) : 0;
        } else {
            switch (j) {
            case 0: b = uchar(p.magic); break;
            case 1: b = uchar(p.magic >> 8); break;
            case 2: b = uchar(sx); break;
            case 3: b = uchar(sx >> 8); break;
            case 4: b = uchar(sy); break;
            case 5: b = uchar(sy >> 8); break;
            case 10: b = uchar(layout.w18); break;
            case 11: b = uchar(layout.w18 >> 8); break;
            case 12: b = uchar((layout.w18 + layout.row0_len)); break;
            case 13: b = uchar((layout.w18 + layout.row0_len) >> 8); break;
            default: b = 0; break;
            }
        }
        out[j] = b;
    }
}
"""#
