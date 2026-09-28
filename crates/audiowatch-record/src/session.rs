//! A running recording: several takes started together, each its own stream
//! and its own file, stopped together — or on their own after a set length,
//! or trimmed to a window of host time the caller names once it knows it.
//!
//! # Why a handle and not a blocking call
//!
//! The caller is usually an agent, and an agent that has to decide a
//! recording's length before it starts has to guess. So a [`Recording`] is
//! started, polled ([`Recording::status`]), and stopped
//! ([`Recording::stop`]) — or, the shape an agent handles best, given its
//! length up front and waited on ([`Recording::wait`]). Either way the files
//! are finished and closed before their paths are handed back.
//!
//! # One stream per take, never mixed
//!
//! Two devices are two clocks: a mixer at 48 kHz and a virtual device at
//! 44.1 kHz do not share a sample, and even two devices nominally at 48 kHz
//! drift apart. So every take gets its own stream, its own ring, its own
//! writer and its own file at its device's own rate, and nothing is resampled
//! or summed. The streams are built first and started back to back, which is
//! as close together as separate clocks can be started.
//!
//! # What the audio callback does
//!
//! Picks the take's channels out of the device's frames into a buffer that
//! was allocated before the stream opened, pushes them into a lock-free ring,
//! and counts. It never allocates, locks or blocks: when the ring is full the
//! frames are *dropped*, and the take comes back with a warning saying so
//! rather than hiding the gap. The writer thread (see [`crate::wav::pump`])
//! does the file I/O.
//!
//! The streams live on a thread of their own for the recording's whole life,
//! because a cpal stream is not promised to be `Send` on every host; the
//! handle talks to that thread through a channel.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, StreamTrait};

use crate::catalog::{self, choose_rate, pick, route, PickError, Route, RouteRefusal, TapSupport};
use crate::frames::extract;
use crate::spec::{Channels, SpecError, TakeSpec};
use crate::wav::{pump, trim, WavSink};

/// How many seconds of audio a take's ring holds before it overflows — the
/// time a writer can stall (a slow disk, a busy machine) without losing a
/// frame.
const RING_SECONDS: u32 = 4;

/// Frames extracted per push. The callback's own buffer is at most a few
/// thousand frames; this only bounds the scratch allocated before the stream.
const SCRATCH_FRAMES: usize = 8192;

/// How often the recording thread checks for an automatic stop.
const POLL: Duration = Duration::from_millis(20);

/// Buffer timing points a take's ring holds between two polls: one per audio
/// callback, so seconds' worth at any buffer size.
const TIMING_POINTS: usize = 4096;

/// After a windowed take's window is given, how long to wait for the audio
/// that covers its end before closing anyway.
const TAIL_GRACE: Duration = Duration::from_secs(2);

/// A take as it was opened: what it records, where, and at what rate.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TakeInfo {
    /// The take as written, `DEVICE:in|out:CHANNELS`, with the device's full
    /// name.
    pub spec: String,
    /// The device's full name, as matched.
    pub device: String,
    /// The channel or pair recorded (`"3"`, `"13-14"`).
    pub channels: Channels,
    /// The device's own rate; the file's rate.
    pub sample_rate: u32,
    /// The file, as an absolute path.
    pub path: String,
}

/// Where a windowed take's first frame lies against the window's start.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TakeSync {
    /// Frames recorded before the window's start and cut off the front.
    pub trimmed_frames: u64,
    /// The file's first frame minus the start's exact (fractional) frame:
    /// within ±0.5, the rounding to a whole frame.
    pub offset_frames: f64,
}

/// The part of a windowed recording to keep, in host nanoseconds — the clock
/// every buffer's capture time is stamped on (`mach_absolute_time` as
/// nanoseconds, on macOS). Whatever decided it is the caller's business.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TakeWindow {
    pub start_ns: u64,
    pub end_ns: u64,
}

/// Everything a finished recording says.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Outcome {
    pub takes: Vec<TakeResult>,
    /// A windowed recording that kept nothing, and why (cancelled, or stopped
    /// before its window was given). Its files were removed.
    pub failed: Option<String>,
}

/// A take while it runs.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TakeStatus {
    pub path: String,
    /// Frames on disk so far.
    pub frames_written: u64,
    /// Audio has been dropped because the writer fell behind.
    pub dropped_audio: bool,
    /// A stream error (device unplugged, …), if one has happened.
    pub error: Option<String>,
}

/// A recording while it runs.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RecordingStatus {
    /// `false` once an automatic stop (the length given at start) has closed
    /// the files; `stop` or `wait` then just hands the results back.
    pub running: bool,
    pub elapsed_s: f64,
    /// The automatic stop, if one was given.
    pub limit_s: Option<f64>,
    pub takes: Vec<TakeStatus>,
}

/// A take after it stopped: a finished, closed file.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TakeResult {
    #[serde(flatten)]
    pub take: TakeInfo,
    /// Length of the file, in seconds at its own rate.
    pub duration_s: f64,
    /// Things worth acting on, each naming the take: dropped audio (the file
    /// has gaps), a stream error, nothing arriving at all.
    pub warnings: Vec<String>,
    /// For a windowed take: where its first frame lies against the window's
    /// start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<TakeSync>,
}

