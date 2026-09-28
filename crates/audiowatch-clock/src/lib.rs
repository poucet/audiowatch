//! Recording in time with a MIDI clock: read a clock off a MIDI input, find
//! its downbeats, and hand the recorder the window between them.
//!
//! - [`reading`] — pure: the clock messages, where the bar is, the tempo,
//!   and the least-squares downbeat fit that averages MIDI's arrival jitter
//!   away. Tested against synthetic timelines with no device.
//! - [`port`] — finding the MIDI input that is sending clock, and listening
//!   to it. Input only: nothing here opens a MIDI output or sends a byte.
//! - [`take`] — a take of N bars: a windowed `audiowatch_record::Recording`
//!   given its window by the clock.
//!
//! The messages themselves, their bytes and the port-name matcher are
//! `simply_midi_clock`'s.

#![forbid(unsafe_code)]

pub mod port;
pub mod reading;
pub mod take;

pub use port::{ClockError, ClockInput, ClockPort};
pub use reading::BarPlan;
pub use take::{record_bars, BarOptions, BarOutcome, BarRecording, ClockReport};
