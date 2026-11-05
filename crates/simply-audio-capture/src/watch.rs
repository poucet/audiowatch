//! What wakes a follower thread, kept free of the HAL so it is tested with
//! fakes: [`Wake`] (poked by listeners, or the poll passing), the thread
//! that ticks on it ([`spawn`]), and [`Listened`], which keeps a listener on
//! each process's devices so a process that moves to another device is
//! seen at once rather than at the next poll.

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

/// Listeners on one property of process objects: on the HAL, the devices
/// a process plays into; in the tests, a fake.
pub(crate) trait Listen {
    /// Start poking the follower when `object`'s devices change.
    fn add(&mut self, object: u32);
    /// Stop (the process is gone, or the follower is).
    fn remove(&mut self, object: u32);
}

/// A [`Backend`] whose listed processes each carry a [`Listen`]er, kept in
/// line with the list on every look and removed with the follower. Only
/// for a tap that reads the devices processes play into (it lists an
/// [`App::PlayingInto`](crate::App::PlayingInto)); for any other it is the
/// backend as it is.
pub(crate) struct Listened<B, L: Listen> {
    inner: B,
    listen: Option<L>,
    on: BTreeSet<u32>,
}

impl<B: Backend, L: Listen> Listened<B, L> {
    pub(crate) fn new(inner: B, tap: &AppTap, listen: impl FnOnce() -> L) -> Listened<B, L> {
        let listen = tap.needs_devices().then(listen);
        Listened { inner, listen, on: BTreeSet::new() }
    }

    fn sync(&mut self, procs: &[Proc]) {
        let Some(listen) = &mut self.listen else { return };
        let now: BTreeSet<u32> = procs.iter().map(|p| p.object).collect();
        for &gone in self.on.difference(&now) {
            listen.remove(gone);
        }
        for &new in now.difference(&self.on) {
            listen.add(new);
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
        if let Some(listen) = &mut self.listen {
            for &object in &self.on {
                listen.remove(object);
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

    /// The fake HAL's listener table: whom to wake when a process's devices
    /// change. Shared between threads, as the HAL's is.
    #[derive(Clone, Default)]
    struct Table(Arc<Mutex<BTreeMap<u32, Arc<Wake>>>>);

    struct FakeListen {
        table: Table,
        wake: Arc<Wake>,
    }

    impl Listen for FakeListen {
        fn add(&mut self, object: u32) {
            self.table.0.lock().unwrap().insert(object, Arc::clone(&self.wake));
        }
        fn remove(&mut self, object: u32) {
            self.table.0.lock().unwrap().remove(&object);
        }
    }

    impl Table {
        /// The HAL says `object`'s devices changed.
        fn changed(&self, object: u32) {
            if let Some(w) = self.0.lock().unwrap().get(&object) {
                w.poke(false);
            }
        }
        fn listened(&self) -> Vec<u32> {
            self.0.lock().unwrap().keys().copied().collect()
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

    #[test]
    fn a_process_moving_to_a_tussen_device_wakes_the_follower_without_waiting_for_the_poll() {
        let (backend, table) = (Shared::default(), Table::default());
        let music = proc(3, 5, MUSIC, "/x/Music");
        backend.0.lock().unwrap().procs = vec![proc(10, 412, DJAY, DJAY_EXE), music.clone()];
        // A poll far longer than the test may take: only a listener can
        // wake the follower in time.
        let wake = Arc::new(Wake::new(Duration::from_secs(600)));
        let listen = FakeListen { table: table.clone(), wake: Arc::clone(&wake) };
        let listened = Listened::new(backend.clone(), &tussen_tap(), || listen);
        let mut follow = Follow::new(listened, tussen_tap(), ME, Box::new(|_: &[f32], _: u64| {}));
        let thread = spawn("test follower", Arc::clone(&wake), move |now| {
            follow.tick(now);
            Ok(())
        })
        .unwrap();
        assert_eq!(table.listened(), [3, 10]);
        assert_eq!(backend.0.lock().unwrap().opened.len(), 1);

        // Music moves to Tussen In 1, and the HAL says so.
        backend.0.lock().unwrap().procs =
            vec![proc(10, 412, DJAY, DJAY_EXE), plays(music, &[TUSSEN_IN_1])];
        let t = Instant::now();
        table.changed(3);
        while backend.0.lock().unwrap().opened.len() < 2 {
            assert!(t.elapsed() < Duration::from_secs(5), "the follower was not woken");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            backend.0.lock().unwrap().opened.last().unwrap().0,
            [Target::AllExcept(vec![3])]
        );

        // Music exits: its listener goes with it (the next look sees it).
        backend.0.lock().unwrap().procs = vec![proc(10, 412, DJAY, DJAY_EXE)];
        table.changed(3);
        let t = Instant::now();
        while table.listened() != [10] {
            assert!(t.elapsed() < Duration::from_secs(5), "{:?}", table.listened());
            std::thread::sleep(Duration::from_millis(1));
        }
        // Stopping drops the follower on its thread: every listener goes.
        wake.poke(true);
        thread.join().unwrap();
        assert!(table.listened().is_empty());
    }

    #[test]
    fn a_tap_that_does_not_read_devices_listens_to_no_process() {
        let (backend, table) = (Shared::default(), Table::default());
        backend.0.lock().unwrap().procs = vec![proc(10, 412, DJAY, DJAY_EXE)];
        let wake = Arc::new(Wake::new(Duration::from_secs(600)));
        let tap = AppTap::all_except([App::This]);
        let listen = FakeListen { table: table.clone(), wake };
        let mut listened = Listened::new(backend, &tap, || listen);
        listened.processes();
        assert!(table.listened().is_empty());
    }
}
