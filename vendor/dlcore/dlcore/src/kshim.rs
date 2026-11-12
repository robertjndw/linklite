//! Kernel-prelude shim for compiling the *literal* in-kernel vino sources in
//! userspace.
//!
//! The whole point of the chimera rig is that it does NOT re-port `cp.rs` — it
//! `include!`s the real kernel file (see [`crate::kvino`]). For that to compile
//! outside the kernel we only have to provide userspace stand-ins for the handful
//! of kernel facilities `cp.rs`/`proto.rs` lean on through `use super::*`:
//!
//!   * [`KVec`]      — the kernel's fallible `Vec`, here a thin wrapper over
//!                     [`std::vec::Vec`] with the same `with_capacity(n, flags)?`
//!                     / `push(v, flags)?` / `extend_from_slice(s, flags)?` API
//!                     and a `Deref<[T]>` so all the slice methods work.
//!   * [`GFP_KERNEL`]/[`Flags`] — the allocation-flag argument (ignored here).
//!   * [`Result`]/[`Error`]/error constants — the kernel error type.
//!   * [`crypto`]    — `Aes128` + `aes_cmac`, the two primitives `cp.rs` calls.
//!   * [`bindings`]  — the one DRM struct/fn `cp.rs` names (never called offline).
//!
//! None of this changes a single byte of `cp.rs`: the AES-CTR keystream, the
//! Dl3Cmac, the wire framing and every message builder are the kernel's own code.

use core::ops::{Deref, DerefMut};

/// The allocation flag type and `GFP_KERNEL` value from the userspace kernel shim.
pub use ::kernel::alloc::flags::Flags;
pub use ::kernel::alloc::flags::GFP_KERNEL;

/// Stand-in for `kernel::error::Error`. Preserve the errno value so the literal
/// kernel sources can distinguish malformed arguments from malformed replies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error(pub i32);

/// Error constants named directly by the included protocol sources.
pub const EINVAL: Error = Error(22);
pub const EPROTO: Error = Error(71);

impl From<::kernel::prelude::Error> for Error {
    fn from(_: ::kernel::prelude::Error) -> Self {
        Error(5)
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "kernel Error({})", self.0)
    }
}
impl std::error::Error for Error {}

/// The kernel `Result` alias (`Result<T = ()>`), reproduced exactly so the `?`
/// operator and bare `Result` return types in `cp.rs`/`proto.rs` type-check.
pub type Result<T = ()> = core::result::Result<T, Error>;

/// Userspace stand-in for `super::rng` (`vino.rs`'s thin wrapper over the kernel
/// CSPRNG `get_random_bytes`). `ake.rs`/`hdcp.rs`/the coldstart orchestration call
/// `rng::fill` for every per-session nonce (rtx/km/rn/ks/riv/OAEP seed) exactly
/// like the kernel driver does; here it's the OS CSPRNG via `getrandom`.
pub mod rng {
    /// Fills `buf` with cryptographically-random bytes from the OS CSPRNG.
    #[cfg(not(feature = "oracle"))]
    pub fn fill(buf: &mut [u8]) {
        getrandom::getrandom(buf).expect("OS CSPRNG");
    }

    #[cfg(feature = "oracle")]
    thread_local! {
        // Per-thread so parallel tests each own their stream; `None` means "not seeded, use the OS".
        static SEEDED: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    }

    /// Makes `fill` on this thread a deterministic splitmix64 stream starting at `seed`.
    ///
    /// Only for byte-identity comparison against another implementation: the output is not secret.
    #[cfg(feature = "oracle")]
    pub fn seed_rng(seed: u64) {
        SEEDED.with(|s| s.set(Some(seed)));
    }

    /// Fills `buf` from the seeded stream when `seed_rng` was called on this thread, otherwise
    /// from the OS CSPRNG.
    #[cfg(feature = "oracle")]
    pub fn fill(buf: &mut [u8]) {
        let Some(mut state) = SEEDED.with(|s| s.get()) else {
            getrandom::getrandom(buf).expect("OS CSPRNG");
            return;
        };
        for chunk in buf.chunks_mut(8) {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            chunk.copy_from_slice(&z.to_le_bytes()[..chunk.len()]);
        }
        SEEDED.with(|s| s.set(Some(state)));
    }

