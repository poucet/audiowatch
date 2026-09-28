//! **Recording audio devices, for an agent: `audiowatch --mcp` and
//! `audiowatch rec`.**
//!
//! The machine's devices — interfaces, mixers, virtual loopback devices,
//! speakers — listed with what can be recorded from each, and a `record` tool
//! that captures channel pairs of any of them to WAV for a fixed length:
//! seconds, or (feature `midi-clock`, on by default) bars of a MIDI clock.
//! All the recording is `audiowatch_record`'s and the clock is
//! `audiowatch_clock`'s; this module is the tool surface over them, as an MCP
//! server ([`server`]) and on the command line ([`cli`]).
//!
//! # Why every recording has a length fixed at the start
//!
//! An agent is poor at real time: between a "start" and a "stop" it spends an
//! unknowable number of seconds thinking, so a recording it stops by hand
//! comes back an arbitrary length. So there is no stop verb. `record` takes
//! its length up front and either blocks for it, or — with `wait: false` —
//! returns at once with an id, so the agent can make something play in its
//! next call and collect the files with `await_recording`.
//!
//! # Never the audio
//!
//! A response names each file by absolute path (a text line, and an MCP
//! `resource_link` with a `file://` URI) and never carries the audio: an
//! agent hands the file to a different server to be measured, and bytes
//! routed through the model are lossy and expensive. A path is only returned
//! once its file is finished and closed — except from `wait: false`, whose
//! paths say where the files *will* be.

pub mod api;
pub mod cli;
pub mod heartbeat;
pub mod media;
pub mod server;
pub mod service;

pub use api::{AudioRecApi, AUDIO_REC_API_META};

/// Reported to MCP clients on `initialize`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The longest `record` that blocks (`wait: true`), in seconds.
///
/// The call blocks for the whole take, and MCP clients time a call out: the
/// MCP TypeScript SDK — what most clients are built on — defaults a request to
/// 60 s. A 60 s take plus opening and closing the devices would exceed that
/// and the agent would get a timeout instead of its files. 50 s leaves ten
/// seconds of headroom. The server also sends progress notifications while a
/// call runs, which clients that honour them use to extend the timeout; the
/// cap does not rely on it.
pub const MAX_SECONDS: f64 = 50.0;

/// The longest `record` with `wait: false`, in seconds. Longer is safe here
/// because nothing blocks for it: `await_recording` can be called again if a
/// client times it out, and the result is still held.
pub const MAX_BACKGROUND_SECONDS: f64 = 600.0;

/// How many `wait: false` recordings the server holds at once, running or
/// finished-but-not-collected. Starting one more drops the oldest *finished*
/// one (its files stay on disk); if all of them are still running it is
/// refused.
pub const MAX_HELD: usize = 8;

/// How long a `wait: false` bar-quantised `record` waits for its first
/// downbeat before giving up, in seconds. A waiting one waits at most what
/// [`MAX_SECONDS`] leaves after the bars.
pub const MAX_ARM_SECONDS: f64 = 120.0;
