//! Tapping the default output and **following** it: when the Mac's default
//! output changes mid-tap (speakers to AirPods), the tap is re-made on the
//! new device and the sink carries on.
//!
//! The switch is a clean restart: the old tap is stopped (its sink comes
//! back), and a new one is started on the new default with the same sink,
//! straight away. The gap the sink sees is the time the backend takes to
//! make the new tap; nothing waits in between.
//!
//! Everything here is pure, like [`crate::apps`]'s follower: the HAL is
//! reached through [`Outputs`], so the switching is tested against a fake.

use std::time::Instant;

use crate::apps::RETRY;
use crate::{Error, Sink};

/// The side effects [`FollowDefault`] needs. The real one (macOS) makes
/// device taps; the tests' one is a script.
pub(crate) trait Outputs {
    type Tap;
    /// The default output's id right now (cheap: read on every look), or
    /// None when there is none.
    fn default_output(&mut self) -> Option<u32>;
    /// Tap output device `id` into `sink`: the tap, the device's name and
    /// the rate it runs at. On failure the sink comes back.
    fn open(&mut self, id: u32, sink: Sink) -> Result<(Self::Tap, String, u32), (Error, Sink)>;
    /// Stop and destroy a tap; its sink comes back.
    fn close(&mut self, tap: Self::Tap) -> Sink;
    /// Why a running tap no longer delivers, if it doesn't.
    fn health(&mut self, tap: &Self::Tap) -> Option<String>;
}

/// What a default-output capture is doing: [`crate::Capture::device`],
/// [`crate::Capture::device_rate`] and [`crate::Capture::error`] read it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Now {
    /// The device tapped right now (the last one, while none is).
    pub device: String,
    pub device_rate: u32,
    /// The problem right now: no default output, the new default's tap
    /// could not be made (retried), or the running tap's health.
    pub error: Option<String>,
}

struct Open<T> {
    tap: T,
    id: u32,
}

/// Keeps one tap on whatever the default output is: one
/// [`tick`](FollowDefault::tick) per HAL change or poll.
pub(crate) struct FollowDefault<O: Outputs> {
    outputs: O,
    /// The sink, while no tap holds it.
    sink: Option<Sink>,
    open: Option<Open<O::Tap>>,
    now: Now,
    /// The device that last failed to open, and when.
    failed: Option<(u32, Instant)>,
}

impl<O: Outputs> FollowDefault<O> {
    pub(crate) fn new(outputs: O, sink: Sink) -> FollowDefault<O> {
        FollowDefault { outputs, sink: Some(sink), open: None, now: Now::default(), failed: None }
    }

    /// Where it is, as of the last tick.
    pub(crate) fn now(&self) -> &Now {
        &self.now
    }

    /// Look at the default output and keep the tap on it. The first tick's
    /// error is the capture's start failing.
    pub(crate) fn tick(&mut self, now: Instant) -> Result<&Now, Error> {
        let Some(id) = self.outputs.default_output() else {
            let e = Error::new(crate::ErrorKind::NotFound, "there is no default output device");
            // Keep tapping the old device, if there is one: it may still be
            // playing, and the default comes back.
            self.now.error = Some(e.to_string());
            return match self.open {
                Some(_) => Ok(&self.now),
                None => Err(e),
            };
        };
        if let Some(open) = &self.open {
            if open.id == id {
                self.now.error = self.outputs.health(&open.tap);
                return Ok(&self.now);
            }
        }
        if let Some((failed, at)) = self.failed {
            if failed == id && now.duration_since(at) < RETRY {
                return Ok(&self.now);
            }
        }
        // A clean restart: stop the old tap, start the new one at once.
        if let Some(open) = self.open.take() {
            self.sink = Some(self.outputs.close(open.tap));
        }
        let sink = self.sink.take().expect("no tap holds the sink");
        match self.outputs.open(id, sink) {
            Ok((tap, device, device_rate)) => {
                self.failed = None;
                self.now = Now { device, device_rate, error: None };
                self.open = Some(Open { tap, id });
                Ok(&self.now)
            }
            Err((e, sink)) => {
                self.sink = Some(sink);
                self.failed = Some((id, now));
                self.now.error = Some(e.to_string());
                Err(e)
            }
        }
    }

    /// Stop the tap, if one runs; the sink is dropped with the follower.
    pub(crate) fn stop(&mut self) {
        if let Some(open) = self.open.take() {
            self.sink = Some(self.outputs.close(open.tap));
        }
    }
}

impl<O: Outputs> Drop for FollowDefault<O> {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// The fake HAL: devices by id, which is the default, and the taps.
    #[derive(Default)]
    struct Script {
        devices: Vec<(u32, &'static str, u32)>,
        default: Option<u32>,
        /// Next open fails with this.
        fail: Option<String>,
        sick: Option<String>,
        opened: Vec<u32>,
        closed: Vec<u32>,
        /// When the last tap was closed and the last one opened.
        closed_at: Option<Instant>,
        opened_at: Option<Instant>,
    }

    #[derive(Clone, Default)]
    struct Fake(Rc<RefCell<Script>>);

    /// A fake tap: its device, and the sink it feeds while it runs.
    struct Tap {
        id: u32,
        sink: Sink,
    }

    impl Outputs for Fake {
        type Tap = Tap;
        fn default_output(&mut self) -> Option<u32> {
            self.0.borrow().default
        }
        fn open(&mut self, id: u32, sink: Sink) -> Result<(Tap, String, u32), (Error, Sink)> {
            let mut s = self.0.borrow_mut();
            if let Some(e) = s.fail.take() {
                return Err((Error::new(ErrorKind::Device, e), sink));
            }
            let (_, name, rate) = *s.devices.iter().find(|d| d.0 == id).expect("listed");
            s.opened.push(id);
            s.opened_at = Some(Instant::now());
            Ok((Tap { id, sink }, name.into(), rate))
        }
        fn close(&mut self, tap: Tap) -> Sink {
            let mut s = self.0.borrow_mut();
            s.closed.push(tap.id);
            s.closed_at = Some(Instant::now());
            tap.sink
        }
        fn health(&mut self, _: &Tap) -> Option<String> {
            self.0.borrow_mut().sick.take()
        }
    }

