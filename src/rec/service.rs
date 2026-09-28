//! [`AudioRecApi`] over the recorder. The service holds where files go by
//! default, and the recordings started with `wait: false` until they are
//! collected.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
#[cfg(feature = "midi-clock")]
use audiowatch_clock::{BarOptions, BarPlan, BarRecording, ClockReport};
use audiowatch_record::{Channels, DeviceInfo, Outcome, Recording, TakeInfo, TakeSync};
use simply_api::mcp::McpApiServer;
use simply_api::{ApiError, ApiResult};

use super::api::{audio_rec_api_tool_router, AudioRecApi};
use super::heartbeat::Heartbeat;
#[cfg(feature = "midi-clock")]
use super::MAX_ARM_SECONDS;
use super::{MAX_BACKGROUND_SECONDS, MAX_HELD, MAX_SECONDS, VERSION};

/// What a connecting agent reads first. Edit `instructions.md`, not a string.
pub const INSTRUCTIONS: &str = include_str!("instructions.md");

/// How often `await_recording` looks whether its recording has finished.
const POLL: Duration = Duration::from_millis(100);

/// `list_devices`' answer.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct Devices {
    pub devices: Vec<DeviceInfo>,
}

/// One take of a `record` answer: what it is and where its file is.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct RecordedTake {
    /// `DEVICE:in|out:CHANNELS`, with the device's full name.
    pub spec: String,
    pub device: String,
    /// The channel or pair, `"3"` or `"13-14"`.
    pub channels: Channels,
    /// The file's rate — the device's own.
    pub sample_rate: u32,
    /// Absolute path of the WAV.
    pub path: String,
    /// The file's length; absent while the recording is still running.
    pub duration_s: Option<f64>,
    /// A `bars` take: where its first frame lies against the downbeat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<TakeSync>,
}

/// `record`'s and `await_recording`'s answer.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct Recorded {
    /// Set by `record` with `wait: false`: pass it to `await_recording`.
    pub id: Option<u64>,
    /// Every file is finished and closed. `false` means the paths are where
    /// the files will be, and are not readable yet.
    pub complete: bool,
    /// The length asked for in seconds (absent for a `bars` recording).
    pub seconds: Option<f64>,
    /// The length asked for in bars (absent for a `seconds` recording).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bars: Option<u32>,
    /// A `bars` recording: the MIDI input followed, the tempo measured, the
    /// bars actually recorded.
    #[cfg(feature = "midi-clock")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<ClockReport>,
    pub takes: Vec<RecordedTake>,
    /// Each names its take: dropped audio (the file has gaps), a stream
    /// error, nothing arriving at all.
    pub warnings: Vec<String>,
}

/// The length a `record` was asked for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Asked {
    Seconds(f64),
    Bars(u32),
}

impl Recorded {
    fn pending(id: u64, asked: Asked, takes: &[TakeInfo]) -> Recorded {
        let (seconds, bars) = asked.split();
        Recorded {
            id: Some(id),
            complete: false,
            seconds,
            bars,
            #[cfg(feature = "midi-clock")]
            clock: None,
            takes: takes.iter().map(|t| take(t, None, None)).collect(),
            warnings: Vec::new(),
        }
    }

    fn complete(id: Option<u64>, asked: Asked, finished: Finished) -> Recorded {
        let (seconds, bars) = asked.split();
        let results = finished.outcome.takes;
        let warnings = results
            .iter()
            .flat_map(|r| r.warnings.iter().cloned())
            .collect();
        Recorded {
            id,
            complete: true,
            seconds,
            bars,
            #[cfg(feature = "midi-clock")]
            clock: finished.clock,
            takes: results
                .iter()
                .map(|r| take(&r.take, Some(r.duration_s), r.sync.clone()))
                .collect(),
            warnings,
        }
    }
}

impl Asked {
    fn split(self) -> (Option<f64>, Option<u32>) {
        match self {
            Asked::Seconds(s) => (Some(s), None),
            Asked::Bars(b) => (None, Some(b)),
        }
    }
}

fn take(t: &TakeInfo, duration_s: Option<f64>, sync: Option<TakeSync>) -> RecordedTake {
    RecordedTake {
        spec: t.spec.clone(),
        device: t.device.clone(),
        channels: t.channels,
        sample_rate: t.sample_rate,
        path: t.path.clone(),
        duration_s,
        sync,
    }
}

