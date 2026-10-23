//! Capture any audio device by name, inputs and outputs alike, through one
//! call.
//!
//! The rule ([`resolve`]):
//!
//! * a device with **input channels** is captured as an input, through cpal;
//! * an **output-only** device (speakers, headphones, a display, a
//!   Multi-Output Device) is captured by tapping what the Mac plays on it: a
//!   CoreAudio process tap of every process, mixed to stereo and **not
//!   muted**, inside a private aggregate device this process reads like an
//!   input (macOS 14.2+);
//! * `""` is the current default output, tapped.
//!
//! Either way the device runs at its own rate (nothing on the machine is
//! retuned) and the frames are resampled to the rate asked for, if it
//! differs, before they reach the sink.
//!
//! ```no_run
//! let devices = simply_audio_capture::list_devices();
//! let source = simply_audio_capture::resolve(&devices, "")?; // the default output
//! let capture = simply_audio_capture::start(&source, 48_000, 2, |frames: &[f32], _ns: u64| {
//!     // interleaved stereo at 48 kHz; runs on the audio thread
//! })?;
//! std::thread::sleep(std::time::Duration::from_secs(2));
//! drop(capture); // stops; the tap and its aggregate device are destroyed
//! # Ok::<(), simply_audio_capture::Error>(())
//! ```
//!
//! # Tapping applications
//!
//! [`start_app`] taps **applications** rather than a device, so an app's
//! audio reaches this process without Loopback or BlackHole (macOS 14.2+):
//!
//! * [`Apps::Only`]: these apps (by bundle id, executable name or pid; one
//!   or several), mixed to stereo, wherever they play;
//! * [`Apps::AllExcept`]: every process except these (leave yourself out,
//!   [`App::This`], so your own output doesn't feed back).
//!
//! The capture **follows** the apps. While none of them runs it reports
//! [`AppStatus::Waiting`] ("waiting for djay Pro"); it attaches when one
//! starts, and makes a new tap when one restarts (a new process is a new
//! HAL process object). A tapped app that exits, a clock device that
//! disappears, or a tap that stops calling back is reported in
//! [`AppCapture::status`] and [`AppCapture::error`], never as silence. The
//! tap is clocked by the default output at the moment it is made.
//! [`Mute`] chooses whether the app stays audible while tapped.
//! [`list_apps`] lists what can be tapped, for a picker.
//!
//! ```no_run
//! use simply_audio_capture::{App, AppTap, Mute};
//! let tap = AppTap::only([App::parse("com.algoriddim.djay-iphone-free")]).with_mute(Mute::Unmuted);
//! let capture = simply_audio_capture::start_app(&tap, 48_000, 2, |frames: &[f32], _ns: u64| {
//!     // the same sink as `start`'s: interleaved stereo at 48 kHz, host-clock ns
//! })?;
//! println!("{}", capture.status()); // "waiting for com.algoriddim.djay-iphone-free", or "tapping djay Pro (pid 412) at 48000 Hz"
//! # Ok::<(), simply_audio_capture::Error>(())
//! ```
//!
//! # The sink
//!
//! `sink(frames, capture_ns)` gets interleaved `channels`-wide frames at the
//! requested rate, and the host time (nanoseconds, the
//! `mach_absolute_time` clock on macOS) at which the slice's first frame was
//! captured, or 0 when the device did not say. It runs on the device's
//! real-time thread: it must not allocate, lock, block or log. Copy into a
//! lock-free ring and return.
//!
//! # Cleanup
//!
//! Dropping the [`Capture`] (or [`AppCapture`]) stops it: once the drop returns the sink is never
//! called again, and a tap's aggregate device and tap are destroyed. A
//! private tap and aggregate also belong to the process that made them, so
//! coreaudiod removes them when the process dies without dropping (a
//! signal). A program that wants them gone *before* it exits on a signal
//! drops its capture in its own signal handler.
//!
//! # Permission
//!
//! Taps need *System Audio Recording* consent. Without it the tap is created
//! and delivers silence, so [`permission`] reads the state up front;
//! [`start`] refuses a tap when it is known to be denied.
//!
//! This crate knows nothing of the program that uses it, so it can be copied
//! between programs verbatim.

