# Benchmark

How LinkLite's resource use is measured against DisplayLink Manager on the same dock, monitors, and workload. The scripts are in [`benchmark/`](benchmark/).

## Method

### Hardware

- MacBook Pro, Apple M5 Pro, 64 GB, macOS 26.6.
- Humanscale M/Connect 2 dock (DL-6xxx "Ridge", USB `17e9:600f`) on a 5 Gbps USB link (checked with `swift run dlprobe`).
- Two HP Z27k G3 monitors, 3840x2160 @ 60 Hz, both set to "looks like" 1920x1080 (HiDPI).

### Runs

Each benchmark is an **A/B/A** sequence on one boot, driven by [`benchmark/run.sh`](benchmark/run.sh):

1. LinkLite (`lite1`)
2. DisplayLink Manager (`dlm`)
3. LinkLite again (`lite2`)

Repeating LinkLite after DisplayLink Manager shows how much of any difference is drift (thermal state, background activity) rather than the driver. Between runs the script quits one driver, opens the other, and waits until both external screens are back.

Each run has three phases:

| Phase | Length | Workload |
|---|---|---|
| `settle` | 15 s | Nothing; lets the displays come up and the driver reach steady state. Not scored. |
| `desktop` | 45 s | The live desktop as it is: menu-bar clock, idle windows. |
| `animation` | 45 s | [`benchmark/motion.html`](benchmark/motion.html), a deterministic 60 fps canvas animation (scrolling gradient, drifting cards, a frame counter) in a 1600x900 Chrome window on the right-most external screen. It changes most of the window every frame, like scrolling a web page. Chrome runs with an isolated, temporary profile so extensions and other tabs do not interfere. |

### Metrics

[`benchmark/sampler.py`](benchmark/sampler.py) samples once a second for the driver's processes (`LinkLite`; `DisplayLinkUserAgent` and `DisplayLinkXpcService` for DisplayLink Manager) and for `WindowServer`, which composites the virtual displays for either driver:

| Metric | Source |
|---|---|
| Per-process GPU time (ms of GPU per second) | The AGX driver's per-client `AppUsage` › `accumulatedGPUTime` counters, read with `ioreg -r -c AGXDeviceUserClient`, summed per process and differenced between samples. This is the counter Activity Monitor's GPU column uses. |
| Whole-GPU utilisation (%) | `Device Utilization %` in the `IOAccelerator` performance statistics (`ioreg -r -c IOAccelerator`). |
| CPU (%), memory (MB), energy impact | `/usr/bin/top -l 0 -s 1 -stats command,cpu,mem,power`. |

Each phase is summarised per driver from its one-second samples. WindowServer is reported alongside the driver, because work a driver pushes onto the compositor still costs the user.

### Running it

Install LinkLite and DisplayLink Manager, attach the dock with both monitors, and start with LinkLite driving the dock and DisplayLink Manager quit. Then:

```sh
docs/benchmark/run.sh
```

The run takes about six minutes. Leave the Mac alone while it runs. `run.log` and `samples.csv` are written to `$TMPDIR/linklite-bench` (override with `BENCH_OUT`); set `LINKLITE_APP` if LinkLite is not in `/Applications`.

Use a unique output directory for each run: `run.sh` deletes any previous `run.log`, `samples.csv` and `stop` in that directory. It requires Google Chrome and controls driver application launches/quits. Its two-screen timeout only logs a warning; its quit loops do not enforce ownership release. Supervise the handoffs and stop the run if the outgoing driver remains alive or either screen is missing. Do not treat the resulting CSV as a valid comparison in that case.

```sh
BENCH_OUT="$HOME/linklite-bench-$(date +%Y%m%d-%H%M%S)" docs/benchmark/run.sh
# Substitute the actual directory printed by run.sh:
python3 docs/benchmark/summarize.py /path/to/run/samples.csv /path/to/run/results.json
```

`BENCH_ONLY=lite` runs only `lite1` without a Manager comparison. The summary averages `lite1` and `lite2` and scores only `desktop` and `animation`; it is not a memory-trend detector. Inspect missing samples, process exits and warnings before accepting means. Record exact installed app versions and source SHA separately.

