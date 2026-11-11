// SPDX-License-Identifier: GPL-2.0-or-later

//! Offline driver for the pixel path, used by the `replay` benchmark. It feeds recorded frames
//! through the same `Shared::submit_frame` / `take_frame` code the engine thread uses, then
//! `HeadScanout::plan_dirty` + `encode`, with no dock attached. This lives under `host` only
//! because `take_frame` and the mailbox are private there; nothing here is used by the engine.
//!
//! The video budget runs on a virtual clock that advances one frame period per frame, not on
//! `Instant`. A wall clock would make which frames get held back, and so the record bytes, depend
//! on how fast the machine replays them. The constants and token rules are the engine's
//! ([`VIDEO_BYTES_PER_SEC`], [`VIDEO_BURST_BYTES`]).
//!
//! The stream report (sealed with the session's video key) is left out of packing, since there is
//! no session. The opener, records and trailer are still chunked into 64 KiB transfers as
//! `prefix_frame` does, so the pack stage copies the same payload bytes. Each frame gets one
//! engine pass: submit, then (budget permitting) take and present, then the settle and
//! owed-repaint rules from the engine loop.
//!
//! As on a live head, the first step picks the feed: surfaces ([`Replay::step`]) or strips coded
//! outside the engine ([`Replay::step_encoded`], the GPU encoder's path).

use super::*;
use vino_chimera::kvino;
use vino_chimera::scanout::{CachedEncodeError, HeadScanout};

const TRANSFER_SIZE: usize = 65_536;

/// Wall time spent in each stage of one frame.
#[derive(Clone, Copy, Default)]
pub struct Stages {
    pub submit: Duration,
    pub take: Duration,
    pub plan: Duration,
    pub encode: Duration,
    pub pack: Duration,
}

/// What one frame put on the (null) wire.
#[derive(Default)]
pub struct Step {
    pub stages: Stages,
    /// The EP08 records of this frame's present, empty when nothing was sent.
    pub records: Vec<Vec<u8>>,
    pub presentations: u32,
    /// Bytes the engine would have submitted over USB, all presentations and framing included.
    pub sent_bytes: usize,
    /// A new frame was waiting but the budget was exhausted.
    pub skipped: bool,
}

/// [`VideoBudget`] on a virtual clock, in seconds.
struct Budget {
    tokens: f64,
    at: f64,
}

impl Budget {
    fn advance(&mut self, now: f64) {
        self.tokens = (self.tokens + (now - self.at) * VIDEO_BYTES_PER_SEC).min(VIDEO_BURST_BYTES);
        self.at = now;
    }
}

pub struct Replay {
    shared: Arc<Shared>,
    dock: kvino::DockProfile,
    scanout: HeadScanout,
    shadow: Vec<u8>,
    width: usize,
    height: usize,
    padded_width: usize,
    padded_height: usize,
    budget: Budget,
    period: f64,
    now: f64,
    last_change: f64,
    next_repaint: f64,
    settled: bool,
    seq: u32,
    /// Something was presented from coded strips, so owed retransmissions can be paid from the
    /// bodies the scanout caches (the surface feed checks its shadow instead).
    encoded_fed: bool,
}

impl Replay {
    /// A head of `family` accepting `width` x `height` frames at `fps`. `gamma` must not be the
    /// identity, or the gamma path would silently go unmeasured.
    pub fn new(
        family: kvino::Family,
        width: usize,
        height: usize,
        fps: f64,
        gamma: Option<[[u8; 256]; 3]>,
    ) -> Result<Self, String> {
        let dock = kvino::DockProfile::for_family(family).ok_or("dock family not supported")?;
        let (padded_width, padded_height) = padded(dock, width, height);
        let (strip_w, strip_h) = dock.strip_dims();
        let shared = Shared::new();
        shared.set_gamma(0, gamma);
        if gamma.is_some() && shared.mailboxes[0].lock().unwrap().gamma.is_none() {
            return Err("the gamma table is the identity; use a non-identity table or 'none'".into());
        }
        shared.open(
            0,
            Accepting {
                width,
                height,
                padded_width,
                padded_height,
                strip_w,
                strip_h,
                encoder: EncoderProfile::of(dock),
            },
        );
        Ok(Self {
            shared,
            dock,
            scanout: HeadScanout::new(dock),
            shadow: Vec::new(),
            width,
            height,
            padded_width,
            padded_height,
            budget: Budget { tokens: VIDEO_BURST_BYTES, at: 0.0 },
            period: 1.0 / fps,
            now: 0.0,
            last_change: 0.0,
            next_repaint: 0.0,
            settled: true,
            seq: 0,
            encoded_fed: false,
        })
    }