/// A finished recording: the recorder's outcome, and for a `bars` one what
/// the clock did.
struct Finished {
    outcome: Outcome,
    #[cfg(feature = "midi-clock")]
    clock: Option<ClockReport>,
}

impl From<Outcome> for Finished {
    fn from(outcome: Outcome) -> Finished {
        Finished {
            outcome,
            #[cfg(feature = "midi-clock")]
            clock: None,
        }
    }
}

/// A recording while it runs: for a length, or on a MIDI clock's bars.
enum Running {
    Seconds(Recording),
    #[cfg(feature = "midi-clock")]
    Bars(BarRecording),
}

impl Running {
    fn is_finished(&self) -> bool {
        match self {
            Running::Seconds(r) => r.is_finished(),
            #[cfg(feature = "midi-clock")]
            Running::Bars(r) => r.is_finished(),
        }
    }

    fn takes(&self) -> &[TakeInfo] {
        match self {
            Running::Seconds(r) => r.takes(),
            #[cfg(feature = "midi-clock")]
            Running::Bars(r) => r.takes(),
        }
    }

    fn wait(self) -> Finished {
        match self {
            Running::Seconds(r) => r.wait().into(),
            #[cfg(feature = "midi-clock")]
            Running::Bars(r) => {
                let done = r.wait();
                Finished {
                    outcome: done.outcome,
                    clock: done.clock,
                }
            }
        }
    }
}

/// What starts a recording once every argument has been checked.
type Starter = Box<dyn FnOnce() -> Result<Running, String> + Send>;

/// A finished recording's outcome, or its refusal: a `bars` recording that
/// never found its downbeat recorded nothing.
fn landed(id: Option<u64>, asked: Asked, finished: Finished) -> ApiResult<Recorded> {
    match &finished.outcome.failed {
        Some(why) => Err(failed(match id {
            Some(id) => format!("recording {id} recorded nothing: {why}"),
            None => format!("nothing was recorded: {why}"),
        })),
        None => Ok(Recorded::complete(id, asked, finished)),
    }
}

/// A `wait: false` recording, and the length it was asked for.
struct Held {
    recording: Running,
    asked: Asked,
}

#[derive(Default)]
struct Book {
    next: u64,
    held: BTreeMap<u64, Held>,
    /// Ids dropped to make room, remembered (boundedly) so their refusal can
    /// say so rather than "unknown".
    dropped: VecDeque<u64>,
}

/// Make room for one more of `cap` held items: drop the oldest `finished`
/// one if full, or refuse if every one is still running. Returns the id
/// dropped, if any.
fn make_room<T>(
    held: &mut BTreeMap<u64, T>,
    cap: usize,
    finished: impl Fn(&T) -> bool,
) -> Result<Option<u64>, ()> {
    if held.len() < cap {
        return Ok(None);
    }
    let oldest = held
        .iter()
        .find(|(_, v)| finished(v))
        .map(|(&k, _)| k)
        .ok_or(())?;
    held.remove(&oldest);
    Ok(Some(oldest))
}

/// The served surface.
pub struct AudioRecService {
    dir: PathBuf,
    book: Mutex<Book>,
}

impl AudioRecService {
    /// Files without an explicit path go in `dir` (made absolute, so the
    /// paths an agent gets back work from anywhere).
    pub fn new(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref();
        AudioRecService {
            dir: std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf()),
            book: Mutex::new(Book {
                next: 1,
                ..Book::default()
            }),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn dir_for(&self, dir: Option<String>) -> PathBuf {
        match dir.map(PathBuf::from) {
            Some(d) if d.is_absolute() => d,
            Some(d) => self.dir.join(d),
            None => self.dir.clone(),
        }
    }
}

/// Refuse a length the tool cannot honour, by name — never truncate it.
pub fn check_seconds(seconds: f64, wait: bool) -> ApiResult<()> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(ApiError::invalid_params(format!(
            "seconds must be more than 0, not {seconds}"
        )));
    }
    if wait && seconds > MAX_SECONDS {
        return Err(ApiError::invalid_params(format!(
            "seconds is {seconds}, and a record that waits is at most {MAX_SECONDS} s (longer \
             would outlast the client's request timeout). Use wait: false (up to \
             {MAX_BACKGROUND_SECONDS} s) and await_recording, or several calls."
        )));
    }
    if !wait && seconds > MAX_BACKGROUND_SECONDS {
        return Err(ApiError::invalid_params(format!(
            "seconds is {seconds}, and one recording is at most {MAX_BACKGROUND_SECONDS} s. \
             Record longer pieces as several recordings."
        )));
    }
    Ok(())
}