    #[cfg(all(test, feature = "oracle"))]
    mod tests {
        use super::*;

        fn draw(seed: u64) -> [u8; 37] {
            seed_rng(seed);
            let mut out = [0u8; 37];
            fill(&mut out);
            out
        }

        #[test]
        fn same_seed_repeats_and_different_seeds_differ() {
            assert_eq!(draw(7), draw(7));
            assert_ne!(draw(7), draw(8));
        }

        #[test]
        fn the_stream_advances_between_fills() {
            seed_rng(1);
            let (mut a, mut b) = ([0u8; 16], [0u8; 16]);
            fill(&mut a);
            fill(&mut b);
            assert_ne!(a, b);
        }
    }
}

/// The session's clock: every deadline, pacing wait and stage timing in `session.rs` and
/// `edid_fetch.rs` reads time through [`clock::now`] and waits through [`clock::sleep`].
///
/// Several protocol decisions are deadlines (carrier frames until 700 ms after the bracket anchor,
/// the 120 ms black blank, probe and push drains, the EDID readiness poll), so how many transport
/// calls a session makes depends on what the clock said between them. A trace replay can only
/// reproduce those calls byte for byte if it can hand the session the readings the recorded run
/// saw. With the `oracle` feature a host can therefore install its own clock on a thread
/// ([`clock::set_clock`]), the way `rng::seed_rng` pins the nonces.
///
/// Without the feature this is `std::time::Instant` and `std::thread::sleep` under other names,
/// with no indirection at all. With it and no clock installed, readings come from the same
/// `std::time::Instant` source, so nothing a session does changes either.
pub mod clock {
    use std::time::Duration;

    #[cfg(not(feature = "oracle"))]
    pub use std::time::Instant;

    #[cfg(not(feature = "oracle"))]
    #[inline]
    pub fn now() -> Instant {
        Instant::now()
    }

    #[cfg(not(feature = "oracle"))]
    #[inline]
    pub fn sleep(duration: Duration) {
        std::thread::sleep(duration);
    }

    /// A clock a host supplies for one thread. Readings are nanoseconds on any monotonic scale;
    /// only differences between them are used.
    #[cfg(feature = "oracle")]
    pub trait Clock {
        fn now_ns(&self) -> u64;
        fn sleep(&self, duration: Duration);
    }

    #[cfg(feature = "oracle")]
    thread_local! {
        // Per thread, like the seeded RNG: a session is driven by one thread, and parallel tests
        // or a replay next to a live engine must not see each other's clocks.
        static HOST: std::cell::RefCell<Option<Box<dyn Clock>>> = const { std::cell::RefCell::new(None) };
    }

    /// Installs `clock` for every later clock read and sleep on the calling thread, or with
    /// `None` goes back to the system clock. The previous clock, if any, is dropped here.
    ///
    /// Install it before the session engages and keep it until the session is gone: instants
    /// from two different clocks are on different scales and must not meet in one deadline.
    #[cfg(feature = "oracle")]
    pub fn set_clock(clock: Option<Box<dyn Clock>>) {
        // Taken out first and dropped after the borrow ends, so a clock whose drop reaches back
        // into this module cannot hit a held borrow.
        let previous = HOST.with(|host| std::mem::replace(&mut *host.borrow_mut(), clock));
        drop(previous);
    }

    /// A point in time on the thread's session clock. It supports exactly what the session does
    /// with `std::time::Instant`: add a duration, subtract another instant, compare, elapsed.
    #[cfg(feature = "oracle")]
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Instant(Duration);

    #[cfg(feature = "oracle")]
    fn origin() -> std::time::Instant {
        // Readings without a host clock are measured from one process-wide origin, so they are
        // the same `std::time::Instant` source, merely held as an offset.
        static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        *ORIGIN.get_or_init(std::time::Instant::now)
    }

