//! Finding a MIDI clock on the machine, and listening to it. Input only:
//! nothing here opens a MIDI output or sends a byte anywhere.
//!
//! [`scan`] opens every MIDI input for a moment and counts the clock ticks
//! (`0xF8`) each delivers. [`listen`] picks the port a take follows — the one
//! port that is ticking, or the one named — and keeps it open, so no message
//! between choosing it and recording is lost (a `Start` among them is what
//! says where the bar is).
//!
//! # One clock for MIDI and audio
//!
//! A tick is only useful if its timestamp and an audio buffer's capture time
//! are on the same clock. On macOS they are: midir stamps CoreMIDI packets in
//! host time (`AudioConvertHostTimeToNanos`) and cpal stamps CoreAudio buffers
//! in host time (`mach_absolute_time`). No other platform promises that, so
//! [`listen`] refuses there rather than landing a take somewhere near the bar.

use std::fmt;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use simply_midi_clock::{name_matches, ClockEvent};

use crate::reading::{parse, TempoMeter};

/// How long a scan listens. At 40 bpm a tick is 62.5 ms apart, so this sees
/// a dozen of them from the slowest clock anyone plays.
pub const SCAN: Duration = Duration::from_millis(750);

/// Ticks a port must deliver in a scan to count as a clock.
const MIN_TICKS: u64 = 4;

/// Whether MIDI and audio timestamps share a clock on this platform.
pub const SHARED_HOST_CLOCK: bool = cfg!(target_os = "macos");

/// One MIDI input as a scan heard it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClockPort {
    /// The port's name, as a take's `clock` names it.
    pub name: String,
    /// Clock ticks heard during the scan. 0: not sending clock (or stopped
    /// and silent, which some masters are).
    pub ticks: u64,
    /// The tempo those ticks imply.
    pub bpm: Option<f64>,
}

impl ClockPort {
    pub fn is_clock(&self) -> bool {
        self.ticks >= MIN_TICKS
    }
}

/// Why no clock could be followed.
#[derive(Debug, Clone, PartialEq)]
pub enum ClockError {
    /// Not macOS: MIDI and audio timestamps are on different clocks.
    Unsupported,
    Midi(String),
    NoPorts,
    /// No input is sending clock.
    NoClock {
        ports: Vec<String>,
    },
    /// More than one is: the caller has to say which.
    Several {
        clocks: Vec<ClockPort>,
    },
    NoSuchPort {
        name: String,
        ports: Vec<String>,
    },
    Ambiguous {
        name: String,
        matches: Vec<String>,
    },
}

impl fmt::Display for ClockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let list = |v: &[String]| {
            if v.is_empty() {
                "none".to_string()
            } else {
                v.iter().map(|p| format!("{p:?}")).collect::<Vec<_>>().join(", ")
            }
        };
        match self {
            ClockError::Unsupported => f.write_str(
                "recording to a MIDI clock's bars needs MIDI and audio stamped on one host \
                 clock, which only macOS promises; record by seconds instead",
            ),
            ClockError::Midi(e) => write!(f, "MIDI input could not be opened: {e}"),
            ClockError::NoPorts => f.write_str("there are no MIDI inputs on this machine"),
            ClockError::NoClock { ports } => write!(
                f,
                "no MIDI input is sending clock (listened {:.2} s to {}). Start the master's \
                 clock, or — if it sends clock only while playing — name its port with `clock` \
                 and press play after arming",
                SCAN.as_secs_f64(),
                list(ports)
            ),
            ClockError::Several { clocks } => {
                let named: Vec<String> = clocks
                    .iter()
                    .map(|c| match c.bpm {
                        Some(b) => format!("{:?} ({b:.1} bpm)", c.name),
                        None => format!("{:?}", c.name),
                    })
                    .collect();
                write!(
                    f,
                    "{} MIDI inputs are sending clock: {}. Name the one to follow with `clock`",
                    clocks.len(),
                    named.join(", ")
                )
            }
            ClockError::NoSuchPort { name, ports } => {
                write!(f, "no MIDI input matches {name:?}; the inputs are {}", list(ports))
            }
            ClockError::Ambiguous { name, matches } => {
                write!(
                    f,
                    "{name:?} matches several MIDI inputs: {}; be more specific",
                    list(matches)
                )
            }
        }
    }
}

impl std::error::Error for ClockError {}

/// `(port index, host ns, message)`: what every open port's callback sends.
type Message = (usize, u64, ClockEvent);