## Eight-hour memory qualification (pending)

No eight-hour pass is recorded. Use the sampler directly, not repeated `run.sh` invocations: driver restarts would hide growth. Start the signed build with Manager quit and both heads verified at 4K60. Keep the Mac awake using normal owner-approved settings for this run; test sleep separately. Run from the repository root in a shell kept open for the duration:

```sh
OUT="$HOME/linklite-soak-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT"
printf 'lite1,desktop\n' > "$OUT/phase"
python3 docs/benchmark/sampler.py "$OUT" &
SAMPLER_PID=$!
# Keep this shell open. Perform and timestamp the workload below for eight hours.
# At the end (or on an aborted run):
touch "$OUT/stop"
wait "$SAMPLER_PID"
```

Do not execute the final two commands until the observation period ends. The sampler writes `samples.csv` afresh, samples about once a second, and stops when `stop` exists. It does not create the output directory, drive a workload, detect restarts, or enforce an eight-hour duration. Its `top` reader runs as a child; after shutdown confirm no sampler-owned `top` remains and terminate only that identified child if needed, not unrelated monitoring sessions.

For each of eight hours, leave both heads at the same HiDPI mode for 45 minutes of desktop use followed by 15 minutes of the existing [`motion.html`](benchmark/motion.html) animation in a 1600×900 Chrome window on one external head; alternate the animated head each hour. Label transitions from a second shell with `printf 'lite1,animation\n' > /path/to/run/phase` and `printf 'lite1,desktop\n' > /path/to/run/phase`. Record actual window location, deviations, other workload, mode changes, and timestamps in an event log. Do not mix destructive hotplug testing into the baseline soak. Record the LinkLite PID at start/end and at hourly boundaries so a restart cannot masquerade as a memory improvement.

Retain all rows and inspect the full LinkLite `mem_mb` time series. For each hour, report sample count, desktop last-ten-minute median, animation last-five-minute median and maximum; compare like phases rather than averaging different workloads. Require at least eight elapsed hours without process restart, unexplained sampling gaps, crash, dock reset or increasing retained-memory baseline across successive comparable windows. Report the first-to-last baseline difference and slope as well as the peak. If growth persists, or noise prevents distinguishing a plateau from growth, leave the gate failed/pending and extend or repeat with a controlled workload. Choose and record any numeric memory ceiling/noise tolerance **before** the run; there is no measured release-specific budget yet.

`summarize.py` can produce supplementary phase means with the command above, but those means alone cannot establish bounded memory. `top` memory is not an allocation/leak trace, missing GPU counters are not proof of zero GPU work, and whole-GPU figures include other applications. Keep the raw data alongside any analysis.

The original acceptance criterion also required materially lower CPU than Manager. The recorded A/B/A shows the opposite. That criterion remains **unresolved**, requiring either new representative evidence or an explicit owner-approved criterion revision—not a claim that memory savings satisfy both parts.


## Results

Run on 2026-11-03: LinkLite 0.2.0 (pre-release build), DisplayLink Manager 16.1.25, macOS 26.6. Both drivers rendered each monitor at 1920x1080 points, 2x (3840x2160 pixels), checked in [`run-2026-11-03.log`](benchmark/run-2026-11-03.log). Raw samples: [`samples-2026-11-03.csv`](benchmark/samples-2026-11-03.csv); summary: [`results.json`](benchmark/results.json), produced by [`summarize.py`](benchmark/summarize.py).

### Summary (LinkLite runs averaged)

| Driver | Phase | Whole GPU busy | GPU time: driver + WindowServer | Driver CPU | Driver memory |
|---|---|---|---|---|---|
| LinkLite | Live desktop | 12% | 162 ms/s (0 + 162) | 55% | 133 MB |
| LinkLite | 60 fps animation | **15%** | **250 ms/s (0 + 250)** | 338% | **135 MB** |
| DisplayLink Manager | Live desktop | **2%** | **76 ms/s (9 + 67)** | **2%** | 512 MB |
| DisplayLink Manager | 60 fps animation | 33% | 323 ms/s (168 + 155) | **20%** | 513 MB |

