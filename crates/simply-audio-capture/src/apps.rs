//! Tapping applications rather than devices: which processes a tap wants,
//! and the state machine that follows them as they come and go.
//!
//! Everything here is pure. The HAL is reached through [`Backend`], so the
//! whole follow-the-app logic (waiting, attaching, re-creating the tap when
//! the app restarts, reporting a target that disappeared) is tested against
//! a fake, the same way audiowatch's watcher tests its reconciler against a
//! fake `HalView`.

use std::fmt;
use std::time::{Duration, Instant};

use crate::{Error, Sink};

/// One application to tap, or to leave out of a tap.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum App {
    /// A bundle id, `com.algoriddim.djay-iphone-free`. Also matches the app's
    /// helpers, whose ids extend it (`com.google.Chrome.helper`).
    Bundle(String),
    /// An executable's file name, `djay Pro` (case-insensitive; a trailing
    /// `.app` is ignored, so `djay Pro.app` works too).
    Executable(String),
    /// One process. It does not follow a restart: a new process has a new
    /// pid.
    Pid(i32),
    /// The process calling this crate (for "everything except me").
    This,
    /// Every process playing into an output device whose UID starts with
    /// this prefix, followed as processes start and stop playing there.
    /// In [`Apps::AllExcept`] it keeps a program's own virtual devices out
    /// of "everything": an app playing into one reaches the program
    /// through that device, so the tap would carry it twice. Only that
    /// output is left out: what the app plays into other devices at the
    /// same time is still tapped there.
    PlayingInto(String),
}

impl App {
    /// A bundle id when `s` looks like one (reverse-DNS: a dot, no spaces
    /// or slashes), a pid when it's all digits, else an executable name.
    pub fn parse(s: &str) -> App {
        let s = s.trim();
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(pid) = s.parse() {
                return App::Pid(pid);
            }
        }
        let reverse_dns = s.contains('.')
            && !s.contains([' ', '/'])
            && !s.to_ascii_lowercase().ends_with(".app")
            && s.split('.').all(|p| !p.is_empty());
        if reverse_dns {
            App::Bundle(s.to_owned())
        } else {
            App::Executable(s.to_owned())
        }
    }

    /// Whether process `p` is this app. `this` is the caller's own pid.
    pub(crate) fn matches(&self, p: &Proc, this: i32) -> bool {
        match self {
            App::Bundle(b) => p.bundle.as_deref().is_some_and(|pb| {
                pb.eq_ignore_ascii_case(b)
                    || (pb.len() > b.len()
                        && pb.as_bytes()[b.len()] == b'.'
                        && pb[..b.len()].eq_ignore_ascii_case(b))
            }),
            App::Executable(n) => {
                let n = n.strip_suffix(".app").unwrap_or(n);
                p.exe.as_deref().map(file_name).is_some_and(|f| f.eq_ignore_ascii_case(n))
            }
            App::Pid(pid) => p.pid == *pid,
            App::This => p.pid == this,
            App::PlayingInto(_) => p.devices.iter().any(|uid| self.covers_device(uid)),
        }
    }

    /// Whether this app names the output device `uid` (an
    /// [`App::PlayingInto`] whose prefix it starts with).
    pub(crate) fn covers_device(&self, uid: &str) -> bool {
        matches!(self, App::PlayingInto(prefix) if uid.starts_with(prefix.as_str()))
    }

    /// Whether matching this app needs the devices each process plays into.
    pub(crate) fn needs_devices(&self) -> bool {
        matches!(self, App::PlayingInto(_))
    }
}

impl fmt::Display for App {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            App::Bundle(s) | App::Executable(s) => f.write_str(s),
            App::Pid(p) => write!(f, "pid {p}"),
            App::This => f.write_str("this process"),
            App::PlayingInto(prefix) => write!(f, "whatever plays into {prefix}*"),
        }
    }
}

/// Which processes go into the tap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Apps {
    /// These apps only, mixed to stereo. While none of them is running the
    /// capture waits for them.
    Only(Vec<App>),
    /// Every process on the Mac except these, mixed to stereo (leave
    /// yourself out to avoid feedback). Always tapping; the exclusions are
    /// re-applied as the excluded apps come and go.
    AllExcept(Vec<App>),
}

/// What the tapped app's own output does while it is tapped
/// (`CATapMuteBehavior`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mute {
    /// The app keeps playing to its device as well (the room hears it).
    #[default]
    Unmuted,
    /// Captured silently: the app's audio reaches the tap and nothing else.
    Muted,
    /// Plays to its device until the tap is read, then only to the tap.
    /// (This crate reads the tap as soon as it is made, so in practice the
    /// same as `Muted`.)
    MutedWhenTapped,
}

/// A process tap: which apps, and whether they stay audible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppTap {
    pub apps: Apps,
    pub mute: Mute,
}

impl AppTap {
    /// Tap these apps only, unmuted.
    pub fn only(apps: impl IntoIterator<Item = App>) -> AppTap {
        AppTap { apps: Apps::Only(apps.into_iter().collect()), mute: Mute::Unmuted }
    }

    /// Tap everything except these apps, unmuted.
    pub fn all_except(apps: impl IntoIterator<Item = App>) -> AppTap {
        AppTap { apps: Apps::AllExcept(apps.into_iter().collect()), mute: Mute::Unmuted }
    }

    pub fn with_mute(self, mute: Mute) -> AppTap {
        AppTap { mute, ..self }
    }

    pub(crate) fn list(&self) -> &[App] {
        match &self.apps {
            Apps::Only(a) | Apps::AllExcept(a) => a,
        }
    }

    pub(crate) fn names(&self) -> String {
        join(self.list().iter().map(App::to_string))
    }

