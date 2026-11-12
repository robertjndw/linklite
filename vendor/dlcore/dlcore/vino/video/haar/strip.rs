// SPDX-License-Identifier: GPL-2.0

//! Strip geometry and the encoding of one strip.
//!
//! A strip is the unit the dock accepts: sixteen blocks, laid out per family, carrying Y, Cb
//! and Cr for a rectangle of the surface. Geometry says how a surface is cut into them.

use super::*;

pub(crate) const STRIP_ROW_BLOCKS: usize = 8; // blocks in one coded half
pub(crate) const STRIP_BLOCKS: usize = 16;

/// Everything about a dock's video encoding that differs between platforms.
///
/// Passed by value rather than held in shared state: two docks of different generations may
/// encode concurrently, and each frame must carry its own dock's layout.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Geometry {
    /// `log2` of the strip width in pixels. A strip is always [`STRIP_BLOCKS`] blocks of
    /// `DIM` x `DIM` split into two halves of [`STRIP_ROW_BLOCKS`]; the docks differ only in
    /// how those blocks are laid over pixels. Ridge puts them 8 across x 2 down (64x16), the
    /// DL7400 16 across x 1 down (128x8). Same block count, same coded strip.
    ///
    /// Held as shifts because a strip is a power of two in each direction, so every geometry
    /// query is a shift or a mask: dividing by a value the compiler cannot bound would put a
    /// panic path into the codec's hot functions.
    strip_w_shift: u32,
    /// `log2` of the strip height in pixels; see [`Geometry::strip_w_shift`].
    strip_h_shift: u32,
    /// Whether image records are emitted with even y-bands before odd ones.
    ///
    /// Ridge sends them in raster order; the DL7400 interlaces, which its own records show as
    /// a y sequence of 0, 16, 32 ... over 8-row strips before it returns for 8, 24, 40 ...
    pub(crate) interlaced_bands: bool,
    /// Whether an image record's `sub` carries the y-band parity in bit 4.
    ///
    /// Ridge does, which also means one of its records can never span two bands. Navarro's
    /// image records carry only the connector, and fill to the stride cap across band
    /// boundaries.
    pub(crate) band_parity_bit: bool,
    /// How the dock encodes a connector in a video record's `sub` field, as a left shift.
    ///
    /// Ridge puts the bare connector number there (0, 1). Navarro shifts it by three: its
    /// records use `0x00`/`0x08`/`0x10`/`0x18` and its stream-open ids are
    /// `0x07`/`0x0f`/`0x17`/`0x1f` -- the same eight-apart spacing.
    pub(crate) connector_selector_shift: u8,
    /// The bits a connector's stream id sets over its record `sub`.
    ///
    /// A dock names each video stream by its connector's record `sub` with a fixed low pattern
    /// set: Ridge uses `0x08 | connector`, Navarro `(connector << 3) | 7`. The same id is the
    /// wire `sub` of the stream's control records, the value its `RepeaterAuth_Stream_Manage`
    /// restatement declares, and the byte-7 tweak deriving the stream's AES-CTR nonce from its
    /// SKE RIV.
    pub(crate) stream_id_mask: u8,
    /// How many buffers the dock rotates through as it presents frames; see
    /// `DockProfile::dock_buffers`.
    pub(crate) dock_buffers: u8,
    /// Bits a steady-state image record adds to its `sub`; see
    /// `DockProfile::steady_record_sub_bit`.
    ///
    /// Cleared for the frames that open a stream, which the vendor sends without it.
    pub(crate) steady_sub_bit: u8,
    /// Bits per channel of the surface being encoded; see [`Depth`].
    ///
    /// Geometry is otherwise fixed per dock, and this is not -- a connector moves between
    /// depths at runtime when a compositor turns HDR on. It lives here because it is the one
    /// remaining thing the codec needs to know that is neither a block coordinate nor a
    /// coefficient, and `Geometry` is already threaded through every path that would have to
    /// carry it separately. [`Geometry::new`] gives [`Depth::Eight`]; a 10-bit connector asks
    /// for [`Geometry::with_depth`].
    depth: Depth,
    /// Which dialect of the shared unary code this dock's decoder reads.
    ///
    /// The same profile field states the dock's code tables in its stream configuration, so
    /// the code vino emits and the code it declares cannot drift apart.
    coding: super::super::video_arm::CodeTables,
}
impl Geometry {
    /// Build a dock's geometry from its profile.
    ///
    /// `strip_blocks_x` must divide [`STRIP_BLOCKS`]; anything else falls back to the Ridge
    /// layout rather than producing a strip that is not a whole number of blocks.
    pub(crate) fn new(
        strip_blocks_x: usize,
        interlaced_bands: bool,
        band_parity_bit: bool,
        connector_selector_shift: u8,
        stream_id_mask: u8,
        dock_buffers: u8,
    ) -> Self {
        let (strip_w_shift, strip_h_shift) = match strip_blocks_x {
            16 => (7, 3), // 128 x 8
            _ => (6, 4),  // 64 x 16
        };
        Self {
            strip_w_shift,
            strip_h_shift,
            interlaced_bands,
            band_parity_bit,
            connector_selector_shift,
            stream_id_mask,
            dock_buffers: dock_buffers.max(1),
            steady_sub_bit: 0,
            depth: Depth::Eight,
            coding: super::super::video_arm::CodeTables::Wide,
        }
    }