    #[cfg(feature = "oracle")]
    pub fn now() -> Instant {
        let host = HOST.with(|host| host.borrow().as_ref().map(|clock| clock.now_ns()));
        match host {
            Some(ns) => Instant(Duration::from_nanos(ns)),
            None => Instant(origin().elapsed()),
        }
    }

    #[cfg(feature = "oracle")]
    pub fn sleep(duration: Duration) {
        let slept = HOST.with(|host| match host.borrow().as_ref() {
            Some(clock) => {
                clock.sleep(duration);
                true
            }
            None => false,
        });
        if !slept {
            std::thread::sleep(duration);
        }
    }

    #[cfg(feature = "oracle")]
    impl Instant {
        pub fn now() -> Self {
            now()
        }

        /// The reading on its own clock's scale: the host clock's nanoseconds, or nanoseconds
        /// since the process-wide origin when no host clock is installed.
        pub fn as_nanos(&self) -> u64 {
            self.0.as_nanos().min(u128::from(u64::MAX)) as u64
        }

        /// One clock read, as `std::time::Instant::elapsed` is.
        pub fn elapsed(&self) -> Duration {
            now().saturating_duration_since(*self)
        }

        pub fn saturating_duration_since(&self, earlier: Instant) -> Duration {
            self.0.saturating_sub(earlier.0)
        }

        pub fn checked_duration_since(&self, earlier: Instant) -> Option<Duration> {
            self.0.checked_sub(earlier.0)
        }
    }

    #[cfg(feature = "oracle")]
    impl core::ops::Add<Duration> for Instant {
        type Output = Instant;
        fn add(self, rhs: Duration) -> Instant {
            Instant(self.0 + rhs)
        }
    }

    #[cfg(feature = "oracle")]
    impl core::ops::AddAssign<Duration> for Instant {
        fn add_assign(&mut self, rhs: Duration) {
            self.0 += rhs;
        }
    }

    /// Saturating, as `std::time::Instant` subtraction has been since Rust 1.60.
    #[cfg(feature = "oracle")]
    impl core::ops::Sub<Instant> for Instant {
        type Output = Duration;
        fn sub(self, rhs: Instant) -> Duration {
            self.saturating_duration_since(rhs)
        }
    }

    #[cfg(all(test, feature = "oracle"))]
    mod tests {
        use super::*;
        use std::cell::Cell;
        use std::rc::Rc;

        struct Fake {
            now: Rc<Cell<u64>>,
            slept: Rc<Cell<u64>>,
        }

        impl Clock for Fake {
            fn now_ns(&self) -> u64 {
                let value = self.now.get();
                self.now.set(value + 1_000_000);
                value
            }
            fn sleep(&self, duration: Duration) {
                self.slept.set(self.slept.get() + duration.as_nanos() as u64);
            }
        }

        #[test]
        fn a_host_clock_serves_this_thread_only() {
            let now = Rc::new(Cell::new(5_000_000));
            let slept = Rc::new(Cell::new(0));
            set_clock(Some(Box::new(Fake { now: now.clone(), slept: slept.clone() })));
            let start = super::now();
            assert_eq!(start, Instant(Duration::from_millis(5)));
            assert_eq!(start.elapsed(), Duration::from_millis(1));
            let before = std::time::Instant::now();
            sleep(Duration::from_secs(30));
            assert!(before.elapsed() < Duration::from_secs(1), "a host sleep must not sleep for real");
            assert_eq!(slept.get(), 30_000_000_000);
            let other = std::thread::spawn(|| {
                let a = super::now();
                std::thread::sleep(Duration::from_millis(2));
                super::now() - a
            })
            .join()
            .unwrap();
            assert!(other >= Duration::from_millis(2), "an unrelated thread keeps the system clock");
            set_clock(None);
            let a = super::now();
            sleep(Duration::from_millis(2));
            assert!(a.elapsed() >= Duration::from_millis(2));
        }

        #[test]
        fn arithmetic_matches_std() {
            let a = Instant(Duration::from_millis(10));
            let b = a + Duration::from_millis(5);
            assert_eq!(b - a, Duration::from_millis(5));
            assert_eq!(a - b, Duration::ZERO);
            assert!(b > a);
            assert_eq!(a.checked_duration_since(b), None);
        }
    }
}

