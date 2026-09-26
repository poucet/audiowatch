//! The state machine that turns "what the HAL looks like now" into events.
//!
//! It is deliberately pure: it reads through [`HalView`] and returns records.
//! No logging, no notifications, no CoreAudio — so the whole of the
//! event-detection logic, including the short-lived-process case, is testable.

use crate::event::{Kind, Record};
use crate::hal::HalView;
use crate::proc::Ancestor;
use crate::sys::AudioObjectId;
use std::collections::HashMap;

/// Why a record may not be able to name a live process.
pub const EXITED_NOTE: &str =
    "process had already exited; identity captured when it connected to the audio HAL";
pub const UNKNOWN_NOTE: &str = "executable path never readable (process exited too fast)";

/// The note explains why an identity might be stale, which is only worth saying
/// about something *starting*. On a stop or a disconnect it is tautological --
/// of course the process has gone -- so it is left off.
fn note_for(kind: Kind, note: &Option<String>) -> Option<String> {
    match kind {
        Kind::Connect | Kind::OutputStart | Kind::InputStart | Kind::Baseline => note.clone(),
        Kind::OutputStop | Kind::InputStop | Kind::Disconnect => None,
    }
}

#[derive(Debug, Clone, Default)]
struct Tracked {
    pid: i32,
    bundle: Option<String>,
    /// Captured the first time this process object was seen -- while the process
    /// was still alive -- and never dropped afterwards.
    exe: Option<String>,
    running_output: bool,
    running_input: bool,
    /// Last known non-empty output device list, so a stop event can still say
    /// which device fell silent.
    devices: Vec<String>,
    /// The chain above the process, cached at connect for the same reason `exe`
    /// is: a 200 ms process's parents can be gone before its output is noticed.
    parents: Vec<Ancestor>,
}

#[derive(Default)]
pub struct Reconciler {
    tracked: HashMap<AudioObjectId, Tracked>,
    started: bool,
}

