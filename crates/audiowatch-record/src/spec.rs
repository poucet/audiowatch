//! What to record, written as text: `DEVICE:DIRECTION:CHANNELS[=PATH]`.
//!
//! One line per take is the whole interface, and it is the same line on the
//! command line and in an MCP call, because an agent that learned it in one
//! place should not have to learn a second spelling in the other. It parses
//! from the **right**: a device name may contain almost anything, but the last
//! two `:` fields are always a direction and a channel selection, so a name
//! like `Loopback: Stream` still reads correctly.

use std::fmt;
use std::path::PathBuf;

use simply_audio_device::Direction;

/// Which channels of a device a take keeps, 1-based as a mixer labels them.
///
/// A take is one mono channel or one adjacent pair — the thing a stereo
/// return, a stereo bus or a single microphone is. Anything wider is several
/// takes, which keeps every file a plain mono or stereo WAV a person can open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// On the wire it is the same text a take spec uses: `"3"` or `"3-4"`.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(into = "String", try_from = "String")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema), schemars(with = "String"))]
pub enum Channels {
    /// One channel, written as a mono file.
    Mono(u16),
    /// Two adjacent channels, `first` and `first + 1`, written as a stereo file.
    Pair(u16),
}

impl Channels {
    /// The 1-based first channel.
    pub fn first(self) -> u16 {
        match self {
            Channels::Mono(c) | Channels::Pair(c) => c,
        }
    }

    /// How many channels the file will hold: 1 or 2.
    pub fn count(self) -> u16 {
        match self {
            Channels::Mono(_) => 1,
            Channels::Pair(_) => 2,
        }
    }

    /// The 1-based last channel.
    pub fn last(self) -> u16 {
        self.first() + self.count() - 1
    }

    /// Whether a device with `available` channels has every channel this
    /// selection names.
    pub fn fits(self, available: u16) -> bool {
        self.last() <= available
    }

    /// Every stereo pair a device with `available` channels offers, starting
    /// on the odd channels as a mixer groups them: `1-2, 3-4, …`. A device
    /// with an odd count has no pair for its last channel; that one is still
    /// reachable as mono.
    pub fn pairs(available: u16) -> Vec<Channels> {
        (1..available).step_by(2).map(Channels::Pair).collect()
    }

    /// Parse `3`, `3-4`, or `L-R` style `13-14`. A pair must be adjacent and
    /// ascending: `4-3` and `1-3` are refused rather than guessed at.
    pub fn parse(text: &str) -> Result<Channels, SpecError> {
        let text = text.trim();
        let channel = |s: &str| -> Result<u16, SpecError> {
            match s.trim().parse::<u16>() {
                Ok(0) | Err(_) => Err(SpecError::Channels(text.to_string())),
                Ok(n) => Ok(n),
            }
        };
        match text.split_once('-') {
            None => Ok(Channels::Mono(channel(text)?)),
            Some((a, b)) => {
                let (a, b) = (channel(a)?, channel(b)?);
                if b == a + 1 {
                    Ok(Channels::Pair(a))
                } else {
                    Err(SpecError::Channels(text.to_string()))
                }
            }
        }
    }
}

impl From<Channels> for String {
    fn from(c: Channels) -> String {
        c.to_string()
    }
}

impl TryFrom<String> for Channels {
    type Error = SpecError;
    fn try_from(s: String) -> Result<Channels, SpecError> {
        Channels::parse(&s)
    }
}

impl fmt::Display for Channels {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Channels::Mono(c) => write!(f, "{c}"),
            Channels::Pair(c) => write!(f, "{c}-{}", c + 1),
        }
    }
}

/// One take, as written: which device, which side of it, which channels, and
/// optionally where the file goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakeSpec {
    /// The device name as given — exact, or an unambiguous fragment of one.
    pub device: String,
    /// `in` records what arrives at the device; `out` records what the
    /// computer sends to it (a tap — see [`crate::catalog::Route`]).
    pub direction: Direction,
    pub channels: Channels,
    /// An explicit file for this take. `None` names one in the output folder.
    pub path: Option<PathBuf>,
}

impl TakeSpec {
    /// Parse `DEVICE:in|out:CHANNELS[=PATH.wav]`.
    pub fn parse(text: &str) -> Result<TakeSpec, SpecError> {
        let text = text.trim();
        // A path is only split off when it names a WAV, so a device called
        // `A=B` still parses.
        let (body, path) = match text.rsplit_once('=') {
            Some((body, path)) if path.to_ascii_lowercase().ends_with(".wav") => {
                (body, Some(PathBuf::from(path.trim())))
            }
            _ => (text, None),
        };
        let mut fields = body.rsplitn(3, ':');
        let (Some(channels), Some(direction), Some(device)) =
            (fields.next(), fields.next(), fields.next())
        else {
            return Err(SpecError::Shape(text.to_string()));
        };
        let device = device.trim();
        if device.is_empty() {
            return Err(SpecError::Shape(text.to_string()));
        }
        let direction = match direction.trim().to_ascii_lowercase().as_str() {
            "in" | "input" => Direction::Input,
            "out" | "output" => Direction::Output,
            other => return Err(SpecError::Direction(other.to_string())),
        };
        Ok(TakeSpec {
            device: device.to_string(),
            direction,
            channels: Channels::parse(channels)?,
            path,
        })
    }

