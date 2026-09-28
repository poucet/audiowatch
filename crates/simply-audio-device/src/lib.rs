//! Thin, app-agnostic helpers over cpal's device layer: enumerate device
//! names, resolve a device by its persisted name (or fall back to the system
//! default), and fetch an f32 stream config. Everything here is written once
//! and takes a [`Direction`], because input and output differ only in which
//! cpal call answers the question.
//!
//! The point of the crate is to keep the cpal 0.18 quirks in one place:
//! human-readable names come from `Device::description().name()` (the 0.15-era
//! `Device::name()` is gone), and enumeration errors are folded into "no
//! devices" because a missing backend and an empty backend look the same to a
//! settings dialog. Stream building stays with the caller — what runs inside
//! the callback is the app's business.
//!
//! # Input is not output backwards
//!
//! An output device sets the rate: the app renders at whatever the device
//! opened at. An **input** device has to be asked for a rate, because it is
//! the second stream on a machine that is already playing at one, and two
//! clocks running at different nominal rates is a pitch shift, not a
//! latency. So [`f32_input_config`] takes the rate it must match and fails
//! when the device cannot be held to it, rather than silently opening at
//! another one.

#![forbid(unsafe_code)]

use cpal::traits::{DeviceTrait, HostTrait};

/// Which half of the device a call is about. The only thing that differs
/// between selecting a microphone and selecting a pair of speakers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Input,
    Output,
}

impl Direction {
    /// The word that goes in a message a person reads.
    pub fn label(self) -> &'static str {
        match self {
            Direction::Input => "input",
            Direction::Output => "output",
        }
    }
}

impl std::fmt::Display for Direction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Errors from device selection/configuration. `DeviceNotFound` is the one
/// callers usually want to catch: the persisted device went away, so fall
/// back to `select(dir, None)` or surface it to the user.
#[derive(Debug)]
pub enum Error {
    /// The host reports no default device on this side.
    NoDefaultDevice(Direction),
    /// No device on this side with the requested name exists right now.
    DeviceNotFound(Direction, String),
    /// The device offers no f32 configuration at all.
    UnsupportedSampleFormat(cpal::SampleFormat),
    /// The device has f32 configurations, but none that reaches this rate —
    /// what a microphone stuck at 44.1 kHz says to a 48 kHz output.
    UnsupportedSampleRate(u32),
    /// Underlying cpal failure (enumeration, config query, …).
    Cpal(cpal::Error),
}

impl Error {
    /// The OS refused access to a device that exists and works — on macOS,
    /// microphone permission not (yet) granted. Worth distinguishing from
    /// every other failure because it is the only one the *user* can fix,
    /// and the fix is in a settings panel rather than in the app.
    pub fn is_permission_denied(&self) -> bool {
        matches!(self, Error::Cpal(e) if e.kind() == cpal::ErrorKind::PermissionDenied)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoDefaultDevice(dir) => write!(f, "no default audio {dir} device"),
            Error::DeviceNotFound(dir, name) => write!(f, "{dir} device {name:?} not found"),
            Error::UnsupportedSampleFormat(format) => {
                write!(f, "unsupported sample format {format:?}")
            }
            Error::UnsupportedSampleRate(rate) => {
                write!(f, "the device cannot run at {rate} Hz")
            }
            Error::Cpal(e) => write!(f, "audio device error: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Cpal(e) => Some(e),
            _ => None,
        }
    }
}

impl From<cpal::Error> for Error {
    fn from(e: cpal::Error) -> Self {
        Error::Cpal(e)
    }
}

/// A device resolved by [`select`], with its human-readable name (what
/// [`device_names`] lists, suitable for persisting).
pub struct SelectedDevice {
    pub device: cpal::Device,
    pub name: String,
}

/// The name [`select`] hands back for an output device.
pub type SelectedOutput = SelectedDevice;

/// Human-readable name of a device (cpal 0.18: `description().name()`).
pub fn device_name(device: &cpal::Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_string())
}

