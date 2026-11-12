// SPDX-License-Identifier: GPL-2.0-or-later

//! Offline CPU benchmark of the pixel path: replays a recorded clip through submit, take, plan,
//! encode and pack with no dock attached. See docs/BENCHMARK.md for the clip format and usage.
//!
//!   replay run <clip>... [--family ridge|ella|navarro|all] [--runs 3] [--gamma <file|none>]... [--out file]
//!   replay gen <motion|motion-full|idle> <dir> [--frames 300]
//!   replay gen-gamma <file>

use dllite_engine::host::bench::{Replay, Stages};
use dllite_engine::host::Rect;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;
use dlcore::kvino::Family;

type Lut = [[u8; 256]; 3];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("run") => run_command(&args[1..]),
        Some("gen") => gen_command(&args[1..]),
        Some("gen-gamma") => gen_gamma_command(&args[1..]),
        _ => Err("usage: replay run|gen|gen-gamma ... (see docs/BENCHMARK.md)".to_string()),
    };
    if let Err(error) = result {
        eprintln!("replay: {error}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------------------------
// Clip format
// ---------------------------------------------------------------------------------------------

struct Meta {
    width: usize,
    height: usize,
    stride: usize,
    frames: usize,
    fps: f64,
}

struct ClipFrame {
    /// Index into `Clip::images`; a frame without its own `.bgra` repeats the previous image.
    image: usize,
    dirty: Option<Vec<Rect>>,
}

struct Clip {
    name: String,
    meta: Meta,
    images: Vec<Vec<u8>>,
    frames: Vec<ClipFrame>,
}

/// Pull `"key": <number>` out of the flat meta.json; there is no JSON dependency in the engine.
fn json_number(text: &str, key: &str) -> Result<f64, String> {
    let at = text.find(&format!("\"{key}\"")).ok_or(format!("meta.json has no \"{key}\""))?;
    let rest = text[at..].split_once(':').ok_or("malformed meta.json")?.1.trim_start();
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-')).unwrap_or(rest.len());
    rest[..end].parse().map_err(|_| format!("meta.json \"{key}\" is not a number"))
}

fn load_clip(dir: &Path) -> Result<Clip, String> {
    let text = std::fs::read_to_string(dir.join("meta.json")).map_err(|e| format!("{}: meta.json: {e}", dir.display()))?;
    let meta = Meta {
        width: json_number(&text, "width")? as usize,
        height: json_number(&text, "height")? as usize,
        stride: json_number(&text, "stride")? as usize,
        frames: json_number(&text, "frames")? as usize,
        fps: json_number(&text, "fps")?,
    };
    let image_len = meta.stride * meta.height;
    let mut images: Vec<Vec<u8>> = Vec::new();
    let mut frames = Vec::with_capacity(meta.frames);
    for index in 0..meta.frames {
        let bgra = dir.join(format!("{index:05}.bgra"));
        if bgra.exists() {
            let data = std::fs::read(&bgra).map_err(|e| format!("{}: {e}", bgra.display()))?;
            if data.len() != image_len {
                return Err(format!("{}: {} bytes, expected stride*height = {image_len}", bgra.display(), data.len()));
            }
            images.push(data);
        } else if images.is_empty() {
            return Err(format!("{}: the first frame must have pixels", bgra.display()));
        }
        let dirty = match std::fs::read(dir.join(format!("{index:05}.dirty"))) {
            Ok(bytes) => {
                if bytes.len() % 16 != 0 {
                    return Err(format!("frame {index}: .dirty is not whole x,y,w,h quads"));
                }
                Some(
                    bytes
                        .chunks_exact(16)
                        .map(|q| {
                            let n = |i: usize| u32::from_le_bytes(q[i * 4..i * 4 + 4].try_into().unwrap()) as usize;
                            (n(0), n(1), n(0) + n(2), n(1) + n(3))
                        })
                        .collect(),
                )
            }
            Err(_) => None,
        };
        frames.push(ClipFrame { image: images.len() - 1, dirty });
    }
    let name = dir.file_name().map_or_else(|| "clip".into(), |n| n.to_string_lossy().into_owned());
    Ok(Clip { name, meta, images, frames })
}

fn write_meta(dir: &Path, meta: &Meta) -> Result<(), String> {
    std::fs::write(
        dir.join("meta.json"),
        format!(
            "{{\"width\": {}, \"height\": {}, \"stride\": {}, \"frames\": {}, \"fps\": {}}}\n",
            meta.width, meta.height, meta.stride, meta.frames, meta.fps
        ),
    )
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------------------------
// Process CPU time
// ---------------------------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Timeval {
    sec: i64,
    #[cfg(target_os = "macos")]
    usec: i32,
    #[cfg(not(target_os = "macos"))]
    usec: i64,
}

#[repr(C)]
#[derive(Default)]
struct Rusage {
    utime: Timeval,
    stime: Timeval,
    rest: [i64; 14],
}

extern "C" {
    fn getrusage(who: i32, usage: *mut Rusage) -> i32;
}

/// User plus system CPU seconds of the whole process, which includes the worker pool's threads.
fn cpu_seconds() -> f64 {
    let mut usage = Rusage::default();
    // SAFETY: `usage` matches the C `struct rusage` layout; RUSAGE_SELF is 0.
    assert_eq!(unsafe { getrusage(0, &mut usage) }, 0, "getrusage failed");
    let secs = |t: Timeval| t.sec as f64 + t.usec as f64 / 1e6;
    secs(usage.utime) + secs(usage.stime)
}

// ---------------------------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------------------------

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

#[derive(Clone)]
struct RunResult {
    cpu_ms: Vec<f64>,
    wall_ms: Vec<f64>,
    stages: [f64; 5],
    total_wall_ms: f64,
    record_bytes: u64,
    sent_bytes: u64,
    presents: u64,
    skipped: u64,
    checksum: u64,
}

fn run_once(clip: &Clip, family: Family, gamma: Option<Lut>) -> Result<RunResult, String> {
    let m = &clip.meta;
    let mut replay = Replay::new(family, m.width, m.height, m.fps, gamma)?;
    let mut result = RunResult {
        cpu_ms: Vec::new(),
        wall_ms: Vec::new(),
        stages: [0.0; 5],
        total_wall_ms: 0.0,
        record_bytes: 0,
        sent_bytes: 0,
        presents: 0,
        skipped: 0,
        checksum: 0,
    };
    let mut hash = Fnv::new();
    let mut total = Stages::default();
    for frame in &clip.frames {
        // Hashing the records is the harness's own cost, so it stays outside the measured window.
        let cpu0 = cpu_seconds();
        let wall0 = std::time::Instant::now();
        let step = replay.step(&clip.images[frame.image], m.stride, frame.dirty.as_deref());
        let wall = wall0.elapsed();
        let cpu1 = cpu_seconds();
        result.cpu_ms.push((cpu1 - cpu0) * 1e3);
        result.wall_ms.push(wall.as_secs_f64() * 1e3);
        result.total_wall_ms += wall.as_secs_f64() * 1e3;
        let s = step.stages;
        total = Stages {
            submit: total.submit + s.submit,
            take: total.take + s.take,
            plan: total.plan + s.plan,
            encode: total.encode + s.encode,
            pack: total.pack + s.pack,
        };
        result.skipped += u64::from(step.skipped);
        if step.presentations > 0 {
            result.presents += 1;
            result.sent_bytes += step.sent_bytes as u64;
            hash.write(&step.presentations.to_le_bytes());
            for record in &step.records {
                result.record_bytes += record.len() as u64;
                hash.write(&(record.len() as u32).to_le_bytes());
                hash.write(record);
            }
        }
    }
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    result.stages = [ms(total.submit), ms(total.take), ms(total.plan), ms(total.encode), ms(total.pack)];
    result.checksum = hash.0;
    Ok(result)
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len().max(1) as f64
}

fn percentile(values: &[f64], p: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len()) - 1]
}