mod apps;
#[cfg(target_os = "macos")]
mod tap;

pub use apps::{App, AppInfo, AppStatus, AppTap, Apps, Mute};

use std::fmt;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// How to grant the tap permission, for a message a person reads.
pub const GRANT_HINT: &str = "grant it in System Settings ▸ Privacy & Security ▸ \
Screen & System Audio Recording (the \"System Audio Recording Only\" list) to the \
app running this program (the terminal, or whatever launched it), then restart that app";

/// One audio device as [`resolve`] sees it. Devices sharing a name (a USB
/// microphone's input and output halves) are merged: [`resolve`] goes by
/// name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    pub inputs: bool,
    pub outputs: bool,
    pub is_default_output: bool,
}

/// How a device is captured: what [`resolve`] decides and [`start`] does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// Read the device's inputs.
    Input(String),
    /// Tap what is played on this output device (`""` = the default output).
    Tap(String),
}

impl Source {
    /// The device's name.
    pub fn device(&self) -> &str {
        match self {
            Source::Input(n) | Source::Tap(n) => n,
        }
    }
}

/// What went wrong, by what the caller can do about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// No device by that name (the message lists the ones there are).
    NotFound,
    /// macOS refused: Microphone for an input, System Audio Recording for a
    /// tap. Only a person can fix it.
    PermissionDenied,
    /// Not possible here: a tap off macOS or before 14.2.
    Unsupported,
    /// The device or the OS failed.
    Device,
}

#[derive(Clone, Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Error {
        Error { kind, message: message.into() }
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<cpal::Error> for Error {
    fn from(e: cpal::Error) -> Error {
        let kind = match e.kind() {
            cpal::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
            _ => ErrorKind::Device,
        };
        Error::new(kind, format!("audio device error: {e}"))
    }
}

/// The System Audio Recording (TCC) state of this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    Granted,
    Denied,
    /// Never asked (the first tap prompts), not readable, or not macOS.
    Unknown,
}

/// Read (never request or change) whether this process may tap outputs.
pub fn permission() -> Permission {
    #[cfg(target_os = "macos")]
    return tap::permission();
    #[cfg(not(target_os = "macos"))]
    return Permission::Unknown;
}

/// Every audio device, merged by name (see [`Device`]). Off macOS, cpal's
/// inputs only.
pub fn list_devices() -> Vec<Device> {
    let mut found: Vec<Device> = Vec::new();
    #[cfg(target_os = "macos")]
    for d in tap::list_devices().unwrap_or_default() {
        match found.iter_mut().find(|f| f.name == d.name) {
            Some(f) => {
                f.inputs |= d.inputs;
                f.outputs |= d.outputs;
                f.is_default_output |= d.is_default_output;
            }
            None => found.push(Device {
                name: d.name,
                inputs: d.inputs,
                outputs: d.outputs,
                is_default_output: d.is_default_output,
            }),
        }
    }
    #[cfg(not(target_os = "macos"))]
    for d in cpal::default_host().input_devices().into_iter().flatten() {
        if let Some(name) = device_name(&d) {
            found.push(Device { name, inputs: true, outputs: false, is_default_output: false });
        }
    }
    found
}

/// The rule: `""` taps the default output; a named device with inputs is
/// read as an input; an output-only one is tapped. Names match exactly.
pub fn resolve(devices: &[Device], wanted: &str) -> Result<Source, Error> {
    if wanted.is_empty() {
        return match devices.iter().find(|d| d.is_default_output) {
            Some(d) => Ok(Source::Tap(d.name.clone())),
            None => Err(Error::new(ErrorKind::NotFound, "there is no default output device")),
        };
    }
    match devices.iter().find(|d| d.name == wanted) {
        Some(d) if d.inputs => Ok(Source::Input(d.name.clone())),
        Some(d) => Ok(Source::Tap(d.name.clone())),
        None => {
            let names: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
            Err(Error::new(
                ErrorKind::NotFound,
                format!("audio device {wanted:?} not found; available: {}", names.join(", ")),
            ))
        }
    }
}