    /// Whether following this tap needs the devices each process plays
    /// into (an [`App::PlayingInto`] is listed).
    pub(crate) fn needs_devices(&self) -> bool {
        self.list().iter().any(App::needs_devices)
    }
}

/// One process the HAL knows as an audio client, for a picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppInfo {
    pub pid: i32,
    pub bundle: Option<String>,
    /// The executable's path, when it could be read.
    pub exe: Option<String>,
    /// What to show: the executable's file name, else the bundle id, else
    /// the pid.
    pub name: String,
    /// Playing audio right now.
    pub playing: bool,
    /// The process calling this crate.
    pub this: bool,
}

impl AppInfo {
    /// The [`App`] that picks this one out and follows it across restarts:
    /// its bundle id, else its executable name, else its pid.
    pub fn app(&self) -> App {
        match (&self.bundle, &self.exe) {
            (Some(b), _) => App::Bundle(b.clone()),
            (None, Some(e)) => App::Executable(file_name(e).to_owned()),
            (None, None) => App::Pid(self.pid),
        }
    }

    pub(crate) fn of(p: &Proc, this: i32) -> AppInfo {
        let name = match (&p.exe, &p.bundle) {
            (Some(e), _) => file_name(e).to_owned(),
            (None, Some(b)) => b.clone(),
            (None, None) => format!("pid {}", p.pid),
        };
        AppInfo {
            pid: p.pid,
            bundle: p.bundle.clone(),
            exe: p.exe.clone(),
            name,
            playing: p.playing,
            this: p.pid == this,
        }
    }
}

/// Where an [`AppCapture`](crate::AppCapture) is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppStatus {
    /// None of the apps is running (an [`Apps::Only`] tap). `reason` says
    /// what disappeared, when one was being tapped.
    Waiting { waiting_for: String, reason: Option<String> },
    /// Audio is flowing. `all_except`: which kind of tap it is, an
    /// [`Apps::AllExcept`] one (true) or an [`Apps::Only`] one (false).
    /// `processes`: the ones tapped ([`Apps::Only`]) or the ones left out
    /// whole ([`Apps::AllExcept`]; one left out of some devices only is
    /// still heard on the others, so it is not listed). `missing`: the apps
    /// asked for that aren't running (yet).
    Tapping { all_except: bool, processes: Vec<AppInfo>, missing: Vec<String>, device_rate: u32 },
    /// The tap could not be made; it is retried every few seconds.
    Failed { error: String },
}

impl AppStatus {
    pub fn is_tapping(&self) -> bool {
        matches!(self, AppStatus::Tapping { .. })
    }
}

impl fmt::Display for AppStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppStatus::Waiting { waiting_for, reason: None } => {
                write!(f, "waiting for {waiting_for}")
            }
            AppStatus::Waiting { waiting_for, reason: Some(r) } => {
                write!(f, "{r}; waiting for {waiting_for}")
            }
            AppStatus::Tapping { all_except: true, processes, device_rate, .. }
                if processes.is_empty() =>
            {
                write!(f, "tapping everything at {device_rate} Hz")
            }
            AppStatus::Tapping { all_except: true, processes, device_rate, .. } => {
                write!(f, "tapping everything except {} at {device_rate} Hz", describe(processes))
            }
            AppStatus::Tapping { all_except: false, processes, device_rate, .. } => {
                write!(f, "tapping {} at {device_rate} Hz", describe(processes))
            }
            AppStatus::Failed { error } => write!(f, "tap failed: {error}"),
        }
    }
}

/// One HAL process object as read on a tick.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Proc {
    /// The HAL's AudioObjectID for the process (what a tap names).
    pub object: u32,
    pub pid: i32,
    pub bundle: Option<String>,
    pub exe: Option<String>,
    pub playing: bool,
    /// The UIDs of the output devices the process plays into. Read only
    /// for a tap that lists an [`App::PlayingInto`]; empty otherwise.
    pub devices: Vec<String>,
}

/// One tap the HAL is asked to make. A capture is one or more of them,
/// mixed in one aggregate.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Target {
    /// Stereo mixdown of these process objects.
    Processes(Vec<u32>),
    /// Stereo mixdown of everything except these process objects.
    AllExcept(Vec<u32>),
    /// Stereo mixdown of what these process objects play into the output
    /// device with this UID, and nothing they play elsewhere.
    OnDevice(Vec<u32>, String),
}

/// A process a tap takes in: all of it, or (`devices`) only what it plays
/// into these output devices.
#[derive(Clone, Debug)]
pub(crate) struct Tapped<'a> {
    pub proc: &'a Proc,
    /// `None`: all of the process. `Some`: only its output into these
    /// devices (by UID); the rest of it is left out.
    pub devices: Option<Vec<String>>,
}

/// The side effects [`Follow`] needs. The real one (macOS) makes process
/// taps; the tests' one is a script.
pub(crate) trait Backend {
    type Tap;
    /// Every process object the HAL lists now.
    fn processes(&mut self) -> Vec<Proc>;
    /// `kAudioHardwarePropertyTranslatePIDToProcessObject`: the process
    /// object of `pid`, if it is an audio client.
    fn translate(&mut self, pid: i32) -> Option<u32>;
    /// Make the taps `targets` (one or more, mixed) and start them into
    /// `sink`; on failure the sink comes back.
    fn open(
        &mut self,
        targets: &[Target],
        mute: Mute,
        sink: Sink,
    ) -> Result<(Self::Tap, u32), (Error, Sink)>;
    /// Stop and destroy a tap; its sink comes back.
    fn close(&mut self, tap: Self::Tap) -> Sink;
    /// Why a running tap is no longer good (its clock device vanished, it
    /// stopped calling back), if it isn't. A tap of processes none of which
    /// is `playing` is not called back at all (measured), so its silence is
    /// not a stall.
    fn health(&mut self, tap: &Self::Tap, playing: bool) -> Option<String>;
}