fn parse_family(name: &str) -> Result<Vec<(&'static str, Family)>, String> {
    let all = [("ridge", Family::Ridge), ("ella", Family::Ella), ("navarro", Family::Navarro)];
    let picked: Vec<_> = all.into_iter().filter(|(n, _)| name == "all" || *n == name).collect();
    if picked.is_empty() {
        return Err(format!("unknown family '{name}' (ridge, ella, navarro, all)"));
    }
    Ok(picked)
}

fn load_gamma(path: &str) -> Result<Lut, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    if bytes.len() != 768 {
        return Err(format!("{path}: {} bytes, expected 768 (R, G, B tables of 256)", bytes.len()));
    }
    let mut lut = [[0u8; 256]; 3];
    for (channel, table) in lut.iter_mut().enumerate() {
        table.copy_from_slice(&bytes[channel * 256..(channel + 1) * 256]);
    }
    Ok(lut)
}

fn run_command(args: &[String]) -> Result<(), String> {
    let mut clips: Vec<PathBuf> = Vec::new();
    let mut family = "all".to_string();
    let mut runs = 3usize;
    let mut gammas: Vec<String> = Vec::new();
    let mut out: Option<PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value = |flag: &str| iter.next().cloned().ok_or(format!("{flag} needs a value"));
        match arg.as_str() {
            "--family" => family = value("--family")?,
            "--runs" => runs = value("--runs")?.parse().map_err(|_| "--runs needs a number")?,
            "--gamma" => gammas.push(value("--gamma")?),
            "--out" => out = Some(value("--out")?.into()),
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            dir => clips.push(dir.into()),
        }
    }
    if clips.is_empty() || runs == 0 {
        return Err("usage: replay run <clip>... [--family F] [--runs N] [--gamma file|none]... [--out file]".into());
    }
    if gammas.is_empty() {
        gammas.push("none".into());
    }
    let families = parse_family(&family)?;

    let mut json = String::new();
    json.push_str("{\n  \"schema\": 1,\n  \"tool\": \"replay\",\n");
    let _ = writeln!(json, "  \"runs\": {runs},");
    json.push_str("  \"results\": [\n");
    let mut first = true;
    for dir in &clips {
        let clip = load_clip(dir)?;
        for gamma_arg in &gammas {
            let (gamma, gamma_label) = if gamma_arg == "none" {
                (None, "none".to_string())
            } else {
                let label = Path::new(gamma_arg).file_name().map_or_else(|| gamma_arg.clone(), |n| n.to_string_lossy().into_owned());
                (Some(load_gamma(gamma_arg)?), label)
            };
            for &(family_name, family) in &families {
                let mut results = Vec::new();
                for _ in 0..runs {
                    results.push(run_once(&clip, family, gamma)?);
                }
                if results.iter().any(|r| r.checksum != results[0].checksum) {
                    return Err(format!("{} {family_name}: checksum differs between runs, replay is not deterministic", clip.name));
                }
                // The median run by mean CPU is reported whole, so its stage split and
                // percentiles describe one real run rather than a mix.
                let means: Vec<f64> = results.iter().map(|r| mean(&r.cpu_ms)).collect();
                let mut order: Vec<usize> = (0..runs).collect();
                order.sort_by(|&a, &b| means[a].total_cmp(&means[b]));
                let r = &results[order[runs / 2]];
                let n = clip.frames.len() as f64;
                if !first {
                    json.push_str(",\n");
                }
                first = false;
                let run_means = means.iter().map(|m| format!("{m:.4}")).collect::<Vec<_>>().join(", ");
                let _ = write!(
                    json,
                    "    {{\"clip\": {:?}, \"family\": {:?}, \"gamma\": {:?}, \"frames\": {}, \"width\": {}, \"height\": {}, \"fps\": {},\n     \
                     \"checksum\": \"{:016x}\", \"record_bytes\": {}, \"sent_bytes\": {}, \"presents\": {}, \"skipped\": {},\n     \
                     \"wall_ms_total\": {:.3}, \"wall_ms_per_frame\": {{\"mean\": {:.4}, \"p50\": {:.4}, \"p95\": {:.4}}},\n     \
                     \"cpu_ms_per_frame\": {{\"mean\": {:.4}, \"p50\": {:.4}, \"p95\": {:.4}}},\n     \
                     \"stage_ms_per_frame\": {{\"submit\": {:.4}, \"take\": {:.4}, \"plan\": {:.4}, \"encode\": {:.4}, \"pack\": {:.4}}},\n     \
                     \"run_cpu_means\": [{}]}}",
                    clip.name, family_name, gamma_label, clip.frames.len(), clip.meta.width, clip.meta.height, clip.meta.fps,
                    r.checksum, r.record_bytes, r.sent_bytes, r.presents, r.skipped,
                    r.total_wall_ms, mean(&r.wall_ms), percentile(&r.wall_ms, 0.5), percentile(&r.wall_ms, 0.95),
                    mean(&r.cpu_ms), percentile(&r.cpu_ms, 0.5), percentile(&r.cpu_ms, 0.95),
                    r.stages[0] / n, r.stages[1] / n, r.stages[2] / n, r.stages[3] / n, r.stages[4] / n,
                    run_means,
                );
                eprintln!(
                    "{} {family_name} gamma={gamma_label}: cpu {:.3} ms/frame, checksum {:016x}",
                    clip.name, mean(&r.cpu_ms), r.checksum
                );
            }
        }
    }
    json.push_str("\n  ]\n}\n");
    match out {
        Some(path) => std::fs::write(&path, json).map_err(|e| format!("{}: {e}", path.display())),
        None => {
            print!("{json}");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Synthetic clips
// ---------------------------------------------------------------------------------------------

const W: usize = 3840;
const H: usize = 2160;

struct Canvas {
    px: Vec<u8>,
}

impl Canvas {
    fn fill_rect(&mut self, x: i64, y: i64, w: i64, h: i64, clip: (i64, i64, i64, i64), bgr: [u8; 3]) {
        let (x0, y0) = (x.max(clip.0), y.max(clip.1));
        let (x1, y1) = ((x + w).min(clip.2), (y + h).min(clip.3));
        for yy in y0..y1 {
            for xx in x0..x1 {
                let at = (yy as usize * W + xx as usize) * 4;
                self.px[at..at + 4].copy_from_slice(&[bgr[0], bgr[1], bgr[2], 255]);
            }
        }
    }

    /// A number in seven-segment digits, `scale` pixels per segment unit.
    fn number(&mut self, value: u64, x: i64, y: i64, scale: i64, digits: usize, clip: (i64, i64, i64, i64), bgr: [u8; 3]) {
        // Segments a..g: top, top right, bottom right, bottom, bottom left, top left, middle.
        const SEGMENTS: [u8; 10] = [0x3f, 0x06, 0x5b, 0x4f, 0x66, 0x6d, 0x7d, 0x07, 0x7f, 0x6f];
        let mut rest = value;
        for slot in (0..digits).rev() {
            let bits = SEGMENTS[(rest % 10) as usize];
            rest /= 10;
            let ox = x + slot as i64 * 6 * scale;
            let bars = [
                (0, 0, 4, 1), (3, 0, 1, 4), (3, 4, 1, 4), (0, 7, 4, 1), (0, 4, 1, 4), (0, 0, 1, 4), (0, 3, 4, 1),
            ];
            for (bit, (bx, by, bw, bh)) in bars.into_iter().enumerate() {
                if bits >> bit & 1 == 1 {
                    self.fill_rect(ox + bx * scale, y + by * scale, bw * scale, bh * scale, clip, bgr);
                }
            }
        }
    }
}

fn background() -> Canvas {
    let mut px = vec![0u8; W * H * 4];
    for y in 0..H {
        for x in 0..W {
            let checker = if (x / 24 + y / 24) % 2 == 0 { 0 } else { 14 };
            let at = (y * W + x) * 4;
            px[at] = (60 + checker + x * 40 / W) as u8;
            px[at + 1] = (30 + checker + y * 60 / H) as u8;
            px[at + 2] = (20 + checker + (x + y) * 30 / (W + H)) as u8;
            px[at + 3] = 255;
        }
    }
    Canvas { px }
}

fn hue(h: u32) -> [u8; 3] {
    let sector = (h % 360) / 60;
    let rise = ((h % 60) * 255 / 60) as u8;
    let (r, g, b) = match sector {
        0 => (255, rise, 0),
        1 => (255 - rise, 255, 0),
        2 => (0, 255, rise),
        3 => (0, 255 - rise, 255),
        4 => (rise, 0, 255),
        _ => (255, 0, 255 - rise),
    };
    // Softened toward grey like the card colours in motion.html, in BGR order.
    [(b / 2 + 80), (g / 2 + 80), (r / 2 + 80)]
}

/// The motion.html workload inside `win`: a scrolling gradient, 24 drifting cards with a frame
/// counter, redrawn whole every frame.
fn draw_motion(canvas: &mut Canvas, f: u64, win: (i64, i64, i64, i64)) {
    let (wx, wy, ww, wh) = win;
    for y in wy..wy + wh {
        let phase = ((y as u64 - wy as u64 + f * 4) % 1024) as i64;
        let t = if phase < 512 { phase } else { 1024 - phase };
        let lerp = |a: i64, b: i64| (a + (b - a) * t / 512) as u8;
        let bgr = [lerp(58, 165), lerp(43, 110), lerp(29, 58)];
        for x in wx..wx + ww {
            let at = (y as usize * W + x as usize) * 4;
            canvas.px[at..at + 4].copy_from_slice(&[bgr[0], bgr[1], bgr[2], 255]);
        }
    }
    let clip = (wx, wy, wx + ww, wy + wh);
    for i in 0..24u64 {
        let x = (i * 173 + f * (2 + i % 5)) as i64 % (ww + 300) - 150;
        let y = (i * 97) as i64 % wh;
        canvas.fill_rect(wx + x, wy + y, 260, 120, clip, hue((i * 37 + f) as u32 % 360));
        canvas.number(f, wx + x + 12, wy + y + 16, 4, 5, clip, [255, 255, 255]);
    }
}

fn gen_command(args: &[String]) -> Result<(), String> {
    let kind = args.first().ok_or("gen needs a kind: motion, motion-full or idle")?;
    let dir = PathBuf::from(args.get(1).ok_or("gen needs an output directory")?);
    let mut frames = 300usize;
    let mut rest = args[2..].iter();
    while let Some(flag) = rest.next() {
        match flag.as_str() {
            "--frames" => frames = rest.next().and_then(|v| v.parse().ok()).ok_or("--frames needs a number")?,
            other => return Err(format!("unknown option {other}")),
        }
    }
    let window = match kind.as_str() {
        // The 1600x900 Chrome window motion.html runs in, near the middle of a 4K screen.
        "motion" => Some((1120, 630, 1600, 900)),
        "motion-full" => Some((0, 0, W as i64, H as i64)),
        "idle" => None,
        other => return Err(format!("unknown clip kind '{other}'")),
    };
    let meta = Meta { width: W, height: H, stride: W * 4, frames, fps: 60.0 };
    let base = background();
    let mut previous: Option<Vec<u8>> = None;
    // Frames are written as they are made, 33 MB each, so only the last two are in memory.
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for f in 0..frames as u64 {
        let mut canvas = Canvas { px: base.px.clone() };
        match window {
            Some(win) => draw_motion(&mut canvas, f, win),
            // A clock the size of a menu-bar item that ticks once per 60 frames.
            None => canvas.number(f / 60, W as i64 - 200, 12, 5, 4, (0, 0, W as i64, H as i64), [255, 255, 255]),
        }
        let (bgra, dirty) = match &previous {
            None => (Some(canvas.px.clone()), None),
            Some(prev) => match diff_bounds(prev, &canvas.px) {
                // Identical to the last frame: no pixels file, and an empty damage list.
                None => (None, Some(Vec::new())),
                Some(rect) => (Some(canvas.px.clone()), Some(vec![rect])),
            },
        };
        write_clip_frame(&dir, f as usize, bgra.as_deref(), dirty.as_deref())?;
        if bgra.is_some() {
            previous = Some(canvas.px);
        }
    }
    write_meta(&dir, &meta)
}

fn write_clip_frame(dir: &Path, index: usize, bgra: Option<&[u8]>, dirty: Option<&[Rect]>) -> Result<(), String> {
    if let Some(bgra) = bgra {
        std::fs::write(dir.join(format!("{index:05}.bgra")), bgra).map_err(|e| e.to_string())?;
    }
    if let Some(rects) = dirty {
        let mut bytes = Vec::with_capacity(rects.len() * 16);
        for &(x0, y0, x1, y1) in rects {
            for v in [x0, y0, x1 - x0, y1 - y0] {
                bytes.extend_from_slice(&(v as u32).to_le_bytes());
            }
        }
        std::fs::write(dir.join(format!("{index:05}.dirty")), bytes).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Tight bounding rectangle of the pixels that differ, or `None` if the frames are identical.
fn diff_bounds(a: &[u8], b: &[u8]) -> Option<Rect> {
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
    for y in 0..H {
        let (ra, rb) = (&a[y * W * 4..(y + 1) * W * 4], &b[y * W * 4..(y + 1) * W * 4]);
        if ra == rb {
            continue;
        }
        let first = ra.chunks_exact(4).zip(rb.chunks_exact(4)).position(|(p, q)| p != q).unwrap();
        let last = ra.chunks_exact(4).zip(rb.chunks_exact(4)).rposition(|(p, q)| p != q).unwrap();
        x0 = x0.min(first);
        x1 = x1.max(last + 1);
        y0 = y0.min(y);
        y1 = y + 1;
    }
    (y1 > 0).then_some((x0, y0, x1, y1))
}

/// A deterministic non-identity gamma table (display gamma 2.2 against 2.4) for measuring the
/// gamma path.
fn gen_gamma_command(args: &[String]) -> Result<(), String> {
    let path = args.first().ok_or("gen-gamma needs an output file")?;
    let mut bytes = Vec::with_capacity(768);
    for channel in 0..3 {
        // Slightly different curves per channel so no channel table equals another.
        let exponent = 2.2 / (2.4 + channel as f64 * 0.05);
        for i in 0..256 {
            bytes.push(((i as f64 / 255.0).powf(exponent) * 255.0).round() as u8);
        }
    }
    std::fs::write(path, bytes).map_err(|e| format!("{path}: {e}"))
}