impl Reconciler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tracked_count(&self) -> usize {
        self.tracked.len()
    }

    /// Compare the HAL against what we last saw and return everything that
    /// changed. The first call establishes a baseline: it announces nothing as
    /// new, but records whatever is already running output as [`Kind::Baseline`].
    ///
    /// On an idle tick this does two property reads per audio process and
    /// nothing else — no identity lookup, no device lookup, no `proc_pidpath`.
    /// Those happen only when a process is first seen or one of its flags moves.
    pub fn reconcile(&mut self, hal: &impl HalView, now_ms: i64) -> Vec<Record> {
        let baseline = !self.started;
        self.started = true;

        let objects = hal.process_objects();
        let mut records = Vec::new();
        let mut seen = Vec::with_capacity(objects.len());

        for object in objects {
            let Some(running) = hal.running(object) else {
                // The object is listed but unreadable — it may be on its way out.
                // Treat it as still present rather than inventing a disconnect.
                if self.tracked.contains_key(&object) {
                    seen.push(object);
                }
                continue;
            };
            seen.push(object);

            let known = self.tracked.get(&object);
            let is_new = known.is_none();
            let mut entry = known.cloned().unwrap_or_default();
            let changed = is_new
                || running.output != entry.running_output
                || running.input != entry.running_input;

            // The expensive reads happen only here, on a change.
            let mut note = None;
            if changed {
                if let Some(details) = hal.details(object) {
                    entry.pid = details.pid;
                    entry.exe = details.exe.clone().or(entry.exe);
                    if entry.parents.is_empty() {
                        entry.parents = details.parents.clone();
                    }
                    entry.bundle = details.bundle.clone().or(entry.bundle);
                    if !details.devices.is_empty() {
                        entry.devices = details.devices;
                    }
                    note = if details.exe.is_some() {
                        None
                    } else if entry.exe.is_some() {
                        Some(EXITED_NOTE.to_string())
                    } else {
                        Some(UNKNOWN_NOTE.to_string())
                    };
                } else if entry.exe.is_some() {
                    note = Some(EXITED_NOTE.to_string());
                }
            }

            let mut emit = |kind: Kind, entry: &Tracked| {
                let mut r = Record::new(now_ms, kind, entry.pid);
                r.bundle = entry.bundle.clone();
                r.exe = entry.exe.clone();
                r.parents = entry.parents.clone();
                r.devices = entry.devices.clone();
                r.note = note_for(kind, &note);
                records.push(r);
            };

            if is_new && !baseline {
                emit(Kind::Connect, &entry);
            }

            if running.output != entry.running_output {
                if running.output {
                    emit(
                        if baseline {
                            Kind::Baseline
                        } else {
                            Kind::OutputStart
                        },
                        &entry,
                    );
                } else if !baseline {
                    // A baseline pass never reports something stopping.
                    emit(Kind::OutputStop, &entry);
                }
                entry.running_output = running.output;
            }

            if running.input != entry.running_input && !baseline {
                emit(
                    if running.input {
                        Kind::InputStart
                    } else {
                        Kind::InputStop
                    },
                    &entry,
                );
            }
            entry.running_input = running.input;

            self.tracked.insert(object, entry);
        }

        // Anything we were tracking that is no longer in the list has gone.
        let mut gone: Vec<AudioObjectId> = self
            .tracked
            .keys()
            .copied()
            .filter(|o| !seen.contains(o))
            .collect();
        gone.sort_unstable();
        for object in gone {
            let entry = self.tracked.remove(&object).unwrap_or_default();
            // No note here: every record from this loop is a stop or a
            // disconnect, where "it has exited" goes without saying.
            let mut push = |kind: Kind| {
                let mut r = Record::new(now_ms, kind, entry.pid);
                r.bundle = entry.bundle.clone();
                r.exe = entry.exe.clone();
                r.parents = entry.parents.clone();
                r.devices = entry.devices.clone();
                records.push(r);
            };
            if entry.running_output {
                push(Kind::OutputStop);
            }
            if entry.running_input {
                push(Kind::InputStop);
            }
            push(Kind::Disconnect);
        }

        records
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hal::{ProcessDetails, RunningState};

    /// A HAL we can drive by hand, one instant at a time.
    #[derive(Default)]
    struct Fake {
        processes: Vec<(AudioObjectId, RunningState, ProcessDetails)>,
        /// Objects that appear in the list but whose flags will not read.
        unreadable: Vec<AudioObjectId>,
        /// Objects whose flags read but whose details will not.
        detail_less: Vec<AudioObjectId>,
    }

    impl HalView for Fake {
        fn process_objects(&self) -> Vec<AudioObjectId> {
            let mut v: Vec<_> = self.processes.iter().map(|(o, _, _)| *o).collect();
            v.extend(self.unreadable.iter().copied());
            v
        }
        fn running(&self, object: AudioObjectId) -> Option<RunningState> {
            self.processes
                .iter()
                .find(|(o, _, _)| *o == object)
                .map(|(_, r, _)| *r)
        }
        fn details(&self, object: AudioObjectId) -> Option<ProcessDetails> {
            if self.detail_less.contains(&object) {
                return None;
            }
            self.processes
                .iter()
                .find(|(o, _, _)| *o == object)
                .map(|(_, _, d)| d.clone())
        }
    }

    fn proc(pid: i32, exe: Option<&str>, out: bool) -> (RunningState, ProcessDetails) {
        (
            RunningState {
                output: out,
                input: false,
            },
            ProcessDetails {
                pid,
                bundle: None,
                exe: exe.map(str::to_string),
                parents: vec![Ancestor {
                    pid: 1,
                    exe: Some("/sbin/launchd".into()),
                }],
                devices: if out {
                    vec!["Scarlett 4i4 USB".into()]
                } else {
                    vec![]
                },
            },
        )
    }

    /// Add a process to the fake.
    fn add(hal: &mut Fake, object: AudioObjectId, pid: i32, exe: Option<&str>, out: bool) {
        let (r, d) = proc(pid, exe, out);
        hal.processes.push((object, r, d));
    }

    /// Turn output on the way the real HAL does: the flag and the device list
    /// both appear at the same instant.
    fn start_output(hal: &mut Fake, index: usize) {
        hal.processes[index].1.output = true;
        hal.processes[index].2.devices = vec!["Scarlett 4i4 USB".into()];
    }

    fn kinds(records: &[Record]) -> Vec<Kind> {
        records.iter().map(|r| r.kind).collect()
    }

    #[test]
    fn the_first_pass_is_a_silent_baseline() {
        let mut hal = Fake::default();
        add(&mut hal, 10, 100, Some("/usr/bin/quiet"), false);
        add(&mut hal, 11, 101, Some("/usr/bin/loud"), true);
        let mut r = Reconciler::new();
        let out = r.reconcile(&hal, 1000);
        // No Connect records for what was already there, and the one already
        // playing is recorded as a baseline rather than announced as new.
        assert_eq!(kinds(&out), vec![Kind::Baseline]);
        assert_eq!(out[0].exe.as_deref(), Some("/usr/bin/loud"));
        assert_eq!(r.tracked_count(), 2);
    }

    #[test]
    fn a_process_that_connects_then_plays_then_exits_produces_the_whole_story() {
        let mut r = Reconciler::new();
        let mut hal = Fake::default();
        assert!(r.reconcile(&hal, 0).is_empty());

        // It connects. This is the moment the executable path is readable.
        add(&mut hal, 20, 555, Some("/usr/bin/afplay"), false);
        assert_eq!(kinds(&r.reconcile(&hal, 1000)), vec![Kind::Connect]);

        // Output starts.
        start_output(&mut hal, 0);
        let out = r.reconcile(&hal, 2000);
        assert_eq!(kinds(&out), vec![Kind::OutputStart]);
        assert_eq!(out[0].devices, vec!["Scarlett 4i4 USB".to_string()]);
        assert_eq!(out[0].note, None);

        // And it is gone, still running, before we look again.
        hal.processes.clear();
        assert_eq!(
            kinds(&r.reconcile(&hal, 3000)),
            vec![Kind::OutputStop, Kind::Disconnect]
        );
        assert_eq!(r.tracked_count(), 0);
    }

    #[test]
    fn output_start_is_still_named_when_the_process_died_before_we_looked() {
        // This is the case the whole tool exists for: identity is captured at
        // connect time, so the start event can name a process that is already
        // dead by the time its output flag is seen.
        let mut r = Reconciler::new();
        let mut hal = Fake::default();
        r.reconcile(&hal, 0);

        add(&mut hal, 30, 777, Some("/usr/bin/afplay"), false);
        r.reconcile(&hal, 1000);

        // Output is now running, but the process has exited, so there is no
        // readable path any more.
        start_output(&mut hal, 0);
        hal.processes[0].2.exe = None;
        let out = r.reconcile(&hal, 1100);
        assert_eq!(kinds(&out), vec![Kind::OutputStart]);
        assert_eq!(
            out[0].exe.as_deref(),
            Some("/usr/bin/afplay"),
            "cached identity must survive"
        );
        assert_eq!(out[0].pid, 777);
        assert_eq!(out[0].note.as_deref(), Some(EXITED_NOTE));
    }

    #[test]
    fn output_start_is_still_named_when_the_details_read_fails_outright() {
        // The process object can vanish between the flag read and the detail
        // read. The cached identity is all we have, and it must be used.
        let mut r = Reconciler::new();
        let mut hal = Fake::default();
        r.reconcile(&hal, 0);
        add(&mut hal, 31, 778, Some("/usr/bin/afplay"), false);
        r.reconcile(&hal, 1000);

        start_output(&mut hal, 0);
        hal.detail_less.push(31);
        let out = r.reconcile(&hal, 1100);
        assert_eq!(kinds(&out), vec![Kind::OutputStart]);
        assert_eq!(out[0].exe.as_deref(), Some("/usr/bin/afplay"));
        assert_eq!(out[0].pid, 778);
        assert_eq!(out[0].note.as_deref(), Some(EXITED_NOTE));
    }

    #[test]
    fn a_process_seen_for_the_first_time_already_playing_reports_both_events() {
        let mut r = Reconciler::new();
        let mut hal = Fake::default();
        r.reconcile(&hal, 0);
        // Connect and output-start fell inside one poll interval.
        add(&mut hal, 40, 888, Some("/usr/bin/afplay"), true);
        assert_eq!(
            kinds(&r.reconcile(&hal, 1000)),
            vec![Kind::Connect, Kind::OutputStart]
        );
    }

    #[test]
    fn a_process_that_never_had_a_readable_path_says_so() {
        let mut r = Reconciler::new();
        let mut hal = Fake::default();
        r.reconcile(&hal, 0);
        add(&mut hal, 50, 999, None, true);
        let out = r.reconcile(&hal, 1000);
        assert_eq!(kinds(&out), vec![Kind::Connect, Kind::OutputStart]);
        assert_eq!(out[0].exe, None);
        assert_eq!(out[0].note.as_deref(), Some(UNKNOWN_NOTE));
        // The pid is still there, which is the one thing that always survives.
        assert_eq!(out[0].pid, 999);
    }

    #[test]
    fn steady_state_produces_no_records_at_all() {
        let mut hal = Fake::default();
        add(&mut hal, 60, 100, Some("/a"), true);
        add(&mut hal, 61, 101, Some("/b"), false);
        let mut r = Reconciler::new();
        r.reconcile(&hal, 0);
        for t in 1..20 {
            assert!(
                r.reconcile(&hal, t * 100).is_empty(),
                "poll {t} invented an event"
            );
        }
    }

    #[test]
    fn an_idle_tick_reads_only_the_flags() {
        // The hot path must not touch identity or devices; if it did, a login
        // daemon would burn CPU and hammer proc_pidpath forever.
        struct Counting {
            inner: Fake,
            details: std::cell::Cell<usize>,
        }
        impl HalView for Counting {
            fn process_objects(&self) -> Vec<AudioObjectId> {
                self.inner.process_objects()
            }
            fn running(&self, o: AudioObjectId) -> Option<RunningState> {
                self.inner.running(o)
            }
            fn details(&self, o: AudioObjectId) -> Option<ProcessDetails> {
                self.details.set(self.details.get() + 1);
                self.inner.details(o)
            }
        }
        let mut inner = Fake::default();
        for i in 0..10u32 {
            add(&mut inner, i, 100 + i as i32, Some("/a"), false);
        }
        let hal = Counting {
            inner,
            details: std::cell::Cell::new(0),
        };
        let mut r = Reconciler::new();
        r.reconcile(&hal, 0);
        let after_first = hal.details.get();
        assert_eq!(
            after_first, 10,
            "the first pass should read each process once"
        );
        for t in 1..50 {
            r.reconcile(&hal, t * 100);
        }
        assert_eq!(
            hal.details.get(),
            after_first,
            "an idle tick must read no details"
        );
    }

    #[test]
    fn stopping_output_names_the_device_it_had_been_using() {
        let mut r = Reconciler::new();
        let mut hal = Fake::default();
        add(&mut hal, 70, 100, Some("/a"), true);
        r.reconcile(&hal, 0);
        // The HAL stops reporting devices at the same moment output stops.
        hal.processes[0].1.output = false;
        hal.processes[0].2.devices.clear();
        let out = r.reconcile(&hal, 1000);
        assert_eq!(kinds(&out), vec![Kind::OutputStop]);
        assert_eq!(out[0].devices, vec!["Scarlett 4i4 USB".to_string()]);
    }

    #[test]
    fn input_is_tracked_separately_from_output() {
        let mut r = Reconciler::new();
        let mut hal = Fake::default();
        add(&mut hal, 80, 100, Some("/a"), false);
        r.reconcile(&hal, 0);
        hal.processes[0].1.input = true;
        assert_eq!(kinds(&r.reconcile(&hal, 1000)), vec![Kind::InputStart]);
        hal.processes[0].1.input = false;
        assert_eq!(kinds(&r.reconcile(&hal, 2000)), vec![Kind::InputStop]);
    }

    #[test]
    fn input_already_running_at_startup_is_not_announced() {
        let mut hal = Fake::default();
        add(&mut hal, 81, 100, Some("/a"), false);
        hal.processes[0].1.input = true;
        let mut r = Reconciler::new();
        assert!(
            r.reconcile(&hal, 0).is_empty(),
            "the baseline pass announces nothing"
        );
        // ... and it is remembered, so it does not fire on the next tick either.
        assert!(r.reconcile(&hal, 100).is_empty());
    }

    #[test]
    fn an_object_that_cannot_be_read_is_not_mistaken_for_a_disconnect() {
        let mut r = Reconciler::new();
        let mut hal = Fake::default();
        add(&mut hal, 90, 100, Some("/a"), false);
        r.reconcile(&hal, 0);
        // The object is still listed but its flags now fail to read.
        hal.processes.clear();
        hal.unreadable.push(90);
        assert!(
            r.reconcile(&hal, 1000).is_empty(),
            "an unreadable object is not a disconnect"
        );
        assert_eq!(r.tracked_count(), 1);
        // Once it really leaves the list, it disconnects.
        hal.unreadable.clear();
        assert_eq!(kinds(&r.reconcile(&hal, 2000)), vec![Kind::Disconnect]);
    }

    #[test]
    fn a_bundle_id_learned_later_is_remembered() {
        let mut r = Reconciler::new();
        let mut hal = Fake::default();
        add(&mut hal, 100, 100, Some("/a"), false);
        r.reconcile(&hal, 0);
        hal.processes[0].2.bundle = Some("com.example.late".into());
        start_output(&mut hal, 0);
        let out = r.reconcile(&hal, 1000);
        assert_eq!(out[0].bundle.as_deref(), Some("com.example.late"));
    }

    #[test]
    fn a_stop_or_disconnect_carries_no_already_exited_note() {
        // It would be tautological, and it crowded out the useful lines.
        let mut r = Reconciler::new();
        let mut hal = Fake::default();
        r.reconcile(&hal, 0);
        add(&mut hal, 110, 100, Some("/usr/bin/afplay"), false);
        r.reconcile(&hal, 1000);
        start_output(&mut hal, 0);
        let started = r.reconcile(&hal, 2000);
        assert_eq!(started[0].note, None, "a live start needs no note");

        hal.processes.clear();
        let ended = r.reconcile(&hal, 3000);
        assert_eq!(kinds(&ended), vec![Kind::OutputStop, Kind::Disconnect]);
        for e in &ended {
            assert_eq!(e.note, None, "{} should carry no note", e.kind.wire());
            assert_eq!(
                e.exe.as_deref(),
                Some("/usr/bin/afplay"),
                "but it still names the process"
            );
        }
    }

    #[test]
    fn many_simultaneous_starts_are_all_reported() {
        let mut r = Reconciler::new();
        let mut hal = Fake::default();
        r.reconcile(&hal, 0);
        for i in 0..8u32 {
            add(&mut hal, 200 + i, 1000 + i as i32, Some("/x"), true);
        }
        let out = r.reconcile(&hal, 1000);
        assert_eq!(
            out.iter().filter(|r| r.kind == Kind::OutputStart).count(),
            8
        );
        assert_eq!(out.iter().filter(|r| r.kind == Kind::Connect).count(), 8);
    }
}
