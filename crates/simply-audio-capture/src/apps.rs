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
        }
    }
}

impl fmt::Display for App {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            App::Bundle(s) | App::Executable(s) => f.write_str(s),
            App::Pid(p) => write!(f, "pid {p}"),
            App::This => f.write_str("this process"),
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

    fn list(&self) -> &[App] {
        match &self.apps {
            Apps::Only(a) | Apps::AllExcept(a) => a,
        }
    }

    fn names(&self) -> String {
        join(self.list().iter().map(App::to_string))
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
    /// Audio is flowing. `processes`: the ones tapped ([`Apps::Only`]) or
    /// the ones left out ([`Apps::AllExcept`]). `missing`: the apps asked
    /// for that aren't running (yet).
    Tapping { processes: Vec<AppInfo>, missing: Vec<String>, device_rate: u32 },
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
            AppStatus::Tapping { processes, device_rate, .. } => {
                f.write_str("tapping ")?;
                f.write_str(&join(
                    processes.iter().map(|p| format!("{} (pid {})", p.name, p.pid)),
                ))?;
                write!(f, " at {device_rate} Hz")
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
}

/// What the HAL is asked to tap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// Stereo mixdown of these process objects.
    Processes(Vec<u32>),
    /// Stereo mixdown of everything except these process objects.
    AllExcept(Vec<u32>),
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
    /// Make a tap and start it into `sink`; on failure the sink comes back.
    fn open(
        &mut self,
        target: &Target,
        mute: Mute,
        sink: Sink,
    ) -> Result<(Self::Tap, u32), (Error, Sink)>;
    /// Stop and destroy a tap; its sink comes back.
    fn close(&mut self, tap: Self::Tap) -> Sink;
    /// Why a running tap is no longer good (its clock device vanished, it
    /// stopped calling back), if it isn't.
    fn health(&mut self, tap: &Self::Tap) -> Option<String>;
}

/// How long a failed tap waits before it is tried again.
pub(crate) const RETRY: Duration = Duration::from_secs(3);

struct Open<T> {
    tap: T,
    key: Vec<u32>,
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
    /// The last target that failed to open, and when.
    failed: Option<(Vec<u32>, Instant)>,
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
        let (matched, missing) = self.resolve(&procs);
        let mut key: Vec<u32> = matched.iter().map(|p| p.object).collect();
        key.sort_unstable();
        key.dedup();
        let want = match self.tap.apps {
            Apps::Only(_) => !key.is_empty(),
            Apps::AllExcept(_) => true,
        };
        let infos: Vec<AppInfo> = matched.iter().map(|p| AppInfo::of(p, self.this)).collect();

        let mut reason = match &self.status {
            AppStatus::Waiting { reason, .. } => reason.clone(),
            _ => None,
        };
        if let Some(open) = &self.open {
            let problem = self.backend.health(&open.tap);
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
        let target = match self.tap.apps {
            Apps::Only(_) => Target::Processes(key.clone()),
            Apps::AllExcept(_) => Target::AllExcept(key.clone()),
        };
        let sink = self.sink.take().expect("no tap holds the sink");
        match self.backend.open(&target, self.tap.mute, sink) {
            Ok((tap, device_rate)) => {
                self.failed = None;
                self.status = AppStatus::Tapping { processes: infos.clone(), missing, device_rate };
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

    /// The processes the tap names, and the apps asked for that aren't
    /// running.
    fn resolve(&mut self, procs: &[Proc]) -> (Vec<Proc>, Vec<String>) {
        let mut matched: Vec<Proc> = Vec::new();
        let mut missing = Vec::new();
        for app in self.tap.list().to_vec() {
            let found: Vec<Proc> = match app {
                // A pid goes through the HAL's own translation; the listed
                // process carries the identity, when it is listed.
                App::Pid(pid) => self.by_pid(pid, procs),
                App::This => self.by_pid(self.this, procs),
                _ => procs.iter().filter(|p| app.matches(p, self.this)).cloned().collect(),
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

    fn by_pid(&mut self, pid: i32, procs: &[Proc]) -> Vec<Proc> {
        match self.backend.translate(pid) {
            Some(object) => vec![procs
                .iter()
                .find(|p| p.object == object)
                .cloned()
                .unwrap_or(Proc { object, pid, ..Proc::default() })],
            None => Vec::new(),
        }
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

fn describe(procs: &[AppInfo]) -> String {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn proc(object: u32, pid: i32, bundle: &str, exe: &str) -> Proc {
        Proc {
            object,
            pid,
            bundle: (!bundle.is_empty()).then(|| bundle.into()),
            exe: (!exe.is_empty()).then(|| exe.into()),
            playing: true,
        }
    }

    const DJAY: &str = "com.algoriddim.djay-iphone-free";
    const DJAY_EXE: &str = "/Applications/djay Pro.app/Contents/MacOS/djay Pro";
    const ME: i32 = 900;

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
    struct Script {
        procs: Vec<Proc>,
        /// Next open fails with this.
        fail: Option<String>,
        /// A tap's health problem, by tap id.
        sick: Option<String>,
        opened: Vec<(Target, Mute)>,
        closed: Vec<u32>,
        next: u32,
        live: Vec<u32>,
    }

    #[derive(Clone, Default)]
    struct Fake(Rc<RefCell<Script>>);

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
            target: &Target,
            mute: Mute,
            sink: Sink,
        ) -> Result<(Self::Tap, u32), (Error, Sink)> {
            let mut s = self.0.borrow_mut();
            if let Some(e) = s.fail.take() {
                return Err((Error::new(ErrorKind::Device, e), sink));
            }
            s.opened.push((target.clone(), mute));
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
        fn health(&mut self, _: &Self::Tap) -> Option<String> {
            self.0.borrow_mut().sick.take()
        }
    }

    fn follow(tap: AppTap) -> (Fake, Follow<Fake>) {
        let fake = Fake::default();
        let f = Follow::new(fake.clone(), tap, ME, Box::new(|_: &[f32], _: u64| {}));
        (fake, f)
    }

    fn set(fake: &Fake, procs: Vec<Proc>) {
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
        assert_eq!(fake.0.borrow().opened, [(Target::Processes(vec![10]), Mute::Unmuted)]);

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
        assert_eq!(opened.last(), Some(&(Target::Processes(vec![27]), Mute::Muted)));
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
        assert_eq!(fake.0.borrow().opened.last().unwrap().0, Target::Processes(vec![4, 10]));
        match f.status() {
            AppStatus::Tapping { missing, processes, .. } => {
                assert!(missing.is_empty());
                assert_eq!(processes.len(), 2);
            }
            s => panic!("{s}"),
        }
    }

    #[test]
    fn everything_except_me_taps_at_once_and_re_excludes_me_when_i_appear() {
        let (fake, mut f) = follow(AppTap::all_except([App::This]));
        let t = Instant::now();
        set(&fake, vec![proc(10, 412, DJAY, DJAY_EXE)]);
        assert!(f.tick(t).is_tapping());
        assert_eq!(fake.0.borrow().opened, [(Target::AllExcept(vec![]), Mute::Unmuted)]);

        // This process starts playing, so the HAL lists it: leave it out.
        set(&fake, vec![proc(10, 412, DJAY, DJAY_EXE), proc(12, ME, "", "/x/simply-tussen")]);
        assert!(f.tick(t).is_tapping());
        assert_eq!(fake.0.borrow().opened.last().unwrap().0, Target::AllExcept(vec![12]));
        assert_eq!(fake.0.borrow().live.len(), 1);
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
    }
}
