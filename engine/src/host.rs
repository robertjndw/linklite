// SPDX-License-Identifier: GPL-2.0-or-later

//! The engine thread: engage the dock, light what is plugged into it, and forward frames the
//! display host hands over. Follows the vendored chimera daemon's session loop, with the Linux
//! virtual-display card replaced by per-head mailboxes the macOS host fills.

#[doc(hidden)]
pub mod bench;

use crate::edid::{self, Monitor};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use vino_chimera::kvino::{self, DockProfile, Family, StripEncoding};
use vino_chimera::session::{ControlSession, PresentEncodedError, PresentTiming};
use vino_chimera::pool::STREAMING_THREADS;
use vino_chimera::scanout::StripHasher;
use vino_chimera::MAX_CONNECTORS;

const HEARTBEAT_PERIOD: Duration = Duration::from_secs(3);
const PRESENCE_PERIOD: Duration = Duration::from_secs(1);
/// Presence probes sent, one per [`PRESENCE_PERIOD`], after a dock pushes a downstream change.
/// Three covers the two consecutive negatives a removal needs plus one for the change itself.
const PRESENCE_BURST: u8 = 3;
/// How often sockets with nothing attached are probed while another head is lit.
const EMPTY_SOCKET_PERIOD: Duration = Duration::from_secs(2);
const PRESENCE_GRACE: Duration = Duration::from_secs(10);
const PRESENCE_SILENT_LIMIT: u8 = 3;
const SEARCH_PERIOD: Duration = Duration::from_secs(2);
const RESTART_DELAY: Duration = Duration::from_secs(1);
/// How long a head's surface must be still before its recently changed strips are re-sent to
/// every dock buffer.
const SETTLE_DELAY: Duration = Duration::from_millis(100);

/// Sustained video rate the dock is fed, for all heads together. Vino measured DLM on this dock
/// family at 120 MB/s over one second and 99 MB/s over five. With no limit, full-screen motion
/// (App Exposé on two 4K screens) reset an M/Connect 2 off the USB bus within minutes. This caps
/// the video bytes each present reports, not every byte on the bus: status polls, cursor and other
/// control traffic, and USB protocol overhead come on top.
const VIDEO_BYTES_PER_SEC: f64 = 100_000_000.0;
/// Burst the budget may bank: one 4K keyframe presented to both of a Ridge dock's buffers.
const VIDEO_BURST_BYTES: f64 = 8_000_000.0;

/// Token bucket over video bytes. A frame goes out only while the bucket is positive; its size is
/// then charged, so one large frame can take it negative and hold later frames back until the
/// rate has paid for it. Held-back frames wait in their mailbox, where a newer one replaces them.
struct VideoBudget {
    tokens: f64,
    refilled: Instant,
}

impl VideoBudget {
    fn new() -> Self {
        Self { tokens: VIDEO_BURST_BYTES, refilled: Instant::now() }
    }

    fn refill(&mut self, now: Instant) {
        let earned = now.saturating_duration_since(self.refilled).as_secs_f64() * VIDEO_BYTES_PER_SEC;
        self.tokens = (self.tokens + earned).min(VIDEO_BURST_BYTES);
        self.refilled = now;
    }

    fn available(&mut self) -> bool {
        self.refill(Instant::now());
        self.tokens > 0.0
    }

    fn spend(&mut self, bytes: usize) {
        self.tokens -= bytes as f64;
    }

    /// Whether frames are being held back, as of the last refill.
    fn exhausted(&self) -> bool {
        self.tokens <= 0.0
    }

    /// How long to sleep: no longer than `frame_wait`, and no longer than it takes the bucket to
    /// come back above zero.
    fn wait_hint(&mut self, frame_wait: Duration) -> Duration {
        self.refill(Instant::now());
        if self.tokens > 0.0 {
            return frame_wait;
        }
        Duration::from_secs_f64(-self.tokens / VIDEO_BYTES_PER_SEC + 0.000_5).min(frame_wait)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Searching = 0,
    Engaged = 1,
    Retrying = 2,
}

/// What the engine tells the display host. Implemented over the C callbacks in `ffi`.
pub trait Events: Send + 'static {
    /// `hardware_cursor`: the dock draws the pointer itself, so the host must leave it out of the
    /// frames and report it through `set_cursor_shape` / `move_cursor` instead.
    fn attached(&self, head: u8, monitor: &Monitor, mode: Mode, edid: &[u8], hardware_cursor: bool);
    fn detached(&self, head: u8);
    fn state(&self, state: State, message: &str);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub width: usize,
    pub height: usize,
    pub refresh_hz: u32,
}

/// Geometry a head accepts frames at: the programmed mode, that mode rounded up to whole codec
/// strips (the raster the dock is sent), and the strip size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Accepting {
    width: usize,
    height: usize,
    padded_width: usize,
    padded_height: usize,
    strip_w: usize,
    strip_h: usize,
    /// What a producer that codes strips itself must match; `None` for a dock the vendored
    /// profile table cannot name, which can then only be fed surfaces.
    encoder: Option<EncoderProfile>,
}

/// The dock a head's strips are coded for, as an outside strip producer needs to know it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncoderProfile {
    pub family: Family,
    pub encoding: StripEncoding,
}

impl EncoderProfile {
    fn of(profile: DockProfile) -> Option<Self> {
        Some(Self { family: profile.family()?, encoding: profile.strip_encoding() })
    }
}

impl Accepting {
    fn tiles(&self) -> (usize, usize) {
        (self.padded_width / self.strip_w, self.padded_height / self.strip_h)
    }
}

/// A half-open `(x0, y0, x1, y1)` pixel rectangle of a head's surface.
pub type Rect = (usize, usize, usize, usize);

/// Minimum readable span of a nonempty BGRA image, without trailing row padding. This is also
/// the maximum span the C boundary may turn into a Rust slice.
pub(crate) fn frame_len(stride: usize, width: usize, height: usize) -> Option<usize> {
    let row_bytes = width.checked_mul(4)?;
    if width == 0 || height == 0 || stride < row_bytes {
        return None;
    }
    stride
        .checked_mul(height - 1)?
        .checked_add(row_bytes)
        .filter(|len| *len <= isize::MAX as usize)
}

/// Strips a producer hashed and coded itself, waiting for the engine (see
/// [`Shared::submit_encoded`]).
#[derive(Default)]
struct EncodedStrips {
    /// The newest hash and body of each strip. A strip coded again before the engine takes it
    /// replaces its older copy: only the content on screen now is worth sending.
    strips: Vec<Option<(u64, Vec<u8>)>>,
    /// Indices of the `Some` entries of `strips`, in the order they first arrived.
    changed: Vec<usize>,
}

/// What [`Shared::take`] hands the engine.
#[derive(Debug, PartialEq, Eq)]
enum Taken {
    /// The surface in the shadow moved: in these strip-aligned areas, or (`None`) everywhere.
    Surface(Option<Vec<Rect>>),
    /// These strips were coded again, as `(index, hash, body)`.
    Encoded(Vec<(usize, u64, Vec<u8>)>),
}

/// How a head was last fed, which decides how its owed retransmissions are presented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Feed {
    /// Nothing presented yet.
    Nothing,
    /// From the RGB shadow.
    Surface,
    /// From strip bodies the scanout caches.
    Encoded,
}

#[derive(Default)]
struct Mailbox {
    accepting: Option<Accepting>,
    /// Latest captured surface, packed RGB888 at the padded size.
    surface: Vec<u8>,
    /// Hash of each strip's captured BGRA pixels when it was last converted into `surface`.
    /// Capture reports areas that changed, often far larger than what really did (a terminal
    /// window for a blinking cursor); hashing the raw pixels per strip finds the strips that
    /// actually moved, and only those are converted and handed to the codec.
    strip_hashes: Vec<u64>,
    /// Reused capture-damage scratch; protected by the same lock as the surface.
    candidates: Vec<bool>,
    /// Strips whose pixels changed since the engine last took the surface.
    changed: Vec<bool>,
    changed_count: usize,
    /// The whole surface is new to the engine (first frame, or a new mode).
    all_changed: bool,
    /// Latest display-power request not yet acted on.
    power: Option<bool>,
    /// The virtual display's gamma transfer table (R, G, B), applied while the engine copies
    /// the surface out. Capture returns pre-gamma pixels, so the host hands the table over.
    /// `None` is identity. Survives mode changes: it belongs to the display, not the mode.
    gamma: Option<[[u8; 256]; 3]>,
    /// `Some` while the head is fed coded strips rather than BGRA frames; the surface fields
    /// above are then unused. Whichever kind of submit comes first after a reset picks the mode.
    encoded: Option<EncodedStrips>,
    /// A strip producer must code every strip from scratch: the head has a fresh mailbox, the
    /// gamma table changed (the producer applies it), or the scanout lacked a body it needed.
    full_request: bool,
}

impl Mailbox {
    /// An empty mailbox for `accepting` that keeps the display's gamma table.
    fn reset(&mut self, accepting: Option<Accepting>) {
        let gamma = self.gamma;
        *self = Mailbox { accepting, gamma, full_request: accepting.is_some(), ..Mailbox::default() };
    }
}

/// A pointer image for the dock's hardware cursor: BGRA, top row first, tightly packed.
pub struct CursorShape {
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub hot_x: i32,
    pub hot_y: i32,
}

/// Latest pointer state from the host. Generations let the engine apply only what changed.
#[derive(Default)]
struct CursorBox {
    shape: Option<Arc<CursorShape>>,
    shape_gen: u64,
    /// Head under the pointer and the hotspot's position on it, in the head's mode pixels.
    position: Option<(u8, i32, i32)>,
    position_gen: u64,
}

/// One head's performance counters since the host last took them ([`Shared::take_stats`]).
/// Each is an independent relaxed tally, so a snapshot may split one present's increments across
/// two reports. Times accumulate in nanoseconds and are reported in microseconds.
#[derive(Default)]
struct HeadCounters {
    /// Whether the head is accepting frames; only attached heads are reported.
    attached: AtomicBool,
    /// Set when capture leaves a frame the engine has not taken yet, cleared when the engine takes
    /// the mailbox. Read without the lock, only to tell a budget-held frame from an idle head.
    frame_ready: AtomicBool,
    frames_submitted: AtomicU64,
    submit_ns: AtomicU64,
    take_ns: AtomicU64,
    present_calls: AtomicU64,
    presents: AtomicU64,
    skipped: AtomicU64,
    coarse_fallbacks: AtomicU64,
    bytes_sent: AtomicU64,
    plan_ns: AtomicU64,
    encode_ns: AtomicU64,
    pack_ns: AtomicU64,
    usb_ns: AtomicU64,
    poll_ns: AtomicU64,
    /// GPU time a strip producer reports for this head.
    gpu_ns: AtomicU64,
}

