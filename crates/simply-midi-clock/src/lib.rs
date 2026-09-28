//! MIDI beat clock out, the parts every sender of it shares.
//!
//! Two programs in this repository send MIDI clock to a follower — the
//! standalone app off its own transport (`flux-gui`'s `midi_clock`) and the
//! Link bridge off an Ableton Link session (`simply-link-midi`) — and they
//! differ only in *where the instants come from*. What they share lives
//! here, with no flux dependency:
//!
//! * [`ClockEvent`] — the MIDI 1.0 real-time messages a clock sends, and
//!   their bytes: `F8` Timing Clock (24 per quarter note), `FA` Start,
//!   `FB` Continue, `FC` Stop, `F2` Song Position Pointer (in sixteenths).
//! * [`Follower`] — a mirror of what the follower has been told, so a port
//!   opened mid-play can be told where it is.
//! * [`pace`] and [`spin_until`] — how a sender waits for an instant. A
//!   follower hears every microsecond of jitter as tempo wobble;
//!   `thread::sleep` is good to about a millisecond, a spin to about a
//!   microsecond, so a sender sleeps until [`LEAD`] before the instant and
//!   spins the rest.
//! * [`pick`] / [`name_matches`] — which output port a setting names, and
//!   (feature `device`) [`open_output`] to open it through midir.

#![forbid(unsafe_code)]

use std::time::Duration;

/// MIDI beat clock: 24 ticks per quarter note.
pub const TICKS_PER_BEAT: f64 = 24.0;

/// …and a sixteenth is six ticks.
pub const TICKS_PER_SIXTEENTH: i64 = 6;

/// The pointer is 14 bits wide: 16384 sixteenths, 1024 bars of 4/4.
pub const MAX_SIXTEENTHS: u64 = 0x3FFF;

/// How long before an instant a sender stops sleeping and starts spinning
/// — `thread::sleep` is good to a millisecond or so, a spin is good to a
/// microsecond.
pub const LEAD: Duration = Duration::from_micros(1500);

/// How often a sender re-reads its queue while it has nothing due.
pub const POLL: Duration = Duration::from_millis(1);

/// …and while it has no port to send to either.
pub const IDLE_POLL: Duration = Duration::from_millis(20);

/// One real-time message, as a scheduler decides it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ClockEvent {
    /// `F8` — one twenty-fourth of a beat has passed.
    Tick,
    /// `FA` — play from the top.
    Start,
    /// `FB` — play from wherever the last pointer (or stop) left us.
    Continue,
    /// `FC` — stop.
    Stop,
    /// `F2` — the song position, in sixteenths.
    Position(u16),
}

impl ClockEvent {
    /// The bytes on the wire. A tick is one byte; the pointer is three,
    /// 14 bits little-endian across two data bytes.
    pub fn bytes(self) -> ([u8; 3], usize) {
        match self {
            Self::Tick => ([0xF8, 0, 0], 1),
            Self::Start => ([0xFA, 0, 0], 1),
            Self::Continue => ([0xFB, 0, 0], 1),
            Self::Stop => ([0xFC, 0, 0], 1),
            Self::Position(s) => ([0xF2, (s & 0x7F) as u8, ((s >> 7) & 0x7F) as u8], 3),
        }
    }
}

/// The song position pointer for a tick count: whole sixteenths, clamped
/// to the 14 bits the message has. The one rounding every pointer goes
/// through, so a scheduler and a mirror can never disagree by one.
pub fn sixteenths(ticks: i64) -> u16 {
    ((ticks.max(0) / TICKS_PER_SIXTEENTH) as u64).min(MAX_SIXTEENTHS) as u16
}

/// A sender's view of the follower: is it running, and where does it
/// think it is — so a port opened mid-play can be told the position and
/// continued, and a port closed mid-play can be stopped first.
#[derive(Debug, Default)]
pub struct Follower {
    pub running: bool,
    pub ticks: i64,
}

impl Follower {
    /// Account for one message the follower was sent.
    pub fn observe(&mut self, event: ClockEvent) {
        match event {
            ClockEvent::Tick => self.ticks += 1,
            ClockEvent::Start => {
                self.running = true;
                self.ticks = 0;
            }
            ClockEvent::Continue => self.running = true,
            ClockEvent::Stop => self.running = false,
            ClockEvent::Position(s) => self.ticks = i64::from(s) * TICKS_PER_SIXTEENTH,
        }
    }

    /// Where the follower thinks it is, as a pointer.
    pub fn position(&self) -> u16 {
        sixteenths(self.ticks)
    }
}