/// Open ports, by index.
type Conns = Vec<(usize, midir::MidiInputConnection<()>)>;

/// An open clock port, with everything it has delivered since it opened
/// waiting in [`ClockInput::messages`].
pub struct ClockInput {
    pub port: String,
    /// Tempo heard while choosing it, if it was ticking.
    pub bpm: Option<f64>,
    /// The port was ticking when it was chosen.
    pub ticking: bool,
    index: usize,
    pending: Vec<(u64, ClockEvent)>,
    rx: mpsc::Receiver<Message>,
    _conn: midir::MidiInputConnection<()>,
}

impl ClockInput {
    /// Every clock message received since the last call, in arrival order,
    /// with its host time in nanoseconds.
    pub fn drain(&mut self) -> Vec<(u64, ClockEvent)> {
        let mut out = std::mem::take(&mut self.pending);
        out.extend(self.rx.try_iter().filter(|m| m.0 == self.index).map(|m| (m.1, m.2)));
        out
    }
}

impl fmt::Debug for ClockInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClockInput").field("port", &self.port).field("bpm", &self.bpm).finish()
    }
}

fn input(name: &str) -> Result<midir::MidiInput, ClockError> {
    let mut input = midir::MidiInput::new(name).map_err(|e| ClockError::Midi(e.to_string()))?;
    // Clock is "time" to midir; nothing may be ignored that a clock sends.
    input.ignore(midir::Ignore::Sysex);
    Ok(input)
}

fn port_names() -> Result<Vec<String>, ClockError> {
    let probe = input("audiowatch ports")?;
    Ok(probe.ports().iter().map(|p| probe.port_name(p).unwrap_or_default()).collect())
}

/// Open inputs `indices` and send everything clock-shaped to one channel.
fn open(indices: &[usize], tx: &mpsc::Sender<Message>) -> Result<Conns, ClockError> {
    let mut out = Vec::new();
    for &i in indices {
        let midi = input("audiowatch clock")?;
        let Some(port) = midi.ports().get(i).cloned() else { continue };
        let tx = tx.clone();
        let conn = midi
            .connect(
                &port,
                "audiowatch clock",
                move |us, bytes, _| {
                    if let Some(msg) = parse(bytes) {
                        let _ = tx.send((i, us.saturating_mul(1000), msg));
                    }
                },
                (),
            )
            .map_err(|e| ClockError::Midi(e.to_string()))?;
        out.push((i, conn));
    }
    Ok(out)
}

/// Tick counts per port over one window, from what the ports delivered.
fn heard(names: &[String], messages: &[Message]) -> Vec<ClockPort> {
    names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let mut meter = TempoMeter::default();
            for &(_, t, _) in messages.iter().filter(|m| m.0 == i && m.2 == ClockEvent::Tick) {
                meter.tick(t);
            }
            ClockPort { name: name.clone(), ticks: meter.ticks(), bpm: meter.bpm() }
        })
        .collect()
}

fn listen_to(
    indices: &[usize],
    window: Duration,
) -> Result<(mpsc::Receiver<Message>, Conns, Vec<Message>), ClockError> {
    let (tx, rx) = mpsc::channel();
    let conns = open(indices, &tx)?;
    drop(tx);
    let until = Instant::now() + window;
    let mut seen = Vec::new();
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(m) => seen.push(m),
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok((rx, conns, seen))
}

/// Every MIDI input, and whether it is sending clock. Listens for [`SCAN`].
pub fn scan() -> Result<Vec<ClockPort>, ClockError> {
    let names = port_names()?;
    let all: Vec<usize> = (0..names.len()).collect();
    let (_rx, _conns, seen) = listen_to(&all, SCAN)?;
    Ok(heard(&names, &seen))
}

/// Which port a take follows: the one named (an exact name or an unambiguous
/// fragment), or else the one input that is ticking.
pub fn choose(ports: &[ClockPort], named: Option<&str>) -> Result<usize, ClockError> {
    let names = || ports.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
    if ports.is_empty() {
        return Err(ClockError::NoPorts);
    }
    match named {
        Some(name) => {
            if let Some(i) = ports.iter().position(|p| p.name == name) {
                return Ok(i);
            }
            let hits: Vec<usize> = ports
                .iter()
                .enumerate()
                .filter(|(_, p)| name_matches(&p.name, name))
                .map(|(i, _)| i)
                .collect();
            match hits.as_slice() {
                [i] => Ok(*i),
                [] => Err(ClockError::NoSuchPort { name: name.into(), ports: names() }),
                many => Err(ClockError::Ambiguous {
                    name: name.into(),
                    matches: many.iter().map(|&i| ports[i].name.clone()).collect(),
                }),
            }
        }
        None => {
            let clocks: Vec<usize> = (0..ports.len()).filter(|&i| ports[i].is_clock()).collect();
            match clocks.as_slice() {
                [i] => Ok(*i),
                [] => Err(ClockError::NoClock { ports: names() }),
                many => Err(ClockError::Several {
                    clocks: many.iter().map(|&i| ports[i].clone()).collect(),
                }),
            }
        }
    }
}

