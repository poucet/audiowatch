//! Each app on its own channel: the follower behind
//! [`start_app_channels`](crate::start_app_channels).
//!
//! Where [`Follow`](crate::apps::Follow) keeps one tap mixing every app it
//! follows, [`Split`] keeps one tap per process: a process gets a channel
//! when it starts playing, and loses it when it exits, leaves the tap's
//! selection, or has not played for [`CHANNEL_HOLD`]. Pure, like `Follow`:
//! the HAL is reached through the same [`Backend`], so the tests run it
//! against a fake.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use crate::apps::{self, AppInfo, AppTap, Backend, Proc, Target, RETRY};
use crate::Sink;

/// Where each app's frames go, and what hears an app come and go.
///
/// Both methods run on the capture's own follower thread (never the audio
/// thread), so they may allocate and lock; they hold up following the
/// other apps while they run, so keep them short.
pub trait AppSinks: Send + 'static {
    /// The sink of one app's channel. It is called on that app's own
    /// real-time thread (each channel has its own), with exactly
    /// [`start`](crate::start)'s sink contract: interleaved frames at the
    /// width and rate asked for, and their host-clock capture time in ns.
    type Sink: FnMut(&[f32], u64) + Send + 'static;

    /// `app` started playing: make its channel and return its sink. The
    /// channel exists from now until [`stopped`](AppSinks::stopped), even
    /// while its tap is being (re)made ([`ChannelState::Failed`]).
    fn started(&mut self, app: &AppInfo) -> Self::Sink;

    /// `app`'s channel is gone: it exited, left the tap's selection, or has
    /// not played for [`CHANNEL_HOLD`]; or the capture was dropped. Its
    /// sink has been dropped already and is never called again.
    fn stopped(&mut self, app: &AppInfo);
}

/// How long an app's channel outlives its last sound. A tap of a process
/// that isn't playing is not called back at all (measured), so a held
/// channel costs no audio-thread work; the hold spares a channel's
/// teardown and re-creation (about 50 ms of HAL work, and 60–200 ms before
/// its first frames) across a pause.
pub const CHANNEL_HOLD: Duration = Duration::from_secs(5);

/// One app's channel (see [`AppChannels`](crate::AppChannels)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppChannel {
    /// Who: `app.pid` is the channel's key. A restarted app is a new
    /// process, so a new channel.
    pub app: AppInfo,
    pub state: ChannelState,
}

/// Whether a channel's tap is delivering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChannelState {
    /// Its tap runs, at its clock device's `device_rate` (before
    /// resampling to the rate asked for).
    Tapping { device_rate: u32 },
    /// Its tap could not be made, or went bad; it is retried every few
    /// seconds. The channel stays.
    Failed { error: String },
}

/// The channels now, and the apps asked for that aren't running.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChannelsStatus {
    /// In the order they started.
    pub channels: Vec<AppChannel>,
    /// [`Apps::Only`]: the apps asked for that aren't running (yet).
    /// [`Apps::AllExcept`]: the exclusions that match nothing now.
    pub missing: Vec<String>,
}

struct Chan<T> {
    app: AppInfo,
    /// Order of arrival, for the status.
    seq: u64,
    tap: Option<T>,
    /// The sink, while no tap holds it.
    sink: Option<Sink>,
    state: ChannelState,
    failed_at: Option<Instant>,
}

/// Follows an [`AppTap`]'s apps, one tap each: one [`tick`](Split::tick)
/// per HAL change or poll.
pub(crate) struct Split<B: Backend, S: AppSinks> {
    backend: B,
    tap: AppTap,
    this: i32,
    sinks: S,
    /// By HAL process object.
    chans: BTreeMap<u32, Chan<B::Tap>>,
    /// When each candidate process was last seen playing.
    played: HashMap<u32, Instant>,
    seq: u64,
    status: ChannelsStatus,
}

impl<B: Backend, S: AppSinks> Split<B, S> {
    pub(crate) fn new(backend: B, tap: AppTap, this: i32, sinks: S) -> Split<B, S> {
        Split {
            backend,
            tap,
            this,
            sinks,
            chans: BTreeMap::new(),
            played: HashMap::new(),
            seq: 0,
            status: ChannelsStatus::default(),
        }
    }