    /// The short direction word the spec uses.
    pub fn direction_word(&self) -> &'static str {
        match self.direction {
            Direction::Input => "in",
            Direction::Output => "out",
        }
    }

    /// A file stem that says what the take is: `zoom-l6max-in-13-14`.
    pub fn stem(&self) -> String {
        format!("{}-{}-{}", slug(&self.device), self.direction_word(), self.channels)
    }
}

impl fmt::Display for TakeSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.device, self.direction_word(), self.channels)?;
        if let Some(path) = &self.path {
            write!(f, "={}", path.display())?;
        }
        Ok(())
    }
}

/// Lowercase ASCII alphanumerics joined by single dashes — safe in a file name
/// on every filesystem, and still readable.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("device");
    }
    out
}

/// A take that does not parse, with the form it should have had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecError {
    Shape(String),
    Direction(String),
    Channels(String),
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpecError::Shape(s) => write!(
                f,
                "{s:?} is not a take; write DEVICE:in|out:CHANNELS[=PATH.wav], \
                 e.g. \"BlackHole 16ch:in:1-2\""
            ),
            SpecError::Direction(d) => {
                write!(f, "direction {d:?} is neither `in` nor `out`")
            }
            SpecError::Channels(c) => write!(
                f,
                "channels {c:?}: write one channel (`3`) or an adjacent pair (`3-4`), \
                 counting from 1"
            ),
        }
    }
}

impl std::error::Error for SpecError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_parse_mono_and_adjacent_pairs_only() {
        assert_eq!(Channels::parse("3"), Ok(Channels::Mono(3)));
        assert_eq!(Channels::parse("13-14"), Ok(Channels::Pair(13)));
        assert_eq!(Channels::parse(" 1 - 2 "), Ok(Channels::Pair(1)));
        for bad in ["0", "0-1", "1-3", "4-3", "x", "", "1-"] {
            assert!(Channels::parse(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn a_selection_knows_whether_it_fits() {
        assert!(Channels::Pair(13).fits(14));
        assert!(!Channels::Pair(13).fits(13));
        assert!(Channels::Mono(13).fits(13));
        assert_eq!(Channels::Pair(3).last(), 4);
        assert_eq!(Channels::Pair(3).to_string(), "3-4");
    }

    #[test]
    fn pairs_start_on_odd_channels_and_skip_a_dangling_last() {
        assert_eq!(
            Channels::pairs(6),
            vec![Channels::Pair(1), Channels::Pair(3), Channels::Pair(5)]
        );
        assert_eq!(Channels::pairs(3), vec![Channels::Pair(1)]);
        assert!(Channels::pairs(1).is_empty());
        assert_eq!(Channels::pairs(14).len(), 7);
    }

    #[test]
    fn a_take_parses_from_the_right_so_a_name_may_hold_colons() {
        let t = TakeSpec::parse("Loopback: Stream:in:1-2").unwrap();
        assert_eq!(t.device, "Loopback: Stream");
        assert_eq!(t.direction, Direction::Input);
        assert_eq!(t.channels, Channels::Pair(1));
        assert_eq!(t.path, None);

        let t = TakeSpec::parse("ZOOM L6max:in:13-14=/tmp/mix.wav").unwrap();
        assert_eq!(t.device, "ZOOM L6max");
        assert_eq!(t.path, Some(PathBuf::from("/tmp/mix.wav")));
        assert_eq!(t.to_string(), "ZOOM L6max:in:13-14=/tmp/mix.wav");

        let t = TakeSpec::parse("MacBook Pro Speakers:OUT:1").unwrap();
        assert_eq!(t.direction, Direction::Output);
        assert_eq!(t.channels, Channels::Mono(1));
    }

    #[test]
    fn a_malformed_take_says_what_shape_it_wanted() {
        assert!(matches!(TakeSpec::parse("BlackHole"), Err(SpecError::Shape(_))));
        assert!(matches!(TakeSpec::parse(":in:1-2"), Err(SpecError::Shape(_))));
        assert!(matches!(TakeSpec::parse("X:sideways:1"), Err(SpecError::Direction(_))));
        assert!(matches!(TakeSpec::parse("X:in:1-3"), Err(SpecError::Channels(_))));
        assert!(SpecError::Shape("x".into()).to_string().contains("DEVICE:in|out:CHANNELS"));
    }

    #[test]
    fn a_stem_is_filesystem_safe() {
        let t = TakeSpec::parse("ZOOM L6max:in:13-14").unwrap();
        assert_eq!(t.stem(), "zoom-l6max-in-13-14");
        assert_eq!(slug("  BlackHole 16ch!! "), "blackhole-16ch");
        assert_eq!(slug("???"), "device");
    }
}