/// Userspace stand-in for the kernel's fallible `KVec`. Wraps [`Vec`] and mirrors
/// the subset of the fallible API the included kernel files use; allocation here
/// can't realistically fail, so the flag argument is accepted and ignored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KVec<T>(Vec<T>);

impl<T> KVec<T> {
    /// `KVec::new()` — matches the kernel signature.
    pub fn new() -> Self {
        KVec(Vec::new())
    }

    /// `KVec::with_capacity(n, flags)` — matches the kernel signature.
    pub fn with_capacity(cap: usize, _flags: Flags) -> Result<Self> {
        Ok(KVec(Vec::with_capacity(cap)))
    }

    /// `kv.push(v, flags)` — matches the kernel signature.
    pub fn push(&mut self, value: T, _flags: Flags) -> Result {
        self.0.push(value);
        Ok(())
    }

    /// Consume into a plain `Vec` for the userspace callers (the wire frame).
    pub fn into_vec(self) -> Vec<T> {
        self.0
    }

    /// `kv.clear()` — matches the kernel signature (`hdcp.rs`'s MGF1 loop reuses
    /// one scratch block per round).
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// `kv.len()` — matches the kernel signature (`hdcp.rs`'s MGF1 loop).
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<T> IntoIterator for KVec<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<T: Clone> KVec<T> {
    /// `KVec::from_elem(value, n, flags)` — matches the kernel signature
    /// (`ake.rs`'s zeroed-body allocator).
    pub fn from_elem(value: T, n: usize, _flags: Flags) -> Result<Self> {
        Ok(KVec(vec![value; n]))
    }

    /// `kv.extend_from_slice(s, flags)` — matches the kernel signature.
    pub fn extend_from_slice(&mut self, other: &[T], _flags: Flags) -> Result {
        self.0.extend_from_slice(other);
        Ok(())
    }

    /// `kv.resize(new_len, value, flags)` — matches the kernel signature.
    pub fn resize(&mut self, new_len: usize, value: T, _flags: Flags) -> Result {
        self.0.resize(new_len, value);
        Ok(())
    }
}

/// Userspace stand-in for the kernel's vmalloc-backed `VVec` (used by
/// `video::Encoder` for the multi-MiB shadow framebuffer). Same Vec wrapper.
#[derive(Clone, Debug, Default)]
pub struct VVec<T>(Vec<T>);

impl<T: Clone> VVec<T> {
    /// `VVec::from_elem(value, n, flags)` — matches the kernel signature.
    pub fn from_elem(value: T, n: usize, _flags: Flags) -> Result<Self> {
        Ok(VVec(vec![value; n]))
    }
}

impl<T> Deref for VVec<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.0
    }
}
impl<T> DerefMut for VVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.0
    }
}

/// `kernel::error::code::*` paths the included files name in full. The bare
/// `EINVAL` (above) and these are the same value via different paths.
pub mod kernel {
    pub use ::kernel::crypto;

    pub mod error {
        pub mod code {
            use crate::kshim::Error;
            pub const EINVAL: Error = Error(22);
            pub const EPROTO: Error = Error(71);
            pub const EOVERFLOW: Error = Error(75);
            pub const EOPNOTSUPP: Error = Error(95);
        }
    }

    // NOTE: `ake.rs`'s `id` submodule does `use kernel::bindings;` from a NESTED
    // module scope. Rust `use` path resolution doesn't see through a parent
    // module's glob import into a child module -- only the extern prelude is
    // visible at every nesting depth (exactly how the real kernel crate
    // resolves there). So the HDCP_2_2_* constants live in the separate
    // `kernel-shim` crate (renamed to `kernel` in Cargo.toml), NOT here.

    /// Userspace representation of the safe KMS mode object used by Vino's mode builder.
    pub mod drm {
        // This module shadows the extern `kernel` crate wherever the glob above reaches, so
        // anything the vendored sources name under `kernel::drm` has to be reachable from here
        // too. Re-exported rather than restated: two copies of a pixel-format code would be one
        // copy too many.
        pub use ::kernel::drm::fourcc;