/// Why a recording could not start.
#[derive(Debug)]
pub enum RecordError {
    NoTakes,
    Spec(SpecError),
    Pick(PickError),
    Route(RouteRefusal),
    NoRate { device: String },
    SamePath(String),
    File { path: String, error: String },
    Stream { take: String, route: Route, error: simply_audio_device::Error },
    Thread,
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordError::NoTakes => {
                f.write_str("no takes given; write at least one DEVICE:in|out:CHANNELS")
            }
            RecordError::Spec(e) => e.fmt(f),
            RecordError::Pick(e) => e.fmt(f),
            RecordError::Route(e) => e.fmt(f),
            RecordError::NoRate { device } => {
                write!(f, "{device:?} reports no sample rate to open at")
            }
            RecordError::SamePath(p) => write!(f, "two takes would write the same file {p}"),
            RecordError::File { path, error } => write!(f, "cannot write {path}: {error}"),
            RecordError::Stream { take, route, error } if error.is_permission_denied() => {
                write!(f, "{take}: macOS refused access. {}", permission_hint(*route))
            }
            RecordError::Stream { take, error, .. } => write!(f, "{take}: {error}"),
            RecordError::Thread => f.write_str("the recording thread stopped before it reported"),
        }
    }
}

impl std::error::Error for RecordError {}

impl From<SpecError> for RecordError {
    fn from(e: SpecError) -> Self {
        RecordError::Spec(e)
    }
}

/// Where a person grants the permission a route needs — the one failure only
/// they can fix. (macOS may also answer a missing permission with a stream of
/// zeros rather than an error; the recorder cannot tell that from a quiet
/// input, which is why the skill doc says to check a first take's levels with
/// an analysis tool.)
pub fn permission_hint(route: Route) -> &'static str {
    match route {
        Route::Input => {
            "Grant microphone access to the app running this recorder (the terminal, or the \
             MCP host) in System Settings → Privacy & Security → Microphone, then restart that app."
        }
        Route::Tap => {
            "Grant system audio recording to the app running this recorder in System Settings → \
             Privacy & Security → Screen & System Audio Recording (\"System Audio Recording \
             Only\"), then restart that app."
        }
    }
}

/// Parse every take, refusing the lot on the first bad one.
pub fn parse_takes<S: AsRef<str>>(takes: &[S]) -> Result<Vec<TakeSpec>, RecordError> {
    takes.iter().map(|t| TakeSpec::parse(t.as_ref()).map_err(RecordError::from)).collect()
}

/// What the callback and the error callback count.
#[derive(Default)]
struct Counters {
    captured: AtomicU64,
    overflow: AtomicU64,
    error: Mutex<Option<String>>,
}

struct Live {
    counters: Arc<Counters>,
    written: Arc<AtomicU64>,
}

/// A running (or automatically stopped) recording. Dropping it stops it and
/// finishes the files.
pub struct Recording {
    takes: Vec<TakeInfo>,
    live: Vec<Live>,
    started: Instant,
    limit_s: Option<f64>,
    finished: Arc<AtomicBool>,
    stopped_after: Arc<Mutex<Option<Duration>>>,
    /// For a windowed recording: the earliest host time every take has audio
    /// from, once it does. 0 until then.
    armed_at: Arc<AtomicU64>,
    control: Option<mpsc::Sender<Control>>,
    thread: Option<JoinHandle<Outcome>>,
}

/// What the handle tells the recording thread.
enum Control {
    Stop,
    Window(TakeWindow),
    Cancel(String),
}

/// How a recording ends.
enum Length {
    /// After this many seconds, or at `stop`.
    Seconds(Option<f64>),
    /// Trimmed to a window given later through [`Recording::set_window`].
    Window,
}

impl Recording {
    /// Open every take and start them together. Files whose take gives no
    /// path go in `dir`, named for what they record. With `seconds`, each take
    /// stops by itself after exactly that many frames at its own rate.
    ///
    /// Returns once every stream is running, or with the first reason one
    /// could not start — in which case nothing is left running and no file is
    /// left behind.
    pub fn start(
        specs: &[TakeSpec],
        dir: &Path,
        seconds: Option<f64>,
    ) -> Result<Recording, RecordError> {
        Recording::begin(specs, dir, Length::Seconds(seconds))
    }

    /// Open every take now, recording into `.part.wav` files beside their
    /// final paths, and keep only the part inside a [`TakeWindow`] given
    /// later with [`Recording::set_window`] — trimmed to the frame whose
    /// capture time matches each end. [`Recording::armed_at`] says from when
    /// every take has audio, which is the earliest a window can start.
    ///
    /// The recording runs until its window's end is covered by every take's
    /// audio (or a grace period after the window arrives), or until it is
    /// cancelled or stopped; stopped before a window was given, it keeps
    /// nothing.
    pub fn start_windowed(specs: &[TakeSpec], dir: &Path) -> Result<Recording, RecordError> {
        Recording::begin(specs, dir, Length::Window)
    }

