//! Wiring: CoreAudio listeners, the poll loop, and what to do with a record.
//!
//! ## How the start of output is detected
//!
//! Measured on this machine (macOS 26.6.2) against a short-lived `afplay`:
//!
//! * `kAudioHardwarePropertyProcessObjectList` **does** send change
//!   notifications, and they arrive within a millisecond of a process
//!   connecting to the HAL. A process connects *before* it makes a sound
//!   (~140-200 ms before, for `afplay`), and it is still alive at that instant,
//!   so this is where the executable path is captured.
//! * `kAudioProcessPropertyIsRunningOutput` **does not** send change
//!   notifications. A listener registers successfully and then never fires. It
//!   has to be polled; that is why this loop exists.
//! * `kAudioDevicePropertyDeviceIsRunningSomewhere` **does** send change
//!   notifications, and fires when IO starts on a device — about 90 ms before a
//!   20 ms poll noticed the process's own output flag. It cannot say *which*
//!   process, so it is used as a trigger to look harder, not as an answer.
//!
//! So: the listeners decide *when* to look, the poll is the backstop that
//! guarantees a look happens anyway, and the process object list is what turns
//! "something started" into "this executable started".

use crate::config::Config;
use crate::event::{Disposition, Kind, Record};
use crate::hal::{CoreAudio, HalView};
use crate::logfile::Writer;
use crate::notify;
use crate::reconcile::Reconciler;
use crate::sys::{self, AudioObjectId, PropertyAddress};
use crate::timefmt;
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Whether a record earns a notification, and if not, why not.
///
/// A suppressed event is still logged; this only decides whether to interrupt.
pub fn decide(config: &Config, record: &Record) -> Disposition {
    let wanted = match record.kind {
        Kind::OutputStart => true,
        Kind::InputStart => config.notify_input,
        Kind::Connect => config.notify_connect,
        _ => false,
    };
    if !config.notify || !wanted {
        return Disposition::Quiet;
    }
    match config.allow.allows(record) {
        Some(rule) => Disposition::Allowed(rule.label()),
        None => Disposition::Notified,
    }
}

// ---------------------------------------------------------------------------
// The wake channel between CoreAudio's listener threads and the poll loop.
//
// Listener callbacks are plain `extern "C" fn`s with no state of their own, and
// they arrive concurrently on more than one HAL-internal thread (observed: two).
// They therefore do no work beyond setting a flag, which keeps every property
// read and every write on the single loop thread.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Flags {
    pending: bool,
    /// Set when a *device* reported IO starting or stopping, which is the cue to
    /// poll fast for a moment to find the process responsible.
    device_event: bool,
}

#[derive(Default)]
struct Wake {
    flags: Mutex<Flags>,
    ready: Condvar,
}

fn wake() -> &'static Wake {
    static WAKE: OnceLock<Wake> = OnceLock::new();
    WAKE.get_or_init(Wake::default)
}

fn signal(device_event: bool) {
    if let Ok(mut f) = wake().flags.lock() {
        f.pending = true;
        f.device_event |= device_event;
        wake().ready.notify_all();
    }
}

/// Block until a listener fires or `timeout` elapses. Returns whether a device
/// reported IO activity while we were asleep.
fn wait(timeout: Duration) -> bool {
    let w = wake();
    let Ok(mut flags) = w.flags.lock() else {
        std::thread::sleep(timeout);
        return false;
    };
    if !flags.pending {
        let (guard, _) = match w.ready.wait_timeout(flags, timeout) {
            Ok(pair) => pair,
            Err(poisoned) => poisoned.into_inner(),
        };
        flags = guard;
    }
    let device_event = flags.device_event;
    flags.pending = false;
    flags.device_event = false;
    device_event
}

extern "C" fn on_process_list(
    _object: AudioObjectId,
    _n: u32,
    _addresses: *const PropertyAddress,
    _client: *mut c_void,
) -> sys::OsStatus {
    signal(false);
    0
}

extern "C" fn on_device_activity(
    _object: AudioObjectId,
    _n: u32,
    _addresses: *const PropertyAddress,
    _client: *mut c_void,
) -> sys::OsStatus {
    signal(true);
    0
}

/// The running watcher.
pub struct Watcher {
    config: Config,
    hal: CoreAudio,
    reconciler: Reconciler,
    writer: Writer,
    /// Devices we have already put a listener on; re-registering answers 'nope'.
    listening_devices: HashSet<AudioObjectId>,
    quiet: bool,
}

