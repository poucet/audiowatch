//! `AudioRecApi` — the served surface as one documented trait. A tool's
//! description is its doc comment, so what an agent most needs told — which
//! direction of which device works, what to do when one does not, and that a
//! path from `wait: false` is not readable yet — ships with the tool.

use async_trait::async_trait;
use simply_api::ApiResult;

use super::service::{Devices, Recorded};

/// Record the machine's audio devices to WAV files.
#[simply_api::api_service]
#[async_trait]
pub trait AudioRecApi: Send + Sync {
    /// Every audio device, both sides: channel count, current rate, formats, the stereo pairs a take can name, and whether it can be recorded `in` (what arrives at it) and `out` (what the computer sends to it). `out` works only for devices with no input side (speakers, displays), through a macOS 14.2+ tap; each refused direction carries a note saying what to do instead. Call this first: takes use these names.
    async fn list_devices(&self) -> ApiResult<Devices>;

    /// Record for a fixed length — `seconds`, or `bars` of a MIDI clock — then return the files: an absolute path and a `resource_link` (file:// URI, audio/wav) per take, never the audio itself. Each take is `DEVICE:in|out:CHANNELS[=PATH.wav]`, e.g. `ZOOM L6max:in:13-14`, `BlackHole 16ch:in:1-2`, `MacBook Pro Speakers:out:1-2`; CHANNELS is one channel (mono file) or an adjacent pair (stereo). All takes start together, each its own stream and 32-bit float file at its device's own rate — never resampled or mixed. `seconds` is at most 50 when waiting. With `wait: false` it returns at once with an `id` and the paths the files WILL have (at most 600 s; not readable until `await_recording` returns) — start it, make something play, then collect. Read `warnings`: a take that dropped audio or got none is named there. To capture a hardware interface's OUTPUT, route it through BlackHole or Loopback and record that device `in`. With `bars` instead of `seconds`, every take starts on the next downbeat of the MIDI clock and ends `bars` bars later, counted in clock ticks (a tempo change still ends on the bar); the answer's `clock` reports the tempo measured and each take's `sync` where its first frame lies against the downbeat. MIDI clock has no bar: it is counted from the clock's Start, so a clock already running when the call arrives waits for the next Start — the person must (re)start the master. macOS only; nothing is sent to any MIDI port.
    #[api(media)]
    #[allow(clippy::too_many_arguments)]
    async fn record(
        &self,
        /// One or more takes, `DEVICE:in|out:CHANNELS[=PATH.wav]`.
        takes: Vec<String>,
        /// How long to record, in seconds: more than 0; at most 50 when waiting, 600 with `wait: false`. Give this or `bars`, not both.
        seconds: Option<f64>,
        /// How long to record, in bars of a MIDI clock, starting on its next downbeat. Give this or `seconds`. Waiting, the bars plus the wait for the downbeat must fit in 50 s at the clock's tempo; otherwise use `wait: false`.
        bars: Option<u32>,
        /// Beats per bar for `bars` (MIDI clock carries no time signature). Default 4.
        beats_per_bar: Option<u32>,
        /// For `bars`: the MIDI input whose clock to follow, exact or an unambiguous fragment. Default: the one input that is sending clock; several, or none, is refused by name. Name it when the master sends clock only while playing.
        clock: Option<String>,
        /// Folder for files whose take names no path (relative paths go under it). Defaults to the server's folder, ~/Music/audio-rec unless it was started with --dir.
        dir: Option<String>,
        /// `true` (default): block until done and return finished files. `false`: return an id at once; collect with `await_recording`.
        wait: Option<bool>,
    ) -> ApiResult<Recorded>;

    /// Wait for a `record` started with `wait: false` to finish, then return its files exactly as a waiting `record` would. Returns at once if it already finished. Sends progress notifications while waiting; if a client still times out, call it again — the result is kept until collected. The server keeps at most 8 uncollected recordings; an id that was already collected (or dropped to make room) is refused by name.
    #[api(media)]
    async fn await_recording(
        &self,
        /// The `id` `record` returned.
        id: u64,
    ) -> ApiResult<Recorded>;
}
