// SPDX-License-Identifier: GPL-2.0-or-later

//! The C ABI declared in `include/dllite.h`.

use crate::edid::Monitor;
use crate::host::{self, CursorShape, Events, HeadStats, Mode, Shared, State};
use dlcore::kvino::{self, CodeTable, DockProfile, Family, QuantBlock};
use std::ffi::{c_char, c_void, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::thread::JoinHandle;

#[repr(C)]
pub struct DLLMonitor {
    pub head: u8,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub width_mm: u32,
    pub height_mm: u32,
    pub vendor_id: u16,
    pub product_code: u16,
    pub serial: u32,
    pub name: *const c_char,
    pub edid: *const u8,
    pub edid_len: usize,
    pub hardware_cursor: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DLLCallbacks {
    pub context: *mut c_void,
    pub monitor_attached: Option<extern "C" fn(*mut c_void, *const DLLMonitor)>,
    pub monitor_detached: Option<extern "C" fn(*mut c_void, u8)>,
    pub state_changed: Option<extern "C" fn(*mut c_void, i32, *const c_char)>,
}

// The host promises in dllite.h that its callbacks may be invoked from the engine thread.
unsafe impl Send for DLLCallbacks {}

impl Events for DLLCallbacks {
    fn attached(&self, head: u8, monitor: &Monitor, mode: Mode, edid: &[u8], hardware_cursor: bool) {
        let Some(callback) = self.monitor_attached else { return };
        let name = c_string(monitor.name.as_deref().unwrap_or("DisplayLink monitor"));
        let vendor_id = monitor
            .manufacturer
            .bytes()
            .fold(0u16, |id, letter| (id << 5) | u16::from(letter.saturating_sub(b'@') & 0x1f));
        let record = DLLMonitor {
            head,
            width: mode.width as u32,
            height: mode.height as u32,
            refresh_hz: mode.refresh_hz,
            width_mm: monitor.width_mm,
            height_mm: monitor.height_mm,
            vendor_id,
            product_code: monitor.product_code,
            serial: monitor.serial,
            name: name.as_ptr(),
            edid: edid.as_ptr(),
            edid_len: edid.len(),
            hardware_cursor,
        };
        callback(self.context, &record);
    }

    fn detached(&self, head: u8) {
        if let Some(callback) = self.monitor_detached {
            callback(self.context, head);
        }
    }

    fn state(&self, state: State, message: &str) {
        if let Some(callback) = self.state_changed {
            let message = c_string(message);
            callback(self.context, state as i32, message.as_ptr());
        }
    }
}

fn c_string(text: &str) -> CString {
    CString::new(text.replace('\0', " ")).expect("NULs removed")
}

pub struct DLLEngine {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

#[no_mangle]
pub extern "C" fn dll_engine_start(callbacks: DLLCallbacks) -> *mut DLLEngine {
    catch_unwind(|| {
        let shared = Shared::new();
        let engine_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("dllite-engine".into())
            .spawn(move || host::run(engine_shared, callbacks))
            .ok()?;
        Some(Box::into_raw(Box::new(DLLEngine { shared, thread: Some(thread) })))
    })
    .ok()
    .flatten()
    .unwrap_or(std::ptr::null_mut())
}

/// Count of u32 elements in the C damage array, bounded by Rust's slice byte-length limit.
fn dirty_len(count: usize) -> Option<usize> {
    let len = count.checked_mul(4)?;
    (len.checked_mul(std::mem::size_of::<u32>())? <= isize::MAX as usize).then_some(len)
}

/// # Safety
/// `engine` must come from `dll_engine_start` and not yet be stopped. For nonempty geometry with
/// representable lengths, `bgra` must point to at least `stride * (height - 1) + width * 4`
/// readable bytes; `dirty`, unless null, to `dirty_count * 4` aligned, readable `u32`s.
/// Neither buffer may be mutated during the call.
#[no_mangle]
pub unsafe extern "C" fn dll_engine_submit_frame(
    engine: *mut DLLEngine,
    head: u8,
    bgra: *const u8,
    stride: usize,
    width: u32,
    height: u32,
    dirty: *const u32,
    dirty_count: usize,
) {
    let (Some(engine), false) = (unsafe { engine.as_ref() }, bgra.is_null()) else { return };
    let Some(len) = host::frame_len(stride, width as usize, height as usize) else { return };
    let dirty_len = if dirty.is_null() {
        0
    } else {
        let Some(len) = dirty_len(dirty_count) else { return };
        len
    };
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let pixels = unsafe { std::slice::from_raw_parts(bgra, len) };
        let full = dirty.is_null().then_some((0, 0, width as usize, height as usize));
        let quads = if dirty.is_null() {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(dirty, dirty_len) }
        };
        let rects = full.into_iter().chain(quads.chunks_exact(4).map(|r| {
            let (x, y) = (r[0] as usize, r[1] as usize);
            (x, y, x.saturating_add(r[2] as usize), y.saturating_add(r[3] as usize))
        }));
        engine.shared.submit_frame(
            usize::from(head),
            pixels,
            stride,
            width as usize,
            height as usize,
            rects,
        )
    }));
}

/// # Safety
/// `engine` must come from `dll_engine_start` and not yet be stopped.
#[no_mangle]
pub unsafe extern "C" fn dll_engine_set_display_power(engine: *mut DLLEngine, head: u8, on: bool) {
    if let Some(engine) = unsafe { engine.as_ref() } {
        let _ = catch_unwind(AssertUnwindSafe(|| engine.shared.set_power(usize::from(head), on)));
    }
}

