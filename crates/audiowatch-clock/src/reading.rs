//! Following a MIDI clock to the bar: where the downbeats are, and when a
//! take of N bars starts and ends.
//!
//! Everything here is pure — timestamped messages in, a window out — so it is
//! tested against synthetic clock timelines with no device anywhere. The
//! device side (finding the port that is sending clock, and listening to it)
//! is [`crate::port`]; the audio side (turning the window into frames and
//! trimming the file) is `audiowatch_record`'s [`TakeWindow`], fed by
//! [`crate::take`].
//!
//! [`TakeWindow`]: audiowatch_record::TakeWindow
//!
//! # What MIDI clock does and does not say
//!
//! A clock is 24 ticks (`0xF8`) per quarter note and nothing else: no bar, no
//! time signature, no tempo. **Where the bar is** comes only from an anchor —
//! `Start` (`0xFA`: the next tick is position 0) or `Song Position` (`0xF2`,
//! in sixteenths) followed by `Continue` (`0xFB`). A clock joined while it is
//! already running has ticks and no anchor, so its bar is unknowable, and a
//! take waits for the next `Start` rather than guess. How many beats make a
//! bar is the caller's to say (4 unless told); the clock cannot.
//!
//! # Landing on the bar, not near it
//!
//! A tick's timestamp is when it *arrived*, and MIDI arrives with a
//! millisecond or so of jitter. The downbeat is therefore not read off one
//! tick: it is a least-squares line through the ticks half a beat either side
//! of it, evaluated at the downbeat's position. Because the take is trimmed
//! after it is recorded, the ticks *after* the downbeat are available too.
//!
//! The end is counted in ticks, not seconds: the take ends on the Nth
//! downbeat however the tempo moved in between.

use simply_midi_clock::{ClockEvent, TICKS_PER_BEAT, TICKS_PER_SIXTEENTH};

/// Ticks per quarter note — MIDI clock's one fixed number.
pub const PPQN: u64 = TICKS_PER_BEAT as u64;

/// Half the fit window, in ticks: half a beat either side of a downbeat.
pub const FIT_HALF: u64 = 12;

/// A recording clock stops counting if no tick arrives for this long.
pub const LOST_NS: u64 = 1_000_000_000;

/// Read one MIDI message as the clock message it is. `None` for anything
/// that is not clock.
pub fn parse(bytes: &[u8]) -> Option<ClockEvent> {
    match *bytes.first()? {
        0xF8 => Some(ClockEvent::Tick),
        0xFA => Some(ClockEvent::Start),
        0xFB => Some(ClockEvent::Continue),
        0xFC => Some(ClockEvent::Stop),
        0xF2 if bytes.len() >= 3 => Some(ClockEvent::Position(
            u16::from(bytes[1] & 0x7F) | (u16::from(bytes[2] & 0x7F) << 7),
        )),
        _ => None,
    }
}

/// What a message meant, once the follower has read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heard {
    /// A tick at `pos` ticks from the song's start, or `None` when the
    /// position is unknown (no anchor yet, or the clock is stopped).
    Tick {
        pos: Option<u64>,
        t: u64,
    },
    /// `Start`, or `Continue` from a known position.
    Started {
        t: u64,
    },
    Stopped {
        t: u64,
    },
    /// A `Song Position` jump.
    Located {
        t: u64,
    },
}

/// Tracks transport state and position from the message stream.
#[derive(Debug, Default, Clone)]
pub struct Listener {
    running: bool,
    /// Position of the *next* tick, once anchored.
    next_pos: Option<u64>,
}

