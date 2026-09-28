//! What the machine has, and which of it can be recorded in which direction.
//!
//! # Two directions, and why "out" is not always possible
//!
//! Recording **in** is ordinary: open the device's input side.
//!
//! Recording **out** — what the computer is sending *to* a device — needs the
//! OS to hand back a copy of the output. On macOS 14.2+ cpal does that with a
//! CoreAudio *process tap* wrapped in a private aggregate device, and it
//! builds one **only when the device has no input side at all**
//! (`cpal/src/host/coreaudio/macos/device.rs`: `if self.supports_input() {…}
//! else { loopback }`). For a duplex device — an audio interface, a mixer,
//! BlackHole — asking to record it opens its *input* instead. So:
//!
//! - an output-only device (built-in speakers, a display, headphones) can be
//!   tapped;
//! - a virtual loopback device (BlackHole, Loopback) never needs a tap: what
//!   was sent to it comes straight back on its input, so record it `in`;
//! - a hardware interface's outputs cannot be recorded through cpal at all.
//!   The lever is routing: send the audio to BlackHole or Loopback as well
//!   (a Multi-Output Device, or Loopback's monitoring), and record that.
//!
//! [`route`] is that decision as a pure function over [`DeviceInfo`], so the
//! refusals are tested without a device in the room and say the lever by name.

use std::fmt;

use cpal::traits::{DeviceTrait, HostTrait};

use crate::spec::Channels;
use simply_audio_device::{device_name, Direction};

/// A range of sample rates one configuration supports (often `min == max`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RateRange {
    pub min: u32,
    pub max: u32,
}

/// One side (input or output) of a device.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SideInfo {
    /// Every channel this side has. A take opens the device at this width.
    pub channels: u16,
    /// The rate the device is running at now, which is the rate a recording
    /// uses — asking for another would retune a device other apps share.
    pub current_rate: Option<u32>,
    /// The rates it can run at.
    pub rates: Vec<RateRange>,
    /// Sample formats offered (`f32`, `i16`, …). Recordings are written as
    /// 32-bit float whatever the device's native format.
    pub formats: Vec<String>,
    /// The stereo pairs a take can name, `1-2, 3-4, …`; any single channel
    /// works as mono too.
    pub pairs: Vec<Channels>,
}

/// One device and what can be recorded from it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceInfo {
    pub name: String,
    pub input: Option<SideInfo>,
    pub output: Option<SideInfo>,
    /// `DEVICE:in:…` works.
    pub record_in: bool,
    /// `DEVICE:out:…` works (a tap on an output-only device).
    pub record_out: bool,
    /// Why a direction is refused, and what to do instead.
    pub notes: Vec<String>,
}

/// Whether this Mac can tap an output device at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TapSupport {
    Yes,
    /// Older than macOS 14.2; carries the version it reported.
    TooOld(String),
    /// Not macOS, or the version could not be read: let the backend decide.
    Unknown,
}

impl TapSupport {
    /// Ask the OS. On macOS this reads `sw_vers -productVersion`.
    pub fn detect() -> TapSupport {
        if !cfg!(target_os = "macos") {
            return TapSupport::Unknown;
        }
        let Ok(out) = std::process::Command::new("sw_vers").arg("-productVersion").output() else {
            return TapSupport::Unknown;
        };
        TapSupport::from_version(String::from_utf8_lossy(&out.stdout).trim())
    }

    /// Classify a macOS product version string: taps need 14.2 or later.
    pub fn from_version(version: &str) -> TapSupport {
        let mut parts = version.split('.').map(|p| p.parse::<u32>().ok());
        match (parts.next().flatten(), parts.next().flatten().or(Some(0))) {
            (Some(major), Some(minor)) if (major, minor) >= (14, 2) => TapSupport::Yes,
            (Some(_), Some(_)) => TapSupport::TooOld(version.to_string()),
            _ => TapSupport::Unknown,
        }
    }
}

/// How a take reaches its audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// The device's own input side.
    Input,
    /// A process tap on an output-only device (macOS 14.2+).
    Tap,
}

/// Why a take cannot be recorded as asked — each with the way round it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteRefusal {
    NoInput { device: String },
    NoOutput { device: String },
    NotTappable { device: String },
    TapsUnavailable { device: String, version: String },
    Channels { device: String, direction: Direction, keep: Channels, available: u16 },
}