/// # Safety
/// `engine` must come from `dll_engine_start` and not yet be stopped; `r`, `g` and `b` must
/// each be NULL or point to 256 readable bytes.
#[no_mangle]
pub unsafe extern "C" fn dll_engine_set_gamma(engine: *mut DLLEngine, head: u8, r: *const u8, g: *const u8, b: *const u8) {
    let Some(engine) = (unsafe { engine.as_ref() }) else { return };
    let lut = if r.is_null() || g.is_null() || b.is_null() {
        None
    } else {
        let read = |p: *const u8| unsafe { *p.cast::<[u8; 256]>() };
        Some([read(r), read(g), read(b)])
    };
    let _ = catch_unwind(AssertUnwindSafe(|| engine.shared.set_gamma(usize::from(head), lut)));
}

/// Largest pointer image the dock's hardware cursor takes.
const CURSOR_MAX: u32 = 64;

/// # Safety
/// `engine` must come from `dll_engine_start` and not yet be stopped; `bgra` must point to
/// `width * height * 4` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn dll_engine_set_cursor(
    engine: *mut DLLEngine,
    bgra: *const u8,
    width: u32,
    height: u32,
    hot_x: i32,
    hot_y: i32,
) {
    let (Some(engine), false) = (unsafe { engine.as_ref() }, bgra.is_null()) else { return };
    if width == 0 || height == 0 || width > CURSOR_MAX || height > CURSOR_MAX {
        return;
    }
    let pixels = unsafe { std::slice::from_raw_parts(bgra, (width * height * 4) as usize) }.to_vec();
    let _ = catch_unwind(AssertUnwindSafe(|| {
        engine.shared.set_cursor_shape(CursorShape { bgra: pixels, width, height, hot_x, hot_y })
    }));
}

/// # Safety
/// `engine` must come from `dll_engine_start` and not yet be stopped.
#[no_mangle]
pub unsafe extern "C" fn dll_engine_move_cursor(engine: *mut DLLEngine, head: i32, x: i32, y: i32) {
    if let Some(engine) = unsafe { engine.as_ref() } {
        let position = u8::try_from(head).ok().map(|head| (head, x, y));
        let _ = catch_unwind(AssertUnwindSafe(|| engine.shared.move_cursor(position)));
    }
}