/// Find the clock a take follows and keep it open. Listens for [`SCAN`]
/// first, to hear which port is ticking and at what tempo; every message the
/// chosen port delivered meanwhile is kept for the take.
pub fn listen(named: Option<&str>) -> Result<ClockInput, ClockError> {
    if !SHARED_HOST_CLOCK {
        return Err(ClockError::Unsupported);
    }
    let names = port_names()?;
    if names.is_empty() {
        return Err(ClockError::NoPorts);
    }
    let all: Vec<usize> = (0..names.len()).collect();
    let (rx, conns, seen) = listen_to(&all, SCAN)?;
    let ports = heard(&names, &seen);
    let index = choose(&ports, named)?;
    let mut keep = None;
    for (i, conn) in conns {
        if i == index {
            keep = Some(conn);
        } else {
            conn.close();
        }
    }
    let conn = keep.ok_or_else(|| ClockError::Midi("the chosen port did not open".into()))?;
    // What the chosen port said during the scan comes first: a `Start` in
    // that window is the anchor.
    let pending: Vec<(u64, ClockEvent)> =
        seen.into_iter().filter(|m| m.0 == index).map(|m| (m.1, m.2)).collect();
    let port = &ports[index];
    Ok(ClockInput {
        port: port.name.clone(),
        bpm: port.bpm,
        ticking: port.is_clock(),
        index,
        pending,
        rx,
        _conn: conn,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(name: &str, ticks: u64) -> ClockPort {
        ClockPort { name: name.into(), ticks, bpm: (ticks > 1).then_some(120.0) }
    }

    #[test]
    fn the_one_ticking_port_is_chosen() {
        let ports = [port("IAC Bus 1", 0), port("Digitakt", 36), port("Keystep", 1)];
        assert_eq!(choose(&ports, None), Ok(1));
    }

    #[test]
    fn several_clocks_are_refused_by_name_and_one_can_be_named() {
        let ports = [port("Digitakt", 36), port("Keystep 37", 36)];
        let e = choose(&ports, None).unwrap_err();
        let text = e.to_string();
        assert!(text.contains("\"Digitakt\" (120.0 bpm)") && text.contains("Keystep 37"), "{text}");
        assert!(text.contains("`clock`"), "{text}");
        assert_eq!(choose(&ports, Some("keystep")), Ok(1));
    }

    #[test]
    fn no_clock_is_refused_naming_every_input() {
        let ports = [port("IAC Bus 1", 0), port("Keystep", 2)];
        let text = choose(&ports, None).unwrap_err().to_string();
        assert!(text.contains("no MIDI input is sending clock"), "{text}");
        assert!(text.contains("\"IAC Bus 1\", \"Keystep\""), "{text}");
        assert_eq!(choose(&[], None), Err(ClockError::NoPorts));
    }

    #[test]
    fn a_named_port_need_not_be_ticking_yet() {
        // A master that sends clock only while playing is silent at arm time.
        let ports = [port("Digitakt", 0), port("Digitone", 0)];
        assert_eq!(choose(&ports, Some("Digitakt")), Ok(0));
        assert!(matches!(choose(&ports, Some("Digit")), Err(ClockError::Ambiguous { .. })));
        assert!(matches!(choose(&ports, Some("Push")), Err(ClockError::NoSuchPort { .. })));
    }

    #[test]
    fn ticks_are_counted_per_port() {
        let names = vec!["A".to_string(), "B".to_string()];
        let mut seen: Vec<Message> =
            (0..10u64).map(|i| (1, i * 20_833_333, ClockEvent::Tick)).collect();
        seen.push((0, 5, ClockEvent::Start));
        let ports = heard(&names, &seen);
        assert_eq!((ports[0].ticks, ports[1].ticks), (0, 10));
        assert!((ports[1].bpm.unwrap() - 120.0).abs() < 0.01);
    }
}