impl fmt::Display for RouteRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RouteRefusal::NoInput { device } => write!(
                f,
                "{device:?} has no input side, so nothing arrives at it to record. To record \
                 what the computer plays through it, use \"{device}:out:1-2\" (a tap)."
            ),
            RouteRefusal::NoOutput { device } => write!(
                f,
                "{device:?} has no output side, so nothing is sent to it. Record what arrives \
                 at it with \"{device}:in:…\"."
            ),
            RouteRefusal::NotTappable { device } => write!(
                f,
                "{device:?} has an input side, and CoreAudio (through cpal) only taps devices \
                 that have none — asking to record it records its input instead. If it is a \
                 loopback device (BlackHole, Loopback), its input IS what was sent to it: use \
                 \"{device}:in:…\". If it is a hardware interface, route what you want to \
                 capture to BlackHole 16ch or a Loopback device as well (a Multi-Output \
                 Device in Audio MIDI Setup, or Loopback's monitors) and record that one's input."
            ),
            RouteRefusal::TapsUnavailable { device, version } => write!(
                f,
                "recording the output of {device:?} needs a CoreAudio process tap, which needs \
                 macOS 14.2 or later; this Mac reports {version}. Route the audio through \
                 BlackHole or Loopback and record that device's input instead."
            ),
            RouteRefusal::Channels { device, direction, keep, available } => {
                let pairs: Vec<String> =
                    Channels::pairs(*available).iter().map(|p| p.to_string()).collect();
                write!(
                    f,
                    "{device:?} {direction} has {available} channel(s), so {keep} is past the end. \
                     Pairs: {}; any single channel 1–{available} works as mono.",
                    if pairs.is_empty() { "none".to_string() } else { pairs.join(", ") }
                )
            }
        }
    }
}

impl std::error::Error for RouteRefusal {}

/// Decide how `dir` of `info` is recorded, and at which side's width.
pub fn route<'a>(
    info: &'a DeviceInfo,
    dir: Direction,
    keep: Channels,
    taps: &TapSupport,
) -> Result<(Route, &'a SideInfo), RouteRefusal> {
    let device = info.name.clone();
    let (route, side) = match dir {
        Direction::Input => match &info.input {
            Some(side) => (Route::Input, side),
            None => return Err(RouteRefusal::NoInput { device }),
        },
        Direction::Output => match (&info.input, &info.output) {
            (_, None) => return Err(RouteRefusal::NoOutput { device }),
            (Some(_), Some(_)) => return Err(RouteRefusal::NotTappable { device }),
            (None, Some(side)) => {
                if let TapSupport::TooOld(version) = taps {
                    return Err(RouteRefusal::TapsUnavailable { device, version: version.clone() });
                }
                (Route::Tap, side)
            }
        },
    };
    if !keep.fits(side.channels) {
        return Err(RouteRefusal::Channels {
            device,
            direction: dir,
            keep,
            available: side.channels,
        });
    }
    Ok((route, side))
}

/// The config a take opens at: the side's full width, at the rate the device
/// is already running (so nothing else on the machine is retuned), else
/// 48 kHz, else the highest it offers.
pub fn choose_rate(side: &SideInfo) -> Option<u32> {
    let supports = |r: u32| side.rates.iter().any(|x| x.min <= r && r <= x.max);
    side.current_rate
        .filter(|&r| supports(r) || side.rates.is_empty())
        .or_else(|| supports(48_000).then_some(48_000))
        .or_else(|| side.rates.iter().map(|r| r.max).max())
}

fn side_info(device: &cpal::Device, dir: Direction) -> Option<SideInfo> {
    let ranges: Vec<cpal::SupportedStreamConfigRange> = match dir {
        Direction::Input => device.supported_input_configs().ok()?.collect(),
        Direction::Output => device.supported_output_configs().ok()?.collect(),
    };
    let channels = ranges.iter().map(|r| r.channels()).max()?;
    if channels == 0 {
        return None;
    }
    let mut rates: Vec<RateRange> = ranges
        .iter()
        .filter(|r| r.channels() == channels)
        .map(|r| RateRange { min: r.min_sample_rate(), max: r.max_sample_rate() })
        .collect();
    rates.sort_by_key(|r| (r.min, r.max));
    rates.dedup();
    let mut formats: Vec<String> = ranges.iter().map(|r| r.sample_format().to_string()).collect();
    formats.sort();
    formats.dedup();
    let current = match dir {
        Direction::Input => device.default_input_config(),
        Direction::Output => device.default_output_config(),
    };
    Some(SideInfo {
        channels,
        current_rate: current.ok().map(|c| c.sample_rate()),
        rates,
        formats,
        pairs: Channels::pairs(channels),
    })
}