        pub mod kms {
            pub mod modes {
                use crate::kshim::bindings::{drm_display_mode, drm_mode_vrefresh};

                /// The `DRM_MODE_FLAG_*` subset `cp.rs` reads, with the same values as UAPI.
                #[derive(Clone, Copy, Default, PartialEq, Eq)]
                pub struct ModeFlags(pub u32);

                impl ModeFlags {
                    pub const PHSYNC: Self = Self(1 << 0);
                    pub const NHSYNC: Self = Self(1 << 1);
                    pub const PVSYNC: Self = Self(1 << 2);
                    pub const NVSYNC: Self = Self(1 << 3);

                    pub fn contains(self, other: Self) -> bool {
                        self & other == other
                    }
                }

                impl core::ops::BitOr for ModeFlags {
                    type Output = Self;

                    fn bitor(self, rhs: Self) -> Self::Output {
                        Self(self.0 | rhs.0)
                    }
                }

                impl core::ops::BitAnd for ModeFlags {
                    type Output = Self;

                    fn bitand(self, rhs: Self) -> Self::Output {
                        Self(self.0 & rhs.0)
                    }
                }

                #[derive(Clone, Copy, Default)]
                pub struct DisplayMode(pub drm_display_mode);

                impl DisplayMode {
                    pub fn hdisplay(&self) -> u16 {
                        self.0.hdisplay
                    }
                    pub fn hsync_start(&self) -> u16 {
                        self.0.hsync_start
                    }
                    pub fn hsync_end(&self) -> u16 {
                        self.0.hsync_end
                    }
                    pub fn htotal(&self) -> u16 {
                        self.0.htotal
                    }
                    pub fn vdisplay(&self) -> u16 {
                        self.0.vdisplay
                    }
                    pub fn vsync_start(&self) -> u16 {
                        self.0.vsync_start
                    }
                    pub fn vsync_end(&self) -> u16 {
                        self.0.vsync_end
                    }
                    pub fn vtotal(&self) -> u16 {
                        self.0.vtotal
                    }
                    pub fn clock(&self) -> i32 {
                        self.0.clock
                    }
                    pub fn cea_vic(&self) -> u8 {
                        match (
                            self.clock(),
                            self.hdisplay(),
                            self.hsync_start(),
                            self.hsync_end(),
                            self.htotal(),
                            self.vdisplay(),
                            self.vsync_start(),
                            self.vsync_end(),
                            self.vtotal(),
                        ) {
                            (74_250, 1280, 1390, 1430, 1650, 720, 725, 730, 750) => 4,
                            (148_500, 1920, 2008, 2052, 2200, 1080, 1084, 1089, 1125) => 16,
                            (297_000, 1920, 2008, 2052, 2200, 1080, 1084, 1089, 1125) => 63,
                            _ => 0,
                        }
                    }
                    pub fn flags(&self) -> ModeFlags {
                        ModeFlags(self.0.flags)
                    }
                    pub fn vrefresh(&self) -> i32 {
                        // SAFETY: the stub `drm_mode_vrefresh` dereferences nothing.
                        unsafe { drm_mode_vrefresh(&self.0) }
                    }
                }
            }
        }
    }
}

impl<T> Deref for KVec<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.0
    }
}
impl<T> DerefMut for KVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.0
    }
}
impl<T> AsRef<[T]> for KVec<T> {
    fn as_ref(&self) -> &[T] {
        &self.0
    }
}