impl Listener {
    pub fn feed(&mut self, t: u64, msg: ClockEvent) -> Heard {
        match msg {
            ClockEvent::Tick => {
                let pos = if self.running { self.next_pos } else { None };
                if let (true, Some(p)) = (self.running, self.next_pos.as_mut()) {
                    *p += 1;
                }
                Heard::Tick { pos, t }
            }
            ClockEvent::Start => {
                self.running = true;
                self.next_pos = Some(0);
                Heard::Started { t }
            }
            ClockEvent::Continue => {
                self.running = true;
                Heard::Started { t }
            }
            ClockEvent::Stop => {
                self.running = false;
                Heard::Stopped { t }
            }
            ClockEvent::Position(s) => {
                self.next_pos = Some(u64::from(s) * TICKS_PER_SIXTEENTH as u64);
                Heard::Located { t }
            }
        }
    }

    /// Running, with a known position: a bar can be found.
    pub fn anchored(&self) -> bool {
        self.running && self.next_pos.is_some()
    }
}

/// Tempo from the spacing of recent ticks, anchored or not.
#[derive(Debug, Default, Clone)]
pub struct TempoMeter {
    first: Option<u64>,
    last: u64,
    count: u64,
}

impl TempoMeter {
    pub fn tick(&mut self, t: u64) {
        if self.first.is_none() {
            self.first = Some(t);
        }
        self.last = t;
        self.count += 1;
    }

    pub fn ticks(&self) -> u64 {
        self.count
    }

    /// Mean tempo over every tick seen, once there are enough to mean it.
    pub fn bpm(&self) -> Option<f64> {
        bpm_between(self.count.checked_sub(1)?, self.first?, self.last)
    }
}

/// Tempo implied by `ticks` tick intervals spanning `from..to` nanoseconds.
pub fn bpm_between(ticks: u64, from: u64, to: u64) -> Option<f64> {
    (ticks >= 2 && to > from).then(|| 60e9 * ticks as f64 / (PPQN as f64 * (to - from) as f64))
}

/// A take measured in bars.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarPlan {
    pub beats_per_bar: u32,
    pub bars: u32,
    /// End the take (with a warning) if it runs longer than this — a tempo
    /// that dropped mid-take can make N bars much longer than planned.
    pub max_take_ns: Option<u64>,
}

impl BarPlan {
    pub fn bar_ticks(&self) -> u64 {
        PPQN * u64::from(self.beats_per_bar.max(1))
    }

    fn take_ticks(&self) -> u64 {
        self.bar_ticks() * u64::from(self.bars)
    }

    /// How long the take lasts at `bpm`, in seconds.
    pub fn seconds_at(&self, bpm: f64) -> f64 {
        f64::from(self.bars) * f64::from(self.beats_per_bar) * 60.0 / bpm
    }
}

/// Why a take ended before its last downbeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EarlyEnd {
    /// The clock sent `Stop`.
    Stopped,
    /// The clock jumped: `Start` or `Song Position` mid-take.
    Located,
    /// No tick for [`LOST_NS`].
    Lost,
    /// Longer than the plan's `max_take_ns`.
    TooLong,
}

/// Where a take lies in time, once it is over.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// The first downbeat, in host nanoseconds.
    pub start_ns: u64,
    /// The last downbeat, or where the take was cut short.
    pub end_ns: u64,
    /// Bars in the take: the plan's count, or fewer when it ended early.
    pub bars: f64,
    /// Mean tempo over the take.
    pub bpm: f64,
    /// Slowest and fastest beat in the take.
    pub bpm_min: f64,
    pub bpm_max: f64,
    /// The song position (in ticks) the take started on.
    pub start_pos: u64,
    pub early: Option<EarlyEnd>,
}

impl Window {
    /// The sentence that goes beside a take that ended early.
    pub fn warning(&self, plan: &BarPlan) -> Option<String> {
        let why = match self.early? {
            EarlyEnd::Stopped => "the clock stopped",
            EarlyEnd::Located => "the clock jumped to another song position",
            EarlyEnd::Lost => "the clock went silent",
            EarlyEnd::TooLong => "it ran past the longest take allowed (the tempo slowed)",
        };
        Some(format!(
            "{why} after {:.2} of {} bars, so the take ends there and is shorter than asked",
            self.bars, plan.bars
        ))
    }
}