    /// Run one frame through the pixel path. `dirty` is `None` for a frame with no damage report.
    pub fn step(&mut self, bgra: &[u8], stride: usize, dirty: Option<&[Rect]>) -> Step {
        let mut step = Step::default();
        self.now += self.period;
        self.budget.advance(self.now);

        let started = Instant::now();
        let full = [(0, 0, self.width, self.height)];
        let rects = dirty.unwrap_or(&full);
        self.shared.submit_frame(0, bgra, stride, self.width, self.height, rects.iter().copied());
        step.stages.submit = started.elapsed();
        self.engine_pass(&mut step);
        step
    }

    /// [`Replay::step`] for a producer that hashed and coded the frame's changed strips itself,
    /// as `(strip index, hash, body)`. Returns `None`, with the virtual clock still advanced,
    /// when the engine refuses the submission (see [`Shared::submit_encoded`]).
    pub fn step_encoded<'a>(
        &mut self,
        ten_bit: bool,
        strips: impl IntoIterator<Item = (usize, u64, &'a [u8])>,
    ) -> Option<Step> {
        let mut step = Step::default();
        self.now += self.period;
        self.budget.advance(self.now);

        let started = Instant::now();
        let accepted = self.shared.submit_encoded(0, self.width, self.height, ten_bit, strips);
        step.stages.submit = started.elapsed();
        if !accepted {
            return None;
        }
        self.engine_pass(&mut step);
        Some(step)
    }

    /// What a strip producer for this head must match, with the padded surface size.
    pub fn encoder_profile(&self) -> Option<(EncoderProfile, usize, usize)> {
        self.shared.encoder_profile(0)
    }

    /// Whether the producer must code every strip again; see [`Shared::take_full_request`].
    pub fn take_full_request(&self) -> bool {
        self.shared.take_full_request(0)
    }

    /// One visit of the engine loop to the head, after a submit.
    fn engine_pass(&mut self, step: &mut Step) {
        if self.budget.tokens <= 0.0 {
            step.skipped = self.shared.perf.heads[0].frame_ready.load(Ordering::Relaxed);
            return;
        }
        let started = Instant::now();
        let taken = self.shared.take(0, &mut self.shadow);
        step.stages.take = started.elapsed();
        match taken {
            Some(taken) => {
                match taken {
                    Taken::Surface(damage) => self.present(damage.as_deref(), step),
                    Taken::Encoded(bodies) => {
                        self.encoded_fed = true;
                        self.present_encoded(bodies, step);
                    }
                }
                self.last_change = self.now;
                self.settled = false;
                self.next_repaint = self.now + self.period;
            }
            None => {
                // Same two rules as the engine loop: settle once the surface has been still, and
                // pay owed retransmissions one refresh apart.
                if !self.settled && self.now - self.last_change >= SETTLE_DELAY.as_secs_f64() {
                    self.scanout.settle();
                    self.settled = true;
                }
                if self.now >= self.next_repaint && self.scanout.owes_retransmission() {
                    if self.encoded_fed {
                        self.present_encoded(Vec::new(), step);
                        self.next_repaint = self.now + self.period;
                    } else if !self.shadow.is_empty() {
                        self.present(Some(&[]), step);
                        self.next_repaint = self.now + self.period;
                    }
                }
            }
        }
    }

    /// The engine's `present_encoded`: plan from the producer's hashes, frame cached bodies. A
    /// selected strip with no current body sends nothing and asks for every strip again.
    fn present_encoded(&mut self, bodies: Vec<(usize, u64, Vec<u8>)>, step: &mut Step) {
        let started = Instant::now();
        let changed: Vec<(usize, u64)> = bodies.iter().map(|&(index, hash, _)| (index, hash)).collect();
        let plan = self.scanout.plan_hashed(self.padded_width, self.padded_height, &changed);
        self.scanout.offer_bodies(bodies);
        let planned = Instant::now();
        step.stages.plan = planned - started;
        let encoded = match self.scanout.encode_cached(&plan, 0) {
            Ok(encoded) => encoded,
            Err(CachedEncodeError::MissingBody { .. }) => {
                self.shared.request_full(0);
                return;
            }
            Err(CachedEncodeError::Codec(error)) => panic!("encode: {error:?}"),
        };
        let encoded_at = Instant::now();
        step.stages.encode = encoded_at - planned;
        let Some(frame) = encoded else { return };
        self.send(frame, &plan, encoded_at, step);
    }