    /// Look at the HAL and bring the channels in line with it.
    pub(crate) fn tick(&mut self, now: Instant) -> &ChannelsStatus {
        let procs = self.backend.processes();
        let (matched, missing) = apps::resolve(&mut self.backend, &self.tap, self.this, &procs);
        let candidates: Vec<&Proc> = apps::tapped(&self.tap.apps, &procs, &matched);
        self.played.retain(|o, _| candidates.iter().any(|p| p.object == *o));
        for p in candidates.iter().filter(|p| p.playing) {
            self.played.insert(p.object, now);
        }
        let wanted: BTreeMap<u32, &Proc> = candidates
            .into_iter()
            .filter(|p| {
                self.played
                    .get(&p.object)
                    .is_some_and(|t| now.saturating_duration_since(*t) < CHANNEL_HOLD)
            })
            .map(|p| (p.object, p))
            .collect();

        let gone: Vec<u32> =
            self.chans.keys().filter(|o| !wanted.contains_key(o)).copied().collect();
        for object in gone {
            let chan = self.chans.remove(&object).expect("listed above");
            self.stop(chan);
        }

        let Split { backend, tap, this, sinks, chans, seq, .. } = self;
        for (&object, p) in &wanted {
            let chan = chans.entry(object).or_insert_with(|| {
                let app = AppInfo::of(p, *this);
                let sink: Sink = Box::new(sinks.started(&app));
                *seq += 1;
                Chan {
                    app,
                    seq: *seq,
                    tap: None,
                    sink: Some(sink),
                    state: ChannelState::Failed { error: "starting".into() },
                    failed_at: None,
                }
            });
            if let Some(t) = &chan.tap {
                let Some(problem) = backend.health(t, p.playing) else { continue };
                let t = chan.tap.take().expect("checked above");
                chan.sink = Some(backend.close(t));
                chan.state = ChannelState::Failed { error: problem };
                chan.failed_at = None;
            }
            if chan.failed_at.is_some_and(|at| now.saturating_duration_since(at) < RETRY) {
                continue;
            }
            let sink = chan.sink.take().expect("no tap holds the sink");
            match backend.open(&Target::Processes(vec![object]), tap.mute, sink) {
                Ok((t, device_rate)) => {
                    chan.tap = Some(t);
                    chan.state = ChannelState::Tapping { device_rate };
                    chan.failed_at = None;
                }
                Err((e, sink)) => {
                    chan.sink = Some(sink);
                    chan.state = ChannelState::Failed { error: e.to_string() };
                    chan.failed_at = Some(now);
                }
            }
        }

        let mut channels: Vec<&Chan<B::Tap>> = self.chans.values().collect();
        channels.sort_by_key(|c| c.seq);
        self.status = ChannelsStatus {
            channels: channels
                .into_iter()
                .map(|c| AppChannel { app: c.app.clone(), state: c.state.clone() })
                .collect(),
            missing,
        };
        &self.status
    }

    /// Close a channel's tap, drop its sink, then tell the caller.
    fn stop(&mut self, mut chan: Chan<B::Tap>) {
        if let Some(t) = chan.tap.take() {
            drop(self.backend.close(t));
        }
        drop(chan.sink.take());
        self.sinks.stopped(&chan.app);
    }
}