impl Watcher {
    pub fn new(config: Config, quiet: bool) -> Result<Self, String> {
        let writer = Writer::open(&config.log_path)
            .map_err(|e| format!("cannot open log {}: {e}", config.log_path.display()))?;
        Ok(Self {
            config,
            hal: CoreAudio::new(),
            reconciler: Reconciler::new(),
            writer,
            listening_devices: HashSet::new(),
            quiet,
        })
    }

    fn attach_system_listeners(&self) -> Result<(), String> {
        for selector in [sys::PROP_PROCESS_OBJECT_LIST, sys::PROP_DEVICES] {
            let address = PropertyAddress::global(selector);
            let status = sys::add_listener(sys::SYSTEM_OBJECT, &address, on_process_list);
            if status != 0 {
                return Err(format!(
                    "cannot listen for '{}' changes: status {} ('{}')",
                    sys::fourcc_str(selector),
                    status,
                    sys::fourcc_str(status as u32)
                ));
            }
        }
        Ok(())
    }

    /// Put a `DeviceIsRunningSomewhere` listener on every device we have not
    /// already covered. Called again whenever the device list changes, so a
    /// newly plugged-in interface is covered too.
    fn attach_device_listeners(&mut self) {
        for device in self.hal.device_objects() {
            if !self.listening_devices.insert(device) {
                continue;
            }
            let address = PropertyAddress::global(sys::PROP_DEVICE_IS_RUNNING_SOMEWHERE);
            let status = sys::add_listener(device, &address, on_device_activity);
            if status != 0 && !self.quiet {
                eprintln!(
                    "audiowatch: no activity listener on device '{}': status {}",
                    self.hal.device_name(device),
                    status
                );
            }
        }
    }

    /// Write a record, then notify if it deserves it. The log comes first on
    /// purpose: the notification is the part that is allowed to fail.
    fn emit(&mut self, mut record: Record) {
        record.disposition = decide(&self.config, &record);
        let notify_it = record.disposition == Disposition::Notified;
        if let Err(e) = self.writer.append(&record) {
            eprintln!(
                "audiowatch: cannot write to {}: {e}",
                self.writer.path().display()
            );
        }
        if !self.quiet {
            println!("{}", record.human());
        }
        if notify_it {
            if let Err(e) = notify::post(&notify::message_for(&record)) {
                eprintln!("audiowatch: notification failed: {e}");
            }
        }
    }

    /// One look at the world.
    pub fn step(&mut self) {
        self.attach_device_listeners();
        let records = self.reconciler.reconcile(&self.hal, timefmt::now_millis());
        for record in records {
            self.emit(record);
        }
    }

    /// Run until killed.
    pub fn run(&mut self) -> Result<(), String> {
        self.attach_system_listeners()?;
        self.attach_device_listeners();

        // The baseline pass: record what is already playing without announcing it.
        self.step();

        if !self.quiet {
            println!(
                "audiowatch: watching {} audio processes across {} devices",
                self.reconciler.tracked_count(),
                self.listening_devices.len()
            );
            println!("audiowatch: log {}", self.writer.path().display());
            println!(
                "audiowatch: poll {}ms, {}ms for {}ms after a device reports activity",
                self.config.poll_ms, self.config.fast_poll_ms, self.config.fast_window_ms
            );
            if self.config.allow.is_empty() {
                println!(
                    "audiowatch: no allow rules -- everything that makes a sound will notify you"
                );
            } else {
                println!("audiowatch: {} allow rules loaded", self.config.allow.len());
            }
            println!();
        }

        let mut fast_until = Instant::now();
        loop {
            let interval = if Instant::now() < fast_until {
                self.config.fast_poll_ms
            } else {
                self.config.poll_ms
            };
            let device_event = wait(Duration::from_millis(interval));
            if device_event {
                fast_until = Instant::now() + Duration::from_millis(self.config.fast_window_ms);
            }
            self.step();
        }
    }
}