/// Every device on this side of the default host. Enumeration failure or an
/// absent backend yields an empty list — settings UIs treat both the same.
fn devices(direction: Direction) -> Vec<cpal::Device> {
    let host = cpal::default_host();
    let listed = match direction {
        Direction::Input => host.input_devices().map(Iterator::collect),
        Direction::Output => host.output_devices().map(Iterator::collect),
    };
    listed.unwrap_or_default()
}

/// Names of every device on this side, in host order.
pub fn device_names(direction: Direction) -> Vec<String> {
    devices(direction).iter().filter_map(device_name).collect()
}

/// Names of every output device on the default host.
pub fn output_device_names() -> Vec<String> {
    device_names(Direction::Output)
}

/// Names of every input device on the default host.
pub fn input_device_names() -> Vec<String> {
    device_names(Direction::Input)
}

/// Resolve a device on the default host: by name when `preferred` is `Some`
/// (an exact match against [`device_names`] entries, erring with
/// [`Error::DeviceNotFound`] when absent — the caller decides whether to
/// fall back), or the system default when `None`.
pub fn select(direction: Direction, preferred: Option<&str>) -> Result<SelectedDevice, Error> {
    let device = match preferred {
        Some(name) => devices(direction)
            .into_iter()
            .find(|d| device_name(d).is_some_and(|n| n == name))
            .ok_or_else(|| Error::DeviceNotFound(direction, name.to_string()))?,
        None => {
            let host = cpal::default_host();
            match direction {
                Direction::Input => host.default_input_device(),
                Direction::Output => host.default_output_device(),
            }
            .ok_or(Error::NoDefaultDevice(direction))?
        }
    };
    let name = device_name(&device).unwrap_or_else(|| direction.label().to_string());
    Ok(SelectedDevice { device, name })
}

/// [`select`] on the output side.
pub fn select_output(preferred: Option<&str>) -> Result<SelectedDevice, Error> {
    select(Direction::Output, preferred)
}

/// [`select`] on the input side.
pub fn select_input(preferred: Option<&str>) -> Result<SelectedDevice, Error> {
    select(Direction::Input, preferred)
}

/// The device's default output config, checked to be `f32` samples — the
/// format both flux and nexus render in. Errs with
/// [`Error::UnsupportedSampleFormat`] otherwise. The device's own default
/// rate is taken as given: on the output side, the device sets the rate.
pub fn default_f32_output_config(
    device: &cpal::Device,
) -> Result<cpal::SupportedStreamConfig, Error> {
    let config = device.default_output_config()?;
    if config.sample_format() != cpal::SampleFormat::F32 {
        return Err(Error::UnsupportedSampleFormat(config.sample_format()));
    }
    Ok(config)
}

