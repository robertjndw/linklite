//! Opt-in protocol trace: `DLL_TRACE=1` writes what the dock says to stderr, prefixed `[trace]`.
//!
//! Call sites only read bytes the session already holds, so the wire is identical with the
//! variable unset. Replies are decrypted with the session's control key and the riv candidates
//! the decoders use; a frame none of them opens is shown raw.

use crate::kvino;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Whether `DLL_TRACE=1` is set. Read once; the answer cannot change mid-session.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("DLL_TRACE").is_some_and(|v| v == "1"))
}

macro_rules! trace {
    ($($arg:tt)*) => {
        if $crate::trace::enabled() {
            eprintln!("[trace] {}", format_args!($($arg)*));
        }
    };
}
pub(crate) use trace;

/// Hex of at most 160 bytes, so one oversized frame cannot bury the lines around it.
pub fn hex(bytes: &[u8]) -> String {
    const CAP: usize = 160;
    let mut out: String = bytes.iter().take(CAP).map(|b| format!("{b:02x}")).collect();
    if bytes.len() > CAP {
        out.push_str(&format!("..(+{})", bytes.len() - CAP));
    }
    out
}

/// Name of an HDCP message id: the HDCP 2.2 numbering plus the three DisplayLink additions for
/// transmitter/receiver info and auth status.
pub fn hdcp_name(msg_id: u8) -> &'static str {
    use kvino::ake::id;
    match msg_id {
        id::AKE_INIT => "AKE_Init",
        id::AKE_SEND_CERT => "AKE_Send_Cert",
        id::AKE_NO_STORED_KM => "AKE_No_Stored_km",
        id::AKE_SEND_RRX => "AKE_Send_rrx",
        id::AKE_SEND_H_PRIME => "AKE_Send_H'",
        id::LC_INIT => "LC_Init",
        id::LC_SEND_L_PRIME => "LC_Send_L'",
        id::SKE_SEND_EKS => "SKE_Send_Eks",
        id::REPEATERAUTH_SEND_RECEIVERID_LIST => "RepeaterAuth_Send_ReceiverID_List",
        id::REPEATERAUTH_SEND_ACK => "RepeaterAuth_Send_Ack",
        id::REPEATERAUTH_STREAM_MANAGE => "RepeaterAuth_Stream_Manage",
        id::REPEATERAUTH_STREAM_READY => "RepeaterAuth_Stream_Ready",
        id::RECEIVER_AUTH_STATUS => "Receiver_Auth_Status",
        id::AKE_TRANSMITTER_INFO => "AKE_Transmitter_Info",
        0x14 => "AKE_Receiver_Info",
        _ => "unknown",
    }
}

/// Decode the front of a `RepeaterAuth_Send_ReceiverID_List` body: `RxInfo` (big-endian, HDCP 2.2
/// sec 2.7), `seq_num_V`, then five-byte receiver ids. Short or padded bodies decode as far as
/// the bytes go.
pub fn receiver_list(body: &[u8]) -> String {
    if body.len() < 5 {
        return format!("receiver list too short ({} bytes): {}", body.len(), hex(body));
    }
    let (b0, b1) = (body[0], body[1]);
    let depth = (b0 >> 1) & 7;
    let devices = usize::from(b0 & 1) << 4 | usize::from(b1 >> 4);
    let ids_present = ((body.len() - 5) / 5).min(devices);
    let ids: Vec<String> = (0..ids_present)
        .map(|i| hex(&body[5 + i * 5..10 + i * 5]))
        .collect();
    format!(
        "RxInfo={b0:02x}{b1:02x} depth={depth} devices={devices} max_devs_exceeded={} \
         max_cascade_exceeded={} hdcp2_0_repeater={} hdcp1_device={} seq_num_V={} \
         ids_shown={}/{devices} [{}]",
        b1 >> 3 & 1,
        b1 >> 2 & 1,
        b1 >> 1 & 1,
        b1 & 1,
        hex(&body[2..5]),
        ids_present,
        ids.join(" ")
    )
}

/// Decrypt a dock-to-host control reply, trying every riv candidate the decoders try.
pub fn plaintext(ks: &[u8; 16], out_riv: &[u8; 8], wire: &[u8]) -> Option<Vec<u8>> {
    kvino::open_reply(ks, out_riv, wire).map(|(_, plaintext)| plaintext)
}