    fn begin(specs: &[TakeSpec], dir: &Path, length: Length) -> Result<Recording, RecordError> {
        let seconds = match &length {
            Length::Seconds(s) => *s,
            Length::Window => None,
        };
        if specs.is_empty() {
            return Err(RecordError::NoTakes);
        }
        // A clean absolute folder, so every path handed back works from
        // anywhere and reads without `..` in it.
        std::fs::create_dir_all(dir).map_err(|e| RecordError::File {
            path: dir.display().to_string(),
            error: e.to_string(),
        })?;
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let paths = assign_paths(specs, &dir, unix_seconds(), |p| p.exists())?;
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (control_tx, control_rx) = mpsc::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let stopped_after = Arc::new(Mutex::new(None));
        let armed_at = Arc::new(AtomicU64::new(0));
        let thread = {
            let specs = specs.to_vec();
            let shared = Shared {
                finished: finished.clone(),
                stopped_after: stopped_after.clone(),
                armed_at: armed_at.clone(),
            };
            std::thread::Builder::new()
                .name("audio-rec".into())
                .spawn(move || run(specs, paths, length, ready_tx, control_rx, shared))
                .map_err(|_| RecordError::Thread)?
        };
        match ready_rx.recv() {
            Ok(Ok((takes, live, started))) => Ok(Recording {
                takes,
                live,
                started,
                limit_s: seconds,
                finished,
                stopped_after,
                armed_at,
                control: Some(control_tx),
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(RecordError::Thread)
            }
        }
    }

    pub fn takes(&self) -> &[TakeInfo] {
        &self.takes
    }

    /// Has an automatic stop already closed the files?
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// For a windowed recording: the earliest host time (ns) from which
    /// every take has audio, once each has delivered its first buffer. A
    /// window starting earlier would begin before some take's first frame.
    pub fn armed_at(&self) -> Option<u64> {
        Some(self.armed_at.load(Ordering::Acquire)).filter(|&t| t != 0)
    }

    /// Give a windowed recording the part to keep. It closes once every
    /// take's audio covers `window.end_ns`. A second window replaces the
    /// first; a recording started with a length ignores it.
    pub fn set_window(&self, window: TakeWindow) {
        if let Some(control) = &self.control {
            let _ = control.send(Control::Window(window));
        }
    }

    /// Stop a windowed recording and keep nothing: its files are removed and
    /// the outcome's `failed` is `why`.
    pub fn cancel(mut self, why: impl Into<String>) -> Outcome {
        if let Some(control) = self.control.take() {
            let _ = control.send(Control::Cancel(why.into()));
        }
        self.thread.take().and_then(|t| t.join().ok()).unwrap_or_default()
    }

    pub fn status(&self) -> RecordingStatus {
        let elapsed = self
            .stopped_after
            .lock()
            .ok()
            .and_then(|g| *g)
            .unwrap_or_else(|| self.started.elapsed());
        RecordingStatus {
            running: !self.is_finished(),
            elapsed_s: elapsed.as_secs_f64(),
            limit_s: self.limit_s,
            takes: self
                .takes
                .iter()
                .zip(&self.live)
                .map(|(t, l)| TakeStatus {
                    path: t.path.clone(),
                    frames_written: l.written.load(Ordering::Relaxed),
                    dropped_audio: l.counters.overflow.load(Ordering::Relaxed) > 0,
                    error: l.counters.error.lock().ok().and_then(|e| e.clone()),
                })
                .collect(),
        }
    }

    /// Stop every take, finish the files, and say what is in them.
    pub fn stop(mut self) -> Outcome {
        self.finish()
    }

    /// Block until the automatic stop given at start has happened (or forever
    /// if none was given — use [`Recording::stop`] then), and return results.
    pub fn wait(mut self) -> Outcome {
        // The stop channel stays open while we wait: closing it is a stop.
        self.thread.take().and_then(|t| t.join().ok()).unwrap_or_default()
    }

    fn finish(&mut self) -> Outcome {
        if let Some(control) = self.control.take() {
            let _ = control.send(Control::Stop);
        }
        self.thread.take().and_then(|t| t.join().ok()).unwrap_or_default()
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.finish();
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Where each take's file goes. An explicit path is used as given (relative
/// ones under `dir`) and may overwrite; a generated one is
/// `<dir>/<device>-<in|out>-<channels>-<unix seconds>.wav`, never an existing
/// file. Two takes may not share a file.
pub fn assign_paths(
    specs: &[TakeSpec],
    dir: &Path,
    stamp: u64,
    exists: impl Fn(&Path) -> bool,
) -> Result<Vec<PathBuf>, RecordError> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(specs.len());
    for spec in specs {
        let path = match &spec.path {
            Some(p) if p.is_absolute() => p.clone(),
            Some(p) => dir.join(p),
            None => {
                let base = format!("{}-{stamp}", spec.stem());
                let mut candidate = dir.join(format!("{base}.wav"));
                let mut n = 2;
                while exists(&candidate) || out.contains(&candidate) {
                    candidate = dir.join(format!("{base}-{n}.wav"));
                    n += 1;
                }
                candidate
            }
        };
        if out.contains(&path) {
            return Err(RecordError::SamePath(path.display().to_string()));
        }
        out.push(path);
    }
    Ok(out)
}

type Ready = Result<(Vec<TakeInfo>, Vec<Live>, Instant), RecordError>;

struct Open {
    info: TakeInfo,
    /// Where the writer writes: the final path, or a `.part.wav` beside it
    /// for a take that is trimmed when it ends.
    written_to: PathBuf,
    timing: Option<rtrb::Consumer<(u64, u64)>>,
    route: Route,
    stream: cpal::Stream,
    writer: JoinHandle<Result<u64, String>>,
    done: Arc<AtomicBool>,
    counters: Arc<Counters>,
    written: Arc<AtomicU64>,
}

/// What the recording thread shares with its handle.
struct Shared {
    finished: Arc<AtomicBool>,
    stopped_after: Arc<Mutex<Option<Duration>>>,
    armed_at: Arc<AtomicU64>,
}

/// The recording thread: open, report, wait, close, measure.
fn run(
    specs: Vec<TakeSpec>,
    paths: Vec<PathBuf>,
    length: Length,
    ready: mpsc::SyncSender<Ready>,
    control: mpsc::Receiver<Control>,
    shared: Shared,
) -> Outcome {
    let (seconds, windowed) = match length {
        Length::Seconds(s) => (s, false),
        Length::Window => (None, true),
    };
    let taps = TapSupport::detect();
    let devices = catalog::devices_with_handles(&taps);
    let infos: Vec<catalog::DeviceInfo> = devices.iter().map(|(_, i)| i.clone()).collect();

    let mut open: Vec<Open> = Vec::new();
    let mut failure = None;
    for (spec, path) in specs.iter().zip(&paths) {
        match open_take(spec, path, &devices, &infos, &taps, seconds, windowed) {
            Ok(o) => open.push(o),
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    if let Some(e) = failure {
        for o in open {
            drop(o.stream);
            o.done.store(true, Ordering::Release);
            let _ = o.writer.join();
            let _ = std::fs::remove_file(&o.written_to);
        }
        let _ = ready.send(Err(e));
        return Outcome::default();
    }

    // Everything is built: start them back to back.
    let mut play_error = None;
    for o in &open {
        if let Err(e) = o.stream.play() {
            play_error = Some(RecordError::Stream {
                take: o.info.spec.clone(),
                route: o.route,
                error: simply_audio_device::Error::Cpal(e),
            });
            break;
        }
    }
    if let Some(e) = play_error {
        for o in open {
            drop(o.stream);
            o.done.store(true, Ordering::Release);
            let _ = o.writer.join();
            let _ = std::fs::remove_file(&o.written_to);
        }
        let _ = ready.send(Err(e));
        return Outcome::default();
    }
    let started = Instant::now();
    let infos: Vec<TakeInfo> = open.iter().map(|o| o.info.clone()).collect();
    let live = open
        .iter()
        .map(|o| Live { counters: o.counters.clone(), written: o.written.clone() })
        .collect();
    if ready.send(Ok((infos, live, started))).is_err() {
        // Nobody is listening; close up as if stopped.
    }

    let mut window = windowed.then(|| Windowed::new(open.len()));
    let limits: Vec<u64> = open.iter().map(|o| frame_limit(seconds, o.info.sample_rate)).collect();
    loop {
        match control.recv_timeout(POLL) {
            Ok(Control::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(w) = window.as_mut() {
                    w.stopped();
                }
                break;
            }
            Ok(Control::Cancel(why)) => {
                if let Some(w) = window.as_mut() {
                    w.failed = Some(why);
                }
                break;
            }
            Ok(Control::Window(given)) => {
                if let Some(w) = window.as_mut() {
                    w.window = Some((given, Instant::now()));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let all_done = match window.as_mut() {
                    Some(w) => w.step(&mut open, &shared.armed_at),
                    None => {
                        seconds.is_some()
                            && open
                                .iter()
                                .zip(&limits)
                                .all(|(o, &l)| o.counters.captured.load(Ordering::Relaxed) >= l)
                    }
                };
                if all_done {
                    break;
                }
            }
        }
    }
    if let Ok(mut g) = shared.stopped_after.lock() {
        *g = Some(started.elapsed());
    }
    if let Some(w) = window.as_mut() {
        // What arrived since the last poll: the end may be in it.
        w.collect(&mut open);
    }

    // Streams first, so nothing more is pushed; then let every writer drain.
    let mut rest = Vec::with_capacity(open.len());
    for o in open {
        drop(o.stream);
        rest.push((o.info, o.written_to, o.writer, o.done, o.counters));
    }
    for (_, _, _, done, _) in &rest {
        done.store(true, Ordering::Release);
    }
    let finished_takes: Vec<Closed> = rest
        .into_iter()
        .map(|(info, written_to, writer, _, counters)| {
            let frames = writer.join().unwrap_or_else(|_| Err("the writer thread panicked".into()));
            Closed { info, written_to, frames, counters }
        })
        .collect();
    let outcome = match window {
        None => Outcome {
            takes: finished_takes
                .into_iter()
                .map(|c| result(c.info, c.frames, &c.counters))
                .collect(),
            ..Outcome::default()
        },
        Some(w) => w.finish(finished_takes),
    };
    shared.finished.store(true, Ordering::Release);
    outcome
}

/// A take whose stream is closed and whose writer has finished.
struct Closed {
    info: TakeInfo,
    written_to: PathBuf,
    frames: Result<u64, String>,
    counters: Arc<Counters>,
}

/// The recording thread's side of a windowed take: collects every take's
/// buffer timing, says when all of them are running, and once the window is
/// given and covered trims each file to it.
struct Windowed {
    /// Per take: `(frame, capture ns)` at the start of every audio buffer.
    points: Vec<Vec<(u64, u64)>>,
    /// The window, and when it arrived.
    window: Option<(TakeWindow, Instant)>,
    failed: Option<String>,
}

impl Windowed {
    fn new(takes: usize) -> Windowed {
        Windowed { points: vec![Vec::new(); takes], window: None, failed: None }
    }

    fn collect(&mut self, open: &mut [Open]) {
        for (o, pts) in open.iter_mut().zip(&mut self.points) {
            if let Some(ring) = o.timing.as_mut() {
                while let Ok(p) = ring.pop() {
                    pts.push(p);
                }
            }
        }
    }

    /// One poll: returns `true` once the recording can close.
    fn step(&mut self, open: &mut [Open], armed_at: &AtomicU64) -> bool {
        self.collect(open);
        if armed_at.load(Ordering::Relaxed) == 0 && self.points.iter().all(|p| !p.is_empty()) {
            // A window must start where every take already has audio.
            let first = self.points.iter().map(|p| p[0].1).max().unwrap_or(0).max(1);
            armed_at.store(first, Ordering::Release);
        }
        let Some((window, given)) = self.window else { return false };
        // Close once every take has audio past the end.
        let covered =
            self.points.iter().all(|p| p.last().is_some_and(|&(_, ns)| ns > window.end_ns));
        covered || given.elapsed() > TAIL_GRACE
    }

    fn stopped(&mut self) {
        if self.window.is_none() && self.failed.is_none() {
            self.failed = Some("stopped before the take's window was known".into());
        }
    }

    fn finish(self, takes: Vec<Closed>) -> Outcome {
        let window = if self.failed.is_none() { self.window.map(|(w, _)| w) } else { None };
        let Some(w) = window else {
            let failed = self.failed.unwrap_or_else(|| "the take was never given a window".into());
            for c in &takes {
                let _ = std::fs::remove_file(&c.written_to);
            }
            return Outcome { takes: Vec::new(), failed: Some(failed) };
        };
        let takes = takes
            .into_iter()
            .zip(&self.points)
            .map(|(c, points)| {
                let rate = c.info.sample_rate;
                land(c.info, &c.written_to, c.frames, &c.counters, points, rate, &w)
            })
            .collect();
        Outcome { takes, failed: None }
    }
}

/// Trim one take's `.part.wav` to the window and put it at its final path.
fn land(
    info: TakeInfo,
    part: &Path,
    frames: Result<u64, String>,
    counters: &Counters,
    points: &[(u64, u64)],
    rate: u32,
    w: &TakeWindow,
) -> TakeResult {
    let spec = info.spec.clone();
    let final_path = PathBuf::from(&info.path);
    let (Some(start), Some(end)) =
        (frame_at(points, rate, w.start_ns), frame_at(points, rate, w.end_ns))
    else {
        let _ = std::fs::remove_file(part);
        let mut r = result(info, Ok(0), counters);
        r.warnings.push(format!(
            "{spec}: this device reported no capture time for its audio, so the take could not \
             be placed in the window; nothing was kept"
        ));
        return r;
    };
    let from = start.round().max(0.0) as u64;
    let to = (end.round().max(0.0) as u64).max(from);
    let trimmed = frames.and_then(|_| trim(part, &final_path, from, to).map_err(|e| e.to_string()));
    let _ = std::fs::remove_file(part);
    let short = matches!(trimmed, Ok(n) if n < to - from);
    let mut r = result(info, trimmed, counters);
    if short {
        r.warnings.push(format!(
            "{spec}: the audio ended before the window's end, so the file is short of it"
        ));
    }
    if counters.overflow.load(Ordering::Relaxed) > 0 {
        r.warnings.push(format!(
            "{spec}: because audio was dropped, frames after the gap sit early in the window"
        ));
    }
    r.sync = Some(TakeSync { trimmed_frames: from, offset_frames: from as f64 - start });
    r
}

/// The (fractional) frame at host instant `t`, from `(frame, capture ns)`
/// timing points — one per audio buffer, in order. Uses the last point at or
/// before `t`, so the device's drift against the host clock only ever spans
/// one buffer. `None` if the audio had not started by `t`.
pub fn frame_at(points: &[(u64, u64)], rate: u32, t: u64) -> Option<f64> {
    let i = points.partition_point(|&(_, ns)| ns <= t);
    let &(frame, ns) = points.get(i.checked_sub(1)?)?;
    Some(frame as f64 + (t - ns) as f64 * f64::from(rate) / 1e9)
}

fn frame_limit(seconds: Option<f64>, rate: u32) -> u64 {
    match seconds {
        Some(s) if s > 0.0 => (s * f64::from(rate)).round() as u64,
        Some(_) => 0,
        None => u64::MAX,
    }
}

fn open_take(
    spec: &TakeSpec,
    path: &Path,
    devices: &[(cpal::Device, catalog::DeviceInfo)],
    infos: &[catalog::DeviceInfo],
    taps: &TapSupport,
    seconds: Option<f64>,
    windowed: bool,
) -> Result<Open, RecordError> {
    let index = pick(infos, &spec.device, spec.direction).map_err(RecordError::Pick)?;
    let (device, dinfo) = &devices[index];
    let (route, side) =
        route(dinfo, spec.direction, spec.channels, taps).map_err(RecordError::Route)?;
    let rate =
        choose_rate(side).ok_or_else(|| RecordError::NoRate { device: dinfo.name.clone() })?;
    let device_channels = side.channels;
    let keep = spec.channels;
    let count = keep.count() as usize;
    let info = TakeInfo {
        spec: TakeSpec { path: None, device: dinfo.name.clone(), ..spec.clone() }.to_string(),
        device: dinfo.name.clone(),
        channels: keep,
        sample_rate: rate,
        path: path.display().to_string(),
    };

    // A windowed take records a pre-roll it trims when it ends, so it writes
    // beside its final path until then.
    let written_to = if windowed { path.with_extension("part.wav") } else { path.to_path_buf() };
    let mut sink = WavSink::create(&written_to, keep.count(), rate)
        .map_err(|e| RecordError::File { path: info.path.clone(), error: e.to_string() })?;
    let (mut timing_tx, timing) = if windowed {
        let (tx, rx) = rtrb::RingBuffer::<(u64, u64)>::new(TIMING_POINTS);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let (mut producer, mut consumer) =
        rtrb::RingBuffer::<f32>::new((rate * RING_SECONDS) as usize * count);
    let done = Arc::new(AtomicBool::new(false));
    let written = Arc::new(AtomicU64::new(0));
    let counters = Arc::new(Counters::default());
    let writer = {
        let (done, written) = (done.clone(), written.clone());
        std::thread::Builder::new()
            .name(format!("audio-rec {}", info.spec))
            .spawn(move || {
                pump(&mut consumer, &mut sink, &written, &done).map_err(|e| e.to_string())?;
                sink.finish().map_err(|e| e.to_string())
            })
            .map_err(|_| RecordError::Thread)?
    };

    let limit = frame_limit(seconds, rate);
    let width = device_channels as usize;
    let mut scratch = vec![0.0f32; SCRATCH_FRAMES * count];
    let mut captured = 0u64;
    let data_counters = counters.clone();
    let data = move |data: &[f32], callback: &cpal::InputCallbackInfo| {
        if let Some(timing) = timing_tx.as_mut() {
            // Frame index of this buffer's first frame, and when it was
            // captured, on the host clock a window is given in.
            let ns = callback.timestamp().capture.as_nanos() as u64;
            let _ = timing.push((captured, ns));
        }
        let arrived = (data.len() / width) as u64;
        let wanted = arrived.min(limit.saturating_sub(captured)) as usize;
        let mut dropped = 0u64;
        let mut from = 0usize;
        while from < wanted {
            let n = (wanted - from).min(SCRATCH_FRAMES).min(producer.slots() / count);
            if n == 0 {
                dropped += (wanted - from) as u64;
                break;
            }
            let got =
                extract(&data[from * width..], device_channels, keep, &mut scratch[..n * count]);
            // `n` frames fit by construction; a refusal would only mean the
            // count was wrong, and then dropping is still the right answer.
            if producer.push_entire_slice(&scratch[..got * count]).is_err() {
                dropped += got as u64;
            }
            from += n;
        }
        captured += wanted as u64;
        data_counters.captured.store(captured, Ordering::Relaxed);
        if dropped > 0 {
            data_counters.overflow.fetch_add(dropped, Ordering::Relaxed);
        }
    };
    let error_counters = counters.clone();
    let on_error = move |e: cpal::Error| {
        if let Ok(mut slot) = error_counters.error.lock() {
            slot.get_or_insert_with(|| e.to_string());
        }
    };
    let config = cpal::StreamConfig {
        channels: device_channels,
        sample_rate: rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let stream = match device.build_input_stream::<f32, _, _>(config, data, on_error, None) {
        Ok(s) => s,
        Err(e) => {
            done.store(true, Ordering::Release);
            let _ = writer.join();
            let _ = std::fs::remove_file(&written_to);
            return Err(RecordError::Stream {
                take: info.spec,
                route,
                error: simply_audio_device::Error::Cpal(e),
            });
        }
    };
    Ok(Open { info, written_to, timing, route, stream, writer, done, counters, written })
}

fn result(info: TakeInfo, frames: Result<u64, String>, counters: &Counters) -> TakeResult {
    let dropped = counters.overflow.load(Ordering::Relaxed) > 0;
    let stream_error = counters.error.lock().ok().and_then(|e| e.clone());
    let mut warnings = Vec::new();
    let frames = match frames {
        Ok(n) => n,
        Err(e) => {
            warnings.push(format!("{}: the file could not be finished: {e}", info.spec));
            0
        }
    };
    if frames == 0 {
        warnings
            .push(format!("{}: no audio arrived before the stop; the file is empty", info.spec));
    }
    if dropped {
        warnings.push(format!(
            "{}: audio was dropped because the writer fell behind the device, so the file \
             has gaps. A slow or busy disk is the usual cause; record it again.",
            info.spec
        ));
    }
    if let Some(e) = &stream_error {
        warnings.push(format!("{}: the stream reported an error: {e}", info.spec));
    }
    TakeResult {
        duration_s: frames as f64 / f64::from(info.sample_rate.max(1)),
        take: info,
        warnings,
        sync: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specs(list: &[&str]) -> Vec<TakeSpec> {
        parse_takes(list).unwrap()
    }

    #[test]
    fn generated_paths_say_what_they_hold_and_never_collide() {
        let s = specs(&["ZOOM L6max:in:13-14", "BlackHole 16ch:in:1-2", "ZOOM L6max:in:13-14"]);
        let taken = PathBuf::from("/r/zoom-l6max-in-13-14-100.wav");
        let paths = assign_paths(&s, Path::new("/r"), 100, |p| p == taken).unwrap();
        assert_eq!(paths[0], PathBuf::from("/r/zoom-l6max-in-13-14-100-2.wav"));
        assert_eq!(paths[1], PathBuf::from("/r/blackhole-16ch-in-1-2-100.wav"));
        assert_eq!(paths[2], PathBuf::from("/r/zoom-l6max-in-13-14-100-3.wav"));
    }

    #[test]
    fn explicit_paths_are_honoured_and_may_not_be_shared() {
        let s = specs(&["A:in:1=take.wav", "B:in:1=/abs/b.wav"]);
        let paths = assign_paths(&s, Path::new("/r"), 1, |_| false).unwrap();
        assert_eq!(paths, vec![PathBuf::from("/r/take.wav"), PathBuf::from("/abs/b.wav")]);
        let clash = specs(&["A:in:1=x.wav", "B:in:1-2=x.wav"]);
        assert!(matches!(
            assign_paths(&clash, Path::new("/r"), 1, |_| false),
            Err(RecordError::SamePath(_))
        ));
    }

    #[test]
    fn a_length_is_a_whole_number_of_frames_at_the_takes_own_rate() {
        assert_eq!(frame_limit(Some(1.5), 44_100), 66_150);
        assert_eq!(frame_limit(Some(2.0), 48_000), 96_000);
        assert_eq!(frame_limit(None, 48_000), u64::MAX);
        assert_eq!(frame_limit(Some(0.0), 48_000), 0);
    }

    #[test]
    fn no_takes_is_refused_before_anything_opens() {
        let dir = std::env::temp_dir();
        assert!(matches!(Recording::start(&[], &dir, Some(1.0)), Err(RecordError::NoTakes)));
    }

    #[test]
    fn a_permission_refusal_names_the_setting_for_its_route() {
        let e = RecordError::Stream {
            take: "Yeti:in:1".into(),
            route: Route::Input,
            error: simply_audio_device::Error::Cpal(cpal::Error::new(
                cpal::ErrorKind::PermissionDenied,
            )),
        };
        assert!(e.to_string().contains("Privacy & Security → Microphone"));
        assert!(permission_hint(Route::Tap).contains("System Audio Recording"));
    }

    fn info() -> TakeInfo {
        TakeInfo {
            spec: "Yeti:in:1".into(),
            device: "Yeti".into(),
            channels: Channels::Mono(1),
            sample_rate: 48_000,
            path: "/x.wav".into(),
        }
    }

    #[test]
    fn a_clean_take_has_a_length_and_no_warnings() {
        let r = result(info(), Ok(480), &Counters::default());
        assert!((r.duration_s - 0.01).abs() < 1e-9);
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn dropped_audio_is_a_warning_naming_the_take_not_a_count() {
        let counters = Counters::default();
        counters.overflow.store(3, Ordering::Relaxed);
        let r = result(info(), Ok(480), &counters);
        assert_eq!(r.warnings.len(), 1);
        assert!(r.warnings[0].starts_with("Yeti:in:1: audio was dropped"), "{}", r.warnings[0]);
        assert!(!r.warnings[0].contains('3'));
    }

    #[test]
    fn an_empty_or_unfinished_file_is_said_so() {
        let r = result(info(), Ok(0), &Counters::default());
        assert!(r.warnings[0].contains("no audio arrived"));
        let r = result(info(), Err("disk full".into()), &Counters::default());
        assert!(r.warnings[0].contains("disk full"));
    }

    #[test]
    fn an_instant_maps_to_a_frame_through_the_buffer_before_it() {
        // Buffers of 512 frames at 48 kHz, captured every 10.667 ms.
        let per = 512.0 / 48_000.0 * 1e9;
        let points: Vec<(u64, u64)> =
            (0..10).map(|i| (i * 512, (5e8 + i as f64 * per) as u64)).collect();
        assert_eq!(frame_at(&points, 48_000, 499_000_000), None);
        let t = (5e8 + 3.0 * per + 1e6) as u64; // 1 ms into the fourth buffer
        let f = frame_at(&points, 48_000, t).unwrap();
        assert!((f - (3.0 * 512.0 + 48.0)).abs() < 0.01, "{f}");
    }

    /// A windowed take with no device: a pre-roll file whose every sample is
    /// its own frame number, buffer timing points a device would have
    /// reported, and a window in host time. The file must come out starting
    /// on the window start's frame and ending on its end's.
    #[test]
    fn a_take_is_trimmed_to_the_windows_frame() {
        let dir = std::env::temp_dir()
            .join(format!("simply-audio-device-{}", std::process::id()))
            .join("land");
        std::fs::create_dir_all(&dir).unwrap();
        let rate = 48_000u32;
        let final_path = dir.join("take.wav");
        let part = final_path.with_extension("part.wav");
        let total = rate as u64 * 6;
        let mut sink = WavSink::create(&part, 1, rate).unwrap();
        let samples: Vec<f32> = (0..total).map(|f| f as f32).collect();
        sink.write(&samples).unwrap();
        sink.finish().unwrap();
        // 512-frame buffers, the first captured at host time 7 s.
        let per_ns = 512.0 * 1e9 / f64::from(rate);
        let points: Vec<(u64, u64)> =
            (0..total / 512).map(|i| (i * 512, (7e9 + i as f64 * per_ns) as u64)).collect();
        // The start 1.25 s in, plus a third of a frame; four seconds long.
        let start_ns = 7e9 + 1.25e9 + 1e9 / f64::from(rate) / 3.0;
        let w = TakeWindow { start_ns: start_ns as u64, end_ns: (start_ns + 4e9) as u64 };
        let r = land(
            TakeInfo { path: final_path.display().to_string(), ..info() },
            &part,
            Ok(total),
            &Counters::default(),
            &points,
            rate,
            &w,
        );
        assert!(!part.exists(), "the pre-roll file is removed");
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        let sync = r.sync.unwrap();
        assert_eq!(sync.trimmed_frames, 60_000);
        assert!(sync.offset_frames.abs() <= 0.5, "{}", sync.offset_frames);
        let reader = hound::WavReader::open(&final_path).unwrap();
        let got: Vec<f32> = reader.into_samples::<f32>().map(Result::unwrap).collect();
        assert_eq!(got.len(), 4 * rate as usize);
        assert_eq!(got[0], 60_000.0);
        assert!((r.duration_s - 4.0).abs() < 1e-9);
    }

    #[test]
    fn a_take_with_no_capture_times_keeps_nothing_and_says_why() {
        let w = TakeWindow { start_ns: 5, end_ns: 10 };
        let r = land(
            info(),
            Path::new("/nonexistent.part.wav"),
            Ok(0),
            &Counters::default(),
            &[],
            48_000,
            &w,
        );
        assert!(r.warnings.iter().any(|w| w.contains("no capture time")), "{:?}", r.warnings);
    }

    /// Not part of any gate: it opens a real device. Run by hand on a machine
    /// with BlackHole 16ch to prove a windowed take lands on its window:
    /// `cargo test -p audiowatch-record -- --ignored`.
    #[test]
    #[ignore = "opens a real audio device (BlackHole 16ch)"]
    fn a_windowed_take_on_blackhole_is_trimmed_to_its_window() {
        let dir = std::env::temp_dir().join(format!("audiowatch-window-{}", std::process::id()));
        let rec = Recording::start_windowed(&specs(&["BlackHole 16ch:in:1-2"]), &dir).unwrap();
        let armed = loop {
            if let Some(t) = rec.armed_at() {
                break t;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        rec.set_window(TakeWindow { start_ns: armed + 200_000_000, end_ns: armed + 700_000_000 });
        let outcome = rec.wait();
        assert!(outcome.failed.is_none(), "{outcome:?}");
        let take = &outcome.takes[0];
        assert!((take.duration_s - 0.5).abs() < 1e-3, "{take:?}");
        assert!(take.sync.as_ref().unwrap().offset_frames.abs() <= 0.5, "{take:?}");
        assert!(!Path::new(&take.take.path).with_extension("part.wav").exists());
    }

    #[test]
    fn a_windowed_recording_cancelled_before_its_window_keeps_nothing() {
        // No device: the refusal to start is what is left to check without
        // one, and `cancel` needs a running recording. So pin the message a
        // stop before the window gives.
        let mut w = Windowed::new(1);
        w.stopped();
        assert!(w.failed.unwrap().contains("before the take's window was known"));
        let mut w = Windowed::new(1);
        w.window = Some((TakeWindow { start_ns: 1, end_ns: 2 }, Instant::now()));
        w.stopped();
        assert!(w.failed.is_none());
    }
}