/// Heads a `dll_engine_stats` call can report; `DLL_MAX_HEADS` in dllite.h.
const MAX_HEADS: usize = 4;
const _: () = assert!(MAX_HEADS == dlcore::MAX_CONNECTORS);

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DLLEngineStats {
    pub loop_passes: u64,
    pub budget_wait_us: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DLLHeadStats {
    pub head: u8,
    pub frames_submitted: u64,
    pub present_calls: u64,
    pub presents: u64,
    pub skipped: u64,
    pub coarse_fallbacks: u64,
    pub bytes_sent: u64,
    pub submit_us: u64,
    pub take_us: u64,
    pub plan_us: u64,
    pub encode_us: u64,
    pub pack_us: u64,
    pub usb_us: u64,
    pub poll_us: u64,
    pub gpu_us: u64,
}

impl DLLHeadStats {
    fn new(head: u8, stats: HeadStats) -> Self {
        Self {
            head,
            frames_submitted: stats.frames_submitted,
            present_calls: stats.present_calls,
            presents: stats.presents,
            skipped: stats.skipped,
            coarse_fallbacks: stats.coarse_fallbacks,
            bytes_sent: stats.bytes_sent,
            submit_us: stats.submit_us,
            take_us: stats.take_us,
            plan_us: stats.plan_us,
            encode_us: stats.encode_us,
            pack_us: stats.pack_us,
            usb_us: stats.usb_us,
            poll_us: stats.poll_us,
            gpu_us: stats.gpu_us,
        }
    }
}

/// # Safety
/// `engine` must come from `dll_engine_start` and not yet be stopped. `engine_stats`, unless
/// null, must point to a writable `DLLEngineStats`; `heads`, unless null, to `max_heads`
/// writable `DLLHeadStats`.
#[no_mangle]
pub unsafe extern "C" fn dll_engine_stats(
    engine: *mut DLLEngine,
    engine_stats: *mut DLLEngineStats,
    heads: *mut DLLHeadStats,
    max_heads: usize,
) -> usize {
    let Some(engine) = (unsafe { engine.as_ref() }) else { return 0 };
    let Ok(stats) = catch_unwind(AssertUnwindSafe(|| engine.shared.take_stats())) else { return 0 };
    if let Some(out) = unsafe { engine_stats.as_mut() } {
        *out = DLLEngineStats { loop_passes: stats.loop_passes, budget_wait_us: stats.budget_wait_us };
    }
    if heads.is_null() {
        return 0;
    }
    let attached = stats.heads.iter().enumerate().filter_map(|(head, stats)| {
        stats.map(|stats| DLLHeadStats::new(head as u8, stats))
    });
    let mut count = 0;
    for entry in attached.take(max_heads) {
        unsafe { heads.add(count).write(entry) };
        count += 1;
    }
    count
}

// ---- strips coded outside the engine (the GPU encoder) --------------------------------------

/// `DLL_FAMILY_*` in dllite.h.
fn family_code(family: Family) -> u32 {
    match family {
        Family::Ella => 0,
        Family::Ridge => 1,
        Family::Navarro => 2,
        Family::Firefly => u32::MAX,
    }
}

/// The dock a stateless strip call codes for, from its `DLL_FAMILY_*` code.
fn dock(family: u32, ten_bit: bool) -> Option<DockProfile> {
    let family = match family {
        0 => Family::Ella,
        1 => Family::Ridge,
        2 => Family::Navarro,
        _ => return None,
    };
    let dock = DockProfile::for_family(family)?;
    Some(if ten_bit { dock.ten_bit() } else { dock })
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DLLEncoderProfile {
    pub family: u32,
    pub code_table: u32,
    pub strip_width: u32,
    pub strip_height: u32,
    pub padded_width: u32,
    pub padded_height: u32,
    pub dc_cmax: u32,
    pub ac_cmax: u32,
    pub chroma_ac_cmax: u32,
    pub magic: [u8; 2],
    pub ten_bit: bool,
}

fn encoder_profile(encoder: host::EncoderProfile, padded_width: u32, padded_height: u32) -> DLLEncoderProfile {
    let e = encoder.encoding;
    DLLEncoderProfile {
        family: family_code(encoder.family),
        code_table: match e.code_table {
            CodeTable::Wide => 0,
            CodeTable::Narrow => 1,
        },
        strip_width: e.strip_w as u32,
        strip_height: e.strip_h as u32,
        padded_width,
        padded_height,
        dc_cmax: e.dc_cmax,
        ac_cmax: e.ac_cmax,
        chroma_ac_cmax: e.chroma_ac_cmax,
        magic: kvino::STRIP_MAGIC,
        ten_bit: e.ten_bit,
    }
}

/// # Safety
/// `engine` must come from `dll_engine_start` and not yet be stopped; `out`, unless null, must
/// point to a writable `DLLEncoderProfile`.
#[no_mangle]
pub unsafe extern "C" fn dll_engine_encoder_profile(engine: *mut DLLEngine, head: u8, out: *mut DLLEncoderProfile) -> bool {
    let (Some(engine), Some(out)) = (unsafe { engine.as_ref() }, unsafe { out.as_mut() }) else { return false };
    let Ok(Some((encoder, padded_width, padded_height))) =
        catch_unwind(AssertUnwindSafe(|| engine.shared.encoder_profile(usize::from(head))))
    else {
        return false;
    };
    let (Ok(padded_width), Ok(padded_height)) = (u32::try_from(padded_width), u32::try_from(padded_height)) else {
        return false;
    };
    *out = encoder_profile(encoder, padded_width, padded_height);
    true
}

/// # Safety
/// `engine` must come from `dll_engine_start` and not yet be stopped. Unless `count` is 0,
/// `strip_index` and `hash` must each point to `count` aligned, readable elements and
/// `body_offsets` to `count + 1`; `bodies` must point to `bodies_len` readable bytes. None may be
/// mutated during the call.
#[no_mangle]
pub unsafe extern "C" fn dll_engine_submit_encoded(
    engine: *mut DLLEngine,
    head: u8,
    width: u32,
    height: u32,
    ten_bit: bool,
    count: usize,
    strip_index: *const u32,
    hash: *const u64,
    bodies: *const u8,
    bodies_len: usize,
    body_offsets: *const u32,
) -> bool {
    let Some(engine) = (unsafe { engine.as_ref() }) else { return false };
    if count > 0 && (strip_index.is_null() || hash.is_null() || bodies.is_null() || body_offsets.is_null()) {
        return false;
    }
    let fits = |size: usize, len: usize| len.checked_mul(size).is_some_and(|bytes| bytes <= isize::MAX as usize);
    let Some(offsets_len) = count.checked_add(1) else { return false };
    if !fits(4, offsets_len) || !fits(8, count) || bodies_len > isize::MAX as usize {
        return false;
    }
    catch_unwind(AssertUnwindSafe(|| {
        if count == 0 {
            return engine.shared.submit_encoded(usize::from(head), width as usize, height as usize, ten_bit, []);
        }
        let (indices, hashes, offsets, bytes) = unsafe {
            (
                std::slice::from_raw_parts(strip_index, count),
                std::slice::from_raw_parts(hash, count),
                std::slice::from_raw_parts(body_offsets, offsets_len),
                std::slice::from_raw_parts(bodies, bodies_len),
            )
        };
        // Bodies are back to back in `bytes`; offsets that run backwards or past the end are a
        // producer bug, and refusing the lot is safer than sending part of a picture.
        let in_order = offsets.windows(2).all(|pair| pair[0] <= pair[1]);
        if !in_order || offsets[count] as usize > bodies_len {
            return false;
        }
        let strips = (0..count).map(|i| {
            (indices[i] as usize, hashes[i], &bytes[offsets[i] as usize..offsets[i + 1] as usize])
        });
        engine.shared.submit_encoded(usize::from(head), width as usize, height as usize, ten_bit, strips)
    }))
    .unwrap_or(false)
}

/// # Safety
/// `engine` must come from `dll_engine_start` and not yet be stopped.
#[no_mangle]
pub unsafe extern "C" fn dll_engine_take_full_request(engine: *mut DLLEngine, head: u8) -> bool {
    let Some(engine) = (unsafe { engine.as_ref() }) else { return false };
    catch_unwind(AssertUnwindSafe(|| engine.shared.take_full_request(usize::from(head)))).unwrap_or(false)
}

/// # Safety
/// `engine` must come from `dll_engine_start` and not yet be stopped.
#[no_mangle]
pub unsafe extern "C" fn dll_engine_add_gpu_time(engine: *mut DLLEngine, head: u8, micros: u64) {
    if let Some(engine) = unsafe { engine.as_ref() } {
        let elapsed = std::time::Duration::from_micros(micros);
        let _ = catch_unwind(AssertUnwindSafe(|| engine.shared.add_gpu_time(usize::from(head), elapsed)));
    }
}

/// Copy `body` to `out` if it fits; returns its length either way, or 0 for no body.
fn write_body(body: Option<Vec<u8>>, out: *mut u8, out_cap: usize) -> usize {
    let Some(body) = body else { return 0 };
    if body.len() <= out_cap && !out.is_null() {
        unsafe { std::ptr::copy_nonoverlapping(body.as_ptr(), out, body.len()) };
    }
    body.len()
}

/// # Safety
/// `out`, unless null, must point to a writable `DLLEncoderProfile`.
#[no_mangle]
pub unsafe extern "C" fn dll_strip_encoding_profile(family: u32, ten_bit: bool, out: *mut DLLEncoderProfile) -> bool {
    let (Some(dock), Some(out)) = (dock(family, ten_bit), unsafe { out.as_mut() }) else { return false };
    let Some(family) = dock.family() else { return false };
    *out = encoder_profile(host::EncoderProfile { family, encoding: dock.strip_encoding() }, 0, 0);
    true
}

/// # Safety
/// `blocks` must point to 16 readable `DLLQuantBlock`s (no alignment needed) and `out`, unless
/// null, to `out_cap` writable bytes. Neither may overlap or be mutated during the call.
#[no_mangle]
pub unsafe extern "C" fn dll_encode_strip_quantized(
    family: u32,
    ten_bit: bool,
    sx: u32,
    sy: u32,
    blocks: *const QuantBlock,
    out: *mut u8,
    out_cap: usize,
) -> usize {
    let (Some(dock), false) = (dock(family, ten_bit), blocks.is_null()) else { return 0 };
    let blocks = unsafe { blocks.cast::<[QuantBlock; 16]>().read_unaligned() };
    let body = catch_unwind(|| kvino::encode_strip_quantized(dock, sx as usize, sy as usize, &blocks).ok())
        .ok()
        .flatten();
    write_body(body, out, out_cap)
}

/// The packed RGB888 surface a reference call reads, if the strip at `(sx, sy)` lies inside it.
///
/// # Safety
/// `rgb` must be null or point to `rgb_len` readable bytes.
unsafe fn reference_surface<'a>(
    dock: DockProfile,
    rgb: *const u8,
    rgb_len: usize,
    w_pad: u32,
    sx: u32,
    sy: u32,
) -> Option<&'a [u8]> {
    if rgb.is_null() || rgb_len > isize::MAX as usize {
        return None;
    }
    let (strip_w, strip_h) = dock.strip_dims();
    let (w_pad, sx, sy) = (w_pad as usize, sx as usize, sy as usize);
    // The codec reads whole strips at strip-aligned positions; anything else is a caller bug.
    if w_pad % strip_w != 0 || sx % strip_w != 0 || sy % strip_h != 0 || sx + strip_w > w_pad {
        return None;
    }
    let needed = (sy + strip_h).checked_mul(w_pad)?.checked_mul(3)?;
    (needed <= rgb_len).then(|| unsafe { std::slice::from_raw_parts(rgb, rgb_len) })
}

