//! `audiowatch rec` — the command-line face of the recorder, for an agent
//! working through a shell (a skill) rather than MCP, and for a person.
//! Non-interactive: a recording always has a length.
//!
//! Every word of logic is in the libraries; this file parses arguments and
//! prints.

use std::path::PathBuf;
use std::process::ExitCode;
#[cfg(feature = "midi-clock")]
use std::time::Duration;

#[cfg(feature = "midi-clock")]
use audiowatch_clock::{BarOptions, BarPlan, ClockPort, ClockReport};
use audiowatch_record::{self as record, DeviceInfo, SideInfo, TakeResult};

const USAGE: &str = "\
audiowatch rec — record channels of any audio device to 32-bit float WAV

USAGE
  audiowatch rec list [--json]
  audiowatch rec clocks [--json]
  audiowatch rec record --seconds N [--dir DIR] [--json] TAKE [TAKE…]
  audiowatch rec record --bars N [--beats-per-bar B] [--clock PORT]
                        [--arm-timeout S] [--dir DIR] [--json] TAKE [TAKE…]

TAKE is DEVICE:in|out:CHANNELS[=PATH.wav]
  DEVICE    the exact device name from `list`, or an unambiguous part of it
  in        what arrives at the device (its input side)
  out       what the computer sends to it — only for devices with no input
            side (speakers, displays, headphones), via a CoreAudio tap on
            macOS 14.2+. A duplex device (an interface, a mixer, BlackHole)
            cannot be tapped: for BlackHole/Loopback record `in` (its input
            IS what was sent to it); for a hardware interface, route the audio
            through BlackHole or Loopback as well and record that.
  CHANNELS  one channel (`3`, a mono file) or an adjacent pair (`3-4`,
            stereo), counting from 1
  PATH      where to write it (relative to --dir); otherwise a name like
            <device>-<in|out>-<channels>-<unix-seconds>.wav in --dir

Several takes record at once, each its own stream and file at its own
device's rate; nothing is resampled or mixed. --dir defaults to
~/Music/audio-rec. Each file is finished and closed before its path is
printed; the audio itself is never printed.

ON A MIDI CLOCK
  --bars N starts every take on the next downbeat of a MIDI clock and ends
  it N bars later, counted in clock ticks (a tempo change still ends on the
  bar). `clocks` lists the MIDI inputs and which are sending clock; without
  --clock the one input that is ticking is followed (several, or none, is
  refused). MIDI clock has no bar: the bar is counted from the clock's Start
  (or Song Position + Continue), so a clock that was already running when
  you armed waits for the next Start — arm, then press play (or stop and
  play) on the master. --beats-per-bar defaults to 4; --arm-timeout (default
  60) is how long to wait for that downbeat. macOS only. Nothing is ever
  sent to any MIDI port. A build without the midi-clock feature records by
  seconds only.

EXAMPLES
  audiowatch rec record --seconds 10 \"ZOOM L6max:in:13-14\" \"BlackHole 16ch:in:1-2\"
  audiowatch rec record --seconds 5 --json \"MacBook Pro Speakers:out:1-2=speakers.wav\"
  audiowatch rec record --bars 4 \"ZOOM L6max:in:13-14\"

PERMISSIONS
  Inputs need microphone access, taps need system audio recording access,
  both granted to the app running this command (System Settings → Privacy &
  Security). macOS may answer a missing permission with silence rather than
  an error, so check a first take with an analysis tool.";

enum Length {
    Seconds(f64),
    #[cfg_attr(not(feature = "midi-clock"), allow(dead_code))]
    Bars {
        bars: u32,
        beats_per_bar: u32,
        clock: Option<String>,
        arm_timeout: f64,
    },
}

enum Command {
    List {
        json: bool,
    },
    #[cfg_attr(not(feature = "midi-clock"), allow(dead_code))]
    Clocks {
        json: bool,
    },
    Record {
        length: Length,
        dir: PathBuf,
        json: bool,
        takes: Vec<String>,
    },
}

fn positive_int(flag: &str, v: &str) -> Result<u32, String> {
    match v.parse::<u32>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!("{flag} must be a whole number above 0, not {v:?}")),
    }
}