/// How long a failed tap waits before it is tried again.
pub(crate) const RETRY: Duration = Duration::from_secs(3);

struct Open<T> {
    tap: T,
    key: Vec<Target>,
    procs: Vec<AppInfo>,
}

/// Follows an [`AppTap`]'s apps: one [`tick`](Follow::tick) per HAL change
/// or poll.
pub(crate) struct Follow<B: Backend> {
    backend: B,
    tap: AppTap,
    this: i32,
    /// The sink, while no tap holds it.
    sink: Option<Sink>,
    open: Option<Open<B::Tap>>,
    status: AppStatus,
    /// The last targets that failed to open, and when.
    failed: Option<(Vec<Target>, Instant)>,
}

impl<B: Backend> Follow<B> {
    pub(crate) fn new(backend: B, tap: AppTap, this: i32, sink: Sink) -> Follow<B> {
        let status = AppStatus::Waiting { waiting_for: tap.names(), reason: None };
        Follow { backend, tap, this, sink: Some(sink), open: None, status, failed: None }
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> &AppStatus {
        &self.status
    }

    /// Look at the HAL and bring the tap in line with it.
    pub(crate) fn tick(&mut self, now: Instant) -> &AppStatus {
        let procs = self.backend.processes();
        let (matched, missing) = resolve(&mut self.backend, &self.tap, self.this, &procs);
        let taken = tapped(&self.tap, self.this, &procs, &matched);
        let key = targets(&self.tap.apps, &matched, &taken);
        let want = match self.tap.apps {
            Apps::Only(_) => !matched.is_empty(),
            Apps::AllExcept(_) => true,
        };
        // Only: the ones tapped. AllExcept: the ones left out whole (one
        // left out only from some devices is still heard on the others).
        let only = matches!(self.tap.apps, Apps::Only(_));
        let infos: Vec<AppInfo> = matched
            .iter()
            .filter(|p| only || !taken.iter().any(|t| t.proc.object == p.object))
            .map(|p| AppInfo::of(p, self.this))
            .collect();

        let mut reason = match &self.status {
            AppStatus::Waiting { reason, .. } => reason.clone(),
            _ => None,
        };
        if let Some(open) = &self.open {
            // A paused app's tap is not called back: that silence is not
            // a stall, so it is not re-made for it.
            let playing = taken.iter().any(|t| t.proc.playing);
            let problem = self.backend.health(&open.tap, playing);
            if problem.is_none() && want && open.key == key {
                if let AppStatus::Tapping { missing: m, .. } = &mut self.status {
                    *m = missing;
                }
                return &self.status;
            }
            let open = self.open.take().expect("checked above");
            if !want {
                reason =
                    Some(problem.unwrap_or_else(|| format!("{} exited", describe(&open.procs))));
            }
            self.sink = Some(self.backend.close(open.tap));
        }
        if !want {
            self.status = AppStatus::Waiting { waiting_for: self.tap.names(), reason };
            return &self.status;
        }
        if let Some((k, at)) = &self.failed {
            if *k == key && now.duration_since(*at) < RETRY {
                return &self.status;
            }
        }
        let sink = self.sink.take().expect("no tap holds the sink");
        match self.backend.open(&key, self.tap.mute, sink) {
            Ok((tap, device_rate)) => {
                self.failed = None;
                self.status = AppStatus::Tapping {
                    all_except: !only,
                    processes: infos.clone(),
                    missing,
                    device_rate,
                };
                self.open = Some(Open { tap, key, procs: infos });
            }
            Err((e, sink)) => {
                self.sink = Some(sink);
                self.failed = Some((key, now));
                self.status = AppStatus::Failed { error: e.to_string() };
            }
        }
        &self.status
    }

    /// Stop the tap, if one runs; the sink is dropped with the follower.
    pub(crate) fn stop(&mut self) {
        if let Some(open) = self.open.take() {
            self.sink = Some(self.backend.close(open.tap));
        }
    }
}

impl<B: Backend> Drop for Follow<B> {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The processes `tap` names, and the apps it asks for that aren't running.
pub(crate) fn resolve<B: Backend>(
    backend: &mut B,
    tap: &AppTap,
    this: i32,
    procs: &[Proc],
) -> (Vec<Proc>, Vec<String>) {
    let mut matched: Vec<Proc> = Vec::new();
    let mut missing = Vec::new();
    for app in tap.list() {
        let found: Vec<Proc> = match app {
            // A pid goes through the HAL's own translation; the listed
            // process carries the identity, when it is listed.
            App::Pid(pid) => by_pid(backend, *pid, procs),
            App::This => by_pid(backend, this, procs),
            _ => procs.iter().filter(|p| app.matches(p, this)).cloned().collect(),
        };
        if found.is_empty() {
            missing.push(app.to_string());
        }
        for p in found {
            if !matched.iter().any(|m| m.object == p.object) {
                matched.push(p);
            }
        }
    }
    (matched, missing)
}

/// The processes a tap takes in, given what [`resolve`] matched: the
/// matched ones for [`Apps::Only`], every other one for [`Apps::AllExcept`].
///
/// One exception for [`Apps::AllExcept`]: a process left out only because
/// it plays into an [`App::PlayingInto`] device, that also plays into
/// other devices, is taken in on those others ([`Tapped::devices`]). Its
/// output into the left-out devices is all that is left out.
pub(crate) fn tapped<'a>(
    tap: &AppTap,
    this: i32,
    procs: &'a [Proc],
    matched: &'a [Proc],
) -> Vec<Tapped<'a>> {
    let whole = |p: &'a Proc| Tapped { proc: p, devices: None };
    let apps = match &tap.apps {
        Apps::Only(_) => return matched.iter().map(whole).collect(),
        Apps::AllExcept(apps) => apps,
    };
    let mut taken: Vec<Tapped<'a>> =
        procs.iter().filter(|p| !matched.iter().any(|m| m.object == p.object)).map(whole).collect();
    for p in matched {
        // Left out by who it is (`This`, a bundle, a pid): all of it. And
        // the follower's own process: the HAL counts the clock device of
        // the aggregates it makes as a device it plays into (measured), so
        // its device list moves with every re-made tap; scoping it would
        // re-make the tap on every look.
        if p.pid == this || apps.iter().any(|a| !a.needs_devices() && a.matches(p, this)) {
            continue;
        }
        let mut kept: Vec<String> = p
            .devices
            .iter()
            .filter(|uid| !apps.iter().any(|a| a.covers_device(uid)))
            .cloned()
            .collect();
        kept.sort_unstable();
        kept.dedup();
        if !kept.is_empty() {
            taken.push(Tapped { proc: p, devices: Some(kept) });
        }
    }
    taken
}