    /// The same dock geometry at a different sample depth; see [`Geometry::depth`].
    #[inline]
    pub(crate) fn with_depth(self, depth: Depth) -> Self {
        Self { depth, ..self }
    }

    /// Select the bitstream dialect; see [`Geometry::coding`].
    pub(crate) fn with_coding(self, coding: super::super::video_arm::CodeTables) -> Self {
        Self { coding, ..self }
    }

    /// The same geometry with the steady-state record bit set; see
    /// [`Geometry::steady_sub_bit`].
    pub(crate) fn with_steady_sub_bit(self, bit: u8) -> Self {
        Self {
            steady_sub_bit: bit,
            ..self
        }
    }

    /// The same geometry for a frame that opens a stream, which carries no steady-state bit.
    pub(crate) fn opening(self) -> Self {
        Self {
            steady_sub_bit: 0,
            ..self
        }
    }

    /// Which dialect of the shared unary code this dock's decoder reads.
    #[inline]
    pub(crate) fn coding(&self) -> super::super::video_arm::CodeTables {
        self.coding
    }

    /// Bits per channel this frame is being encoded at.
    #[inline]
    pub(crate) fn depth(&self) -> Depth {
        self.depth
    }

    /// `log2` of the strip width; see [`Geometry::strip_w_shift`].
    #[inline]
    pub(crate) fn strip_w_shift(&self) -> u32 {
        self.strip_w_shift
    }

    /// `log2` of the strip height; see [`Geometry::strip_w_shift`].
    #[inline]
    pub(crate) fn strip_h_shift(&self) -> u32 {
        self.strip_h_shift
    }

    /// Strip width in pixels: [`Geometry::strip_w_shift`] blocks across, each `DIM` square.
    #[inline]
    pub(crate) fn strip_w(&self) -> usize {
        1usize << self.strip_w_shift
    }

    /// Strip height in pixels; see [`Geometry::strip_w`].
    #[inline]
    pub(crate) fn strip_h(&self) -> usize {
        1usize << self.strip_h_shift
    }

    /// Damage macro-tile: 4x4 strips. A touched macro-tile must be resent in full because the
    /// dock rotates its backing buffers at this granularity.
    #[inline]
    pub(crate) fn macro_w(&self) -> usize {
        4 * self.strip_w()
    }

    /// Damage macro-tile height; see [`Geometry::macro_w`].
    #[inline]
    pub(crate) fn macro_h(&self) -> usize {
        4 * self.strip_h()
    }