    impl Fake {
        fn set_default(&self, id: Option<u32>) {
            self.0.borrow_mut().default = id;
        }
    }

    const SPEAKERS: u32 = 40;
    const AIRPODS: u32 = 77;

    /// A follower on the speakers, whose sink records every sample it is
    /// handed.
    fn follow() -> (Fake, FollowDefault<Fake>, Arc<Mutex<Vec<f32>>>) {
        let fake = Fake::default();
        {
            let mut s = fake.0.borrow_mut();
            s.devices =
                vec![(SPEAKERS, "MacBook Pro Speakers", 44_100), (AIRPODS, "AirPods", 48_000)];
            s.default = Some(SPEAKERS);
        }
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let got = Arc::clone(&got);
            Box::new(move |f: &[f32], _: u64| got.lock().unwrap().extend_from_slice(f))
        };
        (fake.clone(), FollowDefault::new(fake, sink), got)
    }

    /// The running tap's device calls back with `frames`.
    fn play(f: &mut FollowDefault<Fake>, frames: &[f32]) {
        let open = f.open.as_mut().expect("a tap runs");
        (open.tap.sink)(frames, 1);
    }

    #[test]
    fn a_default_change_re_opens_the_tap_on_the_new_device_and_the_stream_carries_on() {
        let (fake, mut f, got) = follow();
        let t = Instant::now();
        let now = f.tick(t).unwrap().clone();
        assert_eq!(
            now,
            Now { device: "MacBook Pro Speakers".into(), device_rate: 44_100, error: None }
        );
        play(&mut f, &[1.0, 1.0]);

        // Nothing changed: the tap is left alone.
        f.tick(t).unwrap();
        assert_eq!(fake.0.borrow().opened, [SPEAKERS]);

        // AirPods connect and become the default.
        fake.set_default(Some(AIRPODS));
        let now = f.tick(t).unwrap().clone();
        assert_eq!(now, Now { device: "AirPods".into(), device_rate: 48_000, error: None });
        {
            let s = fake.0.borrow();
            assert_eq!(
                (s.opened.as_slice(), s.closed.as_slice()),
                (&[SPEAKERS, AIRPODS][..], &[SPEAKERS][..])
            );
            // A clean restart: the new tap is made straight after the old
            // one stops, nothing waited for in between.
            let gap = s.opened_at.unwrap() - s.closed_at.unwrap();
            eprintln!("re-open gap in the fake: {gap:?}");
            assert!(gap < Duration::from_millis(5), "{gap:?}");
        }
        // The same sink, fed by the new device.
        play(&mut f, &[2.0, 2.0]);
        assert_eq!(*got.lock().unwrap(), [1.0, 1.0, 2.0, 2.0]);

        // And back.
        fake.set_default(Some(SPEAKERS));
        assert_eq!(f.tick(t).unwrap().device, "MacBook Pro Speakers");
        play(&mut f, &[3.0, 3.0]);
        assert_eq!(*got.lock().unwrap(), [1.0, 1.0, 2.0, 2.0, 3.0, 3.0]);
        drop(f);
        assert_eq!(fake.0.borrow().closed, [SPEAKERS, AIRPODS, SPEAKERS]);
    }

    #[test]
    fn a_new_default_that_cannot_be_tapped_is_reported_and_retried() {
        let (fake, mut f, got) = follow();
        let t = Instant::now();
        f.tick(t).unwrap();
        fake.set_default(Some(AIRPODS));
        fake.0.borrow_mut().fail = Some("OSStatus 'nope'".into());
        assert!(f.tick(t).is_err());
        assert_eq!(f.now.error.as_deref(), Some("OSStatus 'nope'"));
        assert!(f.open.is_none() && f.sink.is_some(), "the sink is kept for the retry");
        // Too soon to retry.
        f.tick(t + RETRY / 2).unwrap();
        assert_eq!(fake.0.borrow().opened, [SPEAKERS]);
        assert_eq!(f.tick(t + RETRY).unwrap().error, None);
        assert_eq!(fake.0.borrow().opened, [SPEAKERS, AIRPODS]);
        play(&mut f, &[5.0]);
        assert_eq!(*got.lock().unwrap(), [5.0]);
    }

    #[test]
    fn no_default_output_fails_the_start_but_keeps_a_running_tap() {
        let (fake, mut f, _) = follow();
        fake.set_default(None);
        assert_eq!(f.tick(Instant::now()).unwrap_err().kind(), ErrorKind::NotFound);
        assert!(fake.0.borrow().opened.is_empty());

        fake.set_default(Some(SPEAKERS));
        f.tick(Instant::now()).unwrap();
        fake.set_default(None);
        let now = f.tick(Instant::now()).unwrap();
        assert_eq!(now.device, "MacBook Pro Speakers");
        assert!(now.error.is_some());
        assert!(fake.0.borrow().closed.is_empty(), "the old tap keeps running");
    }

    #[test]
    fn the_running_tap_s_health_is_the_error_and_clears() {
        let (fake, mut f, _) = follow();
        f.tick(Instant::now()).unwrap();
        fake.0.borrow_mut().sick = Some("the tap stopped delivering audio".into());
        assert!(f.tick(Instant::now()).unwrap().error.is_some());
        assert_eq!(f.tick(Instant::now()).unwrap().error, None);
    }
}
