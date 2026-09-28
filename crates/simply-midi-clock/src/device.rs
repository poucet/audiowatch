//! Live MIDI output via midir: enumerate the machine's output ports and open
//! one by name ([`crate::pick`]'s rule). Opening is all this does — the
//! sender that owns the connection is its caller's.

use midir::MidiOutput;
pub use midir::MidiOutputConnection;

fn client(name: &str) -> Result<MidiOutput, String> {
    MidiOutput::new(name).map_err(|e| format!("no MIDI client: {e}"))
}

/// The MIDI output ports this machine currently offers, by display name.
/// Empty — never an error — on a machine with no MIDI at all. `client` is
/// the name this process registers with the MIDI system.
pub fn output_port_names(client_name: &str) -> Vec<String> {
    let Ok(output) = client(client_name) else { return Vec::new() };
    output.ports().iter().filter_map(|port| output.port_name(port).ok()).collect()
}

/// Open the output port `select` names. Dropping the connection closes it.
/// The error says which port was asked for, since a status line is the
/// only place it will be read.
pub fn open_output(client_name: &str, select: &str) -> Result<MidiOutputConnection, String> {
    let output = client(client_name)?;
    let ports = output.ports();
    let names: Vec<String> =
        ports.iter().map(|port| output.port_name(port).unwrap_or_default()).collect();
    let index = crate::pick(&names, select)
        .and_then(|name| names.iter().position(|n| n == name))
        .ok_or_else(|| format!("{select} not found"))?;
    output
        .connect(&ports[index], &names[index])
        .map_err(|e| format!("could not open {}: {e}", names[index]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opening a port that does not exist names it, and enumerating on a
    /// machine with no MIDI at all is a list, never a panic. Nothing is
    /// ever sent: the name cannot match a real port.
    #[test]
    fn a_missing_port_is_named_in_the_error() {
        let _ = output_port_names("simply-midi-clock-test");
        match open_output("simply-midi-clock-test", "\u{1}no such port\u{1}") {
            Ok(_) => panic!("a port with that name should not exist"),
            Err(e) => assert!(e.contains("no such port") || e.contains("no MIDI client")),
        }
    }
}