    /// Encode `connector` the way this dock expects it in a record `sub` field.
    #[inline]
    pub(crate) fn connector_selector(&self, connector: u8) -> u8 {
        connector << self.connector_selector_shift
    }

    /// The content-stream id of `connector` on this dock; see [`Geometry::stream_id_mask`].
    #[inline]
    pub(crate) fn stream_id(&self, connector: u8) -> u16 {
        u16::from(self.connector_selector(connector) | self.stream_id_mask)
    }
}
/// The Ridge layout, and the value every geometry-free code path starts from.
pub(crate) const RIDGE_GEOMETRY: Geometry = Geometry {
    strip_w_shift: 6,
    strip_h_shift: 4,
    interlaced_bands: false,
    band_parity_bit: true,
    connector_selector_shift: 0,
    stream_id_mask: 0x08,
    dock_buffers: 2,
    // The geometry-free starting point describes a stream that has not opened yet.
    steady_sub_bit: 0,
    depth: Depth::Eight,
    coding: super::super::video_arm::CodeTables::Wide,
};

/// `log2(DIM)`, so a block index splits into (x, y) by shift and mask rather than division.
pub(crate) const DIM_SHIFT: u32 = 3;

/// Round a byte count up to an even number (every coder sub-region is even-aligned).
pub(crate) fn round_even(n: usize) -> usize {
    n + (n & 1)
}
/// Starting capacity of the buffer a strip is coded into: enough for all but busy strips, so
/// the buffer rarely grows mid-strip.
const STRIP_SCRATCH: usize = 1024;
/// One quantized colour block: the three planes' 64 coefficients and exact last-significant
/// AC positions. Built by [`colour_block`] from a block's per-plane samples.
#[derive(Clone)]
pub(crate) struct ColourBlock {
    pub(crate) qcr: [i32; COEFFS],
    pub(crate) qcb: [i32; COEFFS],
    pub(crate) qy: [i32; COEFFS],
    pub(crate) lcr: usize,
    pub(crate) lcb: usize,
    pub(crate) ly: usize,
}
impl ColourBlock {
    /// An all-zero block, for [`colour_block_into`] to overwrite.
    pub(crate) const fn zeroed() -> Self {
        Self {
            qcr: [0; COEFFS],
            qcb: [0; COEFFS],
            qy: [0; COEFFS],
            lcr: 0,
            lcb: 0,
            ly: 0,
        }
    }
}
/// Return the exact chroma AC extent.
/// Only the KUnit tests call this now: `colour_block` computes the same extent branch-free with
/// [`last_significant`].
#[cfg(CONFIG_DRM_VINO_KUNIT_TEST)]
pub(crate) fn chroma_last(q: &[i32; COEFFS]) -> usize {
    (1..COEFFS).rev().find(|&i| q[i] != 0).unwrap_or(0)
}
/// Transform + quantize one block's three planes (each 64 samples in the codec's x64 fixed
/// point: `cr[i] = 64*(B-G)`, `cb[i] = 64*(R-G)`, `y[i] = 64*G + 64*((Cb+Cr)>>2)`). Luma uses
/// the per-position `quantize`; chroma AC uses `quantize_chroma_ac`; all DCs use
/// `quantize_dc_round`.
///
/// `#[inline(never)]`: see `haar2d`'s doc comment -- part of the kernel-stack-overflow fix.
#[inline(never)]
pub(crate) fn colour_block(
    cr: &[i32; PIXELS],
    cb: &[i32; PIXELS],
    y: &[i32; PIXELS],
) -> ColourBlock {
    let mut block = ColourBlock::zeroed();
    colour_block_into(cr, cb, y, &mut block);
    block
}
/// [`colour_block`] written into `block`, overwriting every field. The frame encoder reuses one
/// block per strip this way instead of zeroing a fresh one and copying it out for every block.
#[inline(never)]
pub(crate) fn colour_block_into(
    cr: &[i32; PIXELS],
    cb: &[i32; PIXELS],
    y: &[i32; PIXELS],
    block: &mut ColourBlock,
) {
    // Do not reach for SIMD here. An AVX2 in-block Haar is byte-exact but parity-to-slower
    // once `kernel_fpu_begin`/`end` are paid per block, costing about 18% more CPU on a live
    // encode. A strip is ~72% entropy coder, which is bit-serial, so even a free transform
    // caps the whole win near 23%.
    let (tcr, tcb, ty) = (transform(cr), transform(cb), transform(y));
    // Quantise band by band. Each band's steps are constants here, so each loop is a fixed
    // shift with no per-coefficient table load or rounding-mode branch, and vectorises where
    // the target allows it. The constants restate `quantize`'s and `quantize_chroma_ac`'s
    // tables; `quant_luma`/`quant_chroma` assert they agree with them in debug builds.
    // Together the bands cover every position, so every coefficient is overwritten.
    block.lcr = quant_chroma_plane(&tcr, &mut block.qcr);
    block.lcb = quant_chroma_plane(&tcb, &mut block.qcb);
    block.ly = quant_luma_plane(&ty, &mut block.qy);
}
/// Quantise one chroma plane (DC step 64, AC `quantize_chroma_ac`); returns its last
/// significant AC position. Bands are `<first, end, shift>`.
#[inline(always)]
fn quant_chroma_plane(t: &[i32; COEFFS], q: &mut [i32; COEFFS]) -> usize {
    q[0] = quantize_dc_round(1, t[0]);
    quant_chroma::<1, 3, 4>(t, q);
    quant_chroma::<3, 4, 5>(t, q);
    quant_chroma::<4, 12, 4>(t, q);
    quant_chroma::<12, 48, 5>(t, q);
    quant_chroma::<48, 64, 6>(t, q);
    last_significant(q)
}
/// Quantise the luma plane (DC step 16, AC `quantize`); returns its last significant AC
/// position. Bands are `<first, end, shift, bias>`; a zero bias truncates towards zero.
#[inline(always)]
fn quant_luma_plane(t: &[i32; COEFFS], q: &mut [i32; COEFFS]) -> usize {
    q[0] = quantize_dc_round(0, t[0]);
    quant_luma::<1, 3, 4, 8>(t, q);
    quant_luma::<3, 4, 5, 16>(t, q);
    quant_luma::<4, 12, 2, 2>(t, q);
    quant_luma::<12, 16, 3, 4>(t, q);
    quant_luma::<16, 48, 1, 0>(t, q);
    quant_luma::<48, 64, 2, 2>(t, q);
    last_significant(q)
}
/// `quantize_chroma_ac` over `LO..HI`, where every position has shift `S`.
#[inline(always)]
fn quant_chroma<const LO: usize, const HI: usize, const S: u32>(
    t: &[i32; COEFFS],
    q: &mut [i32; COEFFS],
) {
    for (i, (q, &t)) in q[LO..HI].iter_mut().zip(&t[LO..HI]).enumerate() {
        *q = (t + (1 << (S - 1))) >> S;
        debug_assert_eq!(*q, quantize_chroma_ac(t, LO + i));
    }
}
/// `quantize` over `LO..HI`, where every position has `(shift, bias) == (S, B)`.
#[inline(always)]
fn quant_luma<const LO: usize, const HI: usize, const S: u32, const B: i32>(
    t: &[i32; COEFFS],
    q: &mut [i32; COEFFS],
) {
    for (i, (q, &t)) in q[LO..HI].iter_mut().zip(&t[LO..HI]).enumerate() {
        let v = if B == 0 {
            // Truncate towards zero.
            let m = t.abs() >> S;
            if t < 0 {
                -m
            } else {
                m
            }
        } else {
            (t + B) >> S
        };
        *q = v.clamp(-2048, 2047);
        debug_assert_eq!(*q, quantize(t, LO + i));
    }
}
/// The last significant AC position (`1..COEFFS`), or 0 when every AC coefficient is zero.
///
/// A branch-free max over `i` where `q[i] != 0` rather than an early-exit reverse scan, so it
/// vectorises where the target allows. Position 0 (the DC) only ever contributes 0.
#[inline(always)]
pub(crate) fn last_significant(q: &[i32; COEFFS]) -> usize {
    let mut last = 0u32;
    for (i, &v) in (0u32..).zip(q) {
        last = last.max(if v != 0 { i } else { 0 });
    }
    last as usize
}
/// Codes one strip a block at a time, so the frame encoder never holds all sixteen quantized
/// blocks at once.
///
/// A strip is a 16-byte header and three byte-aligned coded regions: main (every block's
/// significance tree, then every block's DC deltas), row0 (AC of blocks 0..8) and row1 (AC of
/// blocks 8..16), each zero-padded to an even length, with two extra zero bytes after main.
/// Rows are coded as blocks arrive, in block order, into one scratch buffer; main needs only
/// each block's three last positions and three DCs, which are kept until [`StripCoder::finish`].
struct StripCoder {
    geometry: Geometry,
    /// Row0 then row1, then (in [`StripCoder::finish`]) main, in one scratch buffer.
    bits: Bits,
    /// Byte length of row0 including its padding, once row1 has begun.
    row0_len: usize,
    /// Per block `(lcr, lcb, ly)` and `(Cr, Cb, Y)` DC, in block order.
    lasts: [(usize, usize, usize); STRIP_BLOCKS],
    dcs: [(i32, i32, i32); STRIP_BLOCKS],
    n: usize,
}
impl StripCoder {
    fn new(geometry: Geometry) -> Result<Self> {
        let scratch = KVec::with_capacity(STRIP_SCRATCH, GFP_KERNEL)?;
        Ok(Self {
            geometry,
            bits: Bits::over(scratch, geometry.coding()),
            row0_len: 0,
            lasts: [(0, 0, 0); STRIP_BLOCKS],
            dcs: [(0, 0, 0); STRIP_BLOCKS],
            n: 0,
        })
    }