impl HeadCounters {
    /// Tally one `present_rgb_dirty` that put `sent` video bytes on the wire.
    fn record_present(&self, sent: usize, timing: PresentTiming) {
        self.present_calls.fetch_add(1, Ordering::Relaxed);
        if sent > 0 {
            self.presents.fetch_add(1, Ordering::Relaxed);
            self.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
        }
        if timing.coarse_fallback {
            self.coarse_fallbacks.fetch_add(1, Ordering::Relaxed);
        }
        add_duration(&self.plan_ns, timing.plan);
        add_duration(&self.encode_ns, timing.encode);
        add_duration(&self.pack_ns, timing.pack);
        add_duration(&self.usb_ns, timing.usb);
        add_duration(&self.poll_ns, timing.poll);
    }
}

#[derive(Default)]
struct Perf {
    heads: [HeadCounters; MAX_CONNECTORS],
    /// Engine loop wakeups that went on to visit the heads.
    loop_passes: AtomicU64,
    /// Time the engine loop slept while the video budget was exhausted.
    budget_wait_ns: AtomicU64,
}

fn add_duration(counter: &AtomicU64, elapsed: Duration) {
    counter.fetch_add(u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX), Ordering::Relaxed);
}

fn add_elapsed(counter: &AtomicU64, since: Instant) {
    add_duration(counter, since.elapsed());
}

fn take_us(counter: &AtomicU64) -> u64 {
    counter.swap(0, Ordering::Relaxed) / 1_000
}

/// One head's counters over a [`Shared::take_stats`] interval. Times are microseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeadStats {
    /// Frames capture handed over that matched the head's mode, changed or not.
    pub frames_submitted: u64,
    /// `present_rgb_dirty` calls: new frames and owed retransmissions, including ones that sent
    /// nothing.
    pub present_calls: u64,
    /// Presents that put video bytes on the wire.
    pub presents: u64,
    /// Head visits with a new frame or a due retransmission held back by an exhausted budget.
    pub skipped: u64,
    /// Presents whose change was too scattered for exact damage and went as a coarse superset.
    pub coarse_fallbacks: u64,
    /// Video bytes submitted to the dock.
    pub bytes_sent: u64,
    /// Capture-side hashing and BGRA→RGB conversion, including the mailbox lock wait; for coded
    /// strips, checking and copying the bodies.
    pub submit_us: u64,
    /// Engine-side copy of changed strips out of the mailbox, on every lit, in-budget visit.
    pub take_us: u64,
    pub plan_us: u64,
    pub encode_us: u64,
    /// Report and USB transfer assembly for each presentation.
    pub pack_us: u64,
    /// Video endpoint submission (`submit_video`).
    pub usb_us: u64,
    pub poll_us: u64,
    /// GPU time a strip producer reported ([`Shared::add_gpu_time`]).
    pub gpu_us: u64,
}

/// Engine-wide counters over a [`Shared::take_stats`] interval, and each attached head's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EngineStats {
    pub loop_passes: u64,
    pub budget_wait_us: u64,
    pub heads: [Option<HeadStats>; MAX_CONNECTORS],
}