/// `audiowatch --now`: what is making sound at this instant.
pub fn print_now(hal: &CoreAudio) {
    let mut any = false;
    for object in hal.process_objects() {
        let Some(running) = hal.running(object) else {
            continue;
        };
        if !running.output && !running.input {
            continue;
        }
        any = true;
        let what = match (running.output, running.input) {
            (true, true) => "output+input",
            (true, false) => "output",
            _ => "input",
        };
        let details = hal.details(object).unwrap_or_default();
        println!(
            "{:<12} pid {:<7} {}",
            what,
            details.pid,
            details
                .exe
                .or(details.bundle)
                .unwrap_or_else(|| format!("<pid {} is gone>", details.pid))
        );
        if !details.parents.is_empty() {
            println!(
                "{:>13}from {}",
                "",
                crate::proc::render(&details.parents, crate::proc::MAX_DEPTH)
            );
        }
        if !details.devices.is_empty() {
            println!("{:>13}on {}", "", details.devices.join(", "));
        }
    }
    if !any {
        println!("nothing is running audio IO right now");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::{Field, Rule};

    fn cfg() -> Config {
        Config::default()
    }

    fn rec(kind: Kind, exe: &str, bundle: Option<&str>) -> Record {
        let mut r = Record::new(0, kind, 1);
        r.exe = Some(exe.to_string());
        r.bundle = bundle.map(str::to_string);
        r
    }

    #[test]
    fn an_unknown_process_starting_output_earns_a_notification() {
        assert_eq!(
            decide(&cfg(), &rec(Kind::OutputStart, "/usr/bin/mystery", None)),
            Disposition::Notified
        );
    }

    #[test]
    fn an_allow_listed_process_is_suppressed_but_the_rule_is_recorded() {
        let d = decide(
            &cfg(),
            &rec(
                Kind::OutputStart,
                "/Applications/Spotify.app/Contents/MacOS/Spotify",
                Some("com.spotify.client"),
            ),
        );
        match d {
            Disposition::Allowed(rule) => assert!(rule.contains("spotify"), "{rule}"),
            other => panic!("expected suppression, got {other:?}"),
        }
    }

    #[test]
    fn connect_and_stop_events_never_notify_by_default() {
        for kind in [
            Kind::Connect,
            Kind::Disconnect,
            Kind::OutputStop,
            Kind::Baseline,
            Kind::InputStart,
        ] {
            assert_eq!(
                decide(&cfg(), &rec(kind, "/usr/bin/mystery", None)),
                Disposition::Quiet,
                "{} should be quiet by default",
                kind.wire()
            );
        }
    }

    #[test]
    fn turning_on_connect_notifications_makes_connects_notify() {
        let mut c = cfg();
        c.notify_connect = true;
        assert_eq!(
            decide(&c, &rec(Kind::Connect, "/usr/bin/mystery", None)),
            Disposition::Notified
        );
        // ... and the allow-list still applies to them.
        c.allow.push(Rule::new(Field::Exe, "mystery"));
        assert!(matches!(
            decide(&c, &rec(Kind::Connect, "/usr/bin/mystery", None)),
            Disposition::Allowed(_)
        ));
    }

    #[test]
    fn notify_off_silences_everything_without_touching_the_log() {
        let mut c = cfg();
        c.notify = false;
        assert_eq!(
            decide(&c, &rec(Kind::OutputStart, "/usr/bin/mystery", None)),
            Disposition::Quiet
        );
    }

    /// The wake channel is a process-wide singleton -- a real run has exactly one
    /// poll loop -- so everything that touches it lives in a single test rather
    /// than in several that would race each other for its flags.
    #[test]
    fn the_wake_channel_carries_listener_events_to_the_poll_loop() {
        // A listener firing releases the loop well before its timeout.
        let start = Instant::now();
        let waker = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(20));
            signal(true);
        });
        assert!(
            wait(Duration::from_secs(5)),
            "the device flag should survive the handoff"
        );
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "wait should not have timed out"
        );
        waker.join().unwrap();

        // The flags are consumed, so the next wait sleeps for its whole timeout
        // rather than spinning.
        let t = Instant::now();
        assert!(!wait(Duration::from_millis(50)));
        assert!(t.elapsed() >= Duration::from_millis(40));

        // A process-list event wakes the loop but does not put it in fast mode.
        signal(false);
        let t = Instant::now();
        assert!(
            !wait(Duration::from_millis(500)),
            "a process-list event is not a device event"
        );
        assert!(
            t.elapsed() < Duration::from_millis(400),
            "a pending process-list event should still wake the loop at once"
        );

        // Two HAL threads delivering at once is the observed behaviour, and these
        // are exactly the functions CoreAudio calls.
        let a = std::thread::spawn(|| {
            for _ in 0..200 {
                on_process_list(1, 1, std::ptr::null(), std::ptr::null_mut());
            }
        });
        let b = std::thread::spawn(|| {
            for _ in 0..200 {
                on_device_activity(1, 1, std::ptr::null(), std::ptr::null_mut());
            }
        });
        a.join().unwrap();
        b.join().unwrap();
        assert!(
            wait(Duration::from_millis(10)),
            "device event should be pending"
        );
    }
}