/// Userspace stand-ins for the `kernel::crypto` primitives `cp.rs` calls. The
/// AES-128 block cipher comes from the `aes` crate; AES-CMAC (DisplayLink's
/// "Dl3Cmac" core) is implemented here per RFC 4493 on top of that single block
/// primitive — exactly the math the kernel's `lib/crypto` `aes_cmac()` performs —
/// so no byte of `cp.rs`'s MAC construction is re-stated.
pub mod crypto {
    use super::Result;
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockEncrypt, KeyInit};

    /// An AES-128 key expanded once, mirroring `kernel::crypto::Aes128`.
    pub struct Aes128(aes::Aes128);

    impl Aes128 {
        pub fn new(key: &[u8; 16]) -> Result<Self> {
            Ok(Aes128(aes::Aes128::new(GenericArray::from_slice(key))))
        }

        pub fn encrypt_block(&self, block: &[u8; 16]) -> [u8; 16] {
            let mut b = *GenericArray::from_slice(block);
            self.0.encrypt_block(&mut b);
            b.into()
        }
    }

    /// `AES_ECB(key, block)` -- one 16-byte block. `hdcp.rs`'s `derive_dkey` calls this
    /// directly (mirrors `vino.rs`'s own thin `crypto::aes128_ecb` wrapper over
    /// `Aes128::encrypt_block`).
    pub fn aes128_ecb(key: &[u8; 16], block: &[u8; 16]) -> Result<[u8; 16]> {
        Ok(Aes128::new(key)?.encrypt_block(block))
    }

    /// `SHA256(data)` -- `hdcp.rs`'s MGF1/OAEP hashing.
    pub fn sha256(data: &[u8]) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        Sha256::digest(data).into()
    }

    /// `HMAC-SHA256(key, data)` -- the HDCP `H'`/`L'`/`V`/`M` verifier.
    pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
        use hmac::{Hmac, Mac};
        let mut mac = <Hmac<sha2::Sha256> as Mac>::new_from_slice(key).expect("HMAC key");
        mac.update(data);
        mac.finalize().into_bytes().into()
    }

    /// `dbl` over GF(2^128) with the AES-CMAC reduction polynomial (Rb = 0x87).
    fn dbl(b: &[u8; 16]) -> [u8; 16] {
        let msb = b[0] >> 7;
        let mut o = [0u8; 16];
        for i in 0..16 {
            o[i] = b[i] << 1;
            if i + 1 < 16 {
                o[i] |= b[i + 1] >> 7;
            }
        }
        if msb == 1 {
            o[15] ^= 0x87;
        }
        o
    }

    /// `AES-CMAC-128(key, data)` (RFC 4493) — the same one-shot the kernel's
    /// `kernel::crypto::aes_cmac` exposes. Used by `cp::dl3cmac_tag`.
    pub fn aes_cmac(key: &[u8; 16], data: &[u8]) -> [u8; 16] {
        let c = Aes128::new(key).expect("aes key");
        let l = c.encrypt_block(&[0u8; 16]);
        let k1 = dbl(&l);
        let k2 = dbl(&k1);

        let complete = !data.is_empty() && data.len() % 16 == 0;
        let nblocks = if data.is_empty() {
            1
        } else {
            data.len().div_ceil(16)
        };

        let mut mlast = [0u8; 16];
        let last_off = (nblocks - 1) * 16;
        if complete {
            for i in 0..16 {
                mlast[i] = data[last_off + i] ^ k1[i];
            }
        } else {
            let rem = if data.is_empty() {
                &[][..]
            } else {
                &data[last_off..]
            };
            let mut padded = [0u8; 16];
            padded[..rem.len()].copy_from_slice(rem);
            padded[rem.len()] = 0x80;
            for i in 0..16 {
                mlast[i] = padded[i] ^ k2[i];
            }
        }

        let mut x = [0u8; 16];
        for j in 0..nblocks - 1 {
            let off = j * 16;
            for i in 0..16 {
                x[i] ^= data[off + i];
            }
            x = c.encrypt_block(&x);
        }
        for i in 0..16 {
            x[i] ^= mlast[i];
        }
        c.encrypt_block(&x)
    }
}

/// DRM mode fields consumed by `cp::timing_from_drm_mode`.
pub mod bindings {
    /// Layout-irrelevant userspace stand-in for `struct drm_display_mode`; only
    /// the fields `cp.rs` reads are present (types matched to their usage there).
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct drm_display_mode {
        pub clock: i32,
        pub hdisplay: u16,
        pub hsync_start: u16,
        pub hsync_end: u16,
        pub htotal: u16,
        pub vdisplay: u16,
        pub vsync_start: u16,
        pub vsync_end: u16,
        pub vtotal: u16,
        pub flags: u32,
    }

