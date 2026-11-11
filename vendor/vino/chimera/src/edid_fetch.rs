//! Live per-output EDID discovery using the same message builders and bounded transaction as Vino.
//!
//! Outputs (physical sockets) and heads (video pipes) are separate namespaces in the firmware.
//! The presence probe, the readiness kick and the EDID read select an output; the engage
//! (`id=0x16 sub=0x23`, DLM's `Output::assignHead`) assigns that output to a head, and the
//! post-EDID capability query (`id=0x15 sub=0x53`) takes a head bitmask. Upstream Vino named the
//! connector in all of them, which is only right where output and head coincide. Local
//! displaylink-lite patch 17 (vendor/vino/UPSTREAM.md).

use crate::kvino;
use crate::trace::{self, trace};
use std::time::{Duration, Instant};
use vino_driver::{Dock, Error as UsbError};

const STEP_DELAY: Duration = Duration::from_millis(100);
const POLL_DELAY: Duration = Duration::from_millis(20);
const RECV_TIMEOUT: Duration = Duration::from_millis(5);
const ASYNC_EDID_WAIT: Duration = Duration::from_secs(2);
const POLL_WALL_TIME: Duration = Duration::from_secs(6);
const EARLY_ROUNDS: usize = 1;
const POLL_ITERS: usize = 250;
const POLL_PROBE_EVERY: usize = 8;
const FINAL_FETCH_ROUNDS: usize = 24;

#[derive(Debug)]
pub enum FetchError {
    Build(crate::kshim::Error),
    Transport(UsbError),
    /// No head was free, or every free head was refused (with the last status the dock gave).
    NoHead { output: u8, refused: Option<u16> },
    NoEdid { output: u8 },
}

impl core::fmt::Display for FetchError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Build(e) => write!(f, "control message construction failed: {e}"),
            Self::Transport(e) => write!(f, "USB transport failed: {e:?}"),
            Self::NoHead { output, refused: None } => {
                write!(f, "no video head is free for output {output}")
            }
            Self::NoHead { output, refused: Some(status) } => {
                write!(f, "every free head refused output {output} (last status {status:#06x})")
            }
            Self::NoEdid { output } => write!(f, "output {output} did not return an EDID"),
        }
    }
}

impl std::error::Error for FetchError {}

/// Seal `content` under `id` at the running wire sequence, send it on the CP control endpoint,
/// and advance the wire sequence by the number of AES-CTR blocks consumed.
fn send(
    dock: &Dock,
    ks: &[u8; 16],
    riv: &[u8; 8],
    wire_seq: &mut u32,
    id: u16,
    content: &[u8],
) -> Result<(), FetchError> {
    let frame =
        kvino::seal_interactive(ks, riv, id, *wire_seq, content).map_err(FetchError::Build)?;
    dock.write_ctrl_raw(&frame).map_err(FetchError::Transport)?;
    *wire_seq = wire_seq.wrapping_add(content.len().div_ceil(16) as u32);
    Ok(())
}

fn send_message(
    dock: &Dock,
    ks: &[u8; 16],
    riv: &[u8; 8],
    wire_seq: &mut u32,
    inner_counter: &mut u16,
    id: u16,
    build: impl FnOnce(u16) -> crate::kshim::Result<Vec<u8>>,
) -> Result<(), FetchError> {
    let content = build(*inner_counter).map_err(FetchError::Build)?;
    send(dock, ks, riv, wire_seq, id, &content)?;
    *inner_counter = inner_counter.wrapping_add(1);
    Ok(())
}

/// What one drained reply said.
struct Drained {
    /// A `sub=0x0020` probe reply whose readiness bit is set.
    ready: bool,
    /// The echoed counter and status, when the reply carries one.
    status: Option<(u16, u16)>,
}