fn parse(args: &[String]) -> Result<Command, String> {
    let mut it = args.iter();
    let command = it.next().map(String::as_str);
    let mut json = false;
    let mut seconds = None;
    let (mut bars, mut beats_per_bar, mut clock, mut arm_timeout) = (None, 4, None, 60.0);
    let mut dir = record::default_dir();
    let mut takes = Vec::new();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--seconds" | "-s" => {
                let v = it.next().ok_or("--seconds needs a value")?;
                let s: f64 = v
                    .parse()
                    .map_err(|_| format!("--seconds {v:?} is not a number"))?;
                if !(s > 0.0 && s.is_finite()) {
                    return Err(format!("--seconds must be positive, not {v}"));
                }
                seconds = Some(s);
            }
            "--bars" | "-b" => {
                bars = Some(positive_int(
                    "--bars",
                    it.next().ok_or("--bars needs a value")?,
                )?)
            }
            "--beats-per-bar" => {
                let v = it.next().ok_or("--beats-per-bar needs a value")?;
                beats_per_bar = positive_int("--beats-per-bar", v)?;
            }
            "--clock" => clock = Some(it.next().ok_or("--clock needs a MIDI input name")?.clone()),
            "--arm-timeout" => {
                let v = it.next().ok_or("--arm-timeout needs a value")?;
                arm_timeout = v
                    .parse::<f64>()
                    .ok()
                    .filter(|s| *s > 0.0 && s.is_finite())
                    .ok_or(format!("--arm-timeout must be positive seconds, not {v:?}"))?;
            }
            "--dir" | "-d" => dir = PathBuf::from(it.next().ok_or("--dir needs a value")?),
            "--take" | "-t" => takes.push(it.next().ok_or("--take needs a value")?.clone()),
            "--help" | "-h" => return Err(USAGE.into()),
            flag if flag.starts_with("--") => {
                return Err(format!("unknown option {flag}\n\n{USAGE}"))
            }
            take => takes.push(take.to_string()),
        }
    }
    match command {
        Some("list") if takes.is_empty() => Ok(Command::List { json }),
        Some("clocks") if takes.is_empty() => Ok(Command::Clocks { json }),
        Some("record") => {
            let length = match (seconds, bars) {
                (Some(s), None) => Length::Seconds(s),
                (None, Some(bars)) => Length::Bars {
                    bars,
                    beats_per_bar,
                    clock,
                    arm_timeout,
                },
                (Some(_), Some(_)) => return Err("give --seconds or --bars, not both".into()),
                (None, None) => {
                    return Err(
                        "record needs --seconds N or --bars N: a recording here always \
                                has a length"
                            .into(),
                    )
                }
            };
            if takes.is_empty() {
                return Err(
                    "record needs at least one TAKE, e.g. \"BlackHole 16ch:in:1-2\"".into(),
                );
            }
            Ok(Command::Record {
                length,
                dir,
                json,
                takes,
            })
        }
        _ => Err(USAGE.into()),
    }
}

fn side(label: &str, s: &Option<SideInfo>) -> String {
    match s {
        None => format!("{label}: —"),
        Some(s) => {
            let rate = s
                .current_rate
                .map(|r| format!(" @ {r} Hz"))
                .unwrap_or_default();
            let pairs: Vec<String> = s.pairs.iter().map(|p| p.to_string()).collect();
            format!(
                "{label}: {} ch{rate} [{}] pairs {}",
                s.channels,
                s.formats.join("/"),
                if pairs.is_empty() {
                    "none".into()
                } else {
                    pairs.join(" ")
                }
            )
        }
    }
}

fn print_devices(devices: &[DeviceInfo]) {
    for d in devices {
        let can = |ok: bool| if ok { "yes" } else { "no" };
        println!("{}", d.name);
        println!("    {}", side("in ", &d.input));
        println!("    {}", side("out", &d.output));
        println!(
            "    record in: {}   record out: {}",
            can(d.record_in),
            can(d.record_out)
        );
        for note in &d.notes {
            println!("    note: {note}");
        }
    }
}

fn print_results(results: &[TakeResult]) {
    for r in results {
        println!("{}", r.take.path);
        println!(
            "    {}  {} Hz  {:.3} s",
            r.take.spec, r.take.sample_rate, r.duration_s
        );
        if let Some(s) = &r.sync {
            println!(
                "    downbeat: {} frames trimmed, first frame {:+.3} frames from it",
                s.trimmed_frames, s.offset_frames
            );
        }
        for w in &r.warnings {
            println!("    warning: {w}");
        }
    }
}

