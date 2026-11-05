//! What wakes a follower thread, kept free of the HAL so it is tested with
//! fakes: [`Wake`] (poked by listeners, or the poll passing), the thread
//! that ticks on it ([`spawn`]), and [`Listened`], which keeps listeners on
//! each process so one that starts or stops playing, or moves to another
//! device, is seen at once rather than at the next poll.

use std::collections::BTreeSet;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::apps::{AppTap, Backend, Mute, Proc, Target};
use crate::{Error, ErrorKind, Sink};

/// Wakes a follower: from HAL listeners (a process came or went or moved,
/// a device list changed) or from its owner (stop); else every `poll`.
pub(crate) struct Wake {
    flags: Mutex<(bool, bool)>, // (pending, stop)
    ready: Condvar,
    poll: Duration,
}

impl Wake {
    pub(crate) fn new(poll: Duration) -> Wake {
        Wake { flags: Mutex::new((false, false)), ready: Condvar::new(), poll }
    }

    pub(crate) fn poke(&self, stop: bool) {
        let mut f = self.flags.lock().unwrap_or_else(|e| e.into_inner());
        f.0 = true;
        f.1 |= stop;
        self.ready.notify_all();
    }

    /// Until poked or the poll passes; true = stop.
    fn wait(&self) -> bool {
        let mut f = self.flags.lock().unwrap_or_else(|e| e.into_inner());
        if !f.0 {
            f = self.ready.wait_timeout(f, self.poll).unwrap_or_else(|e| e.into_inner()).0;
        }
        f.0 = false;
        f.1
    }
}

/// Run `tick` on a thread called `name`: once, then on every poke of
/// `wake` (or its poll), until it is poked to stop. Returns after the
/// first tick: its error is returned (and the thread ends); later ones are
/// the follower's own to report. Dropping `tick` at the end drops the
/// follower, on its own thread.
pub(crate) fn spawn<T>(name: &str, wake: Arc<Wake>, mut tick: T) -> Result<JoinHandle<()>, Error>
where
    T: FnMut(Instant) -> Result<(), Error> + Send + 'static,
{
    let (first_tx, first_rx) = mpsc::channel::<Result<(), Error>>();
    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let first = tick(Instant::now());
            let failed = first.is_err();
            let _ = first_tx.send(first);
            if failed {
                return;
            }
            while !wake.wait() {
                let _ = tick(Instant::now());
            }
        })
        .map_err(|e| {
            Error::new(ErrorKind::Device, format!("cannot start the {name} thread: {e}"))
        })?;
    match first_rx.recv() {
        Ok(Ok(())) => Ok(thread),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(Error::new(ErrorKind::Device, format!("the {name} thread died before reporting")))
        }
    }
}

/// A process object property whose change wakes an app follower.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Prop {
    /// It started or stopped playing (`kAudioProcessPropertyIsRunningOutput`):
    /// a new app's channel starts at once rather than at the next poll.
    Playing,
    /// It moved to other output devices (`kAudioProcessPropertyDevices`).
    Devices,
}

/// Listeners on process object properties: on the HAL, the real ones; in
/// the tests, a fake.
pub(crate) trait Listen {
    /// Start poking the follower when `object`'s `prop` changes.
    fn add(&mut self, object: u32, prop: Prop);
    /// Stop (the process is gone, or the follower is).
    fn remove(&mut self, object: u32, prop: Prop);
}

/// A [`Backend`] whose listed processes each carry [`Listen`]ers, kept in
/// line with the list on every look and removed with the follower: on
/// [`Prop::Playing`] always, and on [`Prop::Devices`] for a tap that reads
/// the devices processes play into (it lists an
/// [`App::PlayingInto`](crate::App::PlayingInto)).
pub(crate) struct Listened<B, L: Listen> {
    inner: B,
    listen: L,
    props: Vec<Prop>,
    on: BTreeSet<u32>,
}

impl<B: Backend, L: Listen> Listened<B, L> {
    pub(crate) fn new(inner: B, tap: &AppTap, listen: L) -> Listened<B, L> {
        let mut props = vec![Prop::Playing];
        if tap.needs_devices() {
            props.push(Prop::Devices);
        }
        Listened { inner, listen, props, on: BTreeSet::new() }
    }