    /// Code the next block (raster order: 0..8 top 8-px half, 8..16 bottom).
    fn push(&mut self, b: &ColourBlock) -> Result {
        if self.n == STRIP_BLOCKS {
            return Err(kernel::error::code::EINVAL);
        }
        if self.n == STRIP_ROW_BLOCKS {
            // Row0 is complete: pad it and start row1 byte-aligned after it.
            self.row0_len = self.bits.align_even()?;
        }
        self.bits.colour_block_ac(
            &b.qcr,
            &b.qcb,
            &b.qy,
            b.lcr,
            b.lcb,
            b.ly,
            self.geometry.depth(),
        )?;
        self.lasts[self.n] = (b.lcr, b.lcb, b.ly);
        self.dcs[self.n] = (b.qcr[0], b.qcb[0], b.qy[0]);
        self.n += 1;
        Ok(())
    }

    /// Code main after the rows in the scratch, then lay out the exactly sized strip body.
    fn finish(mut self, x: u16, y: u16) -> Result<KVec<u8>> {
        if self.n != STRIP_BLOCKS {
            return Err(kernel::error::code::EINVAL);
        }
        let rows_len = self.bits.align_even()?;
        for &(lcr, lcb, ly) in &self.lasts {
            self.bits.colour_sync_unit(lcr, lcb, ly)?;
        }
        let dc_cmax = self.geometry.depth().dc_cmax();
        let (mut pcr, mut pcb, mut py) = (0i32, 0i32, 0i32);
        for &(cr, cb, yv) in &self.dcs {
            self.bits.esc(cr - pcr, dc_cmax)?;
            self.bits.esc(cb - pcb, dc_cmax)?;
            self.bits.esc(yv - py, dc_cmax)?;
            (pcr, pcb, py) = (cr, cb, yv);
        }
        let scratch = self.bits.finish()?;
        let (rows, main) = scratch.split_at(rows_len);
        let w18 = 16 + round_even(main.len()) + 2;
        let w1c = w18 + self.row0_len;
        // The 2-byte tail overlaps the end of the row1 region (len = w1c + round_even(row1)).
        let len = w18 + rows_len;

        // Bodies are cached by the scanout for every strip on screen, so allocate exactly.
        let mut out = KVec::with_capacity(len, GFP_KERNEL)?;
        out.resize(16, 0, GFP_KERNEL)?;
        out[0] = 0x01;
        out[1] = 0x28;
        out[2..4].copy_from_slice(&x.to_le_bytes());
        // Strip y is the band's top edge.
        out[4..6].copy_from_slice(&y.to_le_bytes());
        out[10..12].copy_from_slice(&(w18 as u16).to_le_bytes());
        out[12..14].copy_from_slice(&(w1c as u16).to_le_bytes());
        out.extend_from_slice(main, GFP_KERNEL)?;
        out.resize(w18, 0, GFP_KERNEL)?;
        out.extend_from_slice(rows, GFP_KERNEL)?;
        // No forward-hint tail: on the EP08 wire the strip's last 2 bytes are the natural row1
        // bit-packing. The record framing carries the length as `strip_id == len`, so the
        // in-strip echo the sink hook showed is not transmitted on the wire. See `frame_records`.
        Ok(out)
    }
}
/// Encode one 64x16 COLOUR strip at pixel `(x, y)` from its 16 quantized colour blocks
/// (raster: 0..8 top 8-px half, 8..16 bottom).
///
/// `#[inline(never)]`: see `haar2d`'s doc comment -- part of the kernel-stack-overflow fix.
#[inline(never)]
pub(crate) fn colour_strip(
    geometry: Geometry,
    blocks: &[ColourBlock],
    x: u16,
    y: u16,
) -> Result<KVec<u8>> {
    let mut coder = StripCoder::new(geometry)?;
    for b in blocks {
        coder.push(b)?;
    }
    coder.finish(x, y)
}
/// A pixel source for the strip encoder, read eight pixels of one row at a time.
///
/// Every `FnMut(x, y) -> (R, G, B)` is one, reading pixel by pixel. A source that can read a
/// whole run at once implements [`PixelRows::row8`] directly, and saves a call, an address
/// computation and a bounds check per pixel.
pub(crate) trait PixelRows {
    /// The `[R, G, B]` channels of the eight pixels `(x..x + 8, y)`, at the plane's own depth.
    fn row8(&mut self, x: usize, y: usize, rgb: &mut [[i32; DIM]; 3]);
}
impl<F: FnMut(usize, usize) -> (u16, u16, u16)> PixelRows for F {
    #[inline(always)]
    fn row8(&mut self, x: usize, y: usize, rgb: &mut [[i32; DIM]; 3]) {
        for c in 0..DIM {
            let (r, g, b) = self(x + c, y);
            (rgb[0][c], rgb[1][c], rgb[2][c]) = (r.into(), g.into(), b.into());
        }
    }
}
/// Gather and quantize one 64x16 strip's 16 colour blocks from a pixel source, handing each to
/// `each` in raster order (0..8 top 8-px half, 8..16 bottom), matching [`colour_strip`].
/// `px` reads the 8-bit `(R, G, B)` at absolute frame coordinates; `(ox, oy)` is the strip's
/// top-left pixel. Each block's three planes are built in the codec's x64 fixed point via
/// [`colour`] (per-pixel `(Y, Cb, Cr)`, stored `(Cr, Cb, Y)` for [`colour_block`]).
///
/// One block and one set of planes are live at a time and reused, so nothing is allocated,
/// zeroed or copied per block.
#[inline(always)]
fn for_each_strip_block(
    geometry: Geometry,
    ox: usize,
    oy: usize,
    px: &mut impl PixelRows,
    mut each: impl FnMut(&ColourBlock) -> Result,
) -> Result {
    let mut block = ColourBlock::zeroed();
    // Every element is overwritten for each block before it is read.
    let (mut cr, mut cb, mut y) = ([0i32; PIXELS], [0i32; PIXELS], [0i32; PIXELS]);
    let across_shift = geometry.strip_w_shift() - DIM_SHIFT;
    for k in 0..STRIP_BLOCKS {
        let (bx, by) = (k & ((1usize << across_shift) - 1), k >> across_shift);
        for row in 0..DIM {
            let mut rgb = [[0i32; DIM]; 3];
            px.row8(ox + bx * DIM, oy + by * DIM + row, &mut rgb);
            let at = row * DIM;
            for c in 0..DIM {
                let (yv, cbv, crv) = colour(rgb[0][c], rgb[1][c], rgb[2][c]);
                (cr[at + c], cb[at + c], y[at + c]) = (crv, cbv, yv);
            }
        }
        colour_block_into(&cr, &cb, &y, &mut block);
        each(&block)?;
    }
    Ok(())
}
/// Gather one 64x16 strip's 16 colour blocks from a pixel source; see
/// [`for_each_strip_block`].
///
/// `#[inline(never)]`: see `haar2d`'s doc comment -- part of the kernel-stack-overflow fix.
/// The block array is heap allocated because it is about 12 KiB and nested copies can exhaust
/// the kernel stack.
#[inline(never)]
pub(crate) fn colour_strip_blocks(
    geometry: Geometry,
    ox: usize,
    oy: usize,
    px: &mut impl PixelRows,
) -> Result<KVec<ColourBlock>> {
    let mut blocks = KVec::with_capacity(STRIP_BLOCKS, GFP_KERNEL)?;
    for_each_strip_block(geometry, ox, oy, px, |b| {
        blocks.push(b.clone(), GFP_KERNEL)?;
        Ok(())
    })?;
    Ok(blocks)
}
/// Encode ONE 64x16 strip whose top-left output pixel is `(sx, sy)`.
///
/// This is the unit of work both frame encoders are built from, and it is the reason a
/// parallel encode is possible at all: a strip reads only its own 64x16 region through `px`
/// and produces its own independent byte vector, sharing no state with any other strip. The
/// scanout encoder in `drm_sink.rs` fans batches of these across CPUs; see `EncodeChunk`.
///
/// Equivalent to [`colour_strip_blocks`] then [`colour_strip`], but codes each block as it is
/// quantized instead of collecting all sixteen first.
#[inline(never)]
pub(crate) fn colour_strip_at(
    geometry: Geometry,
    sx: usize,
    sy: usize,
    px: &mut impl PixelRows,
) -> Result<KVec<u8>> {
    let mut coder = StripCoder::new(geometry)?;
    for_each_strip_block(geometry, sx, sy, px, |b| coder.push(b))?;
    coder.finish(sx as u16, sy as u16)
}
/// A strip's `y` (the EP08 record bands group strips by row). Reads the `y` field the strip
/// builders write at byte offset 4 ([`colour_strip`] / [`solid_strip`]).
pub(crate) fn strip_y(s: &[u8]) -> u16 {
    u16::from_le_bytes([s[4], s[5]])
}
/// A strip's `x`, written by the strip builders at byte offset 2.
pub(crate) fn strip_x(s: &[u8]) -> u16 {
    u16::from_le_bytes([s[2], s[3]])
}