/// One line describing a received frame: decrypted where it can be, raw where it cannot.
pub fn describe(ks: &[u8; 16], out_riv: &[u8; 8], wire: &[u8]) -> String {
    if wire.len() < 16 {
        return format!("short frame len={} raw={}", wire.len(), hex(wire));
    }
    let wsub = u16::from_le_bytes([wire[8], wire[9]]);
    let seq = u32::from_le_bytes([wire[12], wire[13], wire[14], wire[15]]);
    match kvino::open_reply(ks, out_riv, wire) {
        Some((riv, pt)) if pt.len() >= 6 => {
            let id = u16::from_le_bytes([pt[0], pt[1]]);
            let sub = u16::from_le_bytes([pt[2], pt[3]]);
            let ctr = u16::from_le_bytes([pt[4], pt[5]]);
            // Offset 20 is the reply status (see `kvino::reply_status`), and 22..26 the status
            // word of the 0x44/0x20 probe reply, so both are named rather than left in the hex.
            let status = pt
                .get(20..22)
                .map(|w| format!(" status={:#06x}", u16::from_le_bytes([w[0], w[1]])))
                .unwrap_or_default();
            let word22 = pt
                .get(22..26)
                .map(|w| format!(" w22={:#010x}", u32::from_le_bytes([w[0], w[1], w[2], w[3]])))
                .unwrap_or_default();
            format!(
                "wsub={wsub:#04x} seq={seq} riv={riv} id={id:#06x} sub={sub:#06x} ctr={ctr:#06x} \
                 len={}{status}{word22} pt={}",
                pt.len(),
                hex(&pt)
            )
        }
        Some((riv, pt)) => format!("wsub={wsub:#04x} riv={riv} short plaintext {}", hex(&pt)),
        None if wsub == 0x25 => format!(
            "wsub=0x25 unsealed HDCP body len={} body={}",
            wire.len() - 16,
            hex(&wire[16..])
        ),
        None => format!(
            "wsub={wsub:#04x} seq={seq} not decryptable len={} raw={}",
            wire.len(),
            hex(wire)
        ),
    }
}

/// Repeat suppression for a polling loop, per context.
///
/// A status poll gets the same bytes back hundreds of times, so only changes are shown. The
/// request counter is masked out of the comparison since it always differs.
#[derive(Default)]
struct Repeats {
    last: Vec<u8>,
    suppressed: u32,
}

fn repeats() -> &'static Mutex<HashMap<String, Repeats>> {
    static MAP: OnceLock<Mutex<HashMap<String, Repeats>>> = OnceLock::new();
    MAP.get_or_init(Default::default)
}

/// Print `describe` for a reply under `ctx`, unless it equals the previous one under that `ctx`
/// apart from its counter.
pub fn reply_deduped(ctx: &str, ks: &[u8; 16], out_riv: &[u8; 8], wire: &[u8]) {
    if !enabled() {
        return;
    }
    let key = match plaintext(ks, out_riv, wire) {
        Some(mut plaintext) if plaintext.len() >= 6 => {
            plaintext[4..6].fill(0);
            plaintext
        }
        Some(plaintext) => plaintext,
        None => wire.to_vec(),
    };
    let mut map = repeats().lock().unwrap_or_else(|e| e.into_inner());
    let state = map.entry(ctx.to_string()).or_default();
    if state.last == key {
        state.suppressed += 1;
        return;
    }
    if state.suppressed > 0 {
        eprintln!("[trace] {ctx}: previous reply repeated {} more times", state.suppressed);
        state.suppressed = 0;
    }
    state.last = key;
    eprintln!("[trace] {ctx}: {}", describe(ks, out_riv, wire));
}

/// Report any repeats `reply_deduped` is still holding back for `ctx`.
pub fn flush_repeats(ctx: &str) {
    if !enabled() {
        return;
    }
    let mut map = repeats().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = map.get_mut(ctx) {
        if state.suppressed > 0 {
            eprintln!("[trace] {ctx}: previous reply repeated {} more times", state.suppressed);
            state.suppressed = 0;
        }
        state.last.clear();
    }
}