fn failed(e: impl std::fmt::Display) -> ApiError {
    ApiError::failed(e.to_string())
}

/// The least a waiting `bars` call leaves itself to hear the clock's Start.
#[cfg(feature = "midi-clock")]
const MIN_ARM_SECONDS: f64 = 2.0;

/// Exactly one of `seconds` and `bars`.
pub fn length(seconds: Option<f64>, bars: Option<u32>) -> ApiResult<Asked> {
    match (seconds, bars) {
        (Some(s), None) => Ok(Asked::Seconds(s)),
        (None, Some(0)) => Err(ApiError::invalid_params("bars must be at least 1")),
        (None, Some(b)) => Ok(Asked::Bars(b)),
        (Some(_), Some(_)) => Err(ApiError::invalid_params("give seconds or bars, not both")),
        (None, None) => Err(ApiError::invalid_params(
            "a recording needs a length: seconds, or bars of a MIDI clock",
        )),
    }
}

/// What a `bars` recording answers from a build without the MIDI clock.
#[cfg(not(feature = "midi-clock"))]
const NO_CLOCK: &str =
    "this audiowatch was built without the midi-clock feature, so it records by \
                        seconds only; give seconds";

/// How long a `bars` recording may wait for its downbeat — or its refusal,
/// by name, when the bars cannot fit the call's cap at the clock's tempo.
/// The bars are checked with one more bar allowed for reaching the downbeat.
#[cfg(feature = "midi-clock")]
pub fn check_bars(plan: &BarPlan, bpm: Option<f64>, wait: bool) -> ApiResult<f64> {
    let cap = if wait {
        MAX_SECONDS
    } else {
        MAX_BACKGROUND_SECONDS
    };
    let Some(bpm) = bpm.filter(|b| *b > 0.0) else {
        if wait {
            return Err(ApiError::invalid_params(
                "the clock is not ticking yet, so its tempo — and how long these bars last — is \
                 unknown, and a record that waits is at most 50 s. Use wait: false (the master \
                 can then be started after arming) and await_recording",
            ));
        }
        return Ok(MAX_ARM_SECONDS);
    };
    let bars = plan.seconds_at(bpm);
    let one_bar = f64::from(plan.beats_per_bar) * 60.0 / bpm;
    // A waiting call also keeps a moment for the clock's Start to arrive.
    let arm_floor = if wait { MIN_ARM_SECONDS } else { 0.0 };
    if bars + one_bar + arm_floor > cap {
        let hint = if wait {
            format!(
                "Use wait: false (up to {MAX_BACKGROUND_SECONDS} s) and await_recording, or \
                 fewer bars."
            )
        } else {
            "Record fewer bars per recording.".to_string()
        };
        return Err(ApiError::invalid_params(format!(
            "{} bars of {} at {bpm:.1} bpm last {bars:.1} s, and with up to a bar's wait for the \
             downbeat (and {arm_floor} s to arm) that is more than the {cap} s this call allows. {hint}",
            plan.bars, plan.beats_per_bar
        )));
    }
    Ok(if wait {
        cap - bars - one_bar
    } else {
        MAX_ARM_SECONDS
    })
}

#[async_trait]
impl AudioRecApi for AudioRecService {
    async fn list_devices(&self) -> ApiResult<Devices> {
        let devices = tokio::task::spawn_blocking(audiowatch_record::list_devices)
            .await
            .map_err(|e| failed(format!("device listing failed: {e}")))?;
        Ok(Devices { devices })
    }