pub struct Shared {
    stop: AtomicBool,
    mailboxes: [Mutex<Mailbox>; MAX_CONNECTORS],
    cursor: Mutex<CursorBox>,
    /// Each head's gamma table for the dock cursor, with a generation that changes with it. Kept
    /// apart from the mailbox so the engine never waits on a capture in progress to read it.
    cursor_gamma: [Mutex<(u64, Option<[[u8; 256]; 3]>)>; MAX_CONNECTORS],
    /// Signalled whenever a mailbox or the cursor changes, or stop is requested.
    signal: Mutex<u64>,
    wake: Condvar,
    perf: Perf,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            stop: AtomicBool::new(false),
            mailboxes: core::array::from_fn(|_| Mutex::new(Mailbox::default())),
            cursor: Mutex::new(CursorBox::default()),
            cursor_gamma: core::array::from_fn(|_| Mutex::new((0, None))),
            signal: Mutex::new(0),
            wake: Condvar::new(),
            perf: Perf::default(),
        })
    }

    fn notify(&self) {
        *self.signal.lock().unwrap() += 1;
        self.wake.notify_all();
    }

    /// Wait until notified or `timeout` passes.
    fn wait(&self, seen: &mut u64, timeout: Duration) {
        let guard = self.signal.lock().unwrap();
        let (guard, _) = self
            .wake
            .wait_timeout_while(guard, timeout, |count| *count == *seen && !self.stopping())
            .unwrap();
        *seen = *guard;
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.notify();
    }

    /// Accept one BGRA frame from the host, of which only `dirty` can have changed. Pass the
    /// whole `(0, 0, width, height)` rectangle when unknown, or an empty iterator for no damage.
    /// A new surface always reads the whole frame. Mismatched programmed modes are dropped.
    pub fn submit_frame(
        &self,
        head: usize,
        bgra: &[u8],
        stride: usize,
        width: usize,
        height: usize,
        dirty: impl IntoIterator<Item = Rect>,
    ) {
        let Some(counters) = self.perf.heads.get(head) else {
            return;
        };
        let started = Instant::now();
        if self.convert_frame(head, bgra, stride, width, height, dirty) {
            counters.frames_submitted.fetch_add(1, Ordering::Relaxed);
            add_elapsed(&counters.submit_ns, started);
        }
    }

    /// [`Shared::submit_frame`] for a head in range. Returns whether the frame was accepted (the
    /// head is accepting frames of this size), whatever it changed.
    fn convert_frame(
        &self,
        head: usize,
        bgra: &[u8],
        stride: usize,
        width: usize,
        height: usize,
        dirty: impl IntoIterator<Item = Rect>,
    ) -> bool {
        let mut guard = self.mailboxes[head].lock().unwrap();
        let mailbox = &mut *guard;
        let Some(a) = mailbox.accepting else {
            return false;
        };
        let Some(len) = frame_len(stride, width, height) else { return false };
        if (a.width, a.height) != (width, height) || bgra.len() < len {
            return false;
        }
        if mailbox.encoded.is_some() {
            mailbox.reset(Some(a));
        }
        let (tiles_x, tiles_y) = a.tiles();
        let fresh = mailbox.surface.len() != a.padded_width * 3 * a.padded_height;
        if fresh {
            mailbox.surface = vec![0; a.padded_width * 3 * a.padded_height];
            mailbox.strip_hashes = vec![0; tiles_x * tiles_y];
            mailbox.candidates = vec![false; tiles_x * tiles_y];
            mailbox.changed = vec![false; tiles_x * tiles_y];
            mailbox.changed_count = 0;
            mailbox.all_changed = true;
        }

        // Strips the capture says may have changed, grouped by strip row.
        mailbox.candidates.fill(false);
        let mut any = false;
        let mut mark = |(x0, y0, x1, y1): Rect| {
            let (x1, y1) = (x1.min(width), y1.min(height));
            if x0 >= x1 || y0 >= y1 {
                return;
            }
            for ty in y0 / a.strip_h..y1.div_ceil(a.strip_h) {
                for tx in x0 / a.strip_w..x1.div_ceil(a.strip_w) {
                    mailbox.candidates[ty * tiles_x + tx] = true;
                    any = true;
                }
            }
        };
        if fresh {
            mark((0, 0, width, height));
        } else {
            dirty.into_iter().for_each(mark);
        }
        if !any {
            return true;
        }
        let candidates = &mailbox.candidates;

        // Each strip row owns a disjoint band of the surface, so bands convert in parallel on the
        // shared worker pool.
        let band_bytes = a.padded_width * 3 * a.strip_h;
        let mut bands: Vec<(usize, &mut [u8], &mut [u64], &mut [bool], usize)> = mailbox
            .surface
            .chunks_mut(band_bytes)
            .zip(mailbox.strip_hashes.chunks_mut(tiles_x))
            .zip(mailbox.changed.chunks_mut(tiles_x))
            .enumerate()
            .filter(|(ty, _)| candidates[ty * tiles_x..(ty + 1) * tiles_x].contains(&true))
            .map(|(ty, ((band, hashes), changed))| (ty, band, hashes, changed, 0))
            .collect();
        let work = |(ty, band, hashes, changed, newly): &mut (usize, &mut [u8], &mut [u64], &mut [bool], usize)| {
            let y0 = *ty * a.strip_h;
            let rows = a.strip_h.min(height.saturating_sub(y0));
            for tx in 0..tiles_x {
                if !candidates[*ty * tiles_x + tx] {
                    continue;
                }
                let x0 = tx * a.strip_w;
                let cols = a.strip_w.min(width - x0);
                let hash = bgra_strip_hash(bgra, stride, x0, y0, cols, rows);
                if hash == hashes[tx] {
                    continue;
                }
                hashes[tx] = hash;
                for dy in 0..rows {
                    let source = &bgra[(y0 + dy) * stride + x0 * 4..(y0 + dy) * stride + (x0 + cols) * 4];
                    let target = &mut band[dy * a.padded_width * 3 + x0 * 3..][..cols * 3];
                    bgra_to_rgb(target, source);
                }
                if !changed[tx] {
                    changed[tx] = true;
                    *newly += 1;
                }
            }
        };
        // A few bands (a cursor blink, a typed character) are cheaper than waking a worker.
        let threads = if bands.len() < 4 { 1 } else { STREAMING_THREADS };
        vino_chimera::pool::for_each_mut(&mut bands, threads, |_, band| work(band));
        let newly: usize = bands.iter().map(|band| band.4).sum();
        drop(bands);
        mailbox.changed_count += newly;
        if newly == 0 && !mailbox.all_changed {
            return true;
        }
        self.perf.heads[head].frame_ready.store(true, Ordering::Relaxed);
        drop(guard);
        self.notify();
        true
    }

    pub fn set_power(&self, head: usize, on: bool) {
        if let Some(slot) = self.mailboxes.get(head) {
            slot.lock().unwrap().power = Some(on);
            self.notify();
        }
    }

    /// Set the head's gamma table (`None` or an identity table clears it). A different table
    /// repaints the whole surface on the next visit, even if capture sends nothing new.
    pub fn set_gamma(&self, head: usize, lut: Option<[[u8; 256]; 3]>) {
        let Some(slot) = self.mailboxes.get(head) else { return };
        let is_identity =
            |lut: &[[u8; 256]; 3]| lut.iter().all(|ch| ch.iter().enumerate().all(|(i, &v)| usize::from(v) == i));
        let lut = lut.filter(|lut| !is_identity(lut));
        let mut mailbox = slot.lock().unwrap();
        if mailbox.gamma == lut {
            return;
        }
        mailbox.gamma = lut;
        // A producer that codes strips itself applies the table, so it has to code them all again.
        mailbox.full_request |= mailbox.accepting.is_some();
        {
            // The dock draws the pointer itself, so it gets the same table (see sync_cursor).
            let mut cursor_gamma = self.cursor_gamma[head].lock().unwrap();
            *cursor_gamma = (cursor_gamma.0 + 1, lut);
        }
        self.notify();
        if mailbox.accepting.is_none() || mailbox.surface.is_empty() {
            return;
        }
        mailbox.all_changed = true;
        self.perf.heads[head].frame_ready.store(true, Ordering::Relaxed);
        drop(mailbox);
        self.notify();
    }

    /// Accept strips a producer hashed and coded itself, as `(strip index, hash, body)`: the
    /// alternative to [`Shared::submit_frame`] for a producer that does the pixel work (the GPU
    /// encoder). Indices are raster order over the padded surface's strips; hashes are the
    /// producer's own, only ever compared with each other, and never 0. Each body must be the
    /// strip coded for this head's [`EncoderProfile`], and is copied before this returns.
    ///
    /// The whole submission is refused, and nothing queued, when anything in it does not fit the
    /// head: a frame size other than its mode, a depth other than its link's, an index outside
    /// the surface, a zero hash, or a body that is not a strip at that index's position. A depth
    /// mismatch also asks for every strip again. Returns whether the strips were accepted.
    pub fn submit_encoded<'a>(
        &self,
        head: usize,
        width: usize,
        height: usize,
        ten_bit: bool,
        strips: impl IntoIterator<Item = (usize, u64, &'a [u8])>,
    ) -> bool {
        let Some(counters) = self.perf.heads.get(head) else { return false };
        let started = Instant::now();
        let Some(a) = self.mailboxes[head].lock().unwrap().accepting else { return false };
        if (a.width, a.height) != (width, height) {
            return false;
        }
        if a.encoder.is_some_and(|encoder| encoder.encoding.ten_bit != ten_bit) {
            self.request_full(head);
            return false;
        }
        // Checked and copied before the lock is taken again, so a large submission never holds
        // the engine off its mailbox.
        let (tiles_x, tiles_y) = a.tiles();
        let mut copied = Vec::new();
        for (index, hash, body) in strips {
            let (sx, sy) = ((index % tiles_x.max(1)) * a.strip_w, (index / tiles_x.max(1)) * a.strip_h);
            if index >= tiles_x * tiles_y || hash == 0 || !is_strip_at(body, sx, sy) {
                return false;
            }
            copied.push((index, hash, body.to_vec()));
        }

        let mut guard = self.mailboxes[head].lock().unwrap();
        let mailbox = &mut *guard;
        // The head may have been re-opened at another mode while the bodies were copied.
        if mailbox.accepting != Some(a) {
            return false;
        }
        if mailbox.encoded.is_none() {
            if !mailbox.surface.is_empty() {
                mailbox.reset(Some(a));
            }
            mailbox.encoded = Some(EncodedStrips { strips: vec![None; tiles_x * tiles_y], changed: Vec::new() });
        }
        let encoded = mailbox.encoded.as_mut().expect("set above");
        let any = !copied.is_empty();
        for (index, hash, body) in copied {
            if encoded.strips[index].replace((hash, body)).is_none() {
                encoded.changed.push(index);
            }
        }
        counters.frames_submitted.fetch_add(1, Ordering::Relaxed);
        if any {
            counters.frame_ready.store(true, Ordering::Relaxed);
        }
        drop(guard);
        add_elapsed(&counters.submit_ns, started);
        if any {
            self.notify();
        }
        true
    }

    /// Whether the head's strip producer must code every strip from scratch, clearing the
    /// request. The producer polls this before each frame.
    pub fn take_full_request(&self, head: usize) -> bool {
        self.mailboxes
            .get(head)
            .is_some_and(|slot| core::mem::take(&mut slot.lock().unwrap().full_request))
    }

    fn request_full(&self, head: usize) {
        let mut mailbox = self.mailboxes[head].lock().unwrap();
        mailbox.full_request |= mailbox.accepting.is_some();
    }

    /// What a strip producer for `head` must match, while the head accepts frames.
    pub fn encoder_profile(&self, head: usize) -> Option<(EncoderProfile, usize, usize)> {
        let a = self.mailboxes.get(head)?.lock().unwrap().accepting?;
        Some((a.encoder?, a.padded_width, a.padded_height))
    }

    /// Add GPU time a strip producer spent on `head` to its performance counters.
    pub fn add_gpu_time(&self, head: usize, elapsed: Duration) {
        if let Some(counters) = self.perf.heads.get(head) {
            add_duration(&counters.gpu_ns, elapsed);
        }
    }

    /// Open `head` as attaching a `width` x `height` mode on `profile` would, with no dock.
    #[cfg(test)]
    pub(crate) fn open_for_tests(&self, head: usize, profile: DockProfile, width: usize, height: usize) {
        let (padded_width, padded_height) = padded(profile, width, height);
        let (strip_w, strip_h) = profile.strip_dims();
        let encoder = EncoderProfile::of(profile);
        self.open(head, Accepting { width, height, padded_width, padded_height, strip_w, strip_h, encoder });
    }

    fn open(&self, head: usize, accepting: Accepting) {
        self.mailboxes[head].lock().unwrap().reset(Some(accepting));
        self.perf.heads[head].frame_ready.store(false, Ordering::Relaxed);
        self.perf.heads[head].attached.store(true, Ordering::Relaxed);
    }

    fn close(&self, head: usize) {
        self.mailboxes[head].lock().unwrap().reset(None);
        self.perf.heads[head].frame_ready.store(false, Ordering::Relaxed);
        self.perf.heads[head].attached.store(false, Ordering::Relaxed);
    }

    /// Whatever the head's producer left since the last call: strips it coded, or for a surface,
    /// `shadow` brought up to date with the strip-aligned areas that changed (`None`: all of it).
    /// Copies only those areas.
    fn take(&self, head: usize, shadow: &mut Vec<u8>) -> Option<Taken> {
        let mut guard = self.mailboxes[head].lock().unwrap();
        // Whatever this returns, the mailbox holds nothing new once it does.
        self.perf.heads[head].frame_ready.store(false, Ordering::Relaxed);
        let mailbox = &mut *guard;
        let a = mailbox.accepting?;
        if let Some(EncodedStrips { strips, changed }) = &mut mailbox.encoded {
            if changed.is_empty() {
                return None;
            }
            let bodies = changed
                .drain(..)
                .filter_map(|index| strips[index].take().map(|(hash, body)| (index, hash, body)))
                .collect();
            return Some(Taken::Encoded(bodies));
        }
        if !mailbox.all_changed && mailbox.changed_count == 0 {
            return None;
        }
        let (tiles_x, _) = a.tiles();
        if mailbox.all_changed || shadow.len() != mailbox.surface.len() {
            shadow.clear();
            match &mailbox.gamma {
                None => shadow.extend_from_slice(&mailbox.surface),
                Some(lut) => {
                    shadow.resize(mailbox.surface.len(), 0);
                    apply_gamma(shadow, &mailbox.surface, lut);
                }
            }
            mailbox.changed.fill(false);
            mailbox.changed_count = 0;
            mailbox.all_changed = false;
            return Some(Taken::Surface(None));
        }
        // Runs of changed strips along each strip row become one rectangle each.
        let row = a.padded_width * 3;
        let mut rects = Vec::new();
        for (ty, changed) in mailbox.changed.chunks_mut(tiles_x).enumerate() {
            let mut tx = 0;
            while tx < tiles_x {
                if !changed[tx] {
                    tx += 1;
                    continue;
                }
                let start = tx;
                while tx < tiles_x && changed[tx] {
                    changed[tx] = false;
                    tx += 1;
                }
                let (x0, x1) = (start * a.strip_w, tx * a.strip_w);
                let (y0, y1) = (ty * a.strip_h, (ty + 1) * a.strip_h);
                for y in y0..y1 {
                    let span = y * row + x0 * 3..y * row + x1 * 3;
                    match &mailbox.gamma {
                        None => shadow[span.clone()].copy_from_slice(&mailbox.surface[span]),
                        Some(lut) => apply_gamma(&mut shadow[span.clone()], &mailbox.surface[span], lut),
                    }
                }
                rects.push((x0, y0, x1, y1));
            }
        }
        mailbox.changed_count = 0;
        Some(Taken::Surface(Some(rects)))
    }

    fn take_power(&self, head: usize) -> Option<bool> {
        self.mailboxes[head].lock().unwrap().power.take()
    }

    /// Performance counters accumulated since the previous call, which are reset. Heads are
    /// reported while attached; a detached head's counters are discarded.
    pub fn take_stats(&self) -> EngineStats {
        EngineStats {
            loop_passes: self.perf.loop_passes.swap(0, Ordering::Relaxed),
            budget_wait_us: take_us(&self.perf.budget_wait_ns),
            heads: core::array::from_fn(|head| {
                let c = &self.perf.heads[head];
                let stats = HeadStats {
                    frames_submitted: c.frames_submitted.swap(0, Ordering::Relaxed),
                    present_calls: c.present_calls.swap(0, Ordering::Relaxed),
                    presents: c.presents.swap(0, Ordering::Relaxed),
                    skipped: c.skipped.swap(0, Ordering::Relaxed),
                    coarse_fallbacks: c.coarse_fallbacks.swap(0, Ordering::Relaxed),
                    bytes_sent: c.bytes_sent.swap(0, Ordering::Relaxed),
                    submit_us: take_us(&c.submit_ns),
                    take_us: take_us(&c.take_ns),
                    plan_us: take_us(&c.plan_ns),
                    encode_us: take_us(&c.encode_ns),
                    pack_us: take_us(&c.pack_ns),
                    usb_us: take_us(&c.usb_ns),
                    poll_us: take_us(&c.poll_ns),
                    gpu_us: take_us(&c.gpu_ns),
                };
                c.attached.load(Ordering::Relaxed).then_some(stats)
            }),
        }
    }

    pub fn set_cursor_shape(&self, shape: CursorShape) {
        let mut cursor = self.cursor.lock().unwrap();
        cursor.shape = Some(Arc::new(shape));
        cursor.shape_gen += 1;
        drop(cursor);
        self.notify();
    }

    /// `None` when the pointer is on none of the dock's heads.
    pub fn move_cursor(&self, position: Option<(u8, i32, i32)>) {
        let mut cursor = self.cursor.lock().unwrap();
        if cursor.position == position {
            return;
        }
        cursor.position = position;
        cursor.position_gen += 1;
        drop(cursor);
        self.notify();
    }
}