/// Fill `record_in`, `record_out` and `notes` from the two sides.
pub fn describe(
    name: String,
    input: Option<SideInfo>,
    output: Option<SideInfo>,
    taps: &TapSupport,
) -> DeviceInfo {
    let mut info =
        DeviceInfo { name, input, output, record_in: false, record_out: false, notes: Vec::new() };
    info.record_in = info.input.is_some();
    match route(&info, Direction::Output, Channels::Mono(1), taps) {
        Ok(_) => info.record_out = true,
        Err(RouteRefusal::NoOutput { .. }) => {}
        Err(refusal) => info.notes.push(format!("out: {refusal}")),
    }
    info
}

/// Every device on the default host, with both sides described.
pub fn list_devices() -> Vec<DeviceInfo> {
    let taps = TapSupport::detect();
    devices_with_handles(&taps).into_iter().map(|(_, info)| info).collect()
}

pub(crate) fn devices_with_handles(taps: &TapSupport) -> Vec<(cpal::Device, DeviceInfo)> {
    let host = cpal::default_host();
    let Ok(devices) = host.devices() else { return Vec::new() };
    devices
        .filter_map(|d| {
            let name = device_name(&d)?;
            let info = describe(
                name,
                side_info(&d, Direction::Input),
                side_info(&d, Direction::Output),
                taps,
            );
            (info.input.is_some() || info.output.is_some()).then_some((d, info))
        })
        .collect()
}

/// A device name that answers to nothing, or to several things.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickError {
    NotFound { query: String, names: Vec<String> },
    Ambiguous { query: String, matches: Vec<String> },
}

impl fmt::Display for PickError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PickError::NotFound { query, names } => {
                write!(f, "no audio device is called {query:?}. Devices: {}", names.join(", "))
            }
            PickError::Ambiguous { query, matches } => write!(
                f,
                "{query:?} matches several devices ({}); give the full name",
                matches.join(", ")
            ),
        }
    }
}

impl std::error::Error for PickError {}