/// A running capture. Dropping it stops it (see the crate docs).
pub struct Capture {
    source: Source,
    device_rate: u32,
    error: Arc<Mutex<Option<String>>>,
    running: Running,
}

enum Running {
    /// The cpal stream lives on its own thread (a stream is not `Send` on
    /// every host); closing the channel ends it.
    Input { stop: Option<mpsc::Sender<()>>, thread: Option<JoinHandle<()>> },
    #[cfg(target_os = "macos")]
    Tap(#[allow(dead_code)] tap::OutputTap),
}

impl Capture {
    /// What is being captured; a tap of `""` names the device it found.
    pub fn source(&self) -> &Source {
        &self.source
    }

    /// The rate the device runs at, before resampling.
    pub fn device_rate(&self) -> u32 {
        self.device_rate
    }

    /// The first error the stream reported while running (device unplugged,
    /// …), if any. For a tap: the tapped device disappeared, or the tap
    /// stopped delivering audio for two seconds. It stays set.
    pub fn error(&self) -> Option<String> {
        let mut slot = self.error.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(target_os = "macos")]
        if let (None, Running::Tap(t)) = (&*slot, &self.running) {
            *slot = t.health();
        }
        slot.clone()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Running::Input { stop, thread } = &mut self.running {
            drop(stop.take());
            if let Some(t) = thread.take() {
                let _ = t.join();
            }
        }
        // A tap stops and destroys itself when `running` drops.
    }
}

/// Start capturing `source` as `channels`-wide frames at `rate` into `sink`
/// (see the crate docs). Returns once audio is flowing, or with why not.
pub fn start<F>(source: &Source, rate: u32, channels: u16, sink: F) -> Result<Capture, Error>
where
    F: FnMut(&[f32], u64) + Send + 'static,
{
    let channels = channels.max(1);
    let rate = rate.max(1);
    let error = Arc::new(Mutex::new(None));
    match source {
        Source::Input(name) => {
            let (device_rate, stop, thread) =
                start_input(name, rate, channels, Box::new(sink), Arc::clone(&error))?;
            Ok(Capture {
                source: source.clone(),
                device_rate,
                error,
                running: Running::Input { stop: Some(stop), thread: Some(thread) },
            })
        }
        #[cfg(target_os = "macos")]
        Source::Tap(name) => {
            if tap::permission() == Permission::Denied {
                return Err(Error::new(
                    ErrorKind::PermissionDenied,
                    format!("tapping an output needs System Audio Recording permission, which is denied: {GRANT_HINT}"),
                ));
            }
            let t = tap::OutputTap::start(name, rate, channels as usize, Box::new(sink))?;
            Ok(Capture {
                source: Source::Tap(t.device.clone()),
                device_rate: t.sample_rate,
                error,
                running: Running::Tap(t),
            })
        }
        #[cfg(not(target_os = "macos"))]
        Source::Tap(_) => Err(Error::new(
            ErrorKind::Unsupported,
            "tapping an output needs macOS 14.2+; capture an input (a loopback device) instead",
        )),
    }
}

type Sink = Box<dyn FnMut(&[f32], u64) + Send>;

/// Every process the HAL knows plays (or can play) audio, playing ones
/// first: what an app picker shows. [`AppInfo::app`] is what to tap it by.
/// Needs no permission. Empty off macOS.
pub fn list_apps() -> Vec<AppInfo> {
    #[cfg(target_os = "macos")]
    return tap::list_apps();
    #[cfg(not(target_os = "macos"))]
    return Vec::new();
}

/// A running app tap (see [`start_app`]). Dropping it stops it: once the
/// drop returns the sink is never called again, and the tap and its
/// aggregate device are destroyed.
pub struct AppCapture {
    tap: AppTap,
    #[cfg(target_os = "macos")]
    following: tap::Following,
}

impl AppCapture {
    /// What was asked for.
    pub fn tap(&self) -> &AppTap {
        &self.tap
    }