#[cfg(CONFIG_DRM_VINO_KUNIT_TEST)]
#[kunit_tests(vino_haar_strip)]
mod tests {
    use super::*;

    #[test]
    fn haar_colour_and_quantize() {
        use video::haar;
        // Colour transform against captured transform-DC values: white maps to Y=16320,
        // achromatic pixels have zero chroma, and red's floored luma is 4032.
        assert_eq!(haar::colour(255, 255, 255), (16320, 0, 0));
        assert_eq!(haar::colour(128, 128, 128), (128 * 64, 0, 0));
        assert_eq!(haar::colour(255, 0, 0), (4032, 64 * 255, 0));
        // Green has two negative signed-chroma components.
        assert_eq!(haar::colour(0, 255, 0), (8128, -64 * 255, -64 * 255));
        assert_eq!(haar::colour(0, 0, 255), (4032, 0, 64 * 255));
        // White Y_DC=16320 quantizes to 1020 at DC position zero.
        assert_eq!(haar::quantize(16320, 0), 1020);
        // AC clamps to the 12-bit signed long-token range.
        assert_eq!(haar::quantize(1_000_000, 16), 2047);
        assert_eq!(haar::quantize(-1_000_000, 16), -2048);
    }

    /// The colour transform is depth-agnostic: it carries code words, applies no transfer function
    /// and no matrix, and the host has already encoded whatever curve the sink wants.
    #[test]
    fn haar_colour_carries_ten_bit_unchanged() {
        use video::haar;
        assert_eq!(haar::colour(1023, 1023, 1023), (64 * 1023, 0, 0));
        assert_eq!(haar::colour(512, 512, 512), (512 * 64, 0, 0));
        assert_eq!(haar::colour(1023, 0, 0), (64 * (1023 >> 2), 64 * 1023, 0));
        // 10-bit white quantises to 4092 at DC, which is why the ceiling has to move.
        //
        // Through `quantize_dc_round`, which is what the encoder applies to coefficient zero. The
        // AC quantiser is the wrong function to assert here: its clamp to the 12-bit signed token
        // range caps the value at 2047, and the DC never reaches it. That clamp is deliberate --
        // an over-range AC magnitude saturates in `esc` and only loses detail, whereas an
        // over-sized ceiling desynchronises the dock's decoder outright.
        let (y, _, _) = haar::colour(1023, 1023, 1023);
        assert_eq!(
            haar::quantize_dc_round(0, haar::transform(&[y; haar::BLOCK])[0]),
            4092
        );
    }

