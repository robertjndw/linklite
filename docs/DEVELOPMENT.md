# Development notes

The engineering gate log for LinkLite: the hardware and safety gates the replacement had to pass, with the measured status of each.

The target is intentionally narrow: keep the Humanscale M/Connect 2 (`0x17e9:0x600f`) and replace only the macOS DisplayLink user-space display pipeline with a smaller local implementation.

This cannot turn M/Connect 2 into a native Thunderbolt or DisplayPort Alt Mode dock. Humanscale documents its display path as DisplayLink-dependent. A replacement must still create displays in macOS, capture their pixels, compress changes, and speak the dock's proprietary DL-6xxx USB protocol.

## Hard gates

### Gate 0: USB 3.x transport

Run:

```sh
swift run dlprobe
```

The result must be `PASS` at 5 Gbps or faster. Do not try to compensate for a USB 2.0 link with more aggressive host compression; that works against the lightweight goal.

Status 2026-11-03: **PASS** at 5000 Mbps on the current desk. An earlier setup reported 480 Mbps; if that returns, check the upstream full-feature USB-C cable, adapters, port connection, and macOS accessory authorization, and re-run `dlprobe` after each physical change.

### Gate 1: protocol identity

Independently confirm that product `0x600f` uses the same DL3 control and video endpoint layout documented for the DL-6000 family. This gate is read-only: descriptors and known-safe device queries only.

Licensing: the Vino sources carry per-file `SPDX-License-Identifier: GPL-2.0` / `GPL-2.0-or-later` headers (and `revdi/LICENSES/`), so they are vendored under those terms in `vendor/dlcore/` and this project is GPL-2.0-or-later.

Status 2026-11-03: **layout matches DL-6xxx "Ridge"** (`swift run dlprobe --descriptors`, `bcdDevice 0x3104`):

| Interface | Class | Endpoints | Ridge reference (Dell D6000 `17e9:6006`) |
|---|---|---|---|
| 0 alt 0 | `ff/00/03` vendor | bulk `0x02` OUT, `0x84` IN, `0x08` `0x0a` `0x0b` `0x0c` OUT, all 1024 B | control `0x02`/`0x84`, per-head video bulk-OUT incl. `0x08`, `0x0b` |
| 0 alt 1 | `ff/00/04` vendor | bulk `0x08` OUT | not documented |
| 1 | `fe/01/01` DFU | none | firmware update path; never touch |
| 2-4 | USB audio | iso/int | n/a |
| 5-6 | CDC NCM ethernet | int + bulk | n/a |

Public observations (Vino `docs/protocol/generations.md`, `docs/ridge.md`): Ridge's control plane is plaintext init, then an HDCP 2.2 AKE, then AES-CTR + CMAC-sealed records; video is a vendor wavelet codec per head; the dock has one EDID handler shared between heads. Open in Gate 1: read the running firmware version (a vendor descriptor, not `bcdDevice`) only once a standard-request path for it is confirmed.

### Gate 2: control-plane engagement

Use a spare dock or a deliberate maintenance window. DisplayLink Manager must release its exclusive USB ownership before another process can claim the interface, so the attached DisplayLink screens will go dark during this test.

Success means:

- claim and release the correct USB interface cleanly
- complete the required session and encrypted control-plane setup
- read monitor identity and state
- leave the dock operational after process exit

No video endpoint writes are allowed at this gate.

Status 2026-11-03: **PASS** on the live dock. HDCP 2.2 AKE and the sealed control session complete on macOS over libusb; both monitors read as `HPN HP Z27k G3`, 3840x2160@60 (once the monitors were powered on, and after the local EDID readiness patch in `vendor/dlcore/ORIGIN.md`); quitting the app releases the dock.

### Gate 3: one-head video proof

On a spare dock, send one known-safe solid frame to one output after a valid mode set. Incorrect DL-6xxx video writes can reset the dock or destabilize the live desktop, so this is not a first test on the user's only active dock.

Success means one monitor exits standby, shows the expected frame, and the dock survives repeated start/stop cycles.

Status 2026-11-03: **PASS**. Both heads lit at 3840x2160@60 with a solid test colour each, then with the desktop. Codec checked offline against Vino's decoder on a real screenshot: 49.7 dB PSNR, max error 8/255, no decode failures.

### Gate 4: macOS display pipeline

Only after the hardware proof:

- create one macOS virtual display
- capture dirty regions instead of full frames when possible
- reuse a bounded IOSurface/buffer pool
- encode and send one head, then add the second
- handle hotplug, sleep/wake, resolution changes, and orderly shutdown

macOS does not expose a supported public API for a general USB-backed display. DisplayLink Manager itself imports private virtual-display APIs, so a same-dock replacement will be OS-version-sensitive and unsuitable for the Mac App Store.

Status 2026-11-03: **working on both heads**: CGVirtualDisplay + ScreenCaptureKit with SCK dirty rects (only changed areas are converted and hashed), dock hardware cursor, settle pass and three-slot delivery (no stale boxes or flicker, confirmed on the monitors), 100 MB/s video rate limit (no dock resets under App Exposé), reuse of a virtual display across a dock reset, display sleep/wake mapped to sink power, slow probing of empty sockets for monitors switched on later. Measured idle: ~135 MB footprint, under 1 s of CPU per minute.

## Acceptance criteria

- two HP Z27k G3 displays at 3840x2160 and 60 Hz
- the external displays remain usable with DisplayLink Manager stopped
- bounded memory after an eight-hour run, with no upward trend
- materially lower memory and CPU than an A/B run of DisplayLink Manager on the same 5 Gbps link and workload
- repeatable hotplug and sleep/wake without a process crash, dock reset, logout, or reboot
- DisplayLink Manager remains installed until all criteria pass