### Per run

| Run | Phase | Whole GPU busy | Driver GPU | WindowServer GPU | Driver CPU | Driver memory |
|---|---|---|---|---|---|---|
| lite1 | desktop | 3.8% | 0 ms/s | 123 ms/s | 53% | 136 MB |
| lite1 | animation | 9.7% | 0 ms/s | 216 ms/s | 336% | 135 MB |
| dlm | desktop | 2.4% | 9 ms/s | 67 ms/s | 2% | 512 MB |
| dlm | animation | 33.0% | 168 ms/s | 155 ms/s | 20% | 513 MB |
| lite2 | desktop | 19.8% | 0 ms/s | 200 ms/s | 58% | 131 MB |
| lite2 | animation | 20.9% | 0 ms/s | 285 ms/s | 339% | 134 MB |

### Reading the results

- **Under load LinkLite keeps the GPU about half as busy** (15% vs 33%, lower in both LinkLite runs). DisplayLink Manager compresses video on the GPU (168 ms of GPU per second in its own process); LinkLite's own process uses no GPU at all.
- **Memory: 3.8x less** (134 MB vs 513 MB), steady in every phase. App size: 3.9 MB vs 20.6 MB (LinkLite was 2.6 MB in the measured 0.2.0 build; 0.3.0 adds the updater, and its memory use is unchanged at about 135 MB).
- **On a quiet desktop both drivers barely touch the GPU.** The second LinkLite desktop phase (19.8%) caught background activity on the Mac; a 20-second spot check right after, with LinkLite still driving both monitors, read 4.3%, in line with the first run (3.8%). Whole-GPU and WindowServer figures include every app, so treat the desktop phase as noisy.
- **CPU is where DisplayLink Manager wins.** LinkLite encodes on the CPU: about half a core on a live desktop, and about 3.4 cores under full-window 60 fps motion, against 2% and 20% for DisplayLink Manager. Moving LinkLite's encoder to the GPU is the main item on the roadmap.

### Capture path

LinkLite captures with ScreenCaptureKit. The older `CGDisplayStream`, which DisplayLink Manager also links, was measured as an alternative on the same animation: one extra capture of a 4K display cost WindowServer about 25 ms/s of GPU with ScreenCaptureKit and about 17 ms/s with `CGDisplayStream`. The difference is small, and `CGDisplayStream` is obsoleted in the macOS 15 SDK, so LinkLite stays on ScreenCaptureKit.

## Hardware-free optimization checks — 2026-11-04

These are local, narrowly scoped, hardware-free measurements of the engineering changes shipped in v0.3.7, not a rerun of the two-monitor A/B benchmark above.

| Scenario | Before | After |
|---|---:|---:|
| Hidden searching panel: process CPU over six seconds | 9.54% | 0.18% |
| Pack 50 identical cached 3840×2160 Ridge frames, optimized Rust build | 204.1 ms | 119.2 ms |

The panel measurement launched the production SwiftUI panel and controller in a temporary native host, showed it, dismissed it, waited one second for the fade to finish, then sampled process CPU time with `getrusage`. Both versions used the same searching state. Releasing the hidden view tree stops its repeating status animation. This measures panel overhead, not the USB engine or an attached-display workload.

The packing comparison exercised the former double-copy path and the borrowed-body path against the same patterned RGB frame and cached encoded strips. The new path avoids 16,200 strip-body allocations and 41,988,360 copied body bytes per such full frame; this is allocation/copy traffic, not retained memory. The byte-equivalence regression remains in `vendor/vino`; the temporary timing harness was removed.

Capture smoke checks exercised initial/full, partial, empty, and scaled damage through the actual Swift callback. Cursor checks compared reused-buffer output byte-for-byte and checked hotspots at 1× and 2×. No full-application RSS reduction, USB throughput gain, eight-hour stability result, hotplug result, or older-macOS compatibility result is claimed by these checks.