    /// The same quantised blocks encode to different bytes at the two depths.
    ///
    /// This is the whole of the HDR difference on this wire, so it is worth a test that would fail
    /// if the depth ever stopped reaching the entropy coder: a DC of 4092 saturates to category
    /// 10's largest value at 8 bits and survives intact at 10.
    #[test]
    fn haar_strip_encoding_follows_the_depth() -> Result {
        use video::haar::{self, Depth};
        let white = haar::colour(1023, 1023, 1023).0;
        let plane = [white; haar::PIXELS];
        let zero = [0i32; haar::PIXELS];
        let mut blocks = KVec::new();
        for _ in 0..16 {
            blocks.push(haar::colour_block(&zero, &zero, &plane), GFP_KERNEL)?;
        }
        let geometry = haar::RIDGE_GEOMETRY;
        let eight = haar::colour_strip(geometry, &blocks, 0, 0)?;
        let ten = haar::colour_strip(geometry.with_depth(Depth::Ten), &blocks, 0, 0)?;
        assert_ne!(eight[..], ten[..]);
        // Saturating at the lower ceiling costs bits, so the 8-bit encoding is the shorter one.
        assert!(eight.len() <= ten.len());
        Ok(())
    }

    #[test]
    fn haar_chroma_last_is_exact() {
        use video::haar::{chroma_last, COEFFS};
        let mut q = [0i32; COEFFS];
        assert_eq!(chroma_last(&q), 0);
        for exact in [1usize, 2, 3, 4, 7, 8, 11, 15, 16, 27, 31, 32, 48, 62, 63] {
            q.fill(0);
            q[exact] = 1;
            assert_eq!(chroma_last(&q), exact);
        }
    }
}