/// # Safety
/// `rgb` must point to `rgb_len` readable bytes and `out`, unless null, to `out_cap` writable
/// bytes. Neither may overlap or be mutated during the call.
#[no_mangle]
pub unsafe extern "C" fn dll_reference_encode_strip(
    family: u32,
    ten_bit: bool,
    rgb: *const u8,
    rgb_len: usize,
    w_pad: u32,
    sx: u32,
    sy: u32,
    out: *mut u8,
    out_cap: usize,
) -> usize {
    let Some(dock) = dock(family, ten_bit) else { return 0 };
    let Some(rgb) = (unsafe { reference_surface(dock, rgb, rgb_len, w_pad, sx, sy) }) else { return 0 };
    let body = catch_unwind(|| kvino::encode_strip(dock, w_pad as usize, rgb, sx as usize, sy as usize).ok())
        .ok()
        .flatten();
    write_body(body, out, out_cap)
}

/// # Safety
/// `rgb` must point to `rgb_len` readable bytes and `out` to 16 writable `DLLQuantBlock`s (no
/// alignment needed).
#[no_mangle]
pub unsafe extern "C" fn dll_reference_quantize_strip(
    family: u32,
    ten_bit: bool,
    rgb: *const u8,
    rgb_len: usize,
    w_pad: u32,
    sx: u32,
    sy: u32,
    out: *mut QuantBlock,
) -> bool {
    let (Some(dock), false) = (dock(family, ten_bit), out.is_null()) else { return false };
    let Some(rgb) = (unsafe { reference_surface(dock, rgb, rgb_len, w_pad, sx, sy) }) else { return false };
    let blocks = catch_unwind(|| kvino::quantize_strip(dock, w_pad as usize, rgb, sx as usize, sy as usize).ok());
    let Ok(Some(blocks)) = blocks else { return false };
    unsafe { out.cast::<[QuantBlock; 16]>().write_unaligned(blocks) };
    true
}

// ---- offline replay benchmark ---------------------------------------------------------------