/// A premultiplied BGRA cursor image through a per-channel gamma table: each colour is
/// un-premultiplied, mapped, and premultiplied again, so edges keep their coverage.
fn gamma_premultiplied_bgra(bgra: &[u8], lut: &[[u8; 256]; 3]) -> Vec<u8> {
    let mut out = bgra.to_vec();
    for px in out.chunks_exact_mut(4) {
        let a = u32::from(px[3]);
        if a == 0 {
            continue;
        }
        // BGRA in memory: blue is channel 2 of the table, red channel 0.
        for (byte, channel) in [(0usize, 2usize), (1, 1), (2, 0)] {
            let straight = ((u32::from(px[byte]) * 255 + a / 2) / a).min(255) as usize;
            px[byte] = ((u32::from(lut[channel][straight]) * a + 127) / 255) as u8;
        }
    }
    out
}

/// Copy packed RGB888 `src` into `dst` through a per-channel gamma table. Large copies (a whole
/// 4K surface is about 25 MB: 4.8 ms on one core, 1.4 ms split across 8, benchmarked) are split
/// across the shared worker pool, since this runs on the engine thread while it holds the head's
/// mailbox lock.
fn apply_gamma(dst: &mut [u8], src: &[u8], lut: &[[u8; 256]; 3]) {
    const PARALLEL_BYTES: usize = 1 << 20;
    // Whole pixels per chunk, so no pixel straddles two calls.
    const CHUNK_BYTES: usize = 3 << 16;
    let serial = |dst: &mut [u8], src: &[u8]| {
        for (d, s) in dst.chunks_exact_mut(3).zip(src.chunks_exact(3)) {
            d[0] = lut[0][usize::from(s[0])];
            d[1] = lut[1][usize::from(s[1])];
            d[2] = lut[2][usize::from(s[2])];
        }
    };
    if dst.len() < PARALLEL_BYTES {
        serial(dst, src);
        return;
    }
    let mut chunks: Vec<(&mut [u8], &[u8])> = dst.chunks_mut(CHUNK_BYTES).zip(src.chunks(CHUNK_BYTES)).collect();
    vino_chimera::pool::for_each_mut(&mut chunks, STREAMING_THREADS, |_, (d, s)| serial(d, s));
}

/// Convert BGRA pixels to packed RGB888, `min(rgb.len() / 3, bgra.len() / 4)` of them.
fn bgra_to_rgb(rgb: &mut [u8], bgra: &[u8]) {
    #[cfg(target_arch = "aarch64")]
    let done = bgra_to_rgb_neon(rgb, bgra);
    #[cfg(not(target_arch = "aarch64"))]
    let done = 0;
    bgra_to_rgb_scalar(&mut rgb[done * 3..], &bgra[done * 4..]);
}

fn bgra_to_rgb_scalar(rgb: &mut [u8], bgra: &[u8]) {
    for (rgb, px) in rgb.chunks_exact_mut(3).zip(bgra.chunks_exact(4)) {
        rgb[0] = px[2];
        rgb[1] = px[1];
        rgb[2] = px[0];
    }
}

/// [`bgra_to_rgb_scalar`] 16 pixels at a time: `vld4q_u8` splits the channels into registers,
/// `vst3q_u8` interleaves them back without alpha. Returns how many pixels it converted (a
/// multiple of 16); the caller converts the rest.
#[cfg(target_arch = "aarch64")]
fn bgra_to_rgb_neon(rgb: &mut [u8], bgra: &[u8]) -> usize {
    use core::arch::aarch64::{uint8x16x3_t, vld4q_u8, vst3q_u8};
    let mut done = 0;
    for (rgb, px) in rgb.chunks_exact_mut(48).zip(bgra.chunks_exact(64)) {
        // SAFETY: NEON is part of the aarch64 baseline, and the chunks are exactly the 64 bytes
        // read and 48 bytes written.
        unsafe {
            let bgra = vld4q_u8(px.as_ptr());
            vst3q_u8(rgb.as_mut_ptr(), uint8x16x3_t(bgra.2, bgra.1, bgra.0));
        }
        done += 16;
    }
    done
}

/// Hash of one strip's BGRA pixels (`cols` x `rows` at `x0, y0`). Change detection only: two
/// different strips agreeing is vanishingly unlikely, and the same pixels always agree.
fn bgra_strip_hash(bgra: &[u8], stride: usize, x0: usize, y0: usize, cols: usize, rows: usize) -> u64 {
    let mut h = StripHasher::new(StripHasher::PRIME);
    for y in y0..y0 + rows {
        h.line(&bgra[y * stride + x0 * 4..y * stride + (x0 + cols) * 4]);
    }
    h.finish() | 1 // never 0, which marks a strip whose pixels are not yet known
}

/// Whether `body` reads as a coded strip whose top-left pixel is `(sx, sy)`. A body filed under
/// the wrong index would be framed at that index's place in the records yet decoded at its own
/// coordinates, painting it twice and leaving a hole.
fn is_strip_at(body: &[u8], sx: usize, sy: usize) -> bool {
    let at = |i: usize| usize::from(u16::from_le_bytes([body[i], body[i + 1]]));
    body.len() >= 16 && body[..2] == kvino::STRIP_MAGIC && at(2) == sx && at(4) == sy
}

fn padded(profile: DockProfile, width: usize, height: usize) -> (usize, usize) {
    let (strip_w, strip_h) = profile.strip_dims();
    (width.div_ceil(strip_w) * strip_w, height.div_ceil(strip_h) * strip_h)
}

struct Output {
    mode: Mode,
    lit: bool,
    padded_width: usize,
    padded_height: usize,
    /// The surface last handed to the codec. Updated in place from the mailbox's changed areas.
    /// Empty while the head is fed coded strips.
    shadow: Vec<u8>,
    feed: Feed,
    /// The EDID this head was lit with, used to reject the other socket's monitor echoed back
    /// by a shared EDID handler.
    edid: Vec<u8>,
    presence_debounce: u8,
    silent_probes: u8,
    presence_grace_until: Instant,
    /// Generation of the cursor shape this head's dock cursor holds; 0 for none.
    cursor_shape_gen: u64,
    /// Whether the dock is currently showing the pointer on this head.
    cursor_visible: bool,
    /// Generation of the gamma table the dock cursor image was uploaded through.
    cursor_gamma_gen: u64,
    /// When the last new frame was presented.
    last_change: Instant,
    /// Whether the settle pass has run since the last new frame.
    settled: bool,
    /// Earliest time an owed retransmission of the last frame may go out.
    next_repaint: Instant,
}

/// The engine thread body. Returns when stop is requested.
pub fn run(shared: Arc<Shared>, events: impl Events) {
    let mut failed_before = false;
    while !shared.stopping() {
        let engaged = std::panic::catch_unwind(std::panic::AssertUnwindSafe(ControlSession::engage))
            .unwrap_or_else(|_| Err("dock engage panicked".into()));
        let session = match engaged {
            Ok(session) => session,
            Err(error) => {
                let state = if failed_before { State::Retrying } else { State::Searching };
                events.state(state, &searching_message(&error));
                sleep_unless_stopped(&shared, SEARCH_PERIOD);
                continue;
            }
        };
        failed_before = false;
        let mut outputs: [Option<Output>; MAX_CONNECTORS] = core::array::from_fn(|_| None);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_session(&shared, &events, session, &mut outputs)
        }))
        .unwrap_or_else(|_| Err("engine panicked".into()));
        for (head, output) in outputs.iter_mut().enumerate() {
            if output.take().is_some() {
                shared.close(head);
                events.detached(head as u8);
            }
        }
        if let Err(error) = result {
            failed_before = true;
            events.state(State::Retrying, &format!("Dock session stopped: {error}. Reconnecting…"));
            sleep_unless_stopped(&shared, RESTART_DELAY);
        }
    }
}

fn searching_message(error: &str) -> String {
    if error.contains("Access") || error.contains("Busy") {
        format!("Dock is in use by another app (quit DisplayLink Manager): {error}")
    } else if error.contains("NoDevice") || error.contains("no DisplayLink") || error.contains("not found") {
        "Looking for the DisplayLink dock…".to_string()
    } else {
        format!("Cannot open the dock: {error}")
    }
}

fn sleep_unless_stopped(shared: &Shared, period: Duration) {
    let mut seen = *shared.signal.lock().unwrap();
    let deadline = Instant::now() + period;
    while !shared.stopping() {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        shared.wait(&mut seen, deadline - now);
    }
}