/// Drain one pending EP84 reply (short timeout — this is a rehearsal loop, not the persistent
/// async queue `vino.ko` keeps), opportunistically capturing a real EDID. `None` when nothing
/// arrived.
fn drain_frame(
    dock: &Dock,
    ks: &[u8; 16],
    riv: &[u8; 8],
    output: u8,
    edid: &mut Option<Vec<u8>>,
) -> Result<Option<Drained>, FetchError> {
    let reply = match dock.recv_frame_raw_timeout(4096, RECV_TIMEOUT) {
        Ok(reply) => reply,
        Err(UsbError::Timeout) => return Ok(None),
        Err(e) => return Err(FetchError::Transport(e)),
    };
    let status = kvino::reply_status(ks, riv, &reply);
    if trace::enabled() {
        let ctx = format!("edid output {output} reply");
        // Repeats are folded because the readiness poll sends the same query hundreds of times.
        trace::reply_deduped(&ctx, ks, riv, &reply);
        if let Some((id, word, ready_bit)) = kvino::probe_reply_status(ks, riv, &reply) {
            trace!(
                "{ctx}: probe answer id={id:#06x} status_word={word:#010x} sink={} ready={ready_bit}",
                kvino::probe_reports_sink(word)
            );
        }
        if let Some((sub @ (0x23 | 0x4b | 0x53), counter, code)) = status {
            trace!(
                "{ctx}: sub={sub:#06x} ctr={counter:#06x} status={code:#06x} ({})",
                if kvino::reply_status_ok(code) { "ok" } else { "refused" }
            );
        }
    }
    if edid.is_none() {
        if let Some(decoded) =
            kvino::parse_edid_from_reply(ks, riv, &reply).map_err(FetchError::Build)?
        {
            trace!(
                "edid output {output}: EDID block, {} bytes, vendor bytes {} (placeholder={})",
                decoded.len(),
                trace::hex(decoded.get(8..12).unwrap_or(&[])),
                is_dock_placeholder(&decoded)
            );
            *edid = Some(decoded);
        }
    }
    Ok(Some(Drained {
        ready: matches!(kvino::edid_poll_ready(ks, riv, &reply), Some(true)),
        status: status.map(|(_, counter, code)| (counter, code)),
    }))
}

/// [`drain_frame`] reduced to whether the reply reported the downstream read ready.
fn drain_once(
    dock: &Dock,
    ks: &[u8; 16],
    riv: &[u8; 8],
    output: u8,
    edid: &mut Option<Vec<u8>>,
) -> Result<bool, FetchError> {
    Ok(drain_frame(dock, ks, riv, output, edid)?.is_some_and(|drained| drained.ready))
}

/// Spend one `STEP_DELAY` (plus the usual drain) watching for the status of the request sent
/// with `counter`, so an engage can be checked without adding time or traffic. `None` when the
/// answer did not arrive in that window.
fn await_status(
    dock: &Dock,
    ks: &[u8; 16],
    riv: &[u8; 8],
    output: u8,
    counter: u16,
    edid: &mut Option<Vec<u8>>,
    ready: &mut bool,
) -> Result<Option<u16>, FetchError> {
    let deadline = Instant::now() + RECV_TIMEOUT + STEP_DELAY;
    let mut status = None;
    while status.is_none() && Instant::now() < deadline {
        let Some(drained) = drain_frame(dock, ks, riv, output, edid)? else {
            continue;
        };
        *ready |= drained.ready;
        status = drained.status.filter(|&(echoed, _)| echoed == counter).map(|(_, code)| code);
    }
    std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
    Ok(status)
}

/// The requests of one fetch that select an output or a head.
#[derive(Clone, Copy, Debug)]
enum Request {
    /// `id=0x15 sub=0x20`, the presence/readiness probe.
    Probe,
    /// `id=0x16 sub=0x4b`, start the downstream EDID read.
    Kick,
    /// `id=0x15 sub=0x21`, read the EDID.
    Read,
    /// `id=0x16 sub=0x23`, assign the output to the head.
    Assign,
    /// `id=0x15 sub=0x53`, the cursor-capability query, which takes a head bitmask.
    Caps,
}

impl Request {
    fn id(self) -> u16 {
        match self {
            Self::Probe | Self::Read | Self::Caps => 0x15,
            Self::Kick | Self::Assign => 0x16,
        }
    }

    fn build(self, counter: u16, output: u8, head: u8) -> crate::kshim::Result<Vec<u8>> {
        match self {
            Self::Probe => kvino::get_edid_req_sub(counter, 0x20, output),
            Self::Kick => kvino::edid_readiness_kick(counter, output),
            Self::Read => kvino::get_edid_req(counter, output),
            Self::Assign => kvino::assign_head_req(counter, output, head),
            Self::Caps => kvino::post_edid_query(counter, head),
        }
    }
}

/// PNP id `NVT` (NOVATEK), the manufacturer of the EDID a Ridge dock synthesises for itself before
/// its downstream read completes (docs/ridge.md). Local displaylink-lite patch.
const DOCK_PLACEHOLDER_PNP: u16 = (14 << 10) | (22 << 5) | 20;

/// Whether `edid` is the dock's own placeholder block rather than a monitor's.
fn is_dock_placeholder(edid: &[u8]) -> bool {
    edid.get(8..10)
        .is_none_or(|id| u16::from_be_bytes([id[0], id[1]]) == DOCK_PLACEHOLDER_PNP)
}