/// A bar-quantised take, fed the clock's messages in the order they came.
#[derive(Debug, Clone)]
pub struct BarTake {
    plan: BarPlan,
    follower: Listener,
    /// Ticks earlier than this cannot start the take: the audio was not
    /// running yet.
    not_before: Option<u64>,
    start_pos: Option<u64>,
    /// `(position, arrival)` of every anchored tick since the take was armed
    /// (only the last half-beat of them before a start is chosen).
    ticks: Vec<(u64, u64)>,
    early: Option<(EarlyEnd, u64)>,
    complete: bool,
}

impl BarTake {
    pub fn new(plan: BarPlan) -> BarTake {
        BarTake {
            plan,
            follower: Listener::default(),
            not_before: None,
            start_pos: None,
            ticks: Vec::new(),
            early: None,
            complete: false,
        }
    }

    pub fn plan(&self) -> &BarPlan {
        &self.plan
    }

    /// The earliest instant the first downbeat may be — when every take's
    /// audio is already running.
    pub fn set_not_before(&mut self, t: u64) {
        self.not_before = Some(t);
    }

    pub fn anchored(&self) -> bool {
        self.follower.anchored()
    }

    /// The first downbeat has passed: audio from here on is the take.
    pub fn started(&self) -> bool {
        match (self.start_pos, self.ticks.last()) {
            (Some(s), Some(&(p, _))) => p >= s,
            _ => false,
        }
    }

    pub fn done(&self) -> bool {
        self.complete || self.early.is_some()
    }

    fn end_pos(&self) -> Option<u64> {
        Some(self.start_pos? + self.plan.take_ticks())
    }

    pub fn feed(&mut self, t: u64, msg: ClockEvent) {
        if self.done() {
            return;
        }
        match self.follower.feed(t, msg) {
            Heard::Tick { pos: Some(p), t } => {
                self.ticks.push((p, t));
                match self.start_pos {
                    None => {
                        if self.not_before.is_some_and(|nb| t >= nb) {
                            let bar = self.plan.bar_ticks();
                            self.start_pos = Some(p.div_ceil(bar) * bar);
                        }
                        // Before a start is chosen only the last half-beat
                        // can matter to the fit.
                        let keep = self.ticks.len().saturating_sub(FIT_HALF as usize + 1);
                        self.ticks.drain(..keep);
                    }
                    Some(_) => {
                        if self.end_pos().is_some_and(|e| p >= e + FIT_HALF) {
                            self.complete = true;
                        }
                    }
                }
            }
            Heard::Tick { pos: None, .. } => {}
            Heard::Stopped { t } | Heard::Located { t } | Heard::Started { t } => {
                let early = if matches!(msg, ClockEvent::Stop) {
                    EarlyEnd::Stopped
                } else {
                    EarlyEnd::Located
                };
                // `Continue` right after a `Stop` or `Song Position` is how
                // the position is re-established, not a jump.
                if matches!(msg, ClockEvent::Continue) {
                    return;
                }
                if self.started() {
                    if self.reached_end() {
                        self.complete = true;
                    } else {
                        self.early = Some((early, t));
                    }
                } else {
                    // Not yet at the downbeat: the position moved or stopped,
                    // so choose the downbeat again once it runs.
                    self.start_pos = None;
                    self.ticks.clear();
                }
            }
        }
    }

    /// Called as time passes, with the current host time: notices a clock
    /// that went silent, and a take that runs too long.
    pub fn poll(&mut self, now: u64) {
        if self.done() || !self.started() {
            return;
        }
        let &(_, last) = self.ticks.last().expect("started");
        if self.reached_end() && now.saturating_sub(last) > LOST_NS {
            self.complete = true;
            return;
        }
        if now.saturating_sub(last) > LOST_NS {
            self.early = Some((EarlyEnd::Lost, last + self.tick_ns()));
            return;
        }
        if let (Some(max), Some(start)) = (self.plan.max_take_ns, self.start_ns()) {
            if now.saturating_sub(start) > max {
                self.early = Some((EarlyEnd::TooLong, start + max));
            }
        }
    }