/// An f32 **input** config running at exactly `sample_rate` — the rate the
/// output stream already opened at, since the two feed one plan and a
/// mismatch is a pitch shift (see the module docs).
///
/// The device's own default config is preferred when it already matches, so
/// the buffer-size hint the backend advertises is kept; otherwise the
/// supported ranges are scanned for one that can be held to the rate.
pub fn f32_input_config(
    device: &cpal::Device,
    sample_rate: u32,
) -> Result<cpal::SupportedStreamConfig, Error> {
    let default = device.default_input_config();
    if let Ok(config) = &default {
        if config.sample_format() == cpal::SampleFormat::F32 && config.sample_rate() == sample_rate
        {
            return Ok(*config);
        }
    }
    let mut any_f32 = false;
    for range in device.supported_input_configs()? {
        if range.sample_format() != cpal::SampleFormat::F32 {
            continue;
        }
        any_f32 = true;
        if let Some(config) = range.try_with_sample_rate(sample_rate) {
            return Ok(config);
        }
    }
    Err(if any_f32 {
        Error::UnsupportedSampleRate(sample_rate)
    } else {
        // Nothing f32 anywhere: name the format the device does offer, which
        // is the only useful thing to say about it.
        Error::UnsupportedSampleFormat(
            default.map(|c| c.sample_format()).unwrap_or(cpal::SampleFormat::I16),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: [Direction; 2] = [Direction::Input, Direction::Output];

    /// Enumeration must never panic, on either side; it may legitimately be
    /// empty (CI).
    #[test]
    fn enumeration_is_infallible() {
        for direction in BOTH {
            let _ = device_names(direction);
        }
        assert_eq!(output_device_names(), device_names(Direction::Output));
        assert_eq!(input_device_names(), device_names(Direction::Input));
    }

    /// A name nothing answers to is `DeviceNotFound` on **both** sides, and
    /// the error says which side it looked on — the whole reason the
    /// direction is carried in the variant.
    #[test]
    fn unknown_name_is_device_not_found_on_either_side() {
        for direction in BOTH {
            match select(direction, Some("simply-audio-device-test-nonexistent")) {
                Err(Error::DeviceNotFound(dir, name)) => {
                    assert_eq!(dir, direction);
                    assert_eq!(name, "simply-audio-device-test-nonexistent");
                }
                Err(other) => panic!("expected DeviceNotFound, got {other}"),
                Ok(_) => panic!("nonexistent device resolved"),
            }
        }
    }

    /// On a machine with audio: the default device resolves, its name appears
    /// in the enumeration, and selecting it by that name round-trips — on
    /// both sides. Headless CI (no devices) skips.
    #[test]
    fn default_devices_round_trip_by_name() {
        for direction in BOTH {
            let Ok(default) = select(direction, None) else { continue };
            assert!(!default.name.is_empty());
            assert!(
                device_names(direction).contains(&default.name),
                "default {direction} {:?} missing from enumeration",
                default.name
            );
            let by_name = select(direction, Some(&default.name)).expect("reselect by name");
            assert_eq!(by_name.name, default.name);
        }
    }

    /// The input config is asked for a **rate**, and a rate no device can run
    /// at is refused rather than silently substituted. 1 Hz is chosen because
    /// no audio hardware offers it, so this asserts the same thing on every
    /// machine. Headless CI (no input device) skips.
    #[test]
    fn an_input_device_that_cannot_reach_the_rate_is_refused() {
        let Ok(input) = select_input(None) else { return };
        match f32_input_config(&input.device, 1) {
            Err(Error::UnsupportedSampleRate(1) | Error::UnsupportedSampleFormat(_)) => {}
            Err(other) => panic!("expected a rate refusal, got {other}"),
            Ok(config) => panic!("1 Hz accepted: {config:?}"),
        }
    }

    /// …and the rate the device is already running at is honoured, with the
    /// config that comes back actually carrying it. Headless CI skips.
    #[test]
    fn an_input_device_opens_at_its_own_rate() {
        let Ok(input) = select_input(None) else { return };
        let Ok(default) = input.device.default_input_config() else { return };
        let Ok(config) = f32_input_config(&input.device, default.sample_rate()) else { return };
        assert_eq!(config.sample_rate(), default.sample_rate());
        assert_eq!(config.sample_format(), cpal::SampleFormat::F32);
    }

    #[test]
    fn errors_display_usefully() {
        assert_eq!(
            Error::NoDefaultDevice(Direction::Output).to_string(),
            "no default audio output device"
        );
        assert_eq!(
            Error::DeviceNotFound(Direction::Input, "X".into()).to_string(),
            "input device \"X\" not found"
        );
        assert_eq!(
            Error::UnsupportedSampleRate(48_000).to_string(),
            "the device cannot run at 48000 Hz"
        );
    }

    /// The one failure a person can act on is distinguishable from the ones
    /// they cannot.
    #[test]
    fn a_permission_refusal_is_recognisable() {
        let denied = Error::from(cpal::Error::new(cpal::ErrorKind::PermissionDenied));
        assert!(denied.is_permission_denied());
        assert!(!Error::NoDefaultDevice(Direction::Input).is_permission_denied());
        assert!(!Error::from(cpal::Error::new(cpal::ErrorKind::DeviceBusy)).is_permission_denied());
    }
}