/// Discover `output`'s EDID in an already-engaged session, assigning the output to the first of
/// `heads` the dock accepts. Returns that head and the EDID.
///
/// The first engage doubles as the trial: its status is read during the step delay that follows
/// it anyway, so a dock that accepts the first head sees the same records and timing as before.
/// A refused head is followed by an engage offering the next one.
///
/// `wire_seq` and `inner_counter` are shared with every other control operation in the session and
/// are advanced only after a successful transfer.
#[allow(clippy::too_many_arguments)]
pub fn fetch_edid(
    dock: &Dock,
    profile: kvino::DockProfile,
    ks: &[u8; 16],
    riv: &[u8; 8],
    wire_seq: &mut u32,
    inner_counter: &mut u16,
    output: u8,
    heads: &[u8],
) -> Result<(u8, Vec<u8>), FetchError> {
    // Nothing before the engage names a head; this is only where the requests start from.
    let mut head = *heads.first().ok_or(FetchError::NoHead { output, refused: None })?;
    macro_rules! send {
        ($request:expr) => {{
            let request: Request = $request;
            trace!("edid output {output}: send {request:?} ctr={:#06x} head={head}", *inner_counter);
            send_message(dock, ks, riv, wire_seq, inner_counter, request.id(), |counter| {
                request.build(counter, output, head)
            })
        }};
    }
    let mut edid: Option<Vec<u8>> = None;
    let mut ready = false;

    // A block that arrives before the dock reports its downstream read complete can describe the
    // dock's own bridge rather than the monitor (on Ridge, a synthesised `NVT` NOVATEK 1920x1080
    // panel), and publishing it drives the panel at a timing it never advertised. Offset 26 bit 7
    // of the presence reply is that report; where a dock makes it, drop that placeholder and let
    // the engage below produce the real one. A dock that never sets the bit answers correctly
    // anyway, and gating it discards every block.
    //
    // Local displaylink-lite patch (see vendor/vino/UPSTREAM.md): a Humanscale M/Connect 2 on
    // Ridge firmware 12.2.25 never sets the bit, yet offers each monitor's real EDID. Only the
    // dock's own placeholder is discarded, so a genuine monitor EDID is kept either way.
    let gate_on_ready = profile.edid_ready_reported();
    trace!("fetch_edid output {output}: start, heads offered {heads:?}, gate on ready bit: {gate_on_ready}");
    macro_rules! settled {
        () => {{
            if gate_on_ready && !ready && edid.as_deref().is_some_and(is_dock_placeholder) {
                println!(
                    "  chimera-edid: output {output} discarded the dock's placeholder EDID offered \
                     before the downstream read completed"
                );
                edid = None;
            }
            edid.is_some()
        }};
    }

    for round in 0..EARLY_ROUNDS {
        if settled!() {
            break;
        }
        println!("  chimera-edid: early round {round}");
        for _ in 0..2 {
            send!(Request::Probe)?;
            ready |= drain_once(dock, ks, riv, output, &mut edid)?;
            std::thread::sleep(STEP_DELAY);
        }
        send!(Request::Kick)?;
        ready |= drain_once(dock, ks, riv, output, &mut edid)?;
        std::thread::sleep(STEP_DELAY);
        send!(Request::Read)?;
        ready |= drain_once(dock, ks, riv, output, &mut edid)?;
        if settled!() {
            break;
        }
        let wait_started = Instant::now();
        while edid.is_none() && wait_started.elapsed() < ASYNC_EDID_WAIT {
            ready |= drain_once(dock, ks, riv, output, &mut edid)?;
        }
    }

    // The receiver requires two engage messages even when the asynchronous EDID push arrived
    // during the early fetch. Only the first is a trial: measured on a Targus DOCK310, its HDMI
    // output 2 is refused head 0 (status 0x0d) and accepted on head 1. A reply that does not
    // arrive in time is taken as acceptance, which is how every engage was treated before.
    let mut accepted = false;
    let mut refused = None;
    for &candidate in heads {
        head = candidate;
        let counter = *inner_counter;
        send!(Request::Assign)?;
        match await_status(dock, ks, riv, output, counter, &mut edid, &mut ready)? {
            Some(status) if !kvino::reply_status_ok(status) => {
                println!("  chimera-edid: output {output} refused head {head} (status {status:#06x})");
                refused = Some(status);
            }
            _ => {
                accepted = true;
                break;
            }
        }
    }
    if !accepted {
        return Err(FetchError::NoHead { output, refused });
    }
    send!(Request::Assign)?;
    ready |= drain_once(dock, ks, riv, output, &mut edid)?;
    std::thread::sleep(STEP_DELAY);

    if !settled!() {
        let poll_started = Instant::now();
        for i in 0..POLL_ITERS {
            if edid.is_some() || ready || poll_started.elapsed() >= POLL_WALL_TIME {
                break;
            }
            send_message(dock, ks, riv, wire_seq, inner_counter, 0x14, |counter| {
                kvino::device_query_req(counter, 0x000c)
            })?;
            ready |= drain_once(dock, ks, riv, output, &mut edid)?;
            if i % POLL_PROBE_EVERY == POLL_PROBE_EVERY - 1 {
                send!(Request::Probe)?;
                ready |= drain_once(dock, ks, riv, output, &mut edid)?;
            }
            std::thread::sleep(POLL_DELAY);
        }
        println!("  chimera-edid: readiness poll finished (ready={ready})");
        for _ in 0..FINAL_FETCH_ROUNDS {
            if edid.is_some() {
                break;
            }
            send!(Request::Read)?;
            let _ = drain_once(dock, ks, riv, output, &mut edid)?;
            std::thread::sleep(POLL_DELAY);
        }
    }

    send!(Request::Caps)?;
    ready |= drain_once(dock, ks, riv, output, &mut edid)?;

    let _ = settled!();
    trace::flush_repeats(&format!("edid output {output} reply"));
    trace!(
        "fetch_edid output {output}: done on head {head}, ready={ready}, edid={}",
        edid.as_deref().map_or("none".to_string(), |e| format!("{} bytes", e.len()))
    );
    edid.map(|edid| (head, edid)).ok_or(FetchError::NoEdid { output })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Request; 5] =
        [Request::Probe, Request::Kick, Request::Read, Request::Assign, Request::Caps];

    /// The records each request was built with before outputs and heads were told apart, where
    /// the one number named both.
    fn upstream(request: Request, counter: u16, connector: u8) -> Vec<u8> {
        match request {
            Request::Probe => kvino::get_edid_req_sub(counter, 0x20, connector),
            Request::Kick => kvino::edid_readiness_kick(counter, connector),
            Request::Read => kvino::get_edid_req(counter, connector),
            Request::Assign => kvino::edid_engage_req(counter, connector),
            Request::Caps => kvino::post_edid_query(counter, connector),
        }
        .unwrap()
    }

    /// The M/Connect 2 keeps output 0 on head 0 and output 1 on head 1, and must keep being sent
    /// exactly what it always was. Every request ends in a random tail, from byte 23 on the
    /// one-selector `0x15` requests and from byte 24 on the two-byte `0x16` ones, so the bytes
    /// before it and the length are what can be compared.
    #[test]
    fn an_output_on_its_own_head_is_sent_the_upstream_records() {
        for connector in 0..crate::MAX_CONNECTORS as u8 {
            for request in ALL {
                let fixed = if request.id() == 0x16 { 24 } else { 23 };
                let ours = request.build(0x1234, connector, connector).unwrap();
                let theirs = upstream(request, 0x1234, connector);
                assert_eq!(ours.len(), theirs.len());
                assert_eq!(ours[..fixed], theirs[..fixed]);
            }
        }
    }

    /// The Targus DOCK310 route measured to light: HDMI output 2 on head 1.
    #[test]
    fn requests_select_the_output_or_the_head_they_address() {
        let (output, head) = (2, 1);
        let at = |request: Request| request.build(7, output, head).unwrap();
        assert_eq!(at(Request::Probe)[..4], [0x15, 0, 0x20, 0]);
        assert_eq!(at(Request::Probe)[22], output);
        assert_eq!(at(Request::Kick)[..4], [0x16, 0, 0x4b, 0]);
        assert_eq!(at(Request::Kick)[22..24], [output, 1]);
        assert_eq!(at(Request::Read)[..4], [0x15, 0, 0x21, 0]);
        assert_eq!(at(Request::Read)[22], output);
        assert_eq!(at(Request::Assign)[..4], [0x16, 0, 0x23, 0]);
        assert_eq!(at(Request::Assign)[22..24], [output, head]);
        assert_eq!(at(Request::Caps)[..4], [0x15, 0, 0x53, 0]);
        assert_eq!(at(Request::Caps)[22], 1 << head);
        for request in ALL {
            assert_eq!(request.id(), u16::from(at(request)[0]));
        }
    }
}
