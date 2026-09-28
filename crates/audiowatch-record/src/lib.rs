//! Record any channel, or adjacent pair of channels, of any device — what
//! arrives at it, or (where the OS allows) what the computer sends to it —
//! into a 32-bit float WAV at the device's own rate.
//!
//! Built for an agent first: everything here is non-interactive, a recording
//! has a length or an explicit stop, and what comes back is a finished file
//! by absolute path — never the audio itself, which an agent hands on to
//! whatever measures it. The `audiowatch` binary serves it over MCP
//! (`audiowatch --mcp`) and on the command line (`audiowatch rec`).
//!
//! - [`spec`] — a take written as text, `DEVICE:in|out:CHANNELS[=PATH]`;
//! - [`catalog`] — what the machine has, and which direction of each device
//!   can be recorded (and why not, with the way round it);
//! - [`frames`] — picking channels out of interleaved frames;
//! - [`wav`] — the writer thread: ring in, WAV out;
//! - [`session`] — a [`Recording`]: open takes together, stop them together —
//!   after a length, or trimmed to a [`TakeWindow`] of host time that the
//!   caller supplies once it knows it (a clock's downbeats, say).
//!
//! Nothing here knows about MIDI. What decides a window — a MIDI clock, a
//! Link session, a cue — is the caller's; this crate only keeps the audio
//! that falls inside it, to the frame.
//!
//! Nothing here ever opens an **output** stream. Recording what goes out to a
//! speaker is an *input* stream on a CoreAudio tap (see [`catalog`]).

#![forbid(unsafe_code)]

pub mod catalog;
pub mod frames;
pub mod session;
pub mod spec;
pub mod wav;

pub use catalog::{list_devices, DeviceInfo, Route, SideInfo, TapSupport};
pub use session::{
    frame_at, parse_takes, Outcome, RecordError, Recording, RecordingStatus, TakeInfo, TakeResult,
    TakeStatus, TakeSync, TakeWindow,
};
pub use spec::{Channels, TakeSpec};

/// Where recordings go when nobody says: `~/Music/audio-rec`, an absolute
/// folder that stays put, so a path handed to another tool (an analysis
/// server) still works later. Without a home directory, the system temp dir.
pub fn default_dir() -> std::path::PathBuf {
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => {
            std::path::PathBuf::from(home).join("Music").join("audio-rec")
        }
        _ => std::env::temp_dir().join("audio-rec"),
    }
}

/// Record `takes` for `seconds`, all started together, and return what each
/// file holds. Blocks for the duration.
pub fn record(
    takes: &[TakeSpec],
    dir: &std::path::Path,
    seconds: f64,
) -> Result<Vec<TakeResult>, RecordError> {
    Ok(Recording::start(takes, dir, Some(seconds))?.wait().takes)
}