    /// Tapping, waiting for the app, or failed (and retrying). Cheap: read
    /// it as often as a UI repaints.
    pub fn status(&self) -> AppStatus {
        #[cfg(target_os = "macos")]
        return self.following.status();
        #[cfg(not(target_os = "macos"))]
        unreachable!("an AppCapture is only made on macOS")
    }

    /// The problem right now, if there is one: the tapped app exited (and
    /// is waited for), its tap failed, or its clock device disappeared.
    /// None while tapping, and while waiting for an app that was never
    /// there. Unlike [`Capture::error`] it clears when the tap recovers.
    pub fn error(&self) -> Option<String> {
        match self.status() {
            AppStatus::Tapping { .. } | AppStatus::Waiting { reason: None, .. } => None,
            AppStatus::Waiting { reason: Some(r), .. } => Some(r),
            AppStatus::Failed { error } => Some(error),
        }
    }
}

/// Tap applications rather than a device: `tap` names them (or the ones to
/// leave out), and the capture follows them. `channels`-wide frames at
/// `rate` into `sink`, exactly as [`start`]'s (see the crate docs).
///
/// Returns once it has looked: tapping, or waiting for an app that isn't
/// running. Fails when System Audio Recording is known to be denied, when
/// [`Apps::Only`] names nothing, off macOS, or when the first tap could
/// not be made.
pub fn start_app<F>(tap: &AppTap, rate: u32, channels: u16, sink: F) -> Result<AppCapture, Error>
where
    F: FnMut(&[f32], u64) + Send + 'static,
{
    if matches!(&tap.apps, Apps::Only(a) if a.is_empty()) {
        return Err(Error::new(ErrorKind::NotFound, "an app tap needs at least one app"));
    }
    #[cfg(target_os = "macos")]
    {
        if tap::permission() == Permission::Denied {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                format!("tapping an app needs System Audio Recording permission, which is denied: {GRANT_HINT}"),
            ));
        }
        let following = tap::Following::start(
            tap.clone(),
            rate.max(1),
            channels.max(1) as usize,
            Box::new(sink),
        )?;
        Ok(AppCapture { tap: tap.clone(), following })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (rate, channels, sink);
        Err(Error::new(ErrorKind::Unsupported, "tapping an app needs macOS 14.2+"))
    }
}

fn device_name(d: &cpal::Device) -> Option<String> {
    d.description().ok().map(|d| d.name().to_string())
}

fn start_input(
    name: &str,
    rate: u32,
    channels: u16,
    sink: Sink,
    error: Arc<Mutex<Option<String>>>,
) -> Result<(u32, mpsc::Sender<()>, JoinHandle<()>), Error> {
    let wanted = name.to_owned();
    let (ok_tx, ok_rx) = mpsc::channel::<Result<u32, Error>>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("audio capture".into())
        .spawn(move || {
            let build = move || -> Result<(cpal::Stream, u32), Error> {
                let host = cpal::default_host();
                let device = host
                    .input_devices()?
                    .find(|d| device_name(d).as_deref() == Some(wanted.as_str()))
                    .ok_or_else(|| {
                        Error::new(
                            ErrorKind::NotFound,
                            format!("input device {wanted:?} not found"),
                        )
                    })?;
                let device_rate = device.default_input_config()?.sample_rate();
                let mut feed = Feed::new(sink, channels as usize, device_rate, rate);
                let config = cpal::StreamConfig {
                    channels,
                    sample_rate: device_rate,
                    buffer_size: cpal::BufferSize::Default,
                };
                let stream = device.build_input_stream::<f32, _, _>(
                    config,
                    move |data: &[f32], info: &cpal::InputCallbackInfo| {
                        feed.push(data, info.timestamp().capture.as_nanos() as u64)
                    },
                    move |e| {
                        if let Ok(mut slot) = error.lock() {
                            slot.get_or_insert_with(|| e.to_string());
                        }
                    },
                    None,
                )?;
                stream.play()?;
                Ok((stream, device_rate))
            };
            match build() {
                Ok((stream, device_rate)) => {
                    let _ = ok_tx.send(Ok(device_rate));
                    // Until the Capture drops its sender.
                    let _ = stop_rx.recv();
                    drop(stream);
                }
                Err(e) => {
                    let _ = ok_tx.send(Err(e));
                }
            }
        })
        .map_err(|e| {
            Error::new(ErrorKind::Device, format!("cannot start the capture thread: {e}"))
        })?;
    match ok_rx.recv() {
        Ok(Ok(device_rate)) => Ok((device_rate, stop_tx, thread)),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(Error::new(ErrorKind::Device, "the capture thread died before reporting"))
        }
    }
}