impl<B: Backend, S: AppSinks> Drop for Split<B, S> {
    fn drop(&mut self) {
        let mut chans: Vec<Chan<B::Tap>> = std::mem::take(&mut self.chans).into_values().collect();
        chans.sort_by_key(|c| c.seq);
        for chan in chans {
            self.stop(chan);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::tests::*;
    use crate::apps::{App, Mute};
    use std::sync::{Arc, Mutex};

    /// What the caller's sinks heard, in order: `+name` / `-name`, and the
    /// frames each channel's sink was handed.
    #[derive(Clone, Default)]
    struct Heard(Arc<Mutex<Vec<String>>>);

    impl AppSinks for Heard {
        type Sink = Box<dyn FnMut(&[f32], u64) + Send>;
        fn started(&mut self, app: &AppInfo) -> Self::Sink {
            self.0.lock().unwrap().push(format!("+{}", app.name));
            let (heard, name) = (Arc::clone(&self.0), app.name.clone());
            Box::new(move |f: &[f32], _| heard.lock().unwrap().push(format!("{name}:{}", f.len())))
        }
        fn stopped(&mut self, app: &AppInfo) {
            self.0.lock().unwrap().push(format!("-{}", app.name));
        }
    }

    impl Heard {
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    fn split(tap: AppTap) -> (Fake, Heard, Split<Fake, Heard>) {
        let (fake, heard) = (Fake::default(), Heard::default());
        let s = Split::new(fake.clone(), tap, ME, heard.clone());
        (fake, heard, s)
    }

    fn quiet(p: Proc) -> Proc {
        Proc { playing: false, ..p }
    }

    fn names(s: &ChannelsStatus) -> Vec<String> {
        s.channels.iter().map(|c| format!("{} {}", c.app.name, c.app.pid)).collect()
    }

    fn djay() -> Proc {
        proc(10, 412, DJAY, DJAY_EXE)
    }
    fn music() -> Proc {
        proc(3, 5, MUSIC, "/x/Music")
    }
    fn me() -> Proc {
        proc(12, ME, "", "/x/simply-tussen")
    }

    #[test]
    fn everything_except_me_gives_each_playing_app_its_own_tap() {
        let (fake, heard, mut s) = split(AppTap::all_except([App::This]));
        let t = Instant::now();
        set(&fake, vec![djay(), quiet(music()), me()]);
        assert_eq!(names(s.tick(t)), ["djay Pro 412"]);
        assert_eq!(heard.take(), ["+djay Pro", "djay Pro:2"]);
        assert_eq!(fake.0.borrow().opened, [(Target::Processes(vec![10]), Mute::Unmuted)]);

        // Music starts playing: a channel of its own, djay's left alone.
        set(&fake, vec![djay(), music(), me()]);
        let st = s.tick(t).clone();
        assert_eq!(names(&st), ["djay Pro 412", "Music 5"]);
        assert_eq!(st.channels[1].state, ChannelState::Tapping { device_rate: 48_000 });
        assert_eq!(heard.take(), ["+Music", "Music:2"]);
        let f = fake.0.borrow();
        assert_eq!(f.opened.last().unwrap().0, Target::Processes(vec![3]));
        assert_eq!((f.opened.len(), f.closed.len()), (2, 0));
    }

    #[test]
    fn a_channel_outlives_a_pause_and_goes_after_the_hold() {
        let (fake, heard, mut s) = split(AppTap::all_except([App::This]));
        let t = Instant::now();
        set(&fake, vec![djay()]);
        s.tick(t);
        set(&fake, vec![quiet(djay())]);
        assert_eq!(names(s.tick(t + CHANNEL_HOLD / 2)), ["djay Pro 412"]);
        // Paused, so its silent tap is not a stall.
        assert_eq!(fake.0.borrow().checked, [false]);
        // Plays again within the hold: the same tap, no events.
        set(&fake, vec![djay()]);
        s.tick(t + CHANNEL_HOLD);
        set(&fake, vec![quiet(djay())]);
        assert_eq!(names(s.tick(t + CHANNEL_HOLD * 2 - Duration::from_millis(1))).len(), 1);
        assert_eq!(heard.take(), ["+djay Pro", "djay Pro:2"]);
        assert!(s.tick(t + CHANNEL_HOLD * 2).channels.is_empty());
        assert_eq!(heard.take(), ["-djay Pro"]);
        let f = fake.0.borrow();
        assert_eq!((f.opened.len(), f.closed.as_slice(), f.live.len()), (1, &[1][..], 0));
    }

    #[test]
    fn an_app_that_exits_stops_and_its_next_process_is_a_new_channel() {
        let (fake, heard, mut s) = split(AppTap::only([App::parse(DJAY)]));
        let t = Instant::now();
        set(&fake, vec![djay(), music()]);
        assert_eq!(names(s.tick(t)), ["djay Pro 412"]);
        set(&fake, vec![music()]);
        assert!(s.tick(t).channels.is_empty());
        assert_eq!(s.tick(t).missing, [DJAY]);
        set(&fake, vec![proc(27, 500, DJAY, DJAY_EXE)]);
        assert_eq!(names(s.tick(t)), ["djay Pro 500"]);
        assert_eq!(
            heard.take(),
            ["+djay Pro", "djay Pro:2", "-djay Pro", "+djay Pro", "djay Pro:2"]
        );
        assert_eq!(fake.0.borrow().live, [2]);
    }

    #[test]
    fn only_several_apps_gives_each_its_own_channel_and_ignores_the_rest() {
        let tap = AppTap::only([App::parse(DJAY), App::parse("Spotify")]).with_mute(Mute::Muted);
        let (fake, _, mut s) = split(tap);
        let t = Instant::now();
        let spotify = proc(4, 77, "com.spotify.client", "/x/Spotify");
        set(&fake, vec![djay(), music(), spotify]);
        let st = s.tick(t).clone();
        assert_eq!(names(&st), ["Spotify 77", "djay Pro 412"]);
        assert!(st.missing.is_empty());
        let opened = &fake.0.borrow().opened;
        assert_eq!(
            opened.as_slice(),
            [(Target::Processes(vec![4]), Mute::Muted), (Target::Processes(vec![10]), Mute::Muted)]
        );
    }

    #[test]
    fn an_app_that_starts_playing_into_an_excluded_device_loses_its_channel() {
        let tap = AppTap::all_except([App::This, App::PlayingInto(TUSSEN.into())]);
        let (fake, heard, mut s) = split(tap);
        let t = Instant::now();
        let me = plays(me(), &[TUSSEN_IN_1]);
        set(&fake, vec![djay(), music(), me.clone()]);
        assert_eq!(names(s.tick(t)), ["Music 5", "djay Pro 412"]);
        set(&fake, vec![djay(), plays(music(), &[TUSSEN_IN_1]), me]);
        assert_eq!(names(s.tick(t)), ["djay Pro 412"]);
        assert_eq!(heard.take(), ["+Music", "Music:2", "+djay Pro", "djay Pro:2", "-Music"]);
    }

    #[test]
    fn a_channel_whose_tap_fails_stays_and_is_retried() {
        let (fake, heard, mut s) = split(AppTap::all_except([App::This]));
        let t = Instant::now();
        set(&fake, vec![djay()]);
        fake.0.borrow_mut().fail = Some("OSStatus 'nope'".into());
        let st = s.tick(t).clone();
        assert_eq!(st.channels[0].state, ChannelState::Failed { error: "OSStatus 'nope'".into() });
        assert_eq!(heard.take(), ["+djay Pro"]);
        s.tick(t + RETRY / 2);
        assert!(fake.0.borrow().opened.is_empty());
        let st = s.tick(t + RETRY).clone();
        assert_eq!(st.channels[0].state, ChannelState::Tapping { device_rate: 48_000 });
        assert_eq!(heard.take(), ["djay Pro:2"], "a retry is not a new channel");
    }

    #[test]
    fn a_tap_that_goes_bad_is_re_made_into_the_same_sink() {
        let (fake, heard, mut s) = split(AppTap::all_except([App::This]));
        let t = Instant::now();
        set(&fake, vec![djay()]);
        s.tick(t);
        fake.0.borrow_mut().sick = Some("the tap stopped delivering audio".into());
        assert_eq!(s.tick(t).channels[0].state, ChannelState::Tapping { device_rate: 48_000 });
        assert_eq!(fake.0.borrow().checked, [true]);
        assert_eq!(
            (fake.0.borrow().closed.as_slice(), fake.0.borrow().live.as_slice()),
            (&[1][..], &[2][..])
        );
        // Both taps fed the sink `started` gave; no second channel.
        assert_eq!(heard.take(), ["+djay Pro", "djay Pro:2", "djay Pro:2"]);
    }

    #[test]
    fn dropping_stops_every_channel_after_dropping_its_tap() {
        let (fake, heard, mut s) = split(AppTap::all_except([App::This]));
        let t = Instant::now();
        set(&fake, vec![djay(), music()]);
        s.tick(t);
        assert_eq!(heard.take(), ["+Music", "Music:2", "+djay Pro", "djay Pro:2"]);
        drop(s);
        assert_eq!(heard.take(), ["-Music", "-djay Pro"]);
        let f = fake.0.borrow();
        assert_eq!((f.closed.as_slice(), f.live.len()), (&[1, 2][..], 0));
    }
}