Status 2026-11-04: criteria 1 and 2 were met historically on the M/Connect 2 monitors. The [2026-11-03 A/B/A benchmark](BENCHMARK.md#results) measured lower memory but **higher CPU**, so the original lower-memory-and-CPU criterion remains unresolved, not passed. The owner must either demonstrate lower CPU on an agreed representative workload or explicitly revise that criterion with the tradeoff recorded before 1.0. Eight-hour bounded memory and repeated sleep/wake/hotplug remain pending; component checks do not close them.

Protected-content playback is out of scope for the first implementation.

## Engineering hardening — 2026-11-04 (v0.3.7)

These changes first shipped in v0.3.7; the latest public release is v0.3.15. LinkLite remains beta software; publication does not close the outstanding hardware gates or establish 1.0 readiness.

The C ABI and wire encoding are unchanged. Capture damage is converted lazily at the ABI boundary, frame spans are checked before constructing Rust slices, and cursor/damage/candidate storage is reused. Cached encoded strips are borrowed during packing, and USB chunks are filled without an intermediate full-frame stream. Byte-equivalence regressions cover the affected encoding paths. See [BENCHMARK.md](BENCHMARK.md#hardware-free-optimization-checks--2026-11-04) for the limited measurements and their method.

The native dependency is pinned in `vendor/libusb1-sys/` with documented upstream descriptor-parser backports. The USB receive queue now has a hard capacity and fails closed on overflow. Teardown waits for all submitted transfer callbacks before releasing memory; if libusb never completes cancellation, shutdown can wait indefinitely. Do not replace that wait with a timeout that frees live callback state.

Hardware-free regressions and signed application builds pass. Two-monitor rendering, physical unplug/replug, sleep/wake, the receive-queue high-water mark, and an eight-hour memory run still need live-hardware validation for these changes. These checks do not supersede the acceptance criteria above.

### Hardware cursor

LinkLite draws the pointer with the dock's hardware cursor. `CursorTracker` combines global/local event monitors with adaptive timer polling (10 Hz idle, 120 Hz while the pointer moves without reported events) and skips unchanged positions. Polling is required because macOS delivers no pointer events to global monitors while screenshot selection holds the pointer; before v0.3.10 the dock pointer froze and the crosshair was missing during selection. Polling pauses while the displays sleep and resynchronises the dock pointer on wake.

Known limitation: when an app hides the pointer, the dock still draws it, because macOS has no public API to read cursor visibility.

### Gamma tables

ScreenCaptureKit returns pre-gamma pixels, so a display's transfer table (`CGSetDisplayTransferByTable`, used by gamma-based warm-tint and Night Shift-style utilities) never reaches captured frames; a real GPU output would apply it. LinkLite polls each virtual display's table every 0.5 s (and on attach, capture restart and screen-parameter changes), converts it to 3x256 bytes, and hands changes to the engine with `dll_engine_set_gamma`. The engine applies it while copying changed areas into the frame it encodes (split across cores for large copies: a full 4K frame measured 1.4 ms, 4.8 ms on one core) and repaints the whole picture when the table changes; an identity table costs nothing. The dock's hardware cursor image is re-uploaded through the same table (un-premultiplied, mapped, re-premultiplied) whenever the table or the pointer shape changes. SkyLight accessibility colour matrices and other private display filters are not visible to LinkLite and are not applied.

## Performance logging

`defaults write com.ball.linklite PerformanceLogging -bool true` (read at launch; default off) makes LinkLite log one line per attached head each second under subsystem `com.ball.linklite`, category `perf`: captured fps, presents/s, budget-skipped/s, coarse/s (presents whose change was too scattered for exact damage rectangles and went as a coarse superset), MB/s, and average milliseconds per present for take, plan, encode, pack, USB submit and status poll, plus per-frame capture conversion time and the engine's loop rate and budget-wait time. Stream it with `log stream --predicate 'subsystem == "com.ball.linklite" AND category == "perf"' --level info`. The counters come from `dll_engine_stats` and are always maintained as relaxed atomics; the switch only controls the once-a-second read and log.

## Pre-1.0 qualification matrix

The full gates below remain **pending for the release build** as of 2026-11-05. The latest hardware probe found no DisplayLink USB device. An existing-profile v0.3.6 → v0.3.7 GUI update and settings expansion/collapse passed on macOS 26.6 with login/update choices and the signing identity preserved; this does not close the fresh-profile, attached-display or other-OS gates. Historical working-desktop observations above are not repeated-cycle qualification. Run on a spare dock or in an owner-approved maintenance window, with saved work and a usable built-in display. Keep DisplayLink Manager installed but quit, and disable neither security protections nor consent checks to obtain a pass.

For every run, retain the exact app version/build and source SHA, macOS version/build, Mac/dock/monitor models, cable and USB link speed, monitor modes and scaling, start/end timestamps, operator, steps, expected/actual result, unified logs and benchmark CSV where relevant. Record each attempted cycle, including failed ones; do not discard failures as flakes. Use [contributor logging and dock-report instructions](../CONTRIBUTING.md#testing-on-a-dock). A pass applies only to the recorded combination.

| Gate | Reproducible procedure | Required evidence / pass condition |
|---|---|---|
| Both heads, 4K60 and scaling | Confirm USB 3 with `swift run dlprobe`; attach both HP Z27k G3 monitors. Verify 3840×2160 at 60 Hz on each. Show text, pointer motion and the benchmark animation on each head, then both. Change each head between native and available HiDPI "looks like" modes and back, ten times per head. | Record actual refresh, pixel and point dimensions for each mode. No corruption, stale regions, missing pointer or lost display; both heads recover after each change. |
| Eight-hour bounded memory | Follow the [soak procedure](BENCHMARK.md#eight-hour-memory-qualification-pending), without restarting LinkLite. | Complete time series, workload/event log and comparable-window memory analysis; no continuing upward trend or missing interval masked by an average. |
| Physical hotplug | Twenty cycles: disconnect upstream USB, wait 10 s, reconnect, allow up to 60 s for both heads. Also repeat ten cycles for each monitor's display cable independently. | Both displays return in the recorded modes on every cycle, no crash, unexpected dock reset, USB stall, logout or reboot. |
| Sleep/wake | Twenty user-initiated system sleep/wake cycles with both heads connected; wait at least 30 s asleep and allow up to 60 s after wake. Include five cycles with the dock disconnected while asleep and reconnected after wake. | No stale screens, missing heads or accumulated virtual displays; same failure rules as hotplug. Record each wake delay. |
| Monitor power | Ten off/on cycles per monitor, then ten with both off/on. Leave off at least 30 s; also launch once with both off, then power them on. | Both heads are discovered and return to their selected modes within 60 s; the other head remains usable during single-head cycles. |
| Quit during disconnect | Ten times, initiate normal menu-bar Quit immediately before/during upstream unplug. Observe process exit and reconnect only after exit. | Normal termination within 30 s and successful subsequent launch; a hang is a failed gate, not permission to free live USB state. |
| Update during disconnect | In an isolated candidate test installation, exercise a signed newer candidate update with the dock connected, then disconnected during download, and during requested quit/relaunch. Keep the built-in display available. | Record offered/installed versions and consent, signature validation, normal exit and relaunch; no partial installation or ownership conflict. See [release staging](RELEASING.md#candidate-staging). |
| DisplayLink Manager conflict | In a maintenance window, start LinkLite while Manager owns the dock, then test the reverse. Quit the owning app normally before retrying the other; never force simultaneous ownership. | Non-owner fails safely without disrupting the owner; after normal release the selected app can drive both heads. Record behavior rather than assuming automatic takeover. |
| Fresh-profile consent and login | On an owner-approved fresh macOS account, launch the signed release without pre-existing Screen Recording permission; test deny, grant through Settings, quit/reopen, and revoke while running. Exercise Open at Login on/off and log out/in. | Honest permission UI, no capture before consent, recovery after grant/reopen, and login behavior matches selection. Do not reset the user's existing privacy database. |
| Supported macOS UI | Repeat launch, permissions, menu-bar/settings UI, updater and attached-display checks on each macOS release claimed as tested, including macOS 14. | Per-OS receipts; deployment target and macOS CI compilation are not hardware/GUI compatibility proof. |

The cycle counts and observation windows above define the next qualification campaign, not historical measurements. Stop on any crash, reset, stall, forced logout, unexpected capture or persistent corruption; preserve logs and file a failure before continuing. Receive-queue overflow must fail closed. Retain available queue diagnostics; if no high-water measurement is available, leave that evidence pending rather than infer it from a successful screen image.

### Shutdown safety and recovery

USB cancellation can hang indefinitely if libusb never returns all callbacks. This known risk remains open. Never "fix" it by timing out and freeing transfer or callback state still owned by libusb. A 30-second observation failure above does not authorize unsafe teardown. Save diagnostics from the built-in display; do not launch a second dock owner while the first remains alive. If normal quit cannot complete, stop qualification and have the owner perform a controlled recovery/reboot after saving work. Never terminate WindowServer or kernel_task. Re-test the failure on a spare setup before closing the gate.

### Stable 1.0 decision

- [ ] Complete the matrix with raw evidence, exact hardware/OS scope, and no unresolved safety failures.
- [ ] Resolve the original lower-CPU criterion explicitly; retain the historical CPU tradeoff in public claims.
- [ ] Resolve or explicitly disposition the shutdown-cancellation hang risk with ownership safety preserved.
- [ ] Verify signed candidate install/update, clean-consent and supported-OS UI paths.
- [ ] Verify the protected-main policy for the exact 1.0 head: required pull request and up-to-date `Build and test` CI, administrators covered, no force push/deletion, and release-contract checks passing. Read the effective GitHub policy; a green run alone does not establish protection.
- [ ] Complete owner-controlled signing-key recovery verification and the [release/recovery runbook](RELEASING.md).
- [ ] Review compatibility/support wording against actual evidence; keep [privacy](../PRIVACY.md) and [private vulnerability reporting](../SECURITY.md) links available without adding telemetry.

Public release evidence (latest v0.3.15, plus historical v0.3.7 updater evidence) is recorded in the [release runbook](RELEASING.md#current-evidence); it is not a substitute for these gates.



## Safety boundaries

- Never terminate `WindowServer` or `kernel_task`.
- Never send undocumented vendor or video writes to the live dock.
- Never uninstall DisplayLink Manager before the replacement passes Gate 4.
- Treat a dock reset, USB stall, forced logout, or macOS crash as a failed test.

## Reference implementations and documentation

- [Humanscale M/Connect 2 user guide](https://humanscale.getbynder.com/m/49d81af0b0b26c05/original/hs-technology-tool-m-connect2-user-guide.pdf)
- [Synaptics DL-6000 series](https://drivers.synaptics.com/products/displaylink-graphics/integrated-chipsets/dl-6000)
- [FireBurn/Vino clean-room research](https://lwn.net/Articles/1090989/) (drm/vino v3 cover letter; drives DL-6xxx on Linux)
- [Vino protocol notes](https://github.com/cameron1729/Vino/tree/main/docs) (the `revdi/` sources LinkLite builds on are vendored in `vendor/dlcore/` under their per-file GPL-2.0 / GPL-2.0-or-later SPDX headers; see `vendor/dlcore/ORIGIN.md`)
- [DisplayLink Manager 16.2 release notes](https://drivers.synaptics.com/sites/default/files/release_notes/2026-07/DisplayLink%20Manager%20Graphics%20Connectivity16.2-Release%20Notes.txt)