    /// Userspace equivalent of `drm_mode_vrefresh` for progressive modes.
    ///
    /// # Safety
    /// `mode` must point to a valid `drm_display_mode`.
    pub unsafe fn drm_mode_vrefresh(mode: *const drm_display_mode) -> i32 {
        let mode = &*mode;
        if mode.clock <= 0 || mode.htotal == 0 || mode.vtotal == 0 {
            return 0;
        }
        let numerator = mode.clock as u64 * 1000;
        let denominator = mode.htotal as u64 * mode.vtotal as u64;
        numerator.div_ceil(denominator) as i32
    }
}

// ---- logging ---------------------------------------------------------------
//
// The vendored kernel files call `pr_info!`/`pr_warn!` unqualified. In the kernel those come from
// the prelude; here they go to stderr, prefixed so rig output stays distinguishable from the
// harness's own. `#[macro_export]` puts them at the crate root, and the `pub use` re-exports them
// through this shim so `use super::*;` inside a vendored module finds them.

#[macro_export]
macro_rules! pr_info {
    ($($arg:tt)*) => {{ eprint!("[vino] "); eprint!($($arg)*); }};
}
#[macro_export]
macro_rules! pr_warn {
    ($($arg:tt)*) => {{ eprint!("[vino warn] "); eprint!($($arg)*); }};
}
#[macro_export]
macro_rules! pr_err {
    ($($arg:tt)*) => {{ eprint!("[vino err] "); eprint!($($arg)*); }};
}
#[macro_export]
macro_rules! pr_debug {
    ($($arg:tt)*) => {{ let _ = format_args!($($arg)*); }};
}

/// The driver's own diagnostic macro, defined in `vino.rs` (which is not vendored). In the kernel
/// it is gated on the `debug` module parameter; here it is silent, like `pr_debug!`.
#[macro_export]
macro_rules! vino_debug {
    ($($arg:tt)*) => {{ let _ = format_args!($($arg)*); }};
}

pub use crate::{pr_debug, pr_err, pr_info, pr_warn, vino_debug};

// ---- monotonic time --------------------------------------------------------
//
// The codec times its own phases with `Instant::<Monotonic>::now()`. Only the arithmetic the
// vendored code actually performs is provided: subtract two instants, add a delta, read it back.
// Backed by `std::time::Instant`, which is the same clock the kernel's monotonic source is.

/// Marker for the monotonic clock source, matching `kernel::time::Monotonic`.
pub struct Monotonic;

pub struct Instant<C = Monotonic>(std::time::Instant, core::marker::PhantomData<C>);

// Hand-written, not derived: `#[derive(Copy)]` would add a `C: Copy` bound, and `C` is only a
// clock marker that never exists at runtime.
impl<C> Clone for Instant<C> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<C> Copy for Instant<C> {}

impl<C> Instant<C> {
    pub fn now() -> Self {
        Self(std::time::Instant::now(), core::marker::PhantomData)
    }

    pub fn elapsed(&self) -> Delta {
        Delta(self.0.elapsed())
    }
}

impl<C> core::ops::Sub for Instant<C> {
    type Output = Delta;
    fn sub(self, rhs: Self) -> Delta {
        Delta(self.0.saturating_duration_since(rhs.0))
    }
}

impl<C> core::ops::Add<Delta> for Instant<C> {
    type Output = Self;
    fn add(self, rhs: Delta) -> Self {
        Self(self.0 + rhs.0, core::marker::PhantomData)
    }
}

#[derive(Copy, Clone)]
pub struct Delta(std::time::Duration);

impl Delta {
    pub fn from_millis(ms: i64) -> Self {
        Delta(std::time::Duration::from_millis(ms.max(0) as u64))
    }
    pub fn as_millis(&self) -> i64 {
        self.0.as_millis() as i64
    }
    pub fn as_micros_ceil(&self) -> i64 {
        self.0.as_micros() as i64
    }
    pub fn as_nanos(&self) -> i64 {
        self.0.as_nanos() as i64
    }
}
