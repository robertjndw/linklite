// SPDX-License-Identifier: GPL-2.0-or-later

//! Engage the dock's encrypted session and report what is plugged into each output, and which
//! video head the dock gave it.
//! Needs exclusive use of the dock: DisplayLink Manager must not be running.

use dllite_engine::edid;
use vino_chimera::session::ControlSession;

fn main() {
    let mut session = match ControlSession::engage() {
        Ok(session) => session,
        Err(error) => {
            eprintln!("dllite-probe: {error}");
            std::process::exit(1);
        }
    };
    let profile = session.profile();
    println!(
        "engaged {} ({} outputs, {} heads, shared EDID handler: {}, reports presence: {})",
        profile.name(),
        session.output_count(),
        session.connectors(),
        profile.shared_edid_handler(),
        profile.reports_presence()
    );
    for output in 0..session.output_count() as u8 {
        let present = session.probe_output_present(output);
        print!("output {output}: presence {present:?}; ");
        match session.fetch_edid(output) {
            Ok((head, bytes)) => match edid::parse(&bytes) {
                Ok(m) => println!(
                    "head {head}: {} {} ({}x{}@{}, {}x{} mm, {} EDID bytes)",
                    m.manufacturer,
                    m.name.as_deref().unwrap_or("unnamed"),
                    m.preferred.width,
                    m.preferred.height,
                    m.preferred.refresh_hz,
                    m.width_mm,
                    m.height_mm,
                    bytes.len()
                ),
                Err(error) => println!(
                    "head {head}: EDID unreadable ({error}, {} bytes)",
                    bytes.len()
                ),
            },
            Err(error) => println!("no EDID ({error})"),
        }
    }
}