/// [`host::bench::Replay`] behind the C ABI, so a host-side strip producer (the GPU encoder) is
/// measured through the same engine path as the CPU feed. Records are checksummed the way
/// `replay run` does it, in a separate call so the hashing stays out of measured time.
pub struct DLLBench {
    replay: host::bench::Replay,
    /// Records of the last step, waiting for [`dll_bench_account`].
    pending: Option<(u32, Vec<Vec<u8>>)>,
    totals: DLLBenchTotals,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DLLBenchStep {
    pub submit_ns: u64,
    pub take_ns: u64,
    pub plan_ns: u64,
    pub encode_ns: u64,
    pub pack_ns: u64,
    pub presentations: u32,
    pub sent_bytes: u64,
    pub skipped: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DLLBenchTotals {
    pub checksum: u64,
    pub record_bytes: u64,
    pub sent_bytes: u64,
    pub presents: u64,
    pub skipped: u64,
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

fn fnv(mut hash: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        hash = (hash ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

impl DLLBench {
    fn finish_step(&mut self, step: host::bench::Step, out: *mut DLLBenchStep) {
        let ns = |d: std::time::Duration| d.as_nanos().min(u128::from(u64::MAX)) as u64;
        let s = step.stages;
        let summary = DLLBenchStep {
            submit_ns: ns(s.submit),
            take_ns: ns(s.take),
            plan_ns: ns(s.plan),
            encode_ns: ns(s.encode),
            pack_ns: ns(s.pack),
            presentations: step.presentations,
            sent_bytes: step.sent_bytes as u64,
            skipped: step.skipped,
        };
        self.totals.skipped += u64::from(step.skipped);
        if step.presentations > 0 {
            self.totals.presents += 1;
            self.totals.sent_bytes += step.sent_bytes as u64;
            self.pending = Some((step.presentations, step.records));
        }
        if let Some(out) = unsafe { out.as_mut() } {
            *out = summary;
        }
    }
}

/// # Safety
/// `gamma` must be null or point to 768 readable bytes (R, G, B tables of 256).
#[no_mangle]
pub unsafe extern "C" fn dll_bench_new(
    family: u32,
    width: u32,
    height: u32,
    fps: f64,
    gamma: *const u8,
) -> *mut DLLBench {
    let family = match family {
        0 => Family::Ella,
        1 => Family::Ridge,
        2 => Family::Navarro,
        _ => return std::ptr::null_mut(),
    };
    if width == 0 || height == 0 || !(fps > 0.0) {
        return std::ptr::null_mut();
    }
    let lut = (!gamma.is_null()).then(|| {
        let bytes = unsafe { std::slice::from_raw_parts(gamma, 768) };
        let mut lut = [[0u8; 256]; 3];
        for (channel, table) in lut.iter_mut().enumerate() {
            table.copy_from_slice(&bytes[channel * 256..(channel + 1) * 256]);
        }
        lut
    });
    let replay = catch_unwind(|| host::bench::Replay::new(family, width as usize, height as usize, fps, lut));
    match replay {
        Ok(Ok(replay)) => Box::into_raw(Box::new(DLLBench {
            replay,
            pending: None,
            totals: DLLBenchTotals { checksum: FNV_OFFSET, ..DLLBenchTotals::default() },
        })),
        _ => std::ptr::null_mut(),
    }
}

/// # Safety
/// `bench` must come from `dll_bench_new` and not be freed; `out`, unless null, must be writable.
#[no_mangle]
pub unsafe extern "C" fn dll_bench_encoder_profile(bench: *const DLLBench, out: *mut DLLEncoderProfile) -> bool {
    let (Some(bench), Some(out)) = (unsafe { bench.as_ref() }, unsafe { out.as_mut() }) else { return false };
    let Some((encoder, padded_width, padded_height)) = bench.replay.encoder_profile() else { return false };
    *out = encoder_profile(encoder, padded_width as u32, padded_height as u32);
    true
}

/// # Safety
/// `bench` must come from `dll_bench_new` and not be freed.
#[no_mangle]
pub unsafe extern "C" fn dll_bench_take_full_request(bench: *mut DLLBench) -> bool {
    unsafe { bench.as_ref() }.is_some_and(|bench| bench.replay.take_full_request())
}

/// # Safety
/// `bench` must come from `dll_bench_new` and not be freed. `bgra` must point to `bgra_len`
/// readable bytes; `dirty`, unless null, to `dirty_count * 4` aligned `uint32_t`s; `out`, unless
/// null, must be writable.
#[no_mangle]
pub unsafe extern "C" fn dll_bench_step_surface(
    bench: *mut DLLBench,
    bgra: *const u8,
    bgra_len: usize,
    stride: usize,
    dirty: *const u32,
    dirty_count: usize,
    out: *mut DLLBenchStep,
) -> bool {
    let Some(bench) = (unsafe { bench.as_mut() }) else { return false };
    if bgra.is_null() || bgra_len > isize::MAX as usize || dirty_count > (isize::MAX as usize) / 16 {
        return false;
    }
    let bgra = unsafe { std::slice::from_raw_parts(bgra, bgra_len) };
    let rects: Option<Vec<host::Rect>> = (!dirty.is_null()).then(|| {
        let quads = unsafe { std::slice::from_raw_parts(dirty, dirty_count * 4) };
        quads
            .chunks_exact(4)
            .map(|q| {
                let (x, y) = (q[0] as usize, q[1] as usize);
                (x, y, x + q[2] as usize, y + q[3] as usize)
            })
            .collect()
    });
    let Ok(step) = catch_unwind(AssertUnwindSafe(|| bench.replay.step(bgra, stride, rects.as_deref()))) else {
        return false;
    };
    bench.finish_step(step, out);
    true
}

/// # Safety
/// `bench` must come from `dll_bench_new` and not be freed. The strip arrays follow
/// `dll_engine_submit_encoded`'s rules; `out`, unless null, must be writable.
#[no_mangle]
pub unsafe extern "C" fn dll_bench_step_encoded(
    bench: *mut DLLBench,
    ten_bit: bool,
    count: usize,
    strip_index: *const u32,
    hash: *const u64,
    bodies: *const u8,
    bodies_len: usize,
    body_offsets: *const u32,
    out: *mut DLLBenchStep,
) -> bool {
    let Some(bench) = (unsafe { bench.as_mut() }) else { return false };
    if count > 0 && (strip_index.is_null() || hash.is_null() || bodies.is_null() || body_offsets.is_null()) {
        return false;
    }
    if count > (isize::MAX as usize) / 8 || bodies_len > isize::MAX as usize {
        return false;
    }
    let (indices, hashes, offsets, bytes): (&[u32], &[u64], &[u32], &[u8]) = if count == 0 {
        (&[], &[], &[0], &[])
    } else {
        unsafe {
            (
                std::slice::from_raw_parts(strip_index, count),
                std::slice::from_raw_parts(hash, count),
                std::slice::from_raw_parts(body_offsets, count + 1),
                std::slice::from_raw_parts(bodies, bodies_len),
            )
        }
    };
    if !offsets.windows(2).all(|pair| pair[0] <= pair[1]) || offsets[count] as usize > bodies_len {
        return false;
    }
    let strips = (0..count)
        .map(|i| (indices[i] as usize, hashes[i], &bytes[offsets[i] as usize..offsets[i + 1] as usize]));
    let Ok(Some(step)) = catch_unwind(AssertUnwindSafe(|| bench.replay.step_encoded(ten_bit, strips))) else {
        return false;
    };
    bench.finish_step(step, out);
    true
}

/// # Safety
/// `bench` must come from `dll_bench_new` and not be freed.
#[no_mangle]
pub unsafe extern "C" fn dll_bench_account(bench: *mut DLLBench) {
    let Some(bench) = (unsafe { bench.as_mut() }) else { return };
    let Some((presentations, records)) = bench.pending.take() else { return };
    let mut hash = fnv(bench.totals.checksum, &presentations.to_le_bytes());
    for record in &records {
        bench.totals.record_bytes += record.len() as u64;
        hash = fnv(hash, &(record.len() as u32).to_le_bytes());
        hash = fnv(hash, record);
    }
    bench.totals.checksum = hash;
}

/// # Safety
/// `bench` must come from `dll_bench_new` and not be freed; `out`, unless null, must be writable.
#[no_mangle]
pub unsafe extern "C" fn dll_bench_totals(bench: *const DLLBench, out: *mut DLLBenchTotals) {
    if let (Some(bench), Some(out)) = (unsafe { bench.as_ref() }, unsafe { out.as_mut() }) {
        *out = bench.totals;
    }
}

/// # Safety
/// `bench` must be null or come from `dll_bench_new`; it is freed and must not be used again.
#[no_mangle]
pub unsafe extern "C" fn dll_bench_free(bench: *mut DLLBench) {
    if !bench.is_null() {
        drop(unsafe { Box::from_raw(bench) });
    }
}

/// `DLL_STRIP_BODY_MAX` and `DLLQuantBlock` in dllite.h.
const _: () = assert!(kvino::STRIP_BODY_MAX == 16384);
const _: () = assert!(std::mem::size_of::<QuantBlock>() == 3 * 64 * 2);
const _: () = assert!(std::mem::size_of::<DLLEncoderProfile>() == 40);

/// # Safety
/// `engine` must come from `dll_engine_start`; it is freed and must not be used afterwards.
/// The caller must exclude concurrent calls using this engine, and must not stop from a callback
/// (which runs on the thread this function joins).
#[no_mangle]
pub unsafe extern "C" fn dll_engine_stop(engine: *mut DLLEngine) {
    if engine.is_null() {
        return;
    }
    let mut engine = unsafe { Box::from_raw(engine) };
    engine.shared.request_stop();
    if let Some(thread) = engine.thread.take() {
        let _ = thread.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_lengths_fit_rust_slices_in_bytes_not_elements() {
        assert_eq!(dirty_len(0), Some(0));
        assert_eq!(dirty_len(2), Some(8));
        let max = isize::MAX as usize / (4 * std::mem::size_of::<u32>());
        assert_eq!(dirty_len(max), Some(max * 4));
        assert_eq!(dirty_len(max + 1), None);
        assert_eq!(dirty_len(usize::MAX / 4 + 1), None);
        assert_eq!(dirty_len(usize::MAX), None);
    }

    #[test]
    fn invalid_frame_lengths_return_before_constructing_slices() {
        // No engine thread or USB session is needed to exercise the public boundary.
        let mut engine = DLLEngine { shared: Shared::new(), thread: None };
        let bgra = [0u8; 4];
        let dirty = [0u32; 4];
        unsafe {
            // Arithmetic overflow, then a representable span above isize::MAX.
            dll_engine_submit_frame(&mut engine, 0, bgra.as_ptr(), usize::MAX, 1, 3, std::ptr::null(), 0);
            dll_engine_submit_frame(&mut engine, 0, bgra.as_ptr(), isize::MAX as usize, 1, 2, std::ptr::null(), 0);
            // An invalid damage count must be rejected before either buffer is interpreted.
            dll_engine_submit_frame(&mut engine, 0, bgra.as_ptr(), 4, 1, 1, dirty.as_ptr(), usize::MAX);
            dll_engine_submit_frame(
                &mut engine, 0, bgra.as_ptr(), 4, 1, 1, dirty.as_ptr(),
                isize::MAX as usize / (4 * std::mem::size_of::<u32>()) + 1,
            );
        }
        // Rejected input must not poison engine state for later control calls.
        unsafe { dll_engine_set_display_power(&mut engine, 0, true) };
        assert!(!engine.shared.stopping());
    }

    fn patterned_rgb(width: usize, height: usize) -> Vec<u8> {
        (0..width * height * 3).map(|i| ((i * 37 + (i / (width * 3)) * 19) % 256) as u8).collect()
    }

    /// The stateless calls Swift's byte-equality tests rest on agree with each other and with
    /// the engine's own CPU path, on every family and depth.
    #[test]
    fn quantised_strips_through_the_c_abi_match_the_reference_encode() {
        let (w, h) = (256u32, 32u32);
        let rgb = patterned_rgb(w as usize, h as usize);
        for family in 0..3u32 {
            for ten_bit in [false, true] {
                let dock = dock(family, ten_bit).unwrap();
                let (strip_w, strip_h) = dock.strip_dims();
                for (sx, sy) in kvino::strip_coords(dock, w as usize, h as usize, None).unwrap() {
                    let (sx, sy) = (sx as u32, sy as u32);
                    let mut blocks = [QuantBlock::ZERO; 16];
                    let mut via_blocks = vec![0u8; DLL_BODY_MAX];
                    let mut reference = vec![0u8; DLL_BODY_MAX];
                    unsafe {
                        assert!(dll_reference_quantize_strip(family, ten_bit, rgb.as_ptr(), rgb.len(), w, sx, sy, blocks.as_mut_ptr()));
                        let n = dll_encode_strip_quantized(family, ten_bit, sx, sy, blocks.as_ptr(), via_blocks.as_mut_ptr(), via_blocks.len());
                        let m = dll_reference_encode_strip(family, ten_bit, rgb.as_ptr(), rgb.len(), w, sx, sy, reference.as_mut_ptr(), reference.len());
                        assert!(n >= 16);
                        assert_eq!(via_blocks[..n], reference[..m], "family {family} ten_bit {ten_bit} ({sx}, {sy})");
                        assert_eq!(
                            reference[..m],
                            kvino::encode_strip(dock, w as usize, &rgb, sx as usize, sy as usize).unwrap()[..]
                        );
                    }
                    // A strip that does not fit, or is not at a strip position, is refused.
                    unsafe {
                        let mut out = [0u8; 4];
                        let n = dll_encode_strip_quantized(family, ten_bit, sx, sy, blocks.as_ptr(), out.as_mut_ptr(), out.len());
                        assert!(n > out.len());
                        assert_eq!(out, [0; 4]);
                        let short = rgb.len() - 1;
                        let last = (w - strip_w as u32, h - strip_h as u32);
                        assert_eq!(dll_reference_encode_strip(family, ten_bit, rgb.as_ptr(), short, w, last.0, last.1, std::ptr::null_mut(), 0), 0);
                        assert!(!dll_reference_quantize_strip(family, ten_bit, rgb.as_ptr(), rgb.len(), w, 1, 0, blocks.as_mut_ptr()));
                        assert!(!dll_reference_quantize_strip(family, ten_bit, rgb.as_ptr(), rgb.len(), w, w, 0, blocks.as_mut_ptr()));
                    }
                }
            }
        }
        unsafe {
            let blocks = [QuantBlock::ZERO; 16];
            let mut out = vec![0u8; DLL_BODY_MAX];
            assert_eq!(dll_encode_strip_quantized(3, false, 0, 0, blocks.as_ptr(), out.as_mut_ptr(), out.len()), 0);
            assert_eq!(dll_encode_strip_quantized(1, false, 0, 0, std::ptr::null(), out.as_mut_ptr(), out.len()), 0);
            assert_eq!(dll_encode_strip_quantized(1, false, 1 << 16, 0, blocks.as_ptr(), out.as_mut_ptr(), out.len()), 0);
            // Unaligned blocks are read as they are.
            let mut bytes = vec![0u8; std::mem::size_of::<[QuantBlock; 16]>() + 1];
            let unaligned = bytes.as_mut_ptr().add(1).cast::<QuantBlock>();
            assert!(dll_reference_quantize_strip(1, false, patterned_rgb(64, 16).as_ptr(), 64 * 16 * 3, 64, 0, 0, unaligned));
            assert!(dll_encode_strip_quantized(1, false, 0, 0, unaligned, out.as_mut_ptr(), out.len()) >= 16);
            assert_eq!(dll_reference_encode_strip(1, false, std::ptr::null(), 0, 64, 0, 0, out.as_mut_ptr(), out.len()), 0);
        }
    }

    /// The bench API reproduces `Replay` and checksums each accounted present.
    #[test]
    fn the_bench_api_drives_a_replay() {
        let (w, h) = (100u32, 20u32);
        let identity: Vec<u8> = (0..768).map(|i| (i % 256) as u8).collect();
        unsafe {
            assert!(dll_bench_new(7, w, h, 60.0, std::ptr::null()).is_null());
            assert!(dll_bench_new(1, w, h, 0.0, std::ptr::null()).is_null());
            assert!(dll_bench_new(1, w, h, 60.0, identity.as_ptr()).is_null());
            let bench = dll_bench_new(1, w, h, 60.0, std::ptr::null());
            assert!(!bench.is_null());
            let mut profile = DLLEncoderProfile::default();
            assert!(dll_bench_encoder_profile(bench, &mut profile));
            assert_eq!((profile.family, profile.padded_width, profile.padded_height), (1, 128, 32));
            assert!(dll_bench_take_full_request(bench));
            let bgra: Vec<u8> = (0..(w * h * 4) as usize).map(|i| (i * 7 % 251) as u8).collect();
            let mut step = DLLBenchStep::default();
            assert!(dll_bench_step_surface(bench, bgra.as_ptr(), bgra.len(), w as usize * 4, std::ptr::null(), 0, &mut step));
            assert!(step.presentations > 0);
            let mut totals = DLLBenchTotals::default();
            dll_bench_totals(bench, &mut totals);
            assert_eq!((totals.checksum, totals.record_bytes, totals.presents), (FNV_OFFSET, 0, 1));
            dll_bench_account(bench);
            dll_bench_totals(bench, &mut totals);
            assert!(totals.checksum != FNV_OFFSET && totals.record_bytes > 0);
            dll_bench_free(bench);
        }
    }

    const DLL_BODY_MAX: usize = kvino::STRIP_BODY_MAX;

    #[test]
    fn stateless_profiles_match_the_attached_head() {
        let mut engine = DLLEngine { shared: Shared::new(), thread: None };
        engine.shared.open_for_tests(0, DockProfile::for_family(Family::Navarro).unwrap(), 100, 10);
        let mut attached = DLLEncoderProfile::default();
        let (mut eight, mut ten) = (DLLEncoderProfile::default(), DLLEncoderProfile::default());
        unsafe {
            assert!(dll_engine_encoder_profile(&mut engine, 0, &mut attached));
            assert!(dll_strip_encoding_profile(2, false, &mut eight));
            assert!(dll_strip_encoding_profile(2, true, &mut ten));
            assert!(!dll_strip_encoding_profile(7, false, &mut ten));
            assert!(!dll_strip_encoding_profile(0, false, std::ptr::null_mut()));
        }
        assert_eq!(eight, DLLEncoderProfile { padded_width: 0, padded_height: 0, ..attached });
        assert_eq!((ten.dc_cmax, ten.ac_cmax, ten.chroma_ac_cmax, ten.ten_bit), (12, 11, 12, true));
        let mut ella = DLLEncoderProfile::default();
        assert!(unsafe { dll_strip_encoding_profile(0, false, &mut ella) });
        assert_eq!((ella.code_table, ella.strip_width, ella.strip_height), (1, 64, 16));
    }

    #[test]
    fn encoded_submissions_are_checked_at_the_boundary() {
        let mut engine = DLLEngine { shared: Shared::new(), thread: None };
        let ridge = DockProfile::for_family(Family::Ridge).unwrap();
        // Two Ridge strips across one strip row.
        engine.shared.open_for_tests(0, ridge, 100, 10);
        let mut profile = DLLEncoderProfile::default();
        unsafe {
            assert!(!dll_engine_encoder_profile(&mut engine, 1, &mut profile));
            assert!(!dll_engine_encoder_profile(&mut engine, 0, std::ptr::null_mut()));
            assert!(dll_engine_encoder_profile(&mut engine, 0, &mut profile));
        }
        assert_eq!(
            profile,
            DLLEncoderProfile {
                family: 1,
                code_table: 0,
                strip_width: 64,
                strip_height: 16,
                padded_width: 128,
                padded_height: 16,
                dc_cmax: 10,
                ac_cmax: 9,
                chroma_ac_cmax: 10,
                magic: [0x01, 0x28],
                ten_bit: false,
            }
        );
        unsafe {
            assert!(dll_engine_take_full_request(&mut engine, 0));
            assert!(!dll_engine_take_full_request(&mut engine, 0));
            assert!(!dll_engine_take_full_request(std::ptr::null_mut(), 0));
        }

        let rgb = patterned_rgb(128, 16);
        let strip = |sx: usize| kvino::encode_strip(ridge, 128, &rgb, sx, 0).unwrap();
        let (a, b) = (strip(0), strip(64));
        let bodies = [a.clone(), b.clone()].concat();
        let offsets = [0, a.len() as u32, bodies.len() as u32];
        let (indices, hashes) = ([0u32, 1], [5u64, 6]);
        let submit = |engine: &mut DLLEngine, count: usize, len: usize, offsets: &[u32], hashes: &[u64]| unsafe {
            dll_engine_submit_encoded(
                engine, 0, 100, 10, false, count, indices.as_ptr(), hashes.as_ptr(), bodies.as_ptr(), len, offsets.as_ptr(),
            )
        };
        // Backwards or overlong offsets, a zero hash, and impossible counts are all refused.
        assert!(!submit(&mut engine, 2, bodies.len(), &[0, bodies.len() as u32, a.len() as u32], &hashes));
        assert!(!submit(&mut engine, 2, bodies.len() - 1, &offsets, &hashes));
        assert!(!submit(&mut engine, 2, bodies.len(), &offsets, &[5, 0]));
        assert!(!submit(&mut engine, usize::MAX, bodies.len(), &offsets, &hashes));
        // Bodies swapped between the two indices do not fit their positions.
        assert!(!submit(&mut engine, 2, bodies.len(), &[0, b.len() as u32, bodies.len() as u32], &hashes));
        unsafe {
            assert!(!dll_engine_submit_encoded(
                &mut engine, 0, 100, 10, false, 1, std::ptr::null(), hashes.as_ptr(), bodies.as_ptr(), bodies.len(), offsets.as_ptr(),
            ));
            assert!(!dll_engine_submit_encoded(std::ptr::null_mut(), 0, 100, 10, false, 0, std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, std::ptr::null()));
            // An empty submission needs no buffers.
            assert!(dll_engine_submit_encoded(&mut engine, 0, 100, 10, false, 0, std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, std::ptr::null()));
        }
        assert!(submit(&mut engine, 2, bodies.len(), &offsets, &hashes));
        unsafe { dll_engine_add_gpu_time(&mut engine, 0, 250) };
        let mut heads = [DLLHeadStats::default(); MAX_HEADS];
        let count = unsafe { dll_engine_stats(&mut engine, std::ptr::null_mut(), heads.as_mut_ptr(), heads.len()) };
        assert_eq!(count, 1);
        assert_eq!((heads[0].frames_submitted, heads[0].gpu_us), (2, 250));
    }

    #[test]
    fn stats_report_only_attached_heads_and_tolerate_null_outputs() {
        let mut engine = DLLEngine { shared: Shared::new(), thread: None };
        let mut totals = DLLEngineStats { loop_passes: 9, budget_wait_us: 9 };
        let mut heads = [DLLHeadStats { head: 7, ..Default::default() }; MAX_HEADS];
        let count = unsafe { dll_engine_stats(&mut engine, &mut totals, heads.as_mut_ptr(), heads.len()) };
        assert_eq!(count, 0);
        assert_eq!(totals, DLLEngineStats::default());
        assert_eq!(heads[0].head, 7);
        unsafe {
            assert_eq!(dll_engine_stats(&mut engine, std::ptr::null_mut(), std::ptr::null_mut(), 4), 0);
            assert_eq!(dll_engine_stats(std::ptr::null_mut(), &mut totals, heads.as_mut_ptr(), 4), 0);
        }
    }
}
