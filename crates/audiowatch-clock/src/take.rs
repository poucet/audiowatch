//! A take of N bars: a [`Recording`] started windowed, fed its window by a
//! MIDI clock. The recorder knows nothing about the clock and the clock
//! nothing about audio; this is where the two meet.
//!
//! The recorder starts every take at once into a pre-roll and says from when
//! all of them have audio ([`Recording::armed_at`]). The clock follower is
//! told that instant, so the first downbeat it picks is one every take
//! heard; once the last downbeat has passed (and the ticks half a beat after
//! it, for the fit), the downbeats become a [`TakeWindow`] and the recorder
//! trims every file to it, to the frame.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use audiowatch_record::{Outcome, RecordError, Recording, TakeInfo, TakeSpec, TakeWindow};

use crate::port::{self, ClockInput};
use crate::reading::{BarPlan, BarTake};

/// How often the follower reads the clock.
const POLL: Duration = Duration::from_millis(20);

/// A bar-quantised take: how many bars, how the clock counts them, and how
/// long to wait for a downbeat.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarOptions {
    pub plan: BarPlan,
    /// Give up (recording nothing) if the first downbeat has not come by
    /// then — the clock never started, or never said where the bar is.
    pub arm_timeout: Duration,
}

/// What a bar-quantised recording followed, and what it measured.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClockReport {
    /// The MIDI input whose clock was followed.
    pub port: String,
    pub beats_per_bar: u32,
    /// The bars asked for.
    pub bars_asked: u32,
    /// The bars recorded: `bars_asked`, or fewer if the clock stopped,
    /// jumped or went silent (a warning says which).
    pub bars: f64,
    /// Which bar of the song the take started on, counting from 1 at the
    /// clock's `Start`.
    pub start_bar: u64,
    /// Mean tempo over the take, measured from the ticks.
    pub tempo_bpm: f64,
    /// Slowest and fastest beat within it.
    pub tempo_min_bpm: f64,
    pub tempo_max_bpm: f64,
    /// Seconds between the audio starting and the first downbeat.
    pub waited_s: f64,
}

/// Everything a finished bar-quantised recording says: the recorder's
/// outcome (its `failed` set when nothing was kept), and what the clock did.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BarOutcome {
    pub outcome: Outcome,
    pub clock: Option<ClockReport>,
}

/// A running bar-quantised recording. Dropping it waits for it.
pub struct BarRecording {
    takes: Vec<TakeInfo>,
    finished: Arc<AtomicBool>,
    thread: Option<JoinHandle<BarOutcome>>,
}

impl BarRecording {
    /// Open every take now and follow `clock` to the next downbeat and the
    /// one `options.plan.bars` bars later — counted in clock ticks, so a
    /// tempo change still ends on the bar.
    pub fn start(
        specs: &[TakeSpec],
        dir: &Path,
        clock: ClockInput,
        options: BarOptions,
    ) -> Result<BarRecording, RecordError> {
        let recording = Recording::start_windowed(specs, dir)?;
        let takes = recording.takes().to_vec();
        let finished = Arc::new(AtomicBool::new(false));
        let thread = {
            let finished = finished.clone();
            std::thread::Builder::new()
                .name("audiowatch clock".into())
                .spawn(move || {
                    let outcome = follow(recording, clock, options);
                    finished.store(true, Ordering::Release);
                    outcome
                })
                .map_err(|_| RecordError::Thread)?
        };
        Ok(BarRecording { takes, finished, thread: Some(thread) })
    }

    pub fn takes(&self) -> &[TakeInfo] {
        &self.takes
    }

    /// Has the take ended and its files been closed?
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// Block until the last downbeat (or the reason there is none), and
    /// return the files and what the clock did.
    pub fn wait(mut self) -> BarOutcome {
        self.thread.take().and_then(|t| t.join().ok()).unwrap_or_default()
    }
}

