# dlcore

Userspace DisplayLink DL3 protocol core, derived from Vino's `chimera` rig (see
[`../ORIGIN.md`](../ORIGIN.md)). It compiles the kernel Vino protocol and codec sources
(the copies under `vino/`, which keep their kernel layout) behind a small kernel-prelude shim and
drives a dock over libusb through `dlcore-usb`.

## Offline oracle

`chimera-prove` replays captured DisplayLinkManager traffic through the same protocol sources used
by the kernel driver:

```sh
cargo run --bin chimera-prove
cargo test -p dlcore
```

The oracle checks byte-exact control sealing, message builders, codec records, mode profiles,
video-arm construction, userspace DDC/CI reply bounds and strip-grid padding. Hardware-dependent
operations are deliberately excluded from the automated suite.

## Deterministic randomness (`oracle` feature)

Per-session nonces and keys come from `kshim::rng::fill`, which reads the OS CSPRNG. With
`--features oracle`, calling `kshim::rng::seed_rng(seed)` switches the calling thread to a
splitmix64 stream so two implementations can be compared byte for byte. The feature is off by
default and must never be enabled in a shipping build.

```sh
cargo test -p dlcore --features oracle
```