    #[allow(clippy::too_many_arguments)]
    async fn record(
        &self,
        takes: Vec<String>,
        seconds: Option<f64>,
        bars: Option<u32>,
        beats_per_bar: Option<u32>,
        clock: Option<String>,
        dir: Option<String>,
        wait: Option<bool>,
    ) -> ApiResult<Recorded> {
        let wait = wait.unwrap_or(true);
        let asked = length(seconds, bars)?;
        if let Asked::Seconds(seconds) = asked {
            check_seconds(seconds, wait)?;
            if clock.is_some() {
                return Err(ApiError::invalid_params(
                    "clock names the MIDI input a bars recording follows; give bars with it",
                ));
            }
        }
        let beats_per_bar = beats_per_bar.unwrap_or(4);
        if beats_per_bar == 0 {
            return Err(ApiError::invalid_params("beats_per_bar must be at least 1"));
        }
        let specs = audiowatch_record::parse_takes(&takes)
            .map_err(|e| ApiError::invalid_params(e.to_string()))?;
        let dir = self.dir_for(dir);
        let start: Starter = match asked {
            Asked::Seconds(seconds) => Box::new(move || {
                Recording::start(&specs, &dir, Some(seconds))
                    .map(Running::Seconds)
                    .map_err(|e| e.to_string())
            }),
            Asked::Bars(bars) => start_bars(specs, dir, bars, beats_per_bar, clock, wait).await?,
        };
        if wait {
            let finished = tokio::task::spawn_blocking(move || start().map(Running::wait))
                .await
                .map_err(|e| failed(format!("the recording thread failed: {e}")))?
                .map_err(failed)?;
            return landed(None, asked, finished);
        }

        {
            let mut book = self.book.lock().expect("book lock");
            let full = make_room(&mut book.held, MAX_HELD, |h| h.recording.is_finished());
            match full {
                Ok(Some(dropped)) => {
                    book.dropped.push_back(dropped);
                    if book.dropped.len() > 64 {
                        book.dropped.pop_front();
                    }
                }
                Ok(None) => {}
                Err(()) => {
                    return Err(failed(format!(
                        "{MAX_HELD} recordings are still running; await_recording one of them \
                         before starting another"
                    )))
                }
            }
        }
        let recording = tokio::task::spawn_blocking(start)
            .await
            .map_err(|e| failed(format!("the recording thread failed: {e}")))?
            .map_err(failed)?;
        let mut book = self.book.lock().expect("book lock");
        let id = book.next;
        book.next += 1;
        let answer = Recorded::pending(id, asked, recording.takes());
        book.held.insert(id, Held { recording, asked });
        Ok(answer)
    }