#[cfg(feature = "midi-clock")]
fn print_clocks(ports: &[ClockPort]) {
    if ports.is_empty() {
        println!("no MIDI inputs");
    }
    for p in ports {
        let clock = match (p.is_clock(), p.bpm) {
            (true, Some(bpm)) => format!("clock, {bpm:.1} bpm"),
            (true, None) => "clock".into(),
            (false, _) => "no clock".into(),
        };
        println!("{}    {clock} ({} ticks)", p.name, p.ticks);
    }
}

#[cfg(feature = "midi-clock")]
fn print_clock(c: &ClockReport) {
    println!(
        "clock {:?}: {:.2} of {} bars of {} from bar {}, {:.2} bpm (beats {:.2}–{:.2}), after \
         {:.3} s",
        c.port,
        c.bars,
        c.bars_asked,
        c.beats_per_bar,
        c.start_bar,
        c.tempo_bpm,
        c.tempo_min_bpm,
        c.tempo_max_bpm,
        c.waited_s
    );
}

fn run(command: Command) -> Result<(), String> {
    match command {
        Command::List { json } => {
            let devices = record::list_devices();
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&devices).map_err(|e| e.to_string())?
                );
            } else {
                print_devices(&devices);
            }
        }
        #[cfg(not(feature = "midi-clock"))]
        Command::Clocks { .. }
        | Command::Record {
            length: Length::Bars { .. },
            ..
        } => {
            return Err(NO_CLOCK.into());
        }
        #[cfg(feature = "midi-clock")]
        Command::Clocks { json } => {
            let ports = audiowatch_clock::port::scan().map_err(|e| e.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&ports).map_err(|e| e.to_string())?
                );
            } else {
                print_clocks(&ports);
            }
        }
        Command::Record {
            length: Length::Seconds(seconds),
            dir,
            json,
            takes,
        } => {
            let specs = record::parse_takes(&takes).map_err(|e| e.to_string())?;
            let dir = std::path::absolute(&dir).unwrap_or(dir);
            if !json {
                eprintln!(
                    "recording {} take(s) for {seconds} s into {}",
                    specs.len(),
                    dir.display()
                );
            }
            let results = record::record(&specs, &dir, seconds).map_err(|e| e.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&results).map_err(|e| e.to_string())?
                );
            } else {
                print_results(&results);
            }
        }
        #[cfg(feature = "midi-clock")]
        Command::Record {
            length:
                Length::Bars {
                    bars,
                    beats_per_bar,
                    clock,
                    arm_timeout,
                },
            dir,
            json,
            takes,
        } => {
            let specs = record::parse_takes(&takes).map_err(|e| e.to_string())?;
            let dir = std::path::absolute(&dir).unwrap_or(dir);
            if !json {
                eprintln!(
                    "arming {} take(s) for {bars} bar(s) of {beats_per_bar} on the next downbeat \
                     (waiting up to {arm_timeout} s) into {}",
                    specs.len(),
                    dir.display()
                );
            }
            let options = BarOptions {
                plan: BarPlan {
                    beats_per_bar,
                    bars,
                    max_take_ns: None,
                },
                arm_timeout: Duration::from_secs_f64(arm_timeout),
            };
            let done = audiowatch_clock::record_bars(&specs, &dir, clock.as_deref(), options)?;
            let outcome = done.outcome;
            if json {
                let value = serde_json::json!({ "takes": outcome.takes, "clock": done.clock });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
                );
            } else {
                print_results(&outcome.takes);
                if let Some(c) = &done.clock {
                    print_clock(c);
                }
            }
        }
    }
    Ok(())
}

/// What `clocks` and `--bars` answer from a build without the MIDI clock.
#[cfg(not(feature = "midi-clock"))]
const NO_CLOCK: &str = "this audiowatch was built without the midi-clock feature, so it records \
                        by seconds only";

/// `audiowatch rec ARGS…`, the arguments after `rec`.
pub fn main(args: &[String]) -> ExitCode {
    match parse(args).and_then(run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}