/// How to wait for an instant `remaining` away: `Some(d)` — sleep (or
/// block on a queue) for `d`, then ask again; `None` — it is within
/// [`LEAD`], so spin to it ([`spin_until`]) and send.
pub fn pace(remaining: Duration) -> Option<Duration> {
    (remaining > LEAD).then(|| (remaining - LEAD).min(POLL))
}

/// Spin until `due()` says the instant has come. An instant already past
/// returns at once — late once beats never.
pub fn spin_until(mut due: impl FnMut() -> bool) {
    while !due() {
        std::hint::spin_loop();
    }
}

/// The one port matcher: does `select` pick out a port called `name`? A
/// case-insensitive substring, so "MPK" catches "MPK mini mk3 Port 1".
pub fn name_matches(name: &str, select: &str) -> bool {
    name.to_lowercase().contains(&select.to_lowercase())
}

/// Pick the port `select` names out of `names`: an exact match first
/// (case-insensitive), then the first substring match — so a saved choice
/// keeps finding its port even when a sibling shares a prefix.
pub fn pick<'a>(names: &'a [String], select: &str) -> Option<&'a str> {
    names
        .iter()
        .find(|name| name.eq_ignore_ascii_case(select))
        .or_else(|| names.iter().find(|name| name_matches(name, select)))
        .map(String::as_str)
}

#[cfg(feature = "device")]
mod device;
#[cfg(feature = "device")]
pub use device::{open_output, output_port_names, MidiOutputConnection};

#[cfg(test)]
mod tests {
    use super::*;

    /// Beat 4 is sixteen sixteenths: `F2 10 00`.
    #[test]
    fn the_song_position_pointer_encodes_sixteenths_little_endian() {
        assert_eq!(sixteenths(4 * 24), 16);
        assert_eq!(ClockEvent::Position(16).bytes(), ([0xF2, 0x10, 0x00], 3));
        // 14 bits: 200 sixteenths = 0b1_1001000 → lsb 0x48, msb 0x01.
        assert_eq!(ClockEvent::Position(200).bytes(), ([0xF2, 0x48, 0x01], 3));
        // A tick inside a sixteenth floors to it; the pointer clamps.
        assert_eq!(sixteenths(55), 9);
        assert_eq!(sixteenths(-1), 0);
        assert_eq!(sixteenths(i64::MAX), 0x3FFF);
        assert_eq!(ClockEvent::Tick.bytes(), ([0xF8, 0, 0], 1));
        assert_eq!(ClockEvent::Start.bytes(), ([0xFA, 0, 0], 1));
        assert_eq!(ClockEvent::Continue.bytes(), ([0xFB, 0, 0], 1));
        assert_eq!(ClockEvent::Stop.bytes(), ([0xFC, 0, 0], 1));
    }

    /// A port opened mid-play is told the position the ticks have reached.
    #[test]
    fn the_follower_mirror_tracks_position_through_ticks_and_pointers() {
        let mut follower = Follower::default();
        follower.observe(ClockEvent::Start);
        for _ in 0..30 {
            follower.observe(ClockEvent::Tick);
        }
        assert!(follower.running);
        assert_eq!(follower.position(), 5, "30 ticks = five sixteenths");
        follower.observe(ClockEvent::Stop);
        assert!(!follower.running);
        follower.observe(ClockEvent::Position(16));
        follower.observe(ClockEvent::Continue);
        for _ in 0..6 {
            follower.observe(ClockEvent::Tick);
        }
        assert_eq!(follower.position(), 17);
    }

    /// Far away: sleep, never past the lead and never longer than a poll.
    /// Inside the lead: spin.
    #[test]
    fn pace_sleeps_to_the_lead_then_spins() {
        assert_eq!(pace(Duration::from_millis(100)), Some(POLL));
        assert_eq!(pace(LEAD + Duration::from_micros(300)), Some(Duration::from_micros(300)));
        assert_eq!(pace(LEAD), None);
        assert_eq!(pace(Duration::ZERO), None);
        let mut n = 0;
        spin_until(|| {
            n += 1;
            n == 3
        });
        assert_eq!(n, 3);
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// An exact name wins over a longer one that merely contains it, and a
    /// bare substring still finds the port a person half-typed.
    #[test]
    fn exact_beats_substring_and_both_ignore_case() {
        let ports = names(&["Clock Port 2", "Clock Port", "MPK mini"]);
        assert_eq!(pick(&ports, "clock port"), Some("Clock Port"));
        assert_eq!(pick(&ports, "port 2"), Some("Clock Port 2"));
        assert_eq!(pick(&ports, "mpk"), Some("MPK mini"));
        assert_eq!(pick(&ports, "Launchpad"), None);
        assert_eq!(pick(&[], "anything"), None);
        assert!(name_matches("MPK mini mk3 Port 1", "mpk"));
    }
}