    fn present(&mut self, dirty: Option<&[Rect]>, step: &mut Step) {
        let started = Instant::now();
        let plan = self.scanout.plan_dirty(self.padded_width, self.padded_height, &self.shadow, dirty);
        let planned = Instant::now();
        step.stages.plan = planned - started;
        let encoded = self.scanout.encode(&plan, &self.shadow, 0).expect("encode");
        let encoded_at = Instant::now();
        step.stages.encode = encoded_at - planned;
        let Some(frame) = encoded else { return };
        self.send(frame, &plan, encoded_at, step);
    }

    fn send(&mut self, frame: vino_chimera::scanout::Frame, plan: &vino_chimera::scanout::Plan, encoded_at: Instant, step: &mut Step) {
        for repeat in 0..frame.presentations {
            let transfers = pack(self.dock, &frame.records, self.seq.wrapping_add(repeat));
            step.sent_bytes += transfers.iter().map(Vec::len).sum::<usize>();
        }
        self.seq = self.seq.wrapping_add(frame.presentations);
        step.stages.pack = encoded_at.elapsed();
        self.scanout.presented(plan);
        self.budget.tokens -= step.sent_bytes as f64;
        step.presentations = frame.presentations;
        step.records = frame.records;
    }
}

/// Opener, records and trailer chunked into USB transfers, as `prefix_frame` does without a
/// report.
fn pack(dock: kvino::DockProfile, records: &[Vec<u8>], seq: u32) -> Vec<Vec<u8>> {
    let opener = kvino::frame_opener(dock, 0, seq).unwrap_or_default();
    let trailer = kvino::frame_trailer(dock, 0, seq);
    let total = opener.len() + records.iter().map(Vec::len).sum::<usize>() + trailer.len();
    let mut transfers = Vec::with_capacity(total.div_ceil(TRANSFER_SIZE));
    let mut chunk = Vec::with_capacity(total.min(TRANSFER_SIZE));
    for mut part in std::iter::once(opener.as_slice())
        .chain(records.iter().map(Vec::as_slice))
        .chain(std::iter::once(trailer.as_slice()))
    {
        while !part.is_empty() {
            let count = part.len().min(TRANSFER_SIZE - chunk.len());
            chunk.extend_from_slice(&part[..count]);
            part = &part[count..];
            if chunk.len() == TRANSFER_SIZE {
                transfers.push(chunk);
                chunk = Vec::with_capacity((total - transfers.len() * TRANSFER_SIZE).min(TRANSFER_SIZE));
            }
        }
    }
    if !chunk.is_empty() {
        transfers.push(chunk);
    }
    transfers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(count: usize, width: usize, height: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|f| (0..width * height * 4).map(|i| (i / 4 % 251 + f * 7 + i % 3) as u8).collect())
            .collect()
    }

    fn run(family: kvino::Family, gamma: Option<[[u8; 256]; 3]>) -> Vec<Vec<Vec<u8>>> {
        let (w, h) = (200, 150);
        let mut replay = Replay::new(family, w, h, 60.0, gamma).unwrap();
        frames(4, w, h).iter().map(|bgra| replay.step(bgra, w * 4, None).records).collect()
    }

    #[test]
    fn replay_is_deterministic_and_gamma_changes_the_records() {
        let mut lut = [[0u8; 256]; 3];
        for (c, table) in lut.iter_mut().enumerate() {
            for (i, v) in table.iter_mut().enumerate() {
                *v = 255 - (i as u8).wrapping_add(c as u8);
            }
        }
        for family in [kvino::Family::Ridge, kvino::Family::Ella, kvino::Family::Navarro] {
            let plain = run(family, None);
            assert!(!plain[0].is_empty(), "the first frame is a keyframe");
            assert_eq!(plain, run(family, None));
            assert_ne!(plain, run(family, Some(lut)));
        }
    }

    /// A stand-in for the GPU encoder: hashes each strip of the post-gamma padded RGB surface
    /// and codes the ones whose hash moved with the codec's own `encode_strip`.
    struct StubEncoder {
        dock: kvino::DockProfile,
        hashes: Vec<u64>,
    }

    impl StubEncoder {
        fn strips(&mut self, bgra: &[u8], w: usize, h: usize, lut: Option<&[[u8; 256]; 3]>, full: bool) -> Vec<(usize, u64, Vec<u8>)> {
            let (pw, ph) = padded(self.dock, w, h);
            let (sw, sh) = self.dock.strip_dims();
            let mut rgb = vec![0u8; pw * ph * 3];
            for y in 0..h {
                bgra_to_rgb(&mut rgb[y * pw * 3..][..w * 3], &bgra[y * w * 4..][..w * 4]);
            }
            if let Some(lut) = lut {
                let src = rgb.clone();
                apply_gamma(&mut rgb, &src, lut);
            }
            let tiles_x = pw / sw;
            self.hashes.resize(tiles_x * (ph / sh), 0);
            let mut out = Vec::new();
            for (index, previous) in self.hashes.iter_mut().enumerate() {
                let (sx, sy) = (index % tiles_x * sw, index / tiles_x * sh);
                let mut hasher = StripHasher::new(StripHasher::PRIME);
                for y in sy..sy + sh {
                    hasher.line(&rgb[(y * pw + sx) * 3..(y * pw + sx + sw) * 3]);
                }
                let hash = hasher.finish() | 1;
                if full || hash != *previous {
                    *previous = hash;
                    out.push((index, hash, kvino::encode_strip(self.dock, pw, &rgb, sx, sy).unwrap()));
                }
            }
            out
        }
    }

    /// Strips coded outside the engine put the same records on the wire as the surface feed,
    /// through keyframes, partial and empty changes, settling and owed retransmissions.
    ///
    /// Not alpha-only changes: the surface feed counts those as a new frame (its BGRA hash moved)
    /// and restarts the settle clock, while the encoded feed sees no change at all. The pictures
    /// agree; only when the settle happens differs.
    #[test]
    fn the_encoded_feed_matches_the_surface_feed() {
        let (w, h) = (200, 150);
        let mut lut = [[0u8; 256]; 3];
        for (c, table) in lut.iter_mut().enumerate() {
            for (i, v) in table.iter_mut().enumerate() {
                *v = (255 - i as u8).wrapping_add(c as u8);
            }
        }
        let mut images = frames(3, w, h);
        let mut partial = images[2].clone();
        partial[(40 * w + 130) * 4] ^= 0x55;
        images.push(partial);
        // Then a still screen long enough to settle and pay owed retransmissions.
        let sequence: Vec<usize> = (0..images.len()).chain(std::iter::repeat_n(images.len() - 1, 40)).collect();
        for family in [kvino::Family::Ridge, kvino::Family::Ella, kvino::Family::Navarro] {
            for gamma in [None, Some(lut)] {
                let mut surface = Replay::new(family, w, h, 60.0, gamma).unwrap();
                let mut encoded = Replay::new(family, w, h, 60.0, gamma).unwrap();
                let dock = kvino::DockProfile::for_family(family).unwrap();
                let mut stub = StubEncoder { dock, hashes: Vec::new() };
                for (n, &image) in sequence.iter().enumerate() {
                    let expected = surface.step(&images[image], w * 4, None);
                    let full = encoded.take_full_request();
                    let strips = stub.strips(&images[image], w, h, gamma.as_ref(), full);
                    let got = encoded
                        .step_encoded(false, strips.iter().map(|(i, hash, body)| (*i, *hash, body.as_slice())))
                        .expect("accepted");
                    assert_eq!(
                        (got.presentations, &got.records),
                        (expected.presentations, &expected.records),
                        "{family:?} gamma {} frame {n}",
                        gamma.is_some()
                    );
                }
            }
        }
    }

    #[test]
    fn identity_gamma_is_rejected() {
        let identity: [[u8; 256]; 3] = core::array::from_fn(|_| core::array::from_fn(|i| i as u8));
        assert!(Replay::new(kvino::Family::Ridge, 64, 64, 60.0, Some(identity)).is_err());
    }
}