    // A task wrapper (MCP tasks, once a client is known to turn their
    // notifications into a model turn) would attach here: this is the one
    // call whose answer arrives later than its question.
    async fn await_recording(&self, id: u64) -> ApiResult<Recorded> {
        loop {
            // The lock is taken and released inside this block, never held
            // across an await, so a cancelled await leaves the recording
            // where the next one finds it.
            let finished: Option<Held> = {
                let mut book = self.book.lock().expect("book lock");
                match book.held.get(&id) {
                    Some(h) if h.recording.is_finished() => book.held.remove(&id),
                    Some(_) => None,
                    None => {
                        let why = if id == 0 || id >= book.next {
                            "no recording has that id"
                        } else if book.dropped.contains(&id) {
                            "it finished and was dropped uncollected to make room for newer ones \
                             (its files are still on disk)"
                        } else {
                            "it was already collected"
                        };
                        return Err(ApiError::invalid_params(format!("recording {id}: {why}")));
                    }
                }
            };
            if let Some(held) = finished {
                // Finished: joining the recording thread is immediate.
                let asked = held.asked;
                let finished = tokio::task::spawn_blocking(move || held.recording.wait())
                    .await
                    .map_err(|e| failed(format!("the recording thread failed: {e}")))?;
                return landed(Some(id), asked, finished);
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

/// Follow a MIDI clock for `bars` bars: find the clock, check the bars fit
/// the call at its tempo, and return what starts the take.
#[cfg(feature = "midi-clock")]
async fn start_bars(
    specs: Vec<audiowatch_record::TakeSpec>,
    dir: PathBuf,
    bars: u32,
    beats_per_bar: u32,
    clock: Option<String>,
    wait: bool,
) -> ApiResult<Starter> {
    let input =
        tokio::task::spawn_blocking(move || audiowatch_clock::port::listen(clock.as_deref()))
            .await
            .map_err(|e| failed(format!("the MIDI scan failed: {e}")))?
            .map_err(|e| ApiError::invalid_params(e.to_string()))?;
    let plan = BarPlan {
        beats_per_bar,
        bars,
        max_take_ns: Some((MAX_BACKGROUND_SECONDS * 1e9) as u64),
    };
    let arm = check_bars(&plan, input.bpm, wait)?;
    let options = BarOptions {
        plan,
        arm_timeout: Duration::from_secs_f64(arm),
    };
    Ok(Box::new(move || {
        BarRecording::start(&specs, &dir, input, options)
            .map(Running::Bars)
            .map_err(|e| e.to_string())
    }))
}

/// A build without the MIDI clock refuses `bars` by name.
#[cfg(not(feature = "midi-clock"))]
async fn start_bars(
    _: Vec<audiowatch_record::TakeSpec>,
    _: PathBuf,
    _: u32,
    _: u32,
    _: Option<String>,
    _: bool,
) -> ApiResult<Starter> {
    Err(ApiError::invalid_params(NO_CLOCK))
}

/// The rmcp server over a service, wrapped so long calls send progress.
pub fn mcp_server_over(service: Arc<AudioRecService>) -> Heartbeat<McpApiServer<AudioRecService>> {
    use simply_api::export::rmcp::model::Implementation;
    let mut implementation = Implementation::from_build_env();
    implementation.name = "audiowatch".into();
    implementation.version = VERSION.into();
    implementation.title = Some("audiowatch — record any device's channels to WAV".into());
    Heartbeat::new(
        McpApiServer::new(service, audio_rec_api_tool_router())
            .with_implementation(implementation)
            .with_instructions(INSTRUCTIONS),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiowatch_record::TakeResult;
    use simply_api::ApiErrorKind;

    fn scratch() -> PathBuf {
        std::env::temp_dir().join(format!("audiowatch-rec-{}", std::process::id()))
    }

    #[test]
    fn a_length_over_the_cap_is_refused_by_name_not_truncated() {
        let e = check_seconds(51.0, true).unwrap_err();
        assert_eq!(e.kind, ApiErrorKind::InvalidParams);
        assert!(
            e.message.contains("at most 50") && e.message.contains("wait: false"),
            "{}",
            e.message
        );
        assert!(check_seconds(51.0, false).is_ok());
        assert!(check_seconds(601.0, false)
            .unwrap_err()
            .message
            .contains("at most 600"));
        assert!(check_seconds(0.0, true).is_err());
        assert!(check_seconds(f64::NAN, false).is_err());
        assert!(check_seconds(MAX_SECONDS, true).is_ok());
    }

    /// Every refusal here happens before any device is opened.
    #[tokio::test]
    async fn bad_takes_bad_lengths_and_bad_ids_open_nothing() {
        let service = AudioRecService::new(scratch());
        let e = service
            .record(
                vec!["BlackHole 16ch:in:1-2".into()],
                Some(120.0),
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(e.message.contains("at most 50"));
        let e = service
            .record(
                vec!["BlackHole".into()],
                Some(1.0),
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(e.kind, ApiErrorKind::InvalidParams);
        assert!(
            e.message.contains("DEVICE:in|out:CHANNELS"),
            "{}",
            e.message
        );
        let e = service
            .record(vec![], Some(1.0), None, None, None, None, Some(true))
            .await
            .unwrap_err();
        assert!(e.message.contains("no takes"), "{}", e.message);
        let e = service.await_recording(7).await.unwrap_err();
        assert_eq!(e.kind, ApiErrorKind::InvalidParams);
        assert!(
            e.message.contains("no recording has that id"),
            "{}",
            e.message
        );
    }

    #[test]
    fn room_is_made_by_dropping_the_oldest_finished_never_a_running_one() {
        let mut held: BTreeMap<u64, bool> = (1..=3).map(|i| (i, i != 2)).collect(); // 2 is running
        assert_eq!(make_room(&mut held, 4, |&f| f), Ok(None));
        assert_eq!(make_room(&mut held, 3, |&f| f), Ok(Some(1)));
        assert_eq!(held.keys().copied().collect::<Vec<_>>(), vec![2, 3]);
        let mut running: BTreeMap<u64, bool> = (1..=2).map(|i| (i, false)).collect();
        assert_eq!(make_room(&mut running, 2, |&f| f), Err(()));
    }

    #[test]
    fn folders_are_absolute_and_relative_ones_sit_under_the_servers() {
        let service = AudioRecService::new("recordings");
        assert!(service.dir().is_absolute());
        assert_eq!(service.dir_for(Some("x".into())), service.dir().join("x"));
        assert_eq!(service.dir_for(Some("/abs".into())), PathBuf::from("/abs"));
        assert_eq!(service.dir_for(None), service.dir());
    }

    #[test]
    fn a_complete_answer_gathers_every_takes_warnings() {
        let info = TakeInfo {
            spec: "BlackHole 16ch:in:1-2".into(),
            device: "BlackHole 16ch".into(),
            channels: Channels::Pair(1),
            sample_rate: 48_000,
            path: "/r/a.wav".into(),
        };
        let r = TakeResult {
            take: info.clone(),
            duration_s: 2.0,
            warnings: vec!["BlackHole 16ch:in:1-2: x".into()],
            sync: None,
        };
        let outcome = Outcome {
            takes: vec![r],
            ..Outcome::default()
        };
        let done = Recorded::complete(None, Asked::Seconds(2.0), outcome.into());
        assert!(done.complete);
        assert_eq!(done.takes[0].duration_s, Some(2.0));
        assert_eq!(done.warnings, vec!["BlackHole 16ch:in:1-2: x".to_string()]);
        let pending = Recorded::pending(3, Asked::Seconds(2.0), &[info]);
        assert_eq!(
            (pending.id, pending.complete, pending.takes[0].duration_s),
            (Some(3), false, None)
        );
    }

    #[test]
    fn a_length_is_seconds_or_bars_never_both_or_neither() {
        assert_eq!(length(Some(2.0), None), Ok(Asked::Seconds(2.0)));
        assert_eq!(length(None, Some(4)), Ok(Asked::Bars(4)));
        assert!(length(Some(2.0), Some(4))
            .unwrap_err()
            .message
            .contains("not both"));
        assert!(length(None, None)
            .unwrap_err()
            .message
            .contains("bars of a MIDI clock"));
        assert!(length(None, Some(0)).is_err());
    }

    #[cfg(feature = "midi-clock")]
    fn bars(n: u32) -> BarPlan {
        BarPlan {
            beats_per_bar: 4,
            bars: n,
            max_take_ns: None,
        }
    }

    #[cfg(feature = "midi-clock")]
    #[test]
    fn bars_that_outlast_a_waiting_call_are_refused_by_name_at_the_clocks_tempo() {
        // 8 bars of 4 at 120 bpm = 16 s, plus up to a bar (2 s) to the
        // downbeat: fits, and leaves the rest of 50 s to wait for a Start.
        let arm = check_bars(&bars(8), Some(120.0), true).unwrap();
        assert!((arm - 32.0).abs() < 1e-9, "{arm}");
        // 24 bars = 48 s + 2 s: over 50.
        let e = check_bars(&bars(24), Some(120.0), true).unwrap_err();
        assert_eq!(e.kind, ApiErrorKind::InvalidParams);
        assert!(
            e.message.contains("48.0 s") && e.message.contains("wait: false"),
            "{}",
            e.message
        );
        assert_eq!(
            check_bars(&bars(24), Some(120.0), false).unwrap(),
            MAX_ARM_SECONDS
        );
        // 300 bars = 600 s: over even a background call.
        assert!(check_bars(&bars(300), Some(120.0), false)
            .unwrap_err()
            .message
            .contains("fewer bars"));
    }

    #[cfg(feature = "midi-clock")]
    #[test]
    fn an_unknown_tempo_can_only_be_recorded_in_the_background() {
        let e = check_bars(&bars(2), None, true).unwrap_err();
        assert!(
            e.message.contains("tempo") && e.message.contains("wait: false"),
            "{}",
            e.message
        );
        assert_eq!(check_bars(&bars(2), None, false).unwrap(), MAX_ARM_SECONDS);
    }

    #[tokio::test]
    async fn a_clock_without_bars_is_refused_before_anything_opens() {
        let service = AudioRecService::new(scratch());
        let e = service
            .record(
                vec!["BlackHole 16ch:in:1-2".into()],
                Some(1.0),
                None,
                None,
                Some("Digitakt".into()),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(e.message.contains("give bars with it"), "{}", e.message);
    }

    #[test]
    fn a_bars_recording_that_found_no_downbeat_is_an_error_not_an_empty_answer() {
        let outcome = Outcome {
            failed: Some("no Start".into()),
            ..Outcome::default()
        };
        let e = landed(Some(5), Asked::Bars(4), outcome.into()).unwrap_err();
        assert!(
            e.message.contains("recording 5 recorded nothing: no Start"),
            "{}",
            e.message
        );
    }

    #[cfg(not(feature = "midi-clock"))]
    #[tokio::test]
    async fn a_build_without_the_clock_refuses_bars_by_name() {
        let service = AudioRecService::new(scratch());
        let e = service
            .record(
                vec!["BlackHole 16ch:in:1-2".into()],
                None,
                Some(4),
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(e.kind, ApiErrorKind::InvalidParams);
        assert!(
            e.message.contains("without the midi-clock feature"),
            "{}",
            e.message
        );
    }
}