    fn reached_end(&self) -> bool {
        match (self.end_pos(), self.ticks.last()) {
            (Some(e), Some(&(p, _))) => p >= e,
            _ => false,
        }
    }

    /// Mean tick spacing over what has been seen, for extrapolating past
    /// the last tick.
    fn tick_ns(&self) -> u64 {
        match (self.ticks.first(), self.ticks.last()) {
            (Some(&(p0, t0)), Some(&(p1, t1))) if p1 > p0 => (t1 - t0) / (p1 - p0),
            _ => 0,
        }
    }

    fn start_ns(&self) -> Option<u64> {
        self.started().then(|| fit(&self.ticks, self.start_pos?)).flatten()
    }

    /// The window, once the take is over. `None` while it runs (or if it
    /// never started).
    pub fn window(&self) -> Option<Window> {
        if !self.done() || !self.started() {
            return None;
        }
        let start_pos = self.start_pos?;
        let start_ns = self.start_ns()?;
        let bar = self.plan.bar_ticks() as f64;
        let (end_ns, bars, end_pos, early) = match self.early {
            None => {
                let end_pos = self.end_pos()?;
                (fit(&self.ticks, end_pos)?, f64::from(self.plan.bars), end_pos, None)
            }
            Some((why, t)) => {
                let end_ns = t.max(start_ns);
                let last_pos = self
                    .ticks
                    .iter()
                    .filter(|&&(p, at)| p >= start_pos && at <= end_ns)
                    .map(|&(p, _)| p)
                    .max()?;
                let reached = last_pos + 1 - start_pos;
                (end_ns, reached as f64 / bar, last_pos, Some(why))
            }
        };
        let in_take: Vec<(u64, u64)> =
            self.ticks.iter().copied().filter(|&(p, _)| p >= start_pos && p <= end_pos).collect();
        let bpm = match early {
            None => bpm_between(end_pos - start_pos, start_ns, end_ns),
            Some(_) => {
                let (&(p0, t0), &(p1, t1)) = (in_take.first()?, in_take.last()?);
                bpm_between(p1 - p0, t0, t1)
            }
        }
        .unwrap_or(0.0);
        let (mut bpm_min, mut bpm_max) = (f64::INFINITY, 0.0f64);
        let beats: Vec<(u64, u64)> =
            in_take.iter().copied().filter(|&(p, _)| (p - start_pos) % PPQN == 0).collect();
        for pair in beats.windows(2) {
            if let Some(b) = bpm_between(pair[1].0 - pair[0].0, pair[0].1, pair[1].1) {
                bpm_min = bpm_min.min(b);
                bpm_max = bpm_max.max(b);
            }
        }
        if bpm_max == 0.0 {
            (bpm_min, bpm_max) = (bpm, bpm);
        }
        Some(Window { start_ns, end_ns, bars, bpm, bpm_min, bpm_max, start_pos, early })
    }
}