/// Output frames handed to the sink per call when resampling.
const CHUNK_FRAMES: usize = 4096;

/// What a device callback pushes into: the sink, behind a resampler when
/// the device runs at another rate. Allocates only in `new`.
pub(crate) struct Feed {
    sink: Sink,
    channels: usize,
    from_hz: f64,
    resampler: Option<Resampler>,
    out: Vec<f32>,
}

impl Feed {
    pub(crate) fn new(sink: Sink, channels: usize, from_hz: u32, to_hz: u32) -> Feed {
        let resampler =
            (from_hz != to_hz).then(|| Resampler::new(from_hz as f64, to_hz as f64, channels));
        let out = if resampler.is_some() { vec![0.0; CHUNK_FRAMES * channels] } else { Vec::new() };
        Feed { sink, channels, from_hz: from_hz as f64, resampler, out }
    }

    /// The sink back, for the next tap (an app tap re-made after a restart).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn into_sink(self) -> Sink {
        self.sink
    }

    /// `frames`: interleaved, `channels` wide, at the device's rate; `ns`:
    /// capture time of the first one (0 = unknown).
    pub(crate) fn push(&mut self, frames: &[f32], ns: u64) {
        let Feed { sink, channels, from_hz, resampler, out } = self;
        let Some(r) = resampler else {
            sink(frames, ns);
            return;
        };
        let ch = *channels;
        let (mut filled, mut chunk_ns) = (0usize, 0u64);
        for (i, frame) in frames.chunks_exact(ch).enumerate() {
            r.feed(frame, |o, pos| {
                if filled == 0 && ns != 0 {
                    // Output frame `pos` lies between input frames i-1 and i.
                    let at = ns as f64 + (i as f64 - 1.0 + pos) * 1e9 / *from_hz;
                    chunk_ns = at.max(1.0) as u64;
                }
                out[filled * ch..(filled + 1) * ch].copy_from_slice(o);
                filled += 1;
                if filled == CHUNK_FRAMES {
                    sink(&out[..], chunk_ns);
                    filled = 0;
                }
            });
        }
        if filled > 0 {
            sink(&out[..filled * ch], chunk_ns);
        }
    }
}

/// Linear-interpolation resampler, fed one frame at a time. Good enough to
/// record and analyse; not a mastering-grade SRC.
pub(crate) struct Resampler {
    /// Input frames per output frame.
    step: f64,
    /// Position of the next output frame past `prev`, in input frames.
    frac: f64,
    prev: Vec<f32>,
    cur: Vec<f32>,
    primed: bool,
}

impl Resampler {
    pub(crate) fn new(from_hz: f64, to_hz: f64, channels: usize) -> Resampler {
        Resampler {
            step: from_hz / to_hz,
            frac: 0.0,
            prev: vec![0.0; channels],
            cur: vec![0.0; channels],
            primed: false,
        }
    }