fn run_session(
    shared: &Shared,
    events: &impl Events,
    mut session: ControlSession,
    outputs: &mut [Option<Output>; MAX_CONNECTORS],
) -> Result<(), String> {
    let profile = session.profile();
    events.state(State::Engaged, "Dock connected. Looking for monitors…");

    // Discovery as the chimera daemon does it: a dock with one EDID handler shared between its
    // heads answers a blind fetch on an empty socket with the other head's monitor, so there the
    // presence probe gates the fetch.
    let discover_by_edid = !profile.shared_edid_handler();
    let reports_presence = profile.reports_presence();
    let mut found = 0;
    for head in 0..session.connectors() as u8 {
        if (discover_by_edid || !reports_presence || session.probe_head_present(head)? == Some(true))
            && attach(shared, events, &mut session, outputs, head, Discovery::Gated)?
        {
            found += 1;
        }
    }
    // A cold Ridge dock can report an occupied socket empty until its EDID handler has been
    // engaged for it (docs/ridge.md in the Vino tree). With nothing found, read each socket once,
    // keeping an EDID only if that socket then probes present.
    if found == 0 && !discover_by_edid && reports_presence {
        for head in 0..session.connectors() as u8 {
            if attach(shared, events, &mut session, outputs, head, Discovery::Blind)? {
                found += 1;
            }
        }
    }
    report_monitors(events, found);

    let frame_wait = Duration::from_millis(profile.frame_period_ms().max(1) as u64);
    let keepalive_period = Duration::from_millis(profile.status_period_ms().max(1) as u64);
    let mut next_keepalive = Instant::now() + keepalive_period;
    let mut next_heartbeat = Instant::now() + HEARTBEAT_PERIOD;
    // A dock with one EDID handler shared between its heads loses a lit connector's link to a
    // timed presence probe: measured on the M/Connect 2, probing both heads once a second kept
    // both monitors in standby, and they lit as soon as it stopped. DLM probes such a dock only
    // after it pushes its downstream-change notice, so this does the same, with a short burst so
    // a removal can still be confirmed twice before the head is dropped.
    let timed_presence = reports_presence && !profile.shared_edid_handler();
    let mut burst_probes_left = 0u8;
    let mut next_presence = Instant::now() + PRESENCE_PERIOD;
    let mut next_empty_scan = Instant::now() + EMPTY_SOCKET_PERIOD;
    let mut seen = *shared.signal.lock().unwrap();
    let mut budget = VideoBudget::new();
    let mut first_head = 0usize;
    let mut cursor_position_applied = 0u64;

    while !shared.stopping() {
        // With nothing owed on any head, sleep until new input or the next housekeeping deadline
        // instead of ticking every frame period: an idle desktop then costs a few wakeups a second.
        let busy = outputs.iter().enumerate().any(|(head, output)| {
            output.as_ref().is_some_and(|output| {
                output.lit && (!output.settled || session.owes_repaint(head as u8))
            })
        });
        // Whether this sleep is the budget holding video back, for the performance counters.
        let mut budget_starved = false;
        let wait = if busy {
            let hint = budget.wait_hint(frame_wait);
            budget_starved = budget.exhausted();
            hint
        } else {
            let now = Instant::now();
            // Only deadlines that will actually be acted on: a stale presence deadline for a dock
            // that is not being probed would otherwise wake this loop every millisecond.
            let nothing_lit = !outputs.iter().flatten().any(|output| output.lit);
            let presence_active = timed_presence || nothing_lit || burst_probes_left > 0;
            [
                Some(next_keepalive),
                Some(next_heartbeat),
                presence_active.then_some(next_presence),
                reports_presence.then_some(next_empty_scan),
            ]
            .into_iter()
            .flatten()
            .min()
            .map_or(frame_wait, |due| due.saturating_duration_since(now))
            .max(Duration::from_millis(1))
        };
        let slept = Instant::now();
        shared.wait(&mut seen, wait);
        if budget_starved {
            add_elapsed(&shared.perf.budget_wait_ns, slept);
        }
        if shared.stopping() {
            break;
        }
        shared.perf.loop_passes.fetch_add(1, Ordering::Relaxed);
        if profile.hw_cursor() {
            sync_cursor(shared, &mut session, outputs, &mut cursor_position_applied)?;
        }
        // Start from the next lit head each pass so one busy screen cannot starve the other of the
        // shared budget. Stepping over every connector slot instead would let the first of two
        // monitors go first in three passes out of four.
        first_head = (1..=MAX_CONNECTORS)
            .map(|i| (first_head + i) % MAX_CONNECTORS)
            .find(|&head| outputs[head].as_ref().is_some_and(|output| output.lit))
            .unwrap_or(0);
        for head in (0..MAX_CONNECTORS).map(|i| (first_head + i) % MAX_CONNECTORS) {
            let Some(output) = outputs[head].as_mut() else { continue };
            if let Some(on) = shared.take_power(head) {
                set_power(&mut session, output, head as u8, on)?;
            }
            if !output.lit {
                continue;
            }
            let counters = &shared.perf.heads[head];
            if !budget.available() {
                let repaint_due = output.feed != Feed::Nothing
                    && session.owes_repaint(head as u8)
                    && Instant::now() >= output.next_repaint;
                if counters.frame_ready.load(Ordering::Relaxed) || repaint_due {
                    counters.skipped.fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }
            let now = Instant::now();
            let taken = shared.take(head, &mut output.shadow);
            add_elapsed(&counters.take_ns, now);
            if let Some(taken) = taken {
                let sent = match taken {
                    Taken::Surface(dirty) => {
                        output.feed = Feed::Surface;
                        session.present_rgb_dirty(
                            head as u8,
                            output.padded_width,
                            output.padded_height,
                            &output.shadow,
                            dirty.as_deref(),
                        )?
                    }
                    Taken::Encoded(bodies) => {
                        output.feed = Feed::Encoded;
                        // A shadow kept from a surface feed would go stale unseen.
                        output.shadow = Vec::new();
                        present_encoded(shared, &mut session, output, head, bodies)?
                    }
                };
                counters.record_present(sent, session.last_present());
                budget.spend(sent);
                output.last_change = now;
                output.settled = false;
                output.next_repaint = now + refresh_period(output.mode);
                continue;
            }
            // Once the surface has been still for a moment, send everything that moved since the
            // last settle to every dock buffer again (local patch in vendor/vino/UPSTREAM.md).
            if !output.settled && now.duration_since(output.last_change) >= SETTLE_DELAY {
                session.settle(head as u8);
                output.settled = true;
            }
            // Owed retransmissions go out one refresh apart, so each lands in a different dock
            // buffer however the dock times its buffer swaps.
            if now >= output.next_repaint && session.owes_repaint(head as u8) {
                // Nothing new since the last present, so no strip needs re-hashing.
                let sent = match output.feed {
                    Feed::Nothing => None,
                    Feed::Surface => Some(session.present_rgb_dirty(
                        head as u8,
                        output.padded_width,
                        output.padded_height,
                        &output.shadow,
                        Some(&[]),
                    )?),
                    Feed::Encoded => Some(present_encoded(shared, &mut session, output, head, Vec::new())?),
                };
                if let Some(sent) = sent {
                    counters.record_present(sent, session.last_present());
                    budget.spend(sent);
                    output.next_repaint = now + refresh_period(output.mode);
                }
            }
        }
        if reports_presence && !timed_presence && session.take_downstream_change() {
            burst_probes_left = PRESENCE_BURST;
            next_presence = Instant::now();
        }
        let now = Instant::now();
        // With no head lit there is no link for a probe to disturb, and a dock that has just come
        // back (unplugged, power-cycled) may not announce monitors that wake after it, so poll.
        let nothing_lit = !outputs.iter().flatten().any(|output| output.lit);
        if (timed_presence || nothing_lit || burst_probes_left > 0) && now >= next_presence {
            refresh_topology(shared, events, &mut session, outputs)?;
            burst_probes_left = burst_probes_left.saturating_sub(1);
            next_presence = Instant::now() + PRESENCE_PERIOD;
        } else if reports_presence && now >= next_empty_scan {
            // Measured on the M/Connect 2: a monitor switched on beside a lit one is not announced,
            // so empty sockets are probed slowly. Lit sockets are never probed here.
            for head in 0..session.connectors() {
                if outputs[head].is_none() && session.probe_head_present(head as u8)? == Some(true) {
                    attach(shared, events, &mut session, outputs, head as u8, Discovery::Gated)?;
                    report_monitors(events, outputs.iter().flatten().count());
                }
            }
            next_empty_scan = Instant::now() + EMPTY_SOCKET_PERIOD;
        }
        if now >= next_keepalive {
            session.keepalive_poll()?;
            next_keepalive = Instant::now() + keepalive_period;
        }
        if now >= next_heartbeat {
            session.heartbeat()?;
            next_heartbeat = Instant::now() + HEARTBEAT_PERIOD;
        }
    }

    // Orderly shutdown: blank and power down every lit head so the monitors sleep instead of
    // freezing on the last frame, then drop the session to release the dock.
    for (head, slot) in outputs.iter_mut().enumerate() {
        if let Some(output) = slot.as_mut().filter(|output| output.lit) {
            session.deactivate_mode(head as u8, output.mode.width, output.mode.height)?;
            output.lit = false;
        }
    }
    Ok(())
}

/// Present strips a producer coded, or (with no bodies) what the head still owes from earlier
/// ones. A selected strip the scanout holds no current body for sends nothing and asks the
/// producer to code every strip again; that is recoverable, so it does not end the session.
fn present_encoded(
    shared: &Shared,
    session: &mut ControlSession,
    output: &Output,
    head: usize,
    bodies: Vec<(usize, u64, Vec<u8>)>,
) -> Result<usize, String> {
    let changed: Vec<(usize, u64)> = bodies.iter().map(|&(index, hash, _)| (index, hash)).collect();
    match session.present_encoded(head as u8, output.padded_width, output.padded_height, &changed, bodies) {
        Ok(sent) => Ok(sent),
        Err(PresentEncodedError::MissingBody { .. }) => {
            shared.request_full(head);
            Ok(0)
        }
        Err(PresentEncodedError::Session(error)) => Err(error),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Discovery {
    /// The presence probe said the socket is occupied.
    Gated,
    /// Read without a positive probe; the result must be confirmed before it is published.
    Blind,
}

fn report_monitors(events: &impl Events, count: usize) {
    let message = match count {
        0 => "Dock connected. No monitors detected on its DisplayPort outputs.".to_string(),
        1 => "Dock connected: 1 monitor".to_string(),
        n => format!("Dock connected: {n} monitors"),
    };
    events.state(State::Engaged, &message);
}

/// Fetch a head's EDID, program its preferred mode, and tell the host it is ready.
/// Returns whether the head was lit. A monitor that cannot be read or programmed is reported and
/// skipped, not fatal.
fn attach(
    shared: &Shared,
    events: &impl Events,
    session: &mut ControlSession,
    outputs: &mut [Option<Output>; MAX_CONNECTORS],
    head: u8,
    discovery: Discovery,
) -> Result<bool, String> {
    let report = |message: String| {
        if discovery == Discovery::Gated {
            events.state(State::Engaged, &message);
        }
    };
    let edid_bytes = match session.fetch_edid(head) {
        Ok(bytes) => bytes,
        Err(error) => {
            report(format!("Connector {}: cannot read the monitor ({error})", head + 1));
            return Ok(false);
        }
    };
    // The dock's shared EDID handler can answer for the other socket's monitor.
    if outputs.iter().flatten().any(|output| output.edid == edid_bytes) {
        return Ok(false);
    }
    if discovery == Discovery::Blind && session.probe_head_present(head)? != Some(true) {
        return Ok(false);
    }
    let monitor = match edid::parse(&edid_bytes) {
        Ok(monitor) => monitor,
        Err(error) => {
            report(format!("Connector {}: unusable EDID ({error})", head + 1));
            return Ok(false);
        }
    };
    let mode = Mode {
        width: monitor.preferred.width as usize,
        height: monitor.preferred.height as usize,
        refresh_hz: monitor.preferred.refresh_hz,
    };
    if !session.activate_mode(head, mode.width, mode.height, mode.refresh_hz)? {
        // Only a dock that reconfigures whole declines a single connector; program them together.
        let mut requests: Vec<(u8, usize, usize, u32)> = (0u8..)
            .zip(outputs.iter())
            .filter_map(|(other, output)| {
                output.as_ref().map(|o| (other, o.mode.width, o.mode.height, o.mode.refresh_hz))
            })
            .collect();
        requests.push((head, mode.width, mode.height, mode.refresh_hz));
        session.activate_modes(&requests)?;
    }
    let (padded_width, padded_height) = padded(session.profile(), mode.width, mode.height);
    let (strip_w, strip_h) = session.profile().strip_dims();
    shared.open(
        usize::from(head),
        Accepting {
            width: mode.width,
            height: mode.height,
            padded_width,
            padded_height,
            strip_w,
            strip_h,
            encoder: EncoderProfile::of(session.profile()),
        },
    );
    events.attached(head, &monitor, mode, &edid_bytes, session.profile().hw_cursor());
    outputs[usize::from(head)] = Some(Output {
        mode,
        lit: true,
        padded_width,
        padded_height,
        shadow: Vec::new(),
        feed: Feed::Nothing,
        edid: edid_bytes,
        presence_debounce: 0,
        silent_probes: 0,
        presence_grace_until: Instant::now() + PRESENCE_GRACE,
        cursor_shape_gen: 0,
        cursor_visible: false,
        cursor_gamma_gen: 0,
        last_change: Instant::now(),
        settled: true,
        next_repaint: Instant::now(),
    });
    Ok(true)
}

/// One refresh period of `mode`.
fn refresh_period(mode: Mode) -> Duration {
    Duration::from_secs(1) / mode.refresh_hz.max(1)
}

/// Bring every lit head's dock cursor up to the host's latest pointer shape and position.
fn sync_cursor(
    shared: &Shared,
    session: &mut ControlSession,
    outputs: &mut [Option<Output>; MAX_CONNECTORS],
    position_applied: &mut u64,
) -> Result<(), String> {
    let (shape, shape_gen, position, position_gen) = {
        let cursor = shared.cursor.lock().unwrap();
        (cursor.shape.clone(), cursor.shape_gen, cursor.position, cursor.position_gen)
    };
    let Some(shape) = shape else { return Ok(()) };
    let mut reposition = position_gen != *position_applied;
    for (head, slot) in outputs.iter_mut().enumerate() {
        let Some(output) = slot.as_mut().filter(|output| output.lit) else { continue };
        let (gamma_gen, gamma) = *shared.cursor_gamma[head].lock().unwrap();
        if output.cursor_shape_gen != shape_gen || output.cursor_gamma_gen != gamma_gen {
            match &gamma {
                None => session.set_cursor(head as u8, shape.width, shape.height, &shape.bgra)?,
                Some(lut) => {
                    let tinted = gamma_premultiplied_bgra(&shape.bgra, lut);
                    session.set_cursor(head as u8, shape.width, shape.height, &tinted)?;
                }
            }
            output.cursor_shape_gen = shape_gen;
            output.cursor_gamma_gen = gamma_gen;
            // A fresh image is shown wherever the dock last had the pointer; place it again.
            output.cursor_visible = true;
            reposition = true;
        }
    }
    if !reposition {
        return Ok(());
    }
    for (head, slot) in outputs.iter_mut().enumerate() {
        let Some(output) = slot.as_mut().filter(|output| output.lit) else { continue };
        match position {
            Some((on, x, y)) if usize::from(on) == head => {
                session.move_cursor(head as u8, x.saturating_sub(shape.hot_x), y.saturating_sub(shape.hot_y))?;
                output.cursor_visible = true;
            }
            _ if output.cursor_visible => {
                session.hide_cursor(head as u8)?;
                output.cursor_visible = false;
            }
            _ => {}
        }
    }
    *position_applied = position_gen;
    Ok(())
}

fn set_power(session: &mut ControlSession, output: &mut Output, head: u8, on: bool) -> Result<(), String> {
    match (on, output.lit) {
        (false, true) => {
            session.deactivate_mode(head, output.mode.width, output.mode.height)?;
            output.lit = false;
            // Presence can read negative while a sink is powered down; give it time on wake.
            output.presence_grace_until = Instant::now() + PRESENCE_GRACE;
        }
        (true, false) => {
            let mode = output.mode;
            output.lit = session.activate_mode(head, mode.width, mode.height, mode.refresh_hz)?;
            output.presence_grace_until = Instant::now() + PRESENCE_GRACE;
        }
        _ => {}
    }
    Ok(())
}

fn refresh_topology(
    shared: &Shared,
    events: &impl Events,
    session: &mut ControlSession,
    outputs: &mut [Option<Output>; MAX_CONNECTORS],
) -> Result<(), String> {
    let before = outputs.iter().flatten().count();
    for head in 0..session.connectors() {
        // A deliberately powered-down head can look removed; leave it alone until it is woken.
        if outputs[head].as_ref().is_some_and(|output| !output.lit) {
            continue;
        }
        let present = session.probe_head_present(head as u8)?;
        let Some(output) = outputs[head].as_mut() else {
            if present == Some(true) {
                attach(shared, events, session, outputs, head as u8, Discovery::Gated)?;
            }
            continue;
        };
        let present = match present {
            Some(present) => {
                output.silent_probes = 0;
                present
            }
            None => {
                output.silent_probes = output.silent_probes.saturating_add(1);
                if output.silent_probes < PRESENCE_SILENT_LIMIT {
                    continue;
                }
                false
            }
        };
        if present || Instant::now() < output.presence_grace_until {
            output.presence_debounce = 0;
            continue;
        }
        output.presence_debounce = output.presence_debounce.saturating_add(1);
        if output.presence_debounce >= 2 {
            outputs[head] = None;
            shared.close(head);
            events.detached(head as u8);
        }
    }
    let after = outputs.iter().flatten().count();
    if after != before {
        report_monitors(events, after);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A surface whose codec strips are `strip_w` x `strip_h` (1x1 makes every pixel a strip).
    fn accepting(width: usize, height: usize, padded_width: usize, padded_height: usize) -> Accepting {
        Accepting { width, height, padded_width, padded_height, strip_w: 1, strip_h: 1, encoder: None }
    }

    impl Shared {
        /// [`Shared::take`] for a head fed surfaces.
        fn take_frame(&self, head: usize, shadow: &mut Vec<u8>) -> Option<Option<Vec<Rect>>> {
            self.take(head, shadow).map(|taken| match taken {
                Taken::Surface(dirty) => dirty,
                Taken::Encoded(_) => panic!("expected a surface, got coded strips"),
            })
        }

        fn take_encoded(&self, head: usize) -> Option<Vec<(usize, u64, Vec<u8>)>> {
            self.take(head, &mut Vec::new()).map(|taken| match taken {
                Taken::Encoded(bodies) => bodies,
                Taken::Surface(_) => panic!("expected coded strips, got a surface"),
            })
        }
    }

    /// A 4x2 surface of 2x1 strips: indices 0, 1 on the top row and 2, 3 below.
    fn small_encoded() -> Accepting {
        Accepting { strip_w: 2, ..accepting(4, 2, 4, 2) }
    }

    /// A stand-in strip body: the header a coded strip at `(sx, sy)` starts with, and `tag`.
    fn body(sx: u16, sy: u16, tag: u8) -> Vec<u8> {
        let mut body = vec![0u8; 17];
        body[..2].copy_from_slice(&kvino::STRIP_MAGIC);
        body[2..4].copy_from_slice(&sx.to_le_bytes());
        body[4..6].copy_from_slice(&sy.to_le_bytes());
        body[16] = tag;
        body
    }

    fn strip_xy(index: usize) -> (u16, u16) {
        ((index % 2 * 2) as u16, (index / 2) as u16)
    }

    fn submit(shared: &Shared, strips: &[(usize, u64, u8)]) -> bool {
        let bodies: Vec<(usize, u64, Vec<u8>)> = strips
            .iter()
            .map(|&(index, hash, tag)| {
                let (sx, sy) = strip_xy(index);
                (index, hash, body(sx, sy, tag))
            })
            .collect();
        shared.submit_encoded(0, 4, 2, false, bodies.iter().map(|(i, h, b)| (*i, *h, b.as_slice())))
    }

    fn taken(strips: &[(usize, u64, u8)]) -> Vec<(usize, u64, Vec<u8>)> {
        strips
            .iter()
            .map(|&(index, hash, tag)| {
                let (sx, sy) = strip_xy(index);
                (index, hash, body(sx, sy, tag))
            })
            .collect()
    }

    /// A strip coded again before the engine looks replaces its older copy, keeping its place.
    #[test]
    fn encoded_strips_coalesce_with_the_last_write_winning() {
        let shared = Shared::new();
        shared.open(0, small_encoded());
        assert!(submit(&shared, &[(2, 11, 1), (0, 12, 2)]));
        assert!(shared.perf.heads[0].frame_ready.load(Ordering::Relaxed));
        assert!(submit(&shared, &[(0, 13, 3), (3, 14, 4)]));
        assert_eq!(shared.take_encoded(0), Some(taken(&[(2, 11, 1), (0, 13, 3), (3, 14, 4)])));
        assert!(!shared.perf.heads[0].frame_ready.load(Ordering::Relaxed));
        assert_eq!(shared.take_encoded(0), None);
        // An empty submission is accepted and wakes nobody.
        assert!(submit(&shared, &[]));
        assert!(!shared.perf.heads[0].frame_ready.load(Ordering::Relaxed));
        assert_eq!(shared.take_encoded(0), None);
        assert!(submit(&shared, &[(1, 15, 5)]));
        assert_eq!(shared.take_encoded(0), Some(taken(&[(1, 15, 5)])));
        assert_eq!(shared.take_stats().heads[0].unwrap().frames_submitted, 4);
    }

    /// Anything that does not fit the head refuses the whole submission.
    #[test]
    fn encoded_submissions_that_do_not_fit_the_head_queue_nothing() {
        let shared = Shared::new();
        let try_submit = |strips: Vec<(usize, u64, Vec<u8>)>| {
            shared.submit_encoded(0, 4, 2, false, strips.iter().map(|(i, h, b)| (*i, *h, b.as_slice())))
        };
        // Not attached.
        assert!(!try_submit(vec![(0, 1, body(0, 0, 0))]));
        shared.open(0, small_encoded());
        let good = (0, 1, body(0, 0, 0));
        for bad in [
            (4, 1, body(0, 2, 0)),           // outside the surface
            (1, 0, body(2, 0, 0)),           // zero hash
            (1, 1, body(0, 0, 0)),           // a body for another strip
            (1, 1, body(2, 0, 0)[..15].to_vec()), // shorter than a strip header
            (1, 1, { let mut b = body(2, 0, 0); b[1] = 0; b }), // not a strip
        ] {
            assert!(!try_submit(vec![good.clone(), bad.clone()]), "{bad:?}");
        }
        assert!(!shared.submit_encoded(0, 4, 3, false, [(good.0, good.1, good.2.as_slice())]));
        assert_eq!(shared.take_encoded(0), None);
        assert!(try_submit(vec![good.clone()]));
        assert_eq!(shared.take_encoded(0), Some(vec![good]));
    }

    /// The producer codes everything again after an open, a gamma change, a scanout that lacked
    /// a body, or a depth it got wrong.
    #[test]
    fn a_full_encode_is_requested_on_open_gamma_change_and_missing_bodies() {
        let shared = Shared::new();
        assert!(!shared.take_full_request(0));
        shared.open(0, small_encoded());
        assert!(shared.take_full_request(0));
        assert!(!shared.take_full_request(0));
        shared.set_gamma(0, Some(test_lut()));
        assert!(shared.take_full_request(0));
        // The same table again is no change.
        shared.set_gamma(0, Some(test_lut()));
        assert!(!shared.take_full_request(0));
        shared.set_gamma(0, None);
        assert!(shared.take_full_request(0));
        shared.request_full(0);
        assert!(shared.take_full_request(0));
        // A gamma change still reaches the dock cursor as before.
        assert_eq!(shared.cursor_gamma[0].lock().unwrap().0, 2);

        let ridge = DockProfile::for_family(Family::Ridge).unwrap();
        let encoder = EncoderProfile::of(ridge).unwrap();
        shared.open(1, Accepting { encoder: Some(encoder), ..accepting(64, 16, 64, 16) });
        assert!(shared.take_full_request(1));
        assert_eq!(shared.encoder_profile(1), Some((encoder, 64, 16)));
        assert!(!shared.submit_encoded(1, 64, 16, true, []));
        assert!(shared.take_full_request(1));
        assert!(shared.submit_encoded(1, 64, 16, false, []));
        // Closed heads ask for nothing and describe nothing.
        shared.close(1);
        shared.request_full(1);
        assert!(!shared.take_full_request(1));
        assert_eq!(shared.encoder_profile(1), None);
    }

    /// Switching between frames and coded strips starts the mailbox over, as an open does.
    #[test]
    fn switching_feeds_resets_the_mailbox() {
        let shared = Shared::new();
        shared.open(0, small_encoded());
        assert!(shared.take_full_request(0));
        let mut shadow = Vec::new();
        shared.submit_frame(0, &[1; 32], 16, 4, 2, [(0, 0, 4, 2)]);
        assert_eq!(shared.take_frame(0, &mut shadow), Some(None));
        shared.submit_frame(0, &[2; 32], 16, 4, 2, [(0, 0, 1, 1)]);

        // Coded strips replace the pending surface damage entirely.
        assert!(submit(&shared, &[(3, 7, 1)]));
        assert!(shared.take_full_request(0));
        assert!(shared.mailboxes[0].lock().unwrap().surface.is_empty());
        assert_eq!(shared.take_encoded(0), Some(taken(&[(3, 7, 1)])));

        // And a frame after coded strips is a whole new surface, with no strips left over.
        assert!(submit(&shared, &[(2, 8, 2)]));
        shared.submit_frame(0, &[3; 32], 16, 4, 2, []);
        assert!(shared.take_full_request(0));
        assert_eq!(shared.take_frame(0, &mut shadow), Some(None));
        assert_eq!(shadow, vec![3; 24]);
        assert!(shared.mailboxes[0].lock().unwrap().encoded.is_none());
        // An invalid frame does not switch the feed.
        assert!(submit(&shared, &[(2, 9, 3)]));
        shared.submit_frame(0, &[3; 4], 16, 4, 2, []);
        assert_eq!(shared.take_encoded(0), Some(taken(&[(2, 9, 3)])));
    }

    #[test]
    fn gpu_time_is_reported_per_head() {
        let shared = Shared::new();
        shared.open(2, accepting(2, 2, 2, 2));
        shared.add_gpu_time(2, Duration::from_micros(1500));
        shared.add_gpu_time(9, Duration::from_micros(1));
        assert_eq!(shared.take_stats().heads[2].unwrap().gpu_us, 1500);
    }

    #[test]
    fn frame_lengths_reject_overflow_and_keep_last_row_unpadded() {
        assert_eq!(frame_len(12, 2, 2), Some(20));
        assert_eq!(frame_len(usize::MAX, 1, 1), Some(4));
        assert_eq!(frame_len(8, 0, 2), None);
        assert_eq!(frame_len(8, 2, 0), None);
        assert_eq!(frame_len(7, 2, 2), None);
        assert_eq!(frame_len(usize::MAX, usize::MAX / 4 + 1, 1), None);
        assert_eq!(frame_len(usize::MAX, 1, 2), None);
        assert_eq!(frame_len(usize::MAX, 1, 3), None);
        let max = isize::MAX as usize;
        assert_eq!(frame_len(max - 4, 1, 2), Some(max));
        assert_eq!(frame_len(max - 3, 1, 2), None);
    }

    #[test]
    fn converts_bgra_with_stride_into_a_black_padded_surface() {
        let shared = Shared::new();
        shared.open(0, accepting(2, 2, 3, 3));
        // 2x2 source with a 12-byte stride (one pad pixel per row), padded to 3x3.
        let bgra = [
            1, 2, 3, 255, 4, 5, 6, 255, 9, 9, 9, 9, //
            7, 8, 9, 255, 10, 11, 12, 255, 9, 9, 9, 9,
        ];
        shared.submit_frame(0, &bgra, 12, 2, 2, [(0, 0, 2, 2)]);
        let mut shadow = Vec::new();
        assert_eq!(shared.take_frame(0, &mut shadow), Some(None));
        assert_eq!(
            shadow,
            [
                3, 2, 1, 6, 5, 4, 0, 0, 0, //
                9, 8, 7, 12, 11, 10, 0, 0, 0, //
                0, 0, 0, 0, 0, 0, 0, 0, 0,
            ]
        );
    }

    #[test]
    fn mailbox_drops_frames_it_cannot_use() {
        let shared = Shared::new();
        let mut shadow = Vec::new();
        // No head attached.
        shared.submit_frame(0, &[0; 16], 8, 2, 2, [(0, 0, 2, 2)]);
        assert!(shared.take_frame(0, &mut shadow).is_none());
        shared.open(0, accepting(2, 2, 2, 2));
        // Short buffer, too-small stride, wrong size for the mode.
        shared.submit_frame(0, &[0; 15], 8, 2, 2, [(0, 0, 2, 2)]);
        shared.submit_frame(0, &[0; 16], 4, 2, 2, [(0, 0, 2, 2)]);
        shared.submit_frame(0, &[3; 36], 12, 3, 3, [(0, 0, 3, 3)]);
        // Host-side callers also reject overflowing geometry without poisoning the mailbox.
        shared.submit_frame(0, &[0; 16], usize::MAX, 2, 2, [(0, 0, 2, 2)]);
        shared.submit_frame(0, &[0; 16], 8, usize::MAX, 2, [(0, 0, usize::MAX, 2)]);
        assert!(shared.take_frame(0, &mut shadow).is_none());
        shared.submit_frame(0, &[1; 16], 8, 2, 2, [(0, 0, 2, 2)]);
        assert_eq!(shared.take_frame(0, &mut shadow), Some(None));
        assert_eq!(shadow, [1; 12]);
        shared.close(0);
        shared.submit_frame(0, &[4; 16], 8, 2, 2, [(0, 0, 2, 2)]);
        assert!(shared.take_frame(0, &mut shadow).is_none());
    }

    /// Only strips whose pixels really changed reach the engine, whatever area capture reports,
    /// and they accumulate (as strip-aligned runs) until the engine takes them.
    #[test]
    fn mailbox_hands_over_only_strips_that_really_changed() {
        let shared = Shared::new();
        // 4x2 surface of 2x1 strips: two strips per row.
        shared.open(0, Accepting { strip_w: 2, ..accepting(4, 2, 4, 2) });
        let mut shadow = Vec::new();
        shared.submit_frame(0, &[1; 32], 16, 4, 2, [(0, 0, 4, 2)]);
        assert_eq!(shared.take_frame(0, &mut shadow), Some(None));
        assert_eq!(shadow, vec![1; 24]);

        // Two updates before the engine looks: the left column, then the bottom-right pixel.
        // A reported pixel converts its whole strip.
        shared.submit_frame(0, &[2; 32], 16, 4, 2, [(0, 0, 1, 2)]);
        shared.submit_frame(0, &[3; 32], 16, 4, 2, [(3, 1, 9, 9)]);
        assert_eq!(shared.take_frame(0, &mut shadow), Some(Some(vec![(0, 0, 2, 1), (0, 1, 4, 2)])));
        assert_eq!(shadow, [2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3]);
        assert!(shared.take_frame(0, &mut shadow).is_none());

        // Capture reports a change but the strip's pixels are what it already holds: no frame.
        shared.submit_frame(0, &[3; 32], 16, 4, 2, [(2, 1, 4, 2)]);
        assert!(shared.take_frame(0, &mut shadow).is_none());
        // An empty report is no frame either.
        shared.submit_frame(0, &[4; 32], 16, 4, 2, [(0, 0, 0, 0)]);
        assert!(shared.take_frame(0, &mut shadow).is_none());
        // Unknown damage is represented by the whole frame: every strip is checked.
        shared.submit_frame(0, &[3; 32], 16, 4, 2, [(0, 0, 4, 2)]);
        assert_eq!(shared.take_frame(0, &mut shadow), Some(Some(vec![(0, 0, 4, 1), (0, 1, 2, 2)])));
        assert_eq!(shadow, vec![3; 24]);
    }

    /// A table that sends `v` to `v / 2` (red), `255 - v` (green), and leaves blue alone.
    fn test_lut() -> [[u8; 256]; 3] {
        core::array::from_fn(|c| core::array::from_fn(|i| match c { 0 => i as u8 / 2, 1 => 255 - i as u8, _ => i as u8 }))
    }

    #[test]
    fn gamma_table_applies_to_copied_pixels() {
        let shared = Shared::new();
        shared.open(0, Accepting { strip_w: 2, ..accepting(4, 2, 4, 2) });
        shared.set_gamma(0, Some(test_lut()));
        let mut shadow = Vec::new();
        shared.submit_frame(0, &[100; 32], 16, 4, 2, [(0, 0, 4, 2)]);
        assert_eq!(shared.take_frame(0, &mut shadow), Some(None));
        assert_eq!(shadow, [50, 155, 100].repeat(8));
        // Partial copies go through the table too, and only the changed strip moves.
        let mut bgra = [100; 32];
        bgra[..8].fill(10);
        shared.submit_frame(0, &bgra, 16, 4, 2, [(0, 0, 1, 1)]);
        assert_eq!(shared.take_frame(0, &mut shadow), Some(Some(vec![(0, 0, 2, 1)])));
        assert_eq!(shadow, [[5, 245, 10].repeat(2), [50, 155, 100].repeat(6)].concat());
    }

    #[test]
    fn changing_gamma_repaints_the_whole_surface_without_a_new_frame() {
        let shared = Shared::new();
        shared.open(0, Accepting { strip_w: 2, ..accepting(4, 2, 4, 2) });
        let mut shadow = Vec::new();
        shared.submit_frame(0, &[100; 32], 16, 4, 2, [(0, 0, 4, 2)]);
        assert_eq!(shared.take_frame(0, &mut shadow), Some(None));
        assert!(shared.take_frame(0, &mut shadow).is_none());
        shared.set_gamma(0, Some(test_lut()));
        assert!(shared.perf.heads[0].frame_ready.load(Ordering::Relaxed));
        assert_eq!(shared.take_frame(0, &mut shadow), Some(None));
        assert_eq!(shadow, [50, 155, 100].repeat(8));
        // The same table again changes nothing; clearing it repaints untinted.
        shared.set_gamma(0, Some(test_lut()));
        assert!(shared.take_frame(0, &mut shadow).is_none());
        shared.set_gamma(0, None);
        assert_eq!(shared.take_frame(0, &mut shadow), Some(None));
        assert_eq!(shadow, vec![100; 24]);
    }

    #[test]
    fn cursor_image_goes_through_the_gamma_table_keeping_alpha() {
        // Red halved, green inverted, blue zeroed.
        let lut: [[u8; 256]; 3] =
            core::array::from_fn(|c| core::array::from_fn(|i| match c { 0 => i as u8 / 2, 1 => 255 - i as u8, _ => 0 }));
        // Opaque white, half-transparent white (premultiplied 128), fully transparent.
        let bgra = [255, 255, 255, 255, 128, 128, 128, 128, 0, 0, 0, 0];
        let out = gamma_premultiplied_bgra(&bgra, &lut);
        assert_eq!(&out[0..4], &[0, 0, 127, 255]);
        assert_eq!(&out[4..8], &[0, 0, 64, 128]);
        assert_eq!(&out[8..12], &[0, 0, 0, 0]);
    }

    #[test]
    fn changing_gamma_bumps_the_cursor_table_generation() {
        let shared = Shared::new();
        shared.open(0, Accepting { strip_w: 2, ..accepting(4, 2, 4, 2) });
        shared.set_gamma(0, Some(test_lut()));
        let first = *shared.cursor_gamma[0].lock().unwrap();
        assert_eq!(first, (1, Some(test_lut())));
        shared.set_gamma(0, Some(test_lut()));
        assert_eq!(shared.cursor_gamma[0].lock().unwrap().0, 1, "same table: no re-upload");
        shared.set_gamma(0, None);
        assert_eq!(*shared.cursor_gamma[0].lock().unwrap(), (2, None));
    }

    #[test]
    fn identity_gamma_is_no_table() {
        let shared = Shared::new();
        shared.open(0, Accepting { strip_w: 2, ..accepting(4, 2, 4, 2) });
        let mut shadow = Vec::new();
        shared.submit_frame(0, &[100; 32], 16, 4, 2, [(0, 0, 4, 2)]);
        assert_eq!(shared.take_frame(0, &mut shadow), Some(None));
        shared.set_gamma(0, Some(core::array::from_fn(|_| core::array::from_fn(|i| i as u8))));
        assert!(shared.mailboxes[0].lock().unwrap().gamma.is_none());
        assert!(shared.take_frame(0, &mut shadow).is_none());
    }

    #[test]
    fn stats_count_accepted_frames_per_attached_head_and_reset_when_taken() {
        let shared = Shared::new();
        let mut shadow = Vec::new();
        // Not attached: neither counted nor reported.
        shared.submit_frame(0, &[1; 16], 8, 2, 2, [(0, 0, 2, 2)]);
        assert_eq!(shared.take_stats(), EngineStats::default());

        shared.open(1, accepting(2, 2, 2, 2));
        shared.submit_frame(1, &[1; 16], 8, 2, 2, [(0, 0, 2, 2)]);
        assert!(shared.perf.heads[1].frame_ready.load(Ordering::Relaxed));
        // Unchanged pixels still count as a submitted frame; a wrong size does not.
        shared.submit_frame(1, &[1; 16], 8, 2, 2, [(0, 0, 2, 2)]);
        shared.submit_frame(1, &[1; 36], 12, 3, 3, [(0, 0, 3, 3)]);
        assert!(shared.take_frame(1, &mut shadow).is_some());
        assert!(!shared.perf.heads[1].frame_ready.load(Ordering::Relaxed));
        shared.perf.heads[1].record_present(
            5_000,
            PresentTiming { coarse_fallback: true, usb: Duration::from_micros(7), ..Default::default() },
        );
        shared.perf.heads[1].record_present(0, PresentTiming::default());

        let stats = shared.take_stats();
        assert!(stats.heads[0].is_none());
        let head = stats.heads[1].unwrap();
        assert_eq!(head.frames_submitted, 2);
        assert_eq!((head.present_calls, head.presents, head.bytes_sent), (2, 1, 5_000));
        assert_eq!((head.coarse_fallbacks, head.usb_us), (1, 7));
        assert_eq!(shared.take_stats().heads[1], Some(HeadStats::default()));

        shared.close(1);
        assert_eq!(shared.take_stats(), EngineStats::default());
    }

    #[test]
    fn parallel_bands_preserve_pixels_padding_and_damage_boundaries() {
        let shared = Shared::new();
        shared.open(0, Accepting { strip_w: 2, strip_h: 2, ..accepting(7, 9, 8, 10) });
        let mut bgra: Vec<u8> = (0..frame_len(32, 7, 9).unwrap()).map(|i| i as u8).collect();
        let mut shadow = Vec::new();
        shared.submit_frame(0, &bgra, 32, 7, 9, [(0, 0, 7, 9)]);
        assert_eq!(shared.take_frame(0, &mut shadow), Some(None));
        let mut expected = vec![0; 8 * 10 * 3];
        for y in 0..9 {
            for x in 0..7 {
                let source = &bgra[y * 32 + x * 4..][..4];
                expected[(y * 8 + x) * 3..][..3].copy_from_slice(&[source[2], source[1], source[0]]);
            }
        }
        assert_eq!(shadow, expected);

        // All five strip rows run in parallel, but only the reported strip column may change.
        bgra.fill(128);
        shared.submit_frame(0, &bgra, 32, 7, 9, [(0, 0, 1, 9)]);
        assert_eq!(
            shared.take_frame(0, &mut shadow),
            Some(Some((0..5).map(|row| (0, row * 2, 2, row * 2 + 2)).collect()))
        );
        for y in 0..9 {
            expected[y * 8 * 3..][..6].fill(128);
        }
        assert_eq!(shadow, expected);
        shared.submit_frame(0, &bgra, 32, 7, 9, []);
        assert_eq!(shared.take_frame(0, &mut shadow), None);
    }

    #[test]
    fn bgra_to_rgb_matches_scalar_on_odd_sizes_and_offsets() {
        let bgra: Vec<u8> = (0..4 * 100 + 7).map(|i| (i * 37 + 11) as u8).collect();
        for offset in [0, 1, 3, 4, 5] {
            for pixels in [0, 1, 15, 16, 17, 31, 32, 33, 63, 64, 65, 99] {
                let src = &bgra[offset..offset + pixels * 4];
                let (mut fast, mut slow) = (vec![0xaa; pixels * 3 + 2], vec![0xaa; pixels * 3 + 2]);
                bgra_to_rgb(&mut fast[..pixels * 3], src);
                bgra_to_rgb_scalar(&mut slow[..pixels * 3], src);
                // The bytes past the last pixel stay untouched.
                assert_eq!(fast, slow, "offset {offset} pixels {pixels}");
                #[cfg(target_arch = "aarch64")]
                assert_eq!(bgra_to_rgb_neon(&mut fast[1..pixels * 3 + 1], src), pixels / 16 * 16);
            }
        }
    }

    #[test]
    fn reopening_a_head_discards_previous_pixels_and_pending_damage() {
        let shared = Shared::new();
        shared.open(0, accepting(2, 2, 2, 2));
        shared.submit_frame(0, &[1; 16], 8, 2, 2, [(0, 0, 2, 2)]);
        let mut shadow = Vec::new();
        assert_eq!(shared.take_frame(0, &mut shadow), Some(None));
        shared.submit_frame(0, &[2; 16], 8, 2, 2, [(0, 0, 1, 1)]);
        shared.close(0);
        assert_eq!(shared.take_frame(0, &mut shadow), None);
        // A new mode with the same byte length still needs a complete fresh surface.
        shared.open(0, accepting(1, 4, 1, 4));
        shared.submit_frame(0, &[3; 16], 4, 1, 4, []);
        assert_eq!(shared.take_frame(0, &mut shadow), Some(None));
        assert_eq!(shadow, [3; 12]);
    }

    #[test]
    fn padding_rounds_up_to_whole_ridge_strips() {
        let ridge = vino_chimera::kvino::DockProfile::for_family(vino_chimera::kvino::Family::Ridge)
            .expect("Ridge profile");
        // Ridge strips are 64x16: 4K needs no padding, an odd size rounds up on both axes.
        assert_eq!(padded(ridge, 3840, 2160), (3840, 2160));
        assert_eq!(padded(ridge, 65, 17), (128, 32));
    }

    #[test]
    fn video_budget_holds_back_after_a_burst_and_refills_at_the_rate() {
        let start = Instant::now();
        let mut budget = VideoBudget { tokens: VIDEO_BURST_BYTES, refilled: start };
        // A full burst plus one byte leaves nothing to send with.
        budget.spend(VIDEO_BURST_BYTES as usize + 1_000_000);
        budget.refill(start);
        assert!(budget.tokens < 0.0);
        // 10 ms at 100 MB/s earns 1 MB, exactly the overdraft.
        budget.refill(start + Duration::from_millis(11));
        assert!(budget.tokens > 0.0);
        // A long idle spell banks no more than one burst.
        budget.refill(start + Duration::from_secs(60));
        assert_eq!(budget.tokens, VIDEO_BURST_BYTES);
    }
}