## Offline replay benchmark

`replay` measures the CPU cost of the pixel path with no dock and no Screen Recording permission, so it runs while DisplayLink Manager keeps the dock. It feeds recorded frames through `Shared::submit_frame`, the engine's `take_frame`, then `HeadScanout::plan_dirty` and `encode` from the vendored codec, and packs the records into 64 KiB transfers that go to a null sink instead of USB. It runs once per dock profile (Ridge, Ella, Navarro).

### Clip format

A clip is a directory:

| File | Contents |
|---|---|
| `meta.json` | `{"width": 3840, "height": 2160, "stride": 15360, "frames": 300, "fps": 60}`, flat, numbers only. `stride` is bytes per row. |
| `NNNNN.bgra` | Frame `NNNNN` (zero-padded to five digits, from `00000`) as raw 32-bit BGRA, exactly `stride * height` bytes. If the file is absent the frame repeats the previous image (`00000.bgra` must exist). |
| `NNNNN.dirty` | Optional damage for the frame: packed little-endian `u32` quads `x, y, w, h`, in pixels. An absent file means a full frame, an empty file means nothing changed. |

Clips are large (a 4K frame is 33 MB) and live outside the repository. Replay loads a whole clip into memory, so a 300-frame 4K clip needs about 10 GB of RAM.

### Making clips

Synthetic clips are deterministic, so baseline and later runs see identical pixels:

```sh
cargo build --release --locked --manifest-path engine/Cargo.toml
R=engine/target/release/replay
$R gen motion      clips/motion      # 1600x900 window of the motion.html workload on a 4K desktop
$R gen motion-full clips/motion-full # the same workload over the whole 4K surface
$R gen idle        clips/idle        # a clock-sized region that changes once per 60 frames
$R gen-gamma       clips/gamma.lut   # a non-identity 768-byte table (R, G, B, 256 bytes each)
```

`gen` takes `--frames N` (default 300). The dirty rect of each frame is the exact bounding box of the pixels that changed since the previous frame.

To record a real desktop instead, run motion.html in Chrome and capture it (grant Screen Recording to the terminal first):

```sh
swift run linklite-bench record clips/real-motion --display main --frames 300
```

It captures the way `DisplayCapture` does (32BGRA, sRGB, queue depth 3, damage only while the stream is 1:1 with the display, first frame whole) and does not link the engine. ScreenCaptureKit delivers frames only when something changed, so an idle desktop gives a shorter clip than its wall-clock length, and `meta.json` records the delivered count.

### Running it

```sh
$R run clips/motion clips/motion-full clips/idle --family all --runs 3 \
  --gamma none --gamma clips/gamma.lut --out docs/benchmark/gpu-baseline-2026-11-11.json
python3 docs/benchmark/summarize-gpu.py docs/benchmark/gpu-baseline-2026-11-11.json other.json
```

`--family` is `ridge`, `ella`, `navarro` or `all`. `--gamma` may repeat; `none` is the identity path, a file is applied through the engine's gamma copy (an identity table is rejected, because it would silently skip that path). Each combination of clip, gamma and family runs `--runs` times and reports the run with the median mean CPU; the other runs' means are in `run_cpu_means`. All runs must produce the same checksum or replay fails.

Per entry:

- `cpu_ms_per_frame`: `getrusage` user plus system CPU of the whole process, including the worker pool, around each frame's pipeline (file loading and checksumming excluded): mean, p50, p95.
- `wall_ms_per_frame` and `wall_ms_total`: the same window in wall time, the per-frame latency of the pipeline.
- `stage_ms_per_frame`: wall time per stage, averaged over all frames (frames that do not present count as zero for plan, encode and pack). Stages that run on the worker pool overlap, so these do not add up to the CPU figure.
- `record_bytes`: EP08 record bytes in order; `sent_bytes`: what packing would put on USB (records times presentations, plus openers and trailers); `presents` and `skipped`.
- `checksum`: FNV-1a over every present's presentation count and records, each length-prefixed, in order. The same clip and the same code give the same checksum, so a GPU path that is byte-equal to the CPU path matches the baseline.