/// The taps that take in `taken`: one for [`Apps::Only`]; for
/// [`Apps::AllExcept`], the global one leaving out every matched process,
/// plus one per device that a partly left-out process still plays into.
/// Sorted, so it is the key that says whether the taps must change.
pub(crate) fn targets(apps: &Apps, matched: &[Proc], taken: &[Tapped]) -> Vec<Target> {
    let mut objects: Vec<u32> = matched.iter().map(|p| p.object).collect();
    objects.sort_unstable();
    objects.dedup();
    match apps {
        Apps::Only(_) => vec![Target::Processes(objects)],
        Apps::AllExcept(_) => {
            let mut on: std::collections::BTreeMap<&str, Vec<u32>> = Default::default();
            for t in taken {
                for uid in t.devices.iter().flatten() {
                    on.entry(uid.as_str()).or_default().push(t.proc.object);
                }
            }
            let mut v = vec![Target::AllExcept(objects)];
            v.extend(on.into_iter().map(|(uid, mut o)| {
                o.sort_unstable();
                o.dedup();
                Target::OnDevice(o, uid.to_owned())
            }));
            v
        }
    }
}

/// The taps of one process's own channel: all of it, or what it plays into
/// each of `devices`.
pub(crate) fn own_targets(t: &Tapped) -> Vec<Target> {
    match &t.devices {
        None => vec![Target::Processes(vec![t.proc.object])],
        Some(ds) => {
            ds.iter().map(|uid| Target::OnDevice(vec![t.proc.object], uid.clone())).collect()
        }
    }
}

fn by_pid<B: Backend>(backend: &mut B, pid: i32, procs: &[Proc]) -> Vec<Proc> {
    match backend.translate(pid) {
        Some(object) => vec![procs.iter().find(|p| p.object == object).cloned().unwrap_or(Proc {
            object,
            pid,
            ..Proc::default()
        })],
        None => Vec::new(),
    }
}

pub(crate) fn describe(procs: &[AppInfo]) -> String {
    join(procs.iter().map(|p| format!("{} (pid {})", p.name, p.pid)))
}

fn join(items: impl Iterator<Item = String>) -> String {
    items.collect::<Vec<_>>().join(", ")
}