/// Which of `infos` a take means: an exact name first, else a
/// case-insensitive fragment that names exactly one device. Where one name
/// belongs to two devices (a USB microphone that shows up as an input device
/// and an output device), the one with the side being asked for wins.
pub fn pick(infos: &[DeviceInfo], query: &str, dir: Direction) -> Result<usize, PickError> {
    let has_side = |i: &DeviceInfo| match dir {
        Direction::Input => i.input.is_some(),
        Direction::Output => i.output.is_some(),
    };
    let choose = |hits: Vec<usize>| -> usize {
        hits.iter().copied().find(|&i| has_side(&infos[i])).unwrap_or(hits[0])
    };
    let exact: Vec<usize> = (0..infos.len()).filter(|&i| infos[i].name == query).collect();
    if !exact.is_empty() {
        return Ok(choose(exact));
    }
    let needle = query.to_lowercase();
    let fuzzy: Vec<usize> =
        (0..infos.len()).filter(|&i| infos[i].name.to_lowercase().contains(&needle)).collect();
    let mut names: Vec<String> = fuzzy.iter().map(|&i| infos[i].name.clone()).collect();
    names.dedup();
    match names.len() {
        0 => Err(PickError::NotFound {
            query: query.to_string(),
            names: infos.iter().map(|i| i.name.clone()).collect(),
        }),
        1 => Ok(choose(fuzzy)),
        _ => Err(PickError::Ambiguous { query: query.to_string(), matches: names }),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn side(channels: u16, rate: u32) -> SideInfo {
        SideInfo {
            channels,
            current_rate: Some(rate),
            rates: vec![
                RateRange { min: 44_100, max: 44_100 },
                RateRange { min: 48_000, max: 48_000 },
            ],
            formats: vec!["f32".into()],
            pairs: Channels::pairs(channels),
        }
    }

    /// The shape of Chris's desk, by the numbers `system_profiler` reports.
    pub(crate) fn desk() -> Vec<DeviceInfo> {
        let t = TapSupport::Yes;
        vec![
            describe("ZOOM L6max".into(), Some(side(14, 48_000)), Some(side(4, 48_000)), &t),
            describe("BlackHole 16ch".into(), Some(side(16, 48_000)), Some(side(16, 48_000)), &t),
            describe("Scarlett 4i4 USB".into(), Some(side(6, 48_000)), Some(side(4, 48_000)), &t),
            describe("MacBook Pro Speakers".into(), None, Some(side(2, 48_000)), &t),
            describe("MacBook Pro Microphone".into(), Some(side(1, 48_000)), None, &t),
            describe("Speakers".into(), Some(side(2, 44_100)), Some(side(2, 44_100)), &t),
        ]
    }

    #[test]
    fn a_duplex_mixer_records_in_and_names_the_lever_for_out() {
        let d = &desk()[0];
        assert!(d.record_in && !d.record_out);
        let (r, s) = route(d, Direction::Input, Channels::Pair(13), &TapSupport::Yes).unwrap();
        assert_eq!((r, s.channels), (Route::Input, 14));
        let err = route(d, Direction::Output, Channels::Pair(1), &TapSupport::Yes).unwrap_err();
        assert!(matches!(err, RouteRefusal::NotTappable { .. }));
        assert!(err.to_string().contains("BlackHole"));
        assert!(d.notes[0].starts_with("out: "));
    }

    #[test]
    fn an_output_only_device_is_tapped() {
        let d = &desk()[3];
        assert!(!d.record_in && d.record_out && d.notes.is_empty());
        let (r, _) = route(d, Direction::Output, Channels::Pair(1), &TapSupport::Yes).unwrap();
        assert_eq!(r, Route::Tap);
        let err = route(d, Direction::Input, Channels::Pair(1), &TapSupport::Yes).unwrap_err();
        assert!(err.to_string().contains(":out:"));
    }

    #[test]
    fn a_tap_on_an_old_macos_is_refused_with_the_version() {
        let old = TapSupport::from_version("14.1.2");
        assert_eq!(old, TapSupport::TooOld("14.1.2".into()));
        let d = &desk()[3];
        let err = route(d, Direction::Output, Channels::Pair(1), &old).unwrap_err();
        assert!(err.to_string().contains("14.2") && err.to_string().contains("14.1.2"));
    }

    #[test]
    fn macos_versions_classify() {
        assert_eq!(TapSupport::from_version("14.2"), TapSupport::Yes);
        assert_eq!(TapSupport::from_version("26.0.1"), TapSupport::Yes);
        assert_eq!(TapSupport::from_version("15"), TapSupport::Yes);
        assert!(matches!(TapSupport::from_version("13.6"), TapSupport::TooOld(_)));
        assert_eq!(TapSupport::from_version(""), TapSupport::Unknown);
    }

    #[test]
    fn a_channel_past_the_device_lists_the_pairs_it_has() {
        let d = &desk()[2];
        let err = route(d, Direction::Input, Channels::Pair(7), &TapSupport::Yes).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("6 channel") && text.contains("1-2, 3-4, 5-6"), "{text}");
    }

    #[test]
    fn a_name_matches_exactly_then_by_an_unambiguous_fragment() {
        let d = desk();
        assert_eq!(pick(&d, "Speakers", Direction::Output), Ok(5));
        assert_eq!(pick(&d, "l6max", Direction::Input), Ok(0));
        assert_eq!(pick(&d, "blackhole", Direction::Input), Ok(1));
        assert!(matches!(pick(&d, "MacBook", Direction::Input), Err(PickError::Ambiguous { .. })));
        assert!(matches!(pick(&d, "Moog", Direction::Input), Err(PickError::NotFound { .. })));
    }

    #[test]
    fn one_name_on_two_devices_resolves_to_the_side_asked_for() {
        let t = TapSupport::Yes;
        let d = vec![
            describe("Yeti".into(), Some(side(2, 48_000)), None, &t),
            describe("Yeti".into(), None, Some(side(2, 48_000)), &t),
        ];
        assert_eq!(pick(&d, "Yeti", Direction::Input), Ok(0));
        assert_eq!(pick(&d, "Yeti", Direction::Output), Ok(1));
        assert_eq!(pick(&d, "yet", Direction::Output), Ok(1));
    }

    #[test]
    fn the_rate_is_the_devices_own_when_it_can_be() {
        assert_eq!(choose_rate(&side(2, 44_100)), Some(44_100));
        let mut s = side(2, 96_000); // reports a rate its ranges do not hold
        assert_eq!(choose_rate(&s), Some(48_000));
        s.current_rate = None;
        s.rates = vec![RateRange { min: 44_100, max: 44_100 }];
        assert_eq!(choose_rate(&s), Some(44_100));
    }
}