### The GPU encoder path

`linklite-bench replay` drives the same engine path through the engine's C bench API (`dll_bench_*` in `engine/include/dllite.h`, a thin wrapper over the Rust `Replay`), fed either way:

```sh
swift build -c release --product linklite-bench
B=$(swift build -c release --show-bin-path)/linklite-bench
$B replay clips/motion clips/motion-full clips/idle --cpu --family all --runs 3 --gamma none --gamma clips/gamma.lut
$B replay clips/motion clips/motion-full clips/idle --gpu --family all --runs 3 --gamma none --gamma clips/gamma.lut \
  --out docs/benchmark/gpu-phase2-2026-11-11.json
python3 docs/benchmark/summarize-gpu.py docs/benchmark/gpu-baseline-2026-11-11.json \
  docs/benchmark/gpu-phase1-2026-11-11.json docs/benchmark/gpu-phase2-2026-11-11.json
```

`--cpu` feeds BGRA frames through `dll_engine_submit_frame`'s path and must reproduce `replay run`'s checksums. `--gpu` copies each frame into an IOSurface-backed `CVPixelBuffer` (outside the measured window, since ScreenCaptureKit hands over IOSurfaces) and codes it with `MetalStripEncoder`: hashing, the shadow copy, gamma, the transform, quantisation and the bit coding all on the GPU, after which the strips go straight from the encoder's shared buffers through `dll_engine_submit_encoded`'s path. `--gpu --cpu-pack` is the Phase 1 path: the GPU stops at the quantised blocks and the CPU codes them through `dll_encode_strip_quantized`. The checksum must equal the CPU path's for every clip, family and gamma. The JSON adds `gpu_ms_per_frame` (command buffer `gpuEndTime - gpuStartTime`), a `gpu_encoder` stage (the encoder's wall time: GPU wait, plus CPU packing with `--cpu-pack`), `packing`, and `gpu_memory_bytes` (what the encoder held at the end of the largest run, its body buffer at its high-water mark). `cpu_ms_per_frame` counts the whole process, so with `--cpu-pack` it includes the packing threads.

### What is simplified

- The 100 MB/s video budget uses the engine's constants and token rules on a virtual clock that advances one frame period per frame. A wall clock would make which frames get held back, and so the checksum, depend on replay speed. Replay itself runs as fast as it can, not at the original pacing. Settling and owed repaints follow the same virtual clock.
- One engine pass per frame: submit, then take and present if the budget allows. The synthetic motion clips stay under the budget (about 90 MB over five seconds), so `skipped` is 0; a recorded full-screen video clip is what exercises it.
- The stream report that is sealed with the session's key is not in the packed bytes (there is no session). The checksum covers the records only.
- The CPU figure is whole-process, so it includes the worker pool's threads and anything else the process does during the frame.

## README graphics

The lead graphic uses the saved historical LinkLite 0.2.0 real-dock results in [`benchmark/results.json`](benchmark/results.json), not the v0.3.7 component checks. The memory headline compares each driver's mean memory across desktop and animation: `100 × (1 − 134.0549 / 512.8444) = 73.86%`, rounded to **74% less memory**. The supporting whole-GPU-busy figure under motion is `100 × (1 − 15.2308 / 32.9556) = 53.78%`, rounded to **54% lower**. Neither is a new measurement of v0.3.7.

The separate optimization graphic reads [`benchmark/optimizations-2026-11-04.json`](benchmark/optimizations-2026-11-04.json), which records the rounded measurements and structural counts described above. Its CPU reduction applies only to the hidden-panel host; its packing-time reduction is not an increase in display frame rate. The copied-byte count is for the synthetic full-frame case, not retained RAM.

Regenerate both graphics in light and dark with:

```sh
docs/assets/src/build-chart.sh
```

The renderer uses Google Chrome and a temporary browser profile. It needs neither installed driver applications nor a connected dock, and it publishes the four PNGs only after their measurement data has rendered.