    fn sync(&mut self, procs: &[Proc]) {
        let now: BTreeSet<u32> = procs.iter().map(|p| p.object).collect();
        for &gone in self.on.difference(&now) {
            for &prop in &self.props {
                self.listen.remove(gone, prop);
            }
        }
        for &new in now.difference(&self.on) {
            for &prop in &self.props {
                self.listen.add(new, prop);
            }
        }
        self.on = now;
    }
}

impl<B: Backend, L: Listen> Backend for Listened<B, L> {
    type Tap = B::Tap;
    fn processes(&mut self) -> Vec<Proc> {
        let procs = self.inner.processes();
        self.sync(&procs);
        procs
    }
    fn translate(&mut self, pid: i32) -> Option<u32> {
        self.inner.translate(pid)
    }
    fn open(
        &mut self,
        targets: &[Target],
        mute: Mute,
        sink: Sink,
    ) -> Result<(B::Tap, u32), (Error, Sink)> {
        self.inner.open(targets, mute, sink)
    }
    fn close(&mut self, tap: B::Tap) -> Sink {
        self.inner.close(tap)
    }
    fn health(&mut self, tap: &B::Tap, playing: bool) -> Option<String> {
        self.inner.health(tap, playing)
    }
}

impl<B, L: Listen> Drop for Listened<B, L> {
    fn drop(&mut self) {
        for &object in &self.on {
            for &prop in &self.props {
                self.listen.remove(object, prop);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::tests::*;
    use crate::apps::{App, Follow};
    use std::collections::BTreeMap;

    /// The fake HAL's listener table: whom to wake when a process's
    /// property changes. Shared between threads, as the HAL's is.
    #[derive(Clone, Default)]
    struct Table(Arc<Mutex<Listeners>>);

    /// Whom each process's property wakes.
    type Listeners = BTreeMap<(u32, Prop), Arc<Wake>>;

    struct FakeListen {
        table: Table,
        wake: Arc<Wake>,
    }

    impl Listen for FakeListen {
        fn add(&mut self, object: u32, prop: Prop) {
            self.table.0.lock().unwrap().insert((object, prop), Arc::clone(&self.wake));
        }
        fn remove(&mut self, object: u32, prop: Prop) {
            self.table.0.lock().unwrap().remove(&(object, prop));
        }
    }

    impl Table {
        /// The HAL says `object`'s `prop` changed.
        fn changed(&self, object: u32, prop: Prop) {
            if let Some(w) = self.0.lock().unwrap().get(&(object, prop)) {
                w.poke(false);
            }
        }
        /// The processes listened to on `prop`.
        fn listened(&self, prop: Prop) -> Vec<u32> {
            self.0.lock().unwrap().keys().filter(|k| k.1 == prop).map(|k| k.0).collect()
        }
    }

    /// The fake backend crosses to the follower thread; its script stays
    /// readable here.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Script>>);

    impl Backend for Shared {
        type Tap = u32;
        fn processes(&mut self) -> Vec<Proc> {
            self.0.lock().unwrap().procs.clone()
        }
        fn translate(&mut self, pid: i32) -> Option<u32> {
            self.0.lock().unwrap().procs.iter().find(|p| p.pid == pid).map(|p| p.object)
        }
        fn open(
            &mut self,
            t: &[Target],
            mute: Mute,
            sink: Sink,
        ) -> Result<(u32, u32), (Error, Sink)> {
            let mut s = self.0.lock().unwrap();
            s.opened.push((t.to_vec(), mute));
            s.next += 1;
            s.held = Some(sink);
            Ok((s.next, 48_000))
        }
        fn close(&mut self, _: u32) -> Sink {
            self.0.lock().unwrap().held.take().expect("a tap holds the sink")
        }
        fn health(&mut self, _: &u32, _: bool) -> Option<String> {
            None
        }
    }

    #[derive(Default)]
    struct Script {
        procs: Vec<Proc>,
        opened: Vec<(Vec<Target>, Mute)>,
        next: u32,
        held: Option<Sink>,
    }

    fn tussen_tap() -> AppTap {
        AppTap::all_except([App::This, App::PlayingInto(TUSSEN.into())])
    }

    /// A poll far longer than a test may take: only a listener can wake
    /// the follower in time.
    fn no_poll() -> Arc<Wake> {
        Arc::new(Wake::new(Duration::from_secs(600)))
    }

    /// Until `done` holds, or fail with `why` after 5 s.
    fn soon(why: &str, done: impl Fn() -> bool) {
        let t = Instant::now();
        while !done() {
            assert!(t.elapsed() < Duration::from_secs(5), "{why}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_process_moving_to_a_tussen_device_wakes_the_follower_without_waiting_for_the_poll() {
        let (backend, table, wake) = (Shared::default(), Table::default(), no_poll());
        let music = proc(3, 5, MUSIC, "/x/Music");
        backend.0.lock().unwrap().procs = vec![proc(10, 412, DJAY, DJAY_EXE), music.clone()];
        let listen = FakeListen { table: table.clone(), wake: Arc::clone(&wake) };
        let listened = Listened::new(backend.clone(), &tussen_tap(), listen);
        let mut follow = Follow::new(listened, tussen_tap(), ME, Box::new(|_: &[f32], _: u64| {}));
        let thread = spawn("test follower", Arc::clone(&wake), move |now| {
            follow.tick(now);
            Ok(())
        })
        .unwrap();
        assert_eq!(table.listened(Prop::Devices), [3, 10]);
        assert_eq!(backend.0.lock().unwrap().opened.len(), 1);

        // Music moves to Tussen In 1, and the HAL says so.
        backend.0.lock().unwrap().procs =
            vec![proc(10, 412, DJAY, DJAY_EXE), plays(music, &[TUSSEN_IN_1])];
        table.changed(3, Prop::Devices);
        soon("the follower was not woken", || backend.0.lock().unwrap().opened.len() == 2);
        assert_eq!(
            backend.0.lock().unwrap().opened.last().unwrap().0,
            [Target::AllExcept(vec![3])]
        );

        // Music exits: its listeners go with it (the next look sees it).
        backend.0.lock().unwrap().procs = vec![proc(10, 412, DJAY, DJAY_EXE)];
        table.changed(3, Prop::Devices);
        soon("music's listeners stayed", || {
            table.listened(Prop::Devices) == [10] && table.listened(Prop::Playing) == [10]
        });
        // Stopping drops the follower on its thread: every listener goes.
        wake.poke(true);
        thread.join().unwrap();
        assert!(table.0.lock().unwrap().is_empty());
    }

    #[test]
    fn an_app_that_starts_playing_gets_its_channel_without_waiting_for_the_poll() {
        use crate::channels::{AppSinks, Split};
        struct Quiet;
        impl AppSinks for Quiet {
            type Sink = fn(&[f32], u64);
            fn started(&mut self, _: &crate::AppInfo) -> Self::Sink {
                |_, _| {}
            }
            fn stopped(&mut self, _: &crate::AppInfo) {}
        }
        let (backend, table, wake) = (Shared::default(), Table::default(), no_poll());
        let music = proc(3, 5, MUSIC, "/x/Music");
        backend.0.lock().unwrap().procs = vec![Proc { playing: false, ..music.clone() }];
        let tap = AppTap::all_except([App::This]);
        let listen = FakeListen { table: table.clone(), wake: Arc::clone(&wake) };
        let mut split = Split::new(Listened::new(backend.clone(), &tap, listen), tap, ME, Quiet);
        let thread = spawn("test channels", Arc::clone(&wake), move |now| {
            split.tick(now);
            Ok(())
        })
        .unwrap();
        // Listened to for playing, not for devices (nothing reads them).
        assert_eq!(table.listened(Prop::Playing), [3]);
        assert!(table.listened(Prop::Devices).is_empty());
        assert!(backend.0.lock().unwrap().opened.is_empty());

        // Music starts playing, and the HAL says so.
        backend.0.lock().unwrap().procs = vec![music];
        table.changed(3, Prop::Playing);
        soon("the follower was not woken", || backend.0.lock().unwrap().opened.len() == 1);
        assert_eq!(backend.0.lock().unwrap().opened[0].0, [Target::Processes(vec![3])]);
        wake.poke(true);
        thread.join().unwrap();
        assert!(table.0.lock().unwrap().is_empty());
    }
}