/// The instant of position `pos`: a least-squares line through the ticks
/// within [`FIT_HALF`] of it, read at `pos`. Averages out the arrival jitter
/// of any one tick.
pub fn fit(ticks: &[(u64, u64)], pos: u64) -> Option<u64> {
    let near: Vec<(f64, f64)> = ticks
        .iter()
        .filter(|&&(p, _)| p.abs_diff(pos) <= FIT_HALF)
        .map(|&(p, t)| (p as f64 - pos as f64, t as f64))
        .collect();
    let t0 = near.first()?.1;
    if near.len() == 1 {
        return near.iter().find(|&&(x, _)| x == 0.0).map(|&(_, t)| t as u64);
    }
    let n = near.len() as f64;
    let mx = near.iter().map(|&(x, _)| x).sum::<f64>() / n;
    let my = near.iter().map(|&(_, t)| t - t0).sum::<f64>() / n;
    let sxx: f64 = near.iter().map(|&(x, _)| (x - mx).powi(2)).sum();
    let sxy: f64 = near.iter().map(|&(x, t)| (x - mx) * (t - t0 - my)).sum();
    if sxx == 0.0 {
        return None;
    }
    let slope = sxy / sxx;
    Some((t0 + my - slope * mx).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BPB: u32 = 4;

    fn plan(bars: u32) -> BarPlan {
        BarPlan { beats_per_bar: BPB, bars, max_take_ns: None }
    }

    /// Nanoseconds per tick at `bpm`.
    fn tick_ns(bpm: f64) -> f64 {
        60e9 / (bpm * PPQN as f64)
    }

    /// Feed `n` ticks at `bpm` starting at `t`, returning the time after.
    fn ticks(take: &mut BarTake, t: f64, n: u64, bpm: f64) -> f64 {
        let mut t = t;
        for _ in 0..n {
            take.feed(t.round() as u64, ClockEvent::Tick);
            t += tick_ns(bpm);
        }
        t
    }

    #[test]
    fn clock_messages_parse_and_everything_else_is_ignored() {
        assert_eq!(parse(&[0xF8]), Some(ClockEvent::Tick));
        assert_eq!(parse(&[0xFA]), Some(ClockEvent::Start));
        assert_eq!(parse(&[0xFB]), Some(ClockEvent::Continue));
        assert_eq!(parse(&[0xFC]), Some(ClockEvent::Stop));
        // Song Position 200 sixteenths = 0x48 + (0x01 << 7).
        assert_eq!(parse(&[0xF2, 0x48, 0x01]), Some(ClockEvent::Position(200)));
        assert_eq!(parse(&[0xF2, 0x48]), None);
        assert_eq!(parse(&[0x90, 60, 100]), None);
        assert_eq!(parse(&[]), None);
    }

    #[test]
    fn a_start_makes_the_next_tick_the_downbeat() {
        let mut f = Listener::default();
        assert_eq!(f.feed(0, ClockEvent::Tick), Heard::Tick { pos: None, t: 0 });
        f.feed(1, ClockEvent::Start);
        assert_eq!(f.feed(2, ClockEvent::Tick), Heard::Tick { pos: Some(0), t: 2 });
        assert_eq!(f.feed(3, ClockEvent::Tick), Heard::Tick { pos: Some(1), t: 3 });
        f.feed(4, ClockEvent::Stop);
        assert_eq!(f.feed(5, ClockEvent::Tick), Heard::Tick { pos: None, t: 5 });
        f.feed(6, ClockEvent::Position(4));
        f.feed(7, ClockEvent::Continue);
        assert_eq!(f.feed(8, ClockEvent::Tick), Heard::Tick { pos: Some(24), t: 8 });
    }

    #[test]
    fn a_running_clock_joined_without_an_anchor_never_chooses_a_downbeat() {
        let mut take = BarTake::new(plan(1));
        take.set_not_before(0);
        ticks(&mut take, 0.0, 400, 120.0);
        assert!(!take.anchored() && !take.started());
    }

    #[test]
    fn a_steady_clock_takes_exactly_n_bars_from_the_start() {
        let bpm = 120.0;
        let mut take = BarTake::new(plan(2));
        take.set_not_before(0);
        take.feed(1_000_000_000, ClockEvent::Start);
        let t0 = 1_010_000_000.0;
        ticks(&mut take, t0, 2 * 96 + FIT_HALF + 1, bpm);
        assert!(take.done());
        let w = take.window().unwrap();
        assert_eq!(w.start_pos, 0);
        assert!((w.start_ns as f64 - t0).abs() <= 1.0, "{}", w.start_ns);
        // Two bars of 4/4 at 120 = 4 s.
        assert!(((w.end_ns - w.start_ns) as f64 - 4e9).abs() <= 2.0);
        assert!((w.bpm - bpm).abs() < 1e-6);
        assert_eq!(w.bars, 2.0);
        assert!(w.early.is_none() && w.warning(take.plan()).is_none());
    }

    #[test]
    fn the_downbeat_is_the_next_bar_line_after_the_audio_is_running() {
        let mut take = BarTake::new(plan(1));
        take.feed(0, ClockEvent::Start);
        // Audio is running from 0.6 s: at 120 bpm the tick at 0.6 s is
        // position 28, so the next bar is position 96, at 2 s.
        take.set_not_before(600_000_000);
        ticks(&mut take, 0.0, 96 * 2 + FIT_HALF + 1, 120.0);
        let w = take.window().unwrap();
        assert_eq!(w.start_pos, 96);
        assert!((w.start_ns as f64 - 2e9).abs() <= 1.0, "{}", w.start_ns);
    }

    #[test]
    fn a_tempo_change_mid_take_still_ends_on_the_nth_downbeat() {
        let mut take = BarTake::new(plan(2));
        take.set_not_before(0);
        take.feed(0, ClockEvent::Start);
        // Bar 1 at 120 bpm (2 s), bar 2 at 90 bpm (2.667 s).
        let t = ticks(&mut take, 0.0, 96, 120.0);
        ticks(&mut take, t, 96 + FIT_HALF + 1, 90.0);
        let w = take.window().unwrap();
        let expect = 2e9 + 4.0 * 60e9 / 90.0;
        // The change is mid-take, outside either downbeat's fit window, so
        // both ends are exact to the rounding of a tick's timestamp.
        assert!(((w.end_ns - w.start_ns) as f64 - expect).abs() <= 2.0, "{}", w.end_ns);
        assert!((w.bpm_min - 90.0).abs() < 0.5 && (w.bpm_max - 120.0).abs() < 0.5);
        assert_eq!(w.bars, 2.0);
    }

    #[test]
    fn jitter_is_averaged_out_of_the_downbeat() {
        // ±1 ms of deterministic pseudo-random arrival jitter on every tick.
        let mut take = BarTake::new(plan(1));
        take.set_not_before(0);
        take.feed(0, ClockEvent::Start);
        let (mut seed, spacing) = (12345u64, tick_ns(120.0));
        for i in 0..(96 + FIT_HALF + 1) {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let jitter = ((seed >> 33) as f64 / (1u64 << 31) as f64 - 0.5) * 2e6;
            let t = 1e9 + i as f64 * spacing + jitter;
            take.feed(t.round() as u64, ClockEvent::Tick);
        }
        let w = take.window().unwrap();
        // One tick's jitter is up to 1 ms; the fit's error is well under that.
        assert!((w.start_ns as f64 - 1e9).abs() < 5e5, "{}", w.start_ns);
        assert!((w.end_ns as f64 - 3e9).abs() < 5e5, "{}", w.end_ns);
    }

    #[test]
    fn a_song_position_mid_bar_waits_for_the_next_bar_line() {
        let mut take = BarTake::new(plan(1));
        take.set_not_before(0);
        // Located to sixteenth 6 (beat 2.5 of bar 1), then Continue.
        take.feed(0, ClockEvent::Position(6));
        take.feed(1, ClockEvent::Continue);
        let t0 = 10_000_000.0;
        // Position 36 onwards; the bar line is position 96, 60 ticks later.
        ticks(&mut take, t0, 60 + 96 + FIT_HALF + 1, 120.0);
        let w = take.window().unwrap();
        assert_eq!(w.start_pos, 96);
        assert!((w.start_ns as f64 - (t0 + 60.0 * tick_ns(120.0))).abs() <= 1.0);
    }

    #[test]
    fn a_stop_and_start_before_the_downbeat_rearms_on_the_new_position() {
        let mut take = BarTake::new(plan(1));
        take.set_not_before(0);
        // Mid-bar, waiting for position 96 — then the master restarts.
        take.feed(0, ClockEvent::Position(6));
        take.feed(1, ClockEvent::Continue);
        let t = ticks(&mut take, 10.0, 10, 120.0);
        assert!(!take.started());
        take.feed(t as u64, ClockEvent::Stop);
        take.feed(t as u64 + 1_000, ClockEvent::Start);
        let t1 = t + 5e6;
        ticks(&mut take, t1, 96 + FIT_HALF + 1, 120.0);
        let w = take.window().unwrap();
        assert_eq!(w.start_pos, 0);
        assert!(w.early.is_none());
        assert!((w.start_ns as f64 - t1).abs() <= 1.0);
    }

    #[test]
    fn a_stop_mid_take_ends_it_there_with_a_warning() {
        let mut take = BarTake::new(plan(4));
        take.set_not_before(0);
        take.feed(0, ClockEvent::Start);
        let t = ticks(&mut take, 0.0, 144, 120.0); // a bar and a half
        take.feed(t as u64, ClockEvent::Stop);
        take.feed(t as u64 + 10, ClockEvent::Continue);
        assert!(take.done());
        let w = take.window().unwrap();
        assert_eq!(w.early, Some(EarlyEnd::Stopped));
        assert!((w.bars - 1.5).abs() < 1e-9, "{}", w.bars);
        assert_eq!(w.end_ns, t as u64);
        let warning = w.warning(take.plan()).unwrap();
        assert!(warning.contains("stopped") && warning.contains("1.50 of 4 bars"), "{warning}");
    }

    #[test]
    fn a_stop_after_the_last_downbeat_is_a_complete_take() {
        let mut take = BarTake::new(plan(1));
        take.set_not_before(0);
        take.feed(0, ClockEvent::Start);
        let t = ticks(&mut take, 0.0, 97, 120.0); // through position 96
        take.feed(t as u64, ClockEvent::Stop);
        let w = take.window().unwrap();
        assert!(w.early.is_none());
        assert!((w.end_ns as f64 - 2e9).abs() <= 2.0, "{}", w.end_ns);
    }

    #[test]
    fn a_silent_clock_ends_the_take_and_says_so() {
        let mut take = BarTake::new(plan(2));
        take.set_not_before(0);
        take.feed(0, ClockEvent::Start);
        let t = ticks(&mut take, 0.0, 48, 120.0);
        take.poll(t as u64);
        assert!(!take.done());
        take.poll(t as u64 + LOST_NS + 1);
        let w = take.window().unwrap();
        assert_eq!(w.early, Some(EarlyEnd::Lost));
        assert!(w.warning(take.plan()).unwrap().contains("went silent"));
    }

    #[test]
    fn a_take_past_its_longest_is_cut_there() {
        let mut take =
            BarTake::new(BarPlan { beats_per_bar: 4, bars: 8, max_take_ns: Some(3_000_000_000) });
        take.set_not_before(0);
        take.feed(0, ClockEvent::Start);
        let t = ticks(&mut take, 0.0, 200, 120.0);
        take.poll(t as u64);
        let w = take.window().unwrap();
        assert_eq!(w.early, Some(EarlyEnd::TooLong));
        assert_eq!(w.end_ns, 3_000_000_000);
    }

    #[test]
    fn a_meter_reads_tempo_off_the_spacing() {
        let mut m = TempoMeter::default();
        assert_eq!(m.bpm(), None);
        for i in 0..25u64 {
            m.tick((i as f64 * tick_ns(133.0)) as u64);
        }
        assert!((m.bpm().unwrap() - 133.0).abs() < 0.01);
        assert!((plan(2).seconds_at(120.0) - 4.0).abs() < 1e-12);
    }

    #[test]
    fn a_fit_of_one_tick_is_that_tick() {
        assert_eq!(fit(&[(96, 7)], 96), Some(7));
        assert_eq!(fit(&[(80, 7)], 96), None);
        assert_eq!(fit(&[], 96), None);
    }
}