pub(crate) fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Every listed process as the picker shows it: playing first, then by name.
pub(crate) fn infos(procs: &[Proc], this: i32) -> Vec<AppInfo> {
    let mut v: Vec<AppInfo> = procs.iter().map(|p| AppInfo::of(p, this)).collect();
    v.sort_by(|a, b| {
        b.playing.cmp(&a.playing).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    v
}

/// The "no callbacks for this long" check, kept pure so it can be tested.
/// A count that has not moved for [`STALL`] means the tap went quiet: its
/// IOProc stopped being called.
#[derive(Debug)]
pub(crate) struct Stall {
    count: u64,
    since: Instant,
}

/// How long a tap may go without a callback before it is reported.
pub(crate) const STALL: Duration = Duration::from_secs(2);

impl Stall {
    pub(crate) fn new(now: Instant) -> Stall {
        Stall { count: 0, since: now }
    }

    /// `count`: callbacks so far. True once it has stood still for STALL.
    pub(crate) fn stalled(&mut self, count: u64, now: Instant) -> bool {
        if count != self.count {
            self.count = count;
            self.since = now;
            return false;
        }
        now.duration_since(self.since) >= STALL
    }

    /// Nothing is expected to call back now: start the count over.
    pub(crate) fn rest(&mut self, count: u64, now: Instant) {
        *self = Stall { count, since: now };
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ErrorKind;
    use std::cell::RefCell;
    use std::rc::Rc;

    pub(crate) fn proc(object: u32, pid: i32, bundle: &str, exe: &str) -> Proc {
        Proc {
            object,
            pid,
            bundle: (!bundle.is_empty()).then(|| bundle.into()),
            exe: (!exe.is_empty()).then(|| exe.into()),
            playing: true,
            devices: vec![SPEAKERS.into()],
        }
    }

    /// `p`, now playing into the devices `uids`.
    pub(crate) fn plays(p: Proc, uids: &[&str]) -> Proc {
        Proc { devices: uids.iter().map(|u| u.to_string()).collect(), ..p }
    }

    pub(crate) const SPEAKERS: &str = "BuiltInSpeakerDevice";
    pub(crate) const TUSSEN: &str = "com.simply.audio.tussen.";
    pub(crate) const TUSSEN_IN_1: &str = "com.simply.audio.tussen.in.1";
    pub(crate) const MUSIC: &str = "com.apple.Music";

    pub(crate) const DJAY: &str = "com.algoriddim.djay-iphone-free";
    pub(crate) const DJAY_EXE: &str = "/Applications/djay Pro.app/Contents/MacOS/djay Pro";
    pub(crate) const ME: i32 = 900;

    #[test]
    fn parse_tells_bundle_ids_executables_and_pids_apart() {
        assert_eq!(App::parse(DJAY), App::Bundle(DJAY.into()));
        assert_eq!(App::parse("djay Pro"), App::Executable("djay Pro".into()));
        assert_eq!(App::parse("djay Pro.app"), App::Executable("djay Pro.app".into()));
        assert_eq!(App::parse("Music.app"), App::Executable("Music.app".into()));
        assert_eq!(App::parse("afplay"), App::Executable("afplay".into()));
        assert_eq!(App::parse(" 412 "), App::Pid(412));
        assert_eq!(App::parse("simply-tussen"), App::Executable("simply-tussen".into()));
    }

    #[test]
    fn apps_match_by_bundle_its_helpers_executable_and_pid() {
        let djay = proc(10, 412, DJAY, DJAY_EXE);
        let helper = proc(11, 413, "com.google.Chrome.helper", "/x/Google Chrome Helper");
        let lookalike = proc(12, 414, "com.algoriddim.djay-iphone-freex", "/x/other");
        assert!(App::Bundle(DJAY.into()).matches(&djay, ME));
        assert!(App::Bundle(DJAY.to_uppercase()).matches(&djay, ME));
        assert!(!App::Bundle(DJAY.into()).matches(&lookalike, ME));
        assert!(App::Bundle("com.google.Chrome".into()).matches(&helper, ME));
        assert!(App::Executable("djay pro".into()).matches(&djay, ME));
        assert!(App::Executable("djay Pro.app".into()).matches(&djay, ME));
        assert!(!App::Executable("djay".into()).matches(&djay, ME));
        assert!(App::Pid(412).matches(&djay, ME));
        assert!(App::This.matches(&proc(1, ME, "", "/x/tussen"), ME));
    }

    #[test]
    fn playing_into_matches_a_process_by_the_devices_it_plays_into() {
        let tussen = App::PlayingInto(TUSSEN.into());
        let djay = proc(10, 412, DJAY, DJAY_EXE);
        assert!(!tussen.matches(&djay, ME));
        assert!(tussen.matches(&plays(djay.clone(), &[TUSSEN_IN_1]), ME));
        assert!(
            tussen.matches(&plays(djay.clone(), &[SPEAKERS, "com.simply.audio.tussen.out.1"]), ME)
        );
        assert!(!tussen.matches(&plays(djay.clone(), &["com.simply.audio.tussenx.in.1"]), ME));
        assert!(!tussen.matches(&plays(djay, &[]), ME));
        assert!(AppTap::all_except([App::This, tussen]).needs_devices());
        assert!(!AppTap::all_except([App::This]).needs_devices());
    }

    #[test]
    fn a_picker_entry_names_the_app_that_follows_it() {
        let infos = infos(
            &[
                Proc { playing: false, ..proc(1, 5, "", "/usr/bin/afplay") },
                proc(10, 412, DJAY, DJAY_EXE),
                proc(2, 6, "", ""),
                proc(3, ME, "", "/x/simply-tussen"),
            ],
            ME,
        );
        let names: Vec<&str> = infos.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["djay Pro", "pid 6", "simply-tussen", "afplay"]);
        assert_eq!(infos[0].app(), App::Bundle(DJAY.into()));
        assert_eq!(infos[1].app(), App::Pid(6));
        assert!(infos[2].this);
        assert_eq!(infos[3].app(), App::Executable("afplay".into()));
    }

    /// What the fake HAL has been asked to do.
    #[derive(Default)]
    pub(crate) struct Script {
        pub(crate) procs: Vec<Proc>,
        /// Next open fails with this.
        pub(crate) fail: Option<String>,
        /// A tap's health problem, by tap id.
        pub(crate) sick: Option<String>,
        pub(crate) opened: Vec<(Vec<Target>, Mute)>,
        pub(crate) closed: Vec<u32>,
        pub(crate) next: u32,
        pub(crate) live: Vec<u32>,
        /// What each health check was told: whether the tapped play.
        pub(crate) checked: Vec<bool>,
        /// The taps get no callbacks, as a real tap of processes that
        /// play nothing doesn't: a stall, unless the check is told so.
        pub(crate) silent: bool,
    }

    #[derive(Clone, Default)]
    pub(crate) struct Fake(pub(crate) Rc<RefCell<Script>>);

    impl Backend for Fake {
        type Tap = (u32, Sink);
        fn processes(&mut self) -> Vec<Proc> {
            self.0.borrow().procs.clone()
        }
        fn translate(&mut self, pid: i32) -> Option<u32> {
            self.0.borrow().procs.iter().find(|p| p.pid == pid).map(|p| p.object)
        }
        fn open(
            &mut self,
            targets: &[Target],
            mute: Mute,
            sink: Sink,
        ) -> Result<(Self::Tap, u32), (Error, Sink)> {
            let mut s = self.0.borrow_mut();
            if let Some(e) = s.fail.take() {
                return Err((Error::new(ErrorKind::Device, e), sink));
            }
            s.opened.push((targets.to_vec(), mute));
            // One stereo frame, so a test can tell which sink a tap feeds.
            let mut sink = sink;
            sink(&[0.0, 0.0], 0);
            s.next += 1;
            let id = s.next;
            s.live.push(id);
            Ok(((id, sink), 48_000))
        }
        fn close(&mut self, (id, sink): Self::Tap) -> Sink {
            let mut s = self.0.borrow_mut();
            s.closed.push(id);
            s.live.retain(|l| *l != id);
            sink
        }
        fn health(&mut self, _: &Self::Tap, playing: bool) -> Option<String> {
            let mut s = self.0.borrow_mut();
            s.checked.push(playing);
            let stall = (s.silent && playing).then(|| "the tap stopped delivering audio".into());
            s.sick.take().or(stall)
        }
    }

    fn follow(tap: AppTap) -> (Fake, Follow<Fake>) {
        let fake = Fake::default();
        let f = Follow::new(fake.clone(), tap, ME, Box::new(|_: &[f32], _: u64| {}));
        (fake, f)
    }

    pub(crate) fn set(fake: &Fake, procs: Vec<Proc>) {
        fake.0.borrow_mut().procs = procs;
    }

    #[test]
    fn an_app_that_is_not_running_is_waited_for_then_tapped_when_it_starts() {
        let (fake, mut f) = follow(AppTap::only([App::parse(DJAY)]));
        let t = Instant::now();
        set(&fake, vec![proc(3, 5, "com.apple.Music", "/x/Music")]);
        assert_eq!(f.tick(t).to_string(), format!("waiting for {DJAY}"));
        assert!(fake.0.borrow().opened.is_empty());

        set(&fake, vec![proc(3, 5, "com.apple.Music", "/x/Music"), proc(10, 412, DJAY, DJAY_EXE)]);
        assert_eq!(f.tick(t).to_string(), "tapping djay Pro (pid 412) at 48000 Hz");
        assert_eq!(fake.0.borrow().opened, [(vec![Target::Processes(vec![10])], Mute::Unmuted)]);

        // Nothing changed: the tap is left alone.
        f.tick(t);
        assert_eq!(fake.0.borrow().opened.len(), 1);
    }

    #[test]
    fn an_app_that_exits_is_reported_and_followed_into_its_next_process() {
        let (fake, mut f) = follow(AppTap::only([App::parse("djay Pro")]).with_mute(Mute::Muted));
        let t = Instant::now();
        set(&fake, vec![proc(10, 412, DJAY, DJAY_EXE)]);
        assert!(f.tick(t).is_tapping());

        set(&fake, vec![]);
        let s = f.tick(t).clone();
        assert_eq!(s.to_string(), "djay Pro (pid 412) exited; waiting for djay Pro");
        assert_eq!(fake.0.borrow().closed, [1]);
        assert!(fake.0.borrow().live.is_empty());

        // Restarted: a new pid and a new process object.
        set(&fake, vec![proc(27, 500, DJAY, DJAY_EXE)]);
        assert_eq!(f.tick(t).to_string(), "tapping djay Pro (pid 500) at 48000 Hz");
        let opened = &fake.0.borrow().opened;
        assert_eq!(opened.last(), Some(&(vec![Target::Processes(vec![27])], Mute::Muted)));
    }

    #[test]
    fn a_process_object_that_changes_under_a_running_tap_re_creates_it() {
        let (fake, mut f) = follow(AppTap::only([App::parse(DJAY)]));
        let t = Instant::now();
        set(&fake, vec![proc(10, 412, DJAY, DJAY_EXE)]);
        f.tick(t);
        // Quit and relaunched between two ticks.
        set(&fake, vec![proc(31, 600, DJAY, DJAY_EXE)]);
        assert_eq!(f.tick(t).to_string(), "tapping djay Pro (pid 600) at 48000 Hz");
        let s = fake.0.borrow();
        assert_eq!(s.closed, [1]);
        assert_eq!(s.live, [2]);
    }

    #[test]
    fn several_apps_share_one_tap_and_the_missing_ones_are_named() {
        let (fake, mut f) = follow(AppTap::only([App::parse(DJAY), App::parse("Spotify")]));
        let t = Instant::now();
        set(&fake, vec![proc(10, 412, DJAY, DJAY_EXE)]);
        match f.tick(t) {
            AppStatus::Tapping { missing, .. } => assert_eq!(missing, &["Spotify"]),
            s => panic!("{s}"),
        }
        set(
            &fake,
            vec![proc(10, 412, DJAY, DJAY_EXE), proc(4, 77, "com.spotify.client", "/x/Spotify")],
        );
        f.tick(t);
        assert_eq!(fake.0.borrow().opened.last().unwrap().0, [Target::Processes(vec![4, 10])]);
        match f.status() {
            AppStatus::Tapping { missing, processes, .. } => {
                assert!(missing.is_empty());
                assert_eq!(processes.len(), 2);
            }
            s => panic!("{s}"),
        }
    }

    #[test]
    fn the_status_says_which_kind_of_tap_it_is() {
        let t = Instant::now();
        let djay = proc(10, 412, DJAY, DJAY_EXE);
        let music = proc(3, 5, MUSIC, "/x/Music");

        let (fake, mut f) = follow(AppTap::all_except([App::parse(DJAY)]));
        set(&fake, vec![music.clone()]);
        assert_eq!(f.tick(t).to_string(), "tapping everything at 48000 Hz");
        set(&fake, vec![djay.clone(), music.clone()]);
        let s = f.tick(t).clone();
        assert_eq!(s.to_string(), "tapping everything except djay Pro (pid 412) at 48000 Hz");
        assert!(matches!(s, AppStatus::Tapping { all_except: true, .. }));

        let (fake, mut f) = follow(AppTap::only([App::parse(DJAY)]));
        set(&fake, vec![djay, music]);
        let s = f.tick(t).clone();
        assert_eq!(s.to_string(), "tapping djay Pro (pid 412) at 48000 Hz");
        assert!(matches!(s, AppStatus::Tapping { all_except: false, .. }));
    }

    #[test]
    fn everything_except_me_taps_at_once_and_re_excludes_me_when_i_appear() {
        let (fake, mut f) = follow(AppTap::all_except([App::This]));
        let t = Instant::now();
        set(&fake, vec![proc(10, 412, DJAY, DJAY_EXE)]);
        assert_eq!(f.tick(t).to_string(), "tapping everything at 48000 Hz");
        assert_eq!(fake.0.borrow().opened, [(vec![Target::AllExcept(vec![])], Mute::Unmuted)]);

        // This process starts playing, so the HAL lists it: leave it out.
        set(&fake, vec![proc(10, 412, DJAY, DJAY_EXE), proc(12, ME, "", "/x/simply-tussen")]);
        assert!(f.tick(t).is_tapping());
        assert_eq!(fake.0.borrow().opened.last().unwrap().0, [Target::AllExcept(vec![12])]);
        assert_eq!(fake.0.borrow().live.len(), 1);
    }

    #[test]
    fn everything_except_tussen_leaves_out_what_plays_into_a_tussen_device_while_it_does() {
        let (fake, mut f) =
            follow(AppTap::all_except([App::This, App::PlayingInto(TUSSEN.into())]));
        let t = Instant::now();
        let djay = proc(10, 412, DJAY, DJAY_EXE);
        let music = proc(3, 5, MUSIC, "/x/Music");
        let me = plays(proc(12, ME, "", "/x/simply-tussen"), &[TUSSEN_IN_1]);
        set(&fake, vec![djay.clone(), music.clone(), me.clone()]);
        assert!(f.tick(t).is_tapping());
        assert_eq!(fake.0.borrow().opened, [(vec![Target::AllExcept(vec![12])], Mute::Unmuted)]);

        // Music starts playing into Tussen In 1: it reaches tussen through
        // that device, so the tap is re-made without it.
        set(&fake, vec![djay.clone(), plays(music.clone(), &[TUSSEN_IN_1]), me.clone()]);
        assert!(f.tick(t).is_tapping());
        assert_eq!(fake.0.borrow().opened.last().unwrap().0, [Target::AllExcept(vec![3, 12])]);
        assert_eq!((fake.0.borrow().closed.as_slice(), fake.0.borrow().live.len()), (&[1][..], 1));
        // Nothing else changed: the tap is left alone.
        f.tick(t);
        assert_eq!(fake.0.borrow().opened.len(), 2);

        // It goes back to the speakers: in the tap again.
        set(&fake, vec![djay, music, me]);
        assert!(f.tick(t).is_tapping());
        let s = fake.0.borrow();
        assert_eq!(s.opened.last().unwrap().0, [Target::AllExcept(vec![12])]);
        assert_eq!(
            (s.opened.len(), s.closed.as_slice(), s.live.as_slice()),
            (3, &[1, 2][..], &[3][..])
        );
    }

    #[test]
    fn an_app_playing_into_tussen_and_the_speakers_is_tapped_on_the_speakers_only() {
        let (fake, mut f) =
            follow(AppTap::all_except([App::This, App::PlayingInto(TUSSEN.into())]));
        let t = Instant::now();
        let djay = proc(10, 412, DJAY, DJAY_EXE);
        let music = proc(3, 5, MUSIC, "/x/Music");
        let me = plays(proc(12, ME, "", "/x/simply-tussen"), &[TUSSEN_IN_1, SPEAKERS]);
        let both = plays(music.clone(), &[TUSSEN_IN_1, SPEAKERS, "AirPods"]);
        set(&fake, vec![djay.clone(), both, me.clone()]);
        // Music is left out of everything, and tapped again where it plays
        // besides Tussen: on the speakers and the AirPods, each its own
        // device-scoped tap, mixed into the one capture. Tussen itself
        // (`This`) is left out whole, wherever it plays.
        match f.tick(t) {
            AppStatus::Tapping { processes, .. } => {
                let pids: Vec<i32> = processes.iter().map(|p| p.pid).collect();
                assert_eq!(pids, [ME], "music is still heard, so not listed as left out");
            }
            s => panic!("{s}"),
        }
        assert_eq!(
            fake.0.borrow().opened,
            [(
                vec![
                    Target::AllExcept(vec![3, 12]),
                    Target::OnDevice(vec![3], "AirPods".into()),
                    Target::OnDevice(vec![3], SPEAKERS.into()),
                ],
                Mute::Unmuted
            )]
        );
        // Nothing changed: the taps are left alone.
        f.tick(t);
        assert_eq!(fake.0.borrow().opened.len(), 1);

        // It leaves the AirPods and Tussen: plain "everything" again.
        set(&fake, vec![djay, music, me]);
        f.tick(t);
        let s = fake.0.borrow();
        assert_eq!(s.opened.last().unwrap().0, [Target::AllExcept(vec![12])]);
        assert_eq!((s.closed.as_slice(), s.live.len()), (&[1][..], 1));
    }

    #[test]
    fn the_follower_s_own_process_is_never_scoped_only_left_out_whole() {
        // Without `This` listed: playing into Tussen and the speakers, it
        // is left out whole, not tapped on the speakers.
        let (fake, mut f) = follow(AppTap::all_except([App::PlayingInto(TUSSEN.into())]));
        let me = plays(proc(12, ME, "", "/x/simply-tussen"), &[TUSSEN_IN_1, SPEAKERS]);
        set(&fake, vec![proc(10, 412, DJAY, DJAY_EXE), me]);
        f.tick(Instant::now());
        assert_eq!(fake.0.borrow().opened[0].0, [Target::AllExcept(vec![12])]);
    }

    #[test]
    fn two_apps_on_tussen_and_the_speakers_share_one_scoped_tap() {
        let (fake, mut f) = follow(AppTap::all_except([App::PlayingInto(TUSSEN.into())]));
        let both = |p| plays(p, &[TUSSEN_IN_1, SPEAKERS]);
        set(&fake, vec![both(proc(10, 412, DJAY, DJAY_EXE)), both(proc(3, 5, MUSIC, "/x/Music"))]);
        f.tick(Instant::now());
        assert_eq!(
            fake.0.borrow().opened[0].0,
            [Target::AllExcept(vec![3, 10]), Target::OnDevice(vec![3, 10], SPEAKERS.into())]
        );
    }

    #[test]
    fn only_what_plays_into_a_device_waits_for_a_process_to_play_there() {
        let (fake, mut f) = follow(AppTap::only([App::PlayingInto(TUSSEN.into())]));
        let t = Instant::now();
        set(&fake, vec![proc(10, 412, DJAY, DJAY_EXE)]);
        assert_eq!(f.tick(t).to_string(), format!("waiting for whatever plays into {TUSSEN}*"));
        set(&fake, vec![plays(proc(10, 412, DJAY, DJAY_EXE), &[TUSSEN_IN_1])]);
        assert_eq!(f.tick(t).to_string(), "tapping djay Pro (pid 412) at 48000 Hz");
        assert_eq!(fake.0.borrow().opened, [(vec![Target::Processes(vec![10])], Mute::Unmuted)]);
    }

    #[test]
    fn a_tap_that_cannot_be_made_is_reported_and_retried() {
        let (fake, mut f) = follow(AppTap::only([App::parse(DJAY)]));
        let t = Instant::now();
        set(&fake, vec![proc(10, 412, DJAY, DJAY_EXE)]);
        fake.0.borrow_mut().fail = Some("OSStatus 'nope'".into());
        assert_eq!(f.tick(t).to_string(), "tap failed: OSStatus 'nope'");
        // Too soon to retry.
        assert!(matches!(f.tick(t + RETRY / 2), AppStatus::Failed { .. }));
        assert!(fake.0.borrow().opened.is_empty());
        assert!(f.tick(t + RETRY).is_tapping());
    }

    #[test]
    fn a_tap_that_goes_bad_is_re_made() {
        let (fake, mut f) = follow(AppTap::only([App::parse(DJAY)]));
        let t = Instant::now();
        set(&fake, vec![proc(10, 412, DJAY, DJAY_EXE)]);
        f.tick(t);
        fake.0.borrow_mut().sick = Some("output device \"AirPods\" disappeared".into());
        assert!(f.tick(t).is_tapping());
        let s = fake.0.borrow();
        assert_eq!((s.closed.as_slice(), s.live.as_slice()), (&[1][..], &[2][..]));
    }

    #[test]
    fn a_paused_app_s_silent_tap_is_kept_not_re_made() {
        let (fake, mut f) = follow(AppTap::only([App::parse(DJAY), App::parse("Spotify")]));
        let t = Instant::now();
        let spotify = proc(4, 77, "com.spotify.client", "/x/Spotify");
        let quiet = |p: Proc| Proc { playing: false, ..p };
        set(&fake, vec![proc(10, 412, DJAY, DJAY_EXE), spotify.clone()]);
        assert!(f.tick(t).is_tapping());
        // Both paused: the tap gets no callbacks, and that is no stall.
        set(&fake, vec![quiet(proc(10, 412, DJAY, DJAY_EXE)), quiet(spotify.clone())]);
        fake.0.borrow_mut().silent = true;
        for i in 1..=5 {
            assert!(f.tick(t + STALL * i).is_tapping());
        }
        assert_eq!(fake.0.borrow().checked, [false; 5]);
        assert_eq!((fake.0.borrow().opened.len(), fake.0.borrow().closed.len()), (1, 0));
        // One plays again and its tap still says nothing: now it is a stall.
        set(&fake, vec![quiet(proc(10, 412, DJAY, DJAY_EXE)), spotify]);
        assert!(f.tick(t + STALL * 6).is_tapping());
        let s = fake.0.borrow();
        assert_eq!(
            (s.checked.last(), s.opened.len(), s.closed.as_slice()),
            (Some(&true), 2, &[1][..])
        );
    }

    #[test]
    fn everything_except_keeps_its_silent_tap_while_nothing_else_plays() {
        let (fake, mut f) = follow(AppTap::all_except([App::This]));
        let t = Instant::now();
        let me = proc(12, ME, "", "/x/simply-tussen");
        set(&fake, vec![Proc { playing: false, ..proc(10, 412, DJAY, DJAY_EXE) }, me]);
        fake.0.borrow_mut().silent = true;
        for i in 0..5 {
            assert!(f.tick(t + STALL * i).is_tapping());
        }
        let s = fake.0.borrow();
        assert_eq!((s.opened.len(), s.closed.len(), s.checked.as_slice()), (1, 0, &[false; 4][..]));
    }

    #[test]
    fn a_pid_goes_through_the_hal_translation_and_stops_when_it_exits() {
        let (fake, mut f) = follow(AppTap::only([App::Pid(412)]));
        let t = Instant::now();
        set(&fake, vec![proc(10, 412, DJAY, DJAY_EXE)]);
        assert!(f.tick(t).is_tapping());
        set(&fake, vec![proc(27, 500, DJAY, DJAY_EXE)]);
        assert_eq!(f.tick(t).to_string(), "djay Pro (pid 412) exited; waiting for pid 412");
    }

    #[test]
    fn dropping_the_follower_closes_its_tap() {
        let (fake, mut f) = follow(AppTap::only([App::parse(DJAY)]));
        set(&fake, vec![proc(10, 412, DJAY, DJAY_EXE)]);
        f.tick(Instant::now());
        drop(f);
        assert!(fake.0.borrow().live.is_empty());
    }

    #[test]
    fn a_stall_is_a_count_that_stands_still() {
        let t = Instant::now();
        let mut s = Stall::new(t);
        assert!(!s.stalled(5, t));
        assert!(!s.stalled(5, t + STALL / 2));
        assert!(!s.stalled(9, t + STALL));
        assert!(!s.stalled(9, t + STALL + STALL / 2));
        assert!(s.stalled(9, t + STALL * 2));
        s.rest(9, t + STALL * 3);
        assert!(!s.stalled(9, t + STALL * 3 + STALL / 2));
        assert!(s.stalled(9, t + STALL * 4));
    }
}