impl Drop for BarRecording {
    fn drop(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Record `takes` from the next downbeat of a MIDI clock for
/// `options.plan.bars` bars, and return the files and what the clock did.
/// Blocks until the last downbeat. `clock` names the MIDI input to follow;
/// `None` finds the one input that is sending clock (see [`port::listen`]).
pub fn record_bars(
    takes: &[TakeSpec],
    dir: &Path,
    clock: Option<&str>,
    options: BarOptions,
) -> Result<BarOutcome, String> {
    let input = port::listen(clock).map_err(|e| e.to_string())?;
    let done = BarRecording::start(takes, dir, input, options).map_err(|e| e.to_string())?.wait();
    match &done.outcome.failed {
        Some(why) => Err(why.clone()),
        None => Ok(done),
    }
}

/// The follower: reads the clock, tells the recorder its window, and
/// finishes the recording.
fn follow(recording: Recording, mut input: ClockInput, options: BarOptions) -> BarOutcome {
    let mut take = BarTake::new(options.plan);
    let armed = Instant::now();
    // The latest clock message's host time, and when it was read — so the
    // host clock can be read between messages.
    let mut last_msg: Option<(u64, Instant)> = None;
    let mut not_before: Option<u64> = None;
    loop {
        std::thread::sleep(POLL);
        for (t, msg) in input.drain() {
            take.feed(t, msg);
            last_msg = Some((t, Instant::now()));
        }
        if not_before.is_none() {
            if let Some(first) = recording.armed_at() {
                // The first downbeat must fall where every take has audio.
                not_before = Some(first);
                take.set_not_before(first);
            }
        }
        if let Some((t, at)) = last_msg {
            take.poll(t + at.elapsed().as_nanos() as u64);
        }
        if !take.started() && !take.done() {
            if armed.elapsed() > options.arm_timeout {
                let why = why_no_downbeat(&options, &input.port, not_before, &take, last_msg);
                return BarOutcome { outcome: recording.cancel(why), clock: None };
            }
            continue;
        }
        if take.done() {
            break;
        }
    }
    let Some(w) = take.window() else {
        return BarOutcome { outcome: recording.cancel("the take never started"), clock: None };
    };
    recording.set_window(TakeWindow { start_ns: w.start_ns, end_ns: w.end_ns });
    let mut outcome = recording.wait();
    if outcome.failed.is_some() {
        return BarOutcome { outcome, clock: None };
    }
    let plan = options.plan;
    if let Some(early) = w.warning(&plan) {
        for r in &mut outcome.takes {
            r.warnings.push(format!("{}: {early}", r.take.spec));
        }
    }
    let bar = plan.bar_ticks();
    let clock = ClockReport {
        port: input.port.clone(),
        beats_per_bar: plan.beats_per_bar,
        bars_asked: plan.bars,
        bars: w.bars,
        start_bar: w.start_pos / bar + 1,
        tempo_bpm: w.bpm,
        tempo_min_bpm: w.bpm_min,
        tempo_max_bpm: w.bpm_max,
        waited_s: w.start_ns.saturating_sub(not_before.unwrap_or(w.start_ns)) as f64 / 1e9,
    };
    BarOutcome { outcome, clock: Some(clock) }
}

/// Why no downbeat came within the arm timeout, as the person can act on it.
fn why_no_downbeat(
    options: &BarOptions,
    port: &str,
    not_before: Option<u64>,
    take: &BarTake,
    last_msg: Option<(u64, Instant)>,
) -> String {
    let secs = options.arm_timeout.as_secs_f64();
    if not_before.is_none() {
        format!("no audio arrived within {secs:.0} s, so there was nothing to land on the bar")
    } else if take.anchored() {
        format!("{port:?} was running but no downbeat came within {secs:.0} s")
    } else if last_msg.is_some() {
        format!(
            "{port:?} is sending clock but never said where the bar is: no Start (or Song \
             Position + Continue) within {secs:.0} s of arming. A clock joined while it is \
             already running has no bar; stop the master and start it again after arming"
        )
    } else {
        format!(
            "{port:?} sent no clock within {secs:.0} s of arming; press play on the master \
             after arming"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> BarOptions {
        BarOptions {
            plan: BarPlan { beats_per_bar: 4, bars: 1, max_take_ns: None },
            arm_timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn no_downbeat_says_what_the_person_can_do() {
        let take = BarTake::new(options().plan);
        let text = why_no_downbeat(&options(), "Digitakt", None, &take, None);
        assert!(text.contains("no audio arrived"), "{text}");
        let text = why_no_downbeat(&options(), "Digitakt", Some(1), &take, None);
        assert!(text.contains("sent no clock") && text.contains("press play"), "{text}");
        let text =
            why_no_downbeat(&options(), "Digitakt", Some(1), &take, Some((1, Instant::now())));
        assert!(text.contains("never said where the bar is"), "{text}");
    }
}
