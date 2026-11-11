# Contributing to LinkLite

Thanks for your interest. LinkLite is early, and dock compatibility reports are as valuable as code: every dock and monitor combination behaves a little differently. The recorded hardware baseline is a Humanscale M/Connect 2 with two HP Z27k G3 monitors; other models are not individually certified.

## Getting set up

Requirements: an **Apple Silicon Mac** supported by **Xcode 26 or newer** (or the corresponding command-line tools with the macOS 26 SDK), a **Rust** toolchain, and **Python 3.9 or newer** for packaging license notices and release sources. The Python helper uses only the standard library. Building the Liquid Glass interface needs the newer SDK; the resulting app still targets **macOS 14** and later. Install Rust with [rustup](https://rustup.rs); Homebrew's Rust ships a standard library built for the host macOS version, so an app built with it refuses to run on older macOS releases. `scripts/build-app.sh` prefers rustup's stable toolchain when both are installed.

```sh
scripts/build-app.sh          # builds the engine and the app, signs dist/LinkLite.app
open dist/LinkLite.app
```

The script signs with the first Developer ID Application or Apple Development identity in your keychain (override with `LINKLITE_SIGN_IDENTITY`). Without one the app is signed ad hoc, and macOS asks for Screen Recording permission again after every rebuild.

Before running a build, quit DisplayLink Manager: only one program can drive the dock.

The project is split into:

- **`engine/`**: the dock engine in Rust. USB transport, HDCP 2.2 authentication, encrypted control plane, mode setting, video codec, hardware cursor, and rate limiting, exposed through a C ABI in `engine/include/dllite.h`. libusb is built from source and linked statically.
- **`vendor/vino/`**: the vendored Vino/chimera userspace DL3 protocol core (GPL-2.0-or-later). See [Changing `vendor/vino/`](#changing-vendorvino).
- **`vendor/libusb1-sys/`**: the pinned libusb bindings and statically linked native library, with descriptor-parser safety backports. Provenance and refresh requirements are in its [README](vendor/libusb1-sys/README.md).
- **`Sources/LinkLite/`**: the Swift menu-bar app. Virtual displays, screen capture, pointer tracking, display sleep/wake.
- **`Sources/CDLLite/`**: the C module that exposes the engine's C ABI and the private CoreGraphics virtual-display classes to Swift.
- **`Sources/dlprobe/`**, **`Sources/DLProbeCore/`**: a read-only USB preflight tool for checking a dock's link speed and descriptors.
- **`docs/`**: [development notes](docs/DEVELOPMENT.md) (the hardware gates and their status) and the [benchmark method](docs/BENCHMARK.md).

## Release source archives

The app bundle includes project and third-party license notices. Each release also publishes a separate source archive, so dependency source does not increase the installed app's size.

After committing the release source, create its archive from the full commit SHA:

```sh
python3 scripts/release-sources.py source \
  --commit <full-release-commit-sha> \
  --output dist/LinkLite-<version>-source.tar.gz
```

This packages the committed tree, not uncommitted working files, with locked Rust dependency sources, an archive-local Cargo configuration, and Sparkle source at its pinned revision. `SOURCE-RELEASE.txt` records provenance and build prerequisites. Apple and Rust toolchains are not bundled. Creating the archive needs access to the pinned dependency sources; normal SwiftPM builds still fetch the pinned Sparkle binary unless it is already cached.

## Preparing signed releases

Use the [release and recovery runbook](docs/RELEASING.md) for qualification, staging, validation and publication order. Release packaging requires an explicit channel and owner-provided Developer ID identity and notarization profile:

```sh
LINKLITE_SIGN_IDENTITY='<Developer ID fingerprint>' LINKLITE_NOTARY_PROFILE='<keychain profile>' \
  scripts/release.sh --channel candidate --candidate-base-url https://example.invalid/actual-staging-directory

LINKLITE_SIGN_IDENTITY='<Developer ID fingerprint>' LINKLITE_NOTARY_PROFILE='<keychain profile>' \
  scripts/release.sh --channel public
```

Replace all placeholders before use. Candidate assets and their separate feed stay in `dist/candidates/`; candidate mode does not redirect the shipped updater. Public mode updates root `appcast.xml`. Upload and validate assets before publishing any feed. Do not execute the public command until promotion is approved; do not reuse already released versions. Release-contract regression checks: `python3 scripts/test-release.py` (macOS, Python 3.9+).


## Running the tests

```sh
# Native USB descriptor safety, without hardware (AddressSanitizer + UndefinedBehaviorSanitizer)
scripts/test-usb-descriptors.sh

# Engine
cargo test --locked --manifest-path engine/Cargo.toml

# Vendored protocol core (builds through engine/, since its optional Linux dependency is not vendored)
(cd engine && cargo test --locked -p vino-chimera)

# USB transport queue bounds and cancellation ownership
(cd engine && cargo test --locked -p vino-driver)

# Swift package: build/sign the application first, then exercise the Swift package
scripts/build-app.sh
swift test --force-resolved-versions
```

CI runs the same steps on an Apple Silicon macOS 15 runner with Xcode 26.2 selected explicitly. The newer SDK is required to compile Liquid Glass; it does not raise the app's minimum macOS version. None of the tests need a dock attached.

## Testing on a dock

Malformed DL-6xxx video writes can reset the dock off the USB bus or destabilise the live desktop. If you change the engine or the vendored core:

- Use a spare dock, or save your work first.
- Treat a dock reset, USB stall, forced logout, or macOS crash as a failed test, not a flaky one.
- Never send undocumented vendor writes to a dock. The [development notes](docs/DEVELOPMENT.md) list the safety boundaries this project works within.

Logs go to the unified log under the subsystem `com.ball.linklite`:

```sh
log stream --predicate 'subsystem == "com.ball.linklite"' --level info
```

## Changing `vendor/vino/`

`vendor/vino/` is a copy of upstream [Vino](https://github.com/cameron1729/Vino) `revdi/` at a pinned commit. Every local change to it must be recorded in [`vendor/vino/UPSTREAM.md`](vendor/vino/UPSTREAM.md): which files and functions changed, why, and what was measured. Keep the per-file `SPDX-License-Identifier` headers intact. Changes that are not specific to macOS are worth offering upstream as well.

## Reporting dock compatibility

Use the **Dock compatibility report** issue template, whether your dock works or not. Include:

- The dock's make and model, and the monitors attached to it (model, the dock port each one uses, resolution, refresh rate).
- Your macOS version and Mac model.
- The output of the preflight probe, run from a clone of this repository:

  ```sh
  swift run dlprobe --descriptors
  ```

  `dlprobe` looks for the M/Connect 2 product ID (`600f`) by default. For another dock, find its **Product ID** in System Information › USB and pass it: `swift run dlprobe --descriptors --product 6006`. The probe only reads what macOS already knows about the device; it never claims the dock or sends it vendor requests.

## Pull requests

- Keep changes focused; one logical change per PR.
- Add or update tests for engine logic where it can be tested without hardware.
- Match the surrounding code style, and keep the `SPDX-License-Identifier` header on new source files.
- Measure performance claims. [docs/BENCHMARK.md](docs/BENCHMARK.md) describes how the project compares builds.
- Be precise in user-facing copy: LinkLite works with DisplayLink docks but is not affiliated with Synaptics or DisplayLink. Do not use "DisplayLink" as part of the app's name or branding.

## Reporting bugs

Use the **Bug report** issue template. Display behaviour varies a lot across docks, monitors, and Macs, so the dock model, monitors, macOS version, Mac model, and logs all matter.

Security issues should be reported privately; see [SECURITY.md](SECURITY.md).

## License

By contributing, you agree that your contributions are licensed under the [GNU General Public License, version 2 or later](LICENSE).