    /// Take the next input frame; `emit(frame, pos)` every output frame that
    /// falls between the previous input frame and this one (`pos` in 0..1).
    pub(crate) fn feed(&mut self, frame: &[f32], mut emit: impl FnMut(&[f32], f64)) {
        if !self.primed {
            self.prev.copy_from_slice(frame);
            self.primed = true;
            return;
        }
        while self.frac < 1.0 {
            let t = self.frac as f32;
            for ((c, a), b) in self.cur.iter_mut().zip(&self.prev).zip(frame) {
                *c = a + (b - a) * t;
            }
            emit(&self.cur, self.frac);
            self.frac += self.step;
        }
        self.frac -= 1.0;
        self.prev.copy_from_slice(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, inputs: bool, outputs: bool) -> Device {
        Device { name: name.into(), inputs, outputs, is_default_output: false }
    }

    #[test]
    fn a_device_with_inputs_is_captured_and_an_output_only_one_is_tapped() {
        let mut devices = vec![
            dev("BlackHole 2ch", true, true),
            dev("Scarlett 4i4 USB", true, true),
            dev("MacBook Pro Microphone", true, false),
            dev("MacBook Pro Speakers", false, true),
            dev("Multi-Output Device", false, true),
        ];
        devices[3].is_default_output = true;
        let path = |n| resolve(&devices, n).unwrap();
        assert_eq!(path(""), Source::Tap("MacBook Pro Speakers".into()));
        assert_eq!(path("BlackHole 2ch"), Source::Input("BlackHole 2ch".into()));
        assert_eq!(path("Scarlett 4i4 USB"), Source::Input("Scarlett 4i4 USB".into()));
        assert_eq!(path("MacBook Pro Microphone"), Source::Input("MacBook Pro Microphone".into()));
        assert_eq!(path("MacBook Pro Speakers"), Source::Tap("MacBook Pro Speakers".into()));
        assert_eq!(path("Multi-Output Device"), Source::Tap("Multi-Output Device".into()));
        let err = resolve(&devices, "Nope").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(err.to_string().contains("MacBook Pro Speakers"), "{err}");
        devices[3].is_default_output = false;
        assert_eq!(resolve(&devices, "").unwrap_err().kind(), ErrorKind::NotFound);
    }

    /// Everything `feed` hands its sink, at `from` → `to`.
    fn resample(from: u32, to: u32, input: &[f32], channels: usize) -> (Vec<f32>, Vec<u64>) {
        let got = Arc::new(Mutex::new((Vec::new(), Vec::new())));
        let sink = {
            let got = Arc::clone(&got);
            Box::new(move |f: &[f32], ns: u64| {
                let mut g = got.lock().unwrap();
                g.0.extend_from_slice(f);
                g.1.push(ns);
            })
        };
        let mut feed = Feed::new(sink, channels, from, to);
        feed.push(input, 1_000_000_000);
        drop(feed);
        Arc::try_unwrap(got).ok().unwrap().into_inner().unwrap()
    }

    #[test]
    fn the_same_rate_passes_straight_through() {
        let input = [0.1f32, 0.2, 0.3, 0.4];
        let (out, ns) = resample(48_000, 48_000, &input, 2);
        assert_eq!(out, input);
        assert_eq!(ns, vec![1_000_000_000]);
    }

    #[test]
    fn resampling_44k1_to_48k_yields_the_right_frame_count_and_keeps_dc() {
        let input: Vec<f32> = (0..441).flat_map(|_| [0.25f32, -0.5]).collect();
        let (out, _) = resample(44_100, 48_000, &input, 2);
        let frames = out.len() / 2;
        assert!((479..=481).contains(&frames), "{frames} frames");
        assert!(out.chunks(2).all(|f| f == [0.25, -0.5]));
    }

    #[test]
    fn resampling_interpolates_between_frames() {
        let input: Vec<f32> = (0..8).map(|i| i as f32).collect();
        let (out, ns) = resample(24_000, 48_000, &input, 1);
        assert_eq!(&out[..6], &[0.0, 0.5, 1.0, 1.5, 2.0, 2.5]);
        // The first output frame is the first input frame: same instant.
        assert_eq!(ns, vec![1_000_000_000]);
    }

    #[test]
    fn a_long_buffer_reaches_the_sink_in_chunks_with_advancing_times() {
        let input = vec![0.0f32; 48_000];
        let (out, ns) = resample(48_000, 24_000, &input, 1);
        assert_eq!(out.len(), 24_000);
        assert!(ns.len() > 1 && ns.windows(2).all(|w| w[1] > w[0]), "{ns:?}");
        // The second chunk starts CHUNK_FRAMES output frames (= 2× input) later.
        let expect = 1e9 + (2 * CHUNK_FRAMES) as f64 * 1e9 / 48_000.0;
        assert!((ns[1] as f64 - expect).abs() < 1e5, "{} vs {expect}", ns[1]);
    }
}
