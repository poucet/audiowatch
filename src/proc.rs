//! The process tree above a pid.
//!
//! `afplay` on its own is meaningless — Chris, 2026-09-26, watching bursts he
//! could not attribute: *"I can't tell who's triggering the afplay"*. The
//! answer was `afplay ← zsh ← claude`, and it took twenty minutes of live
//! polling to get because the watcher did not record it.
//!
//! The walk happens **when a process connects to the HAL**, not when its output
//! is noticed: a process connects 113–321 ms before it makes a sound, and by
//! the time a 200 ms burst is seen its parents can be gone too. Same reason the
//! executable path is captured there.
//!
//! The walk itself is behind [`ProcTable`] so it is testable without a process
//! tree — and so that verifying it needs no audio at all.

use crate::sys;

/// How far up to walk. A chain deeper than this is a runaway, not a lineage.
pub const MAX_DEPTH: usize = 12;

/// One ancestor: its pid, and its executable path if that could be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ancestor {
    pub pid: i32,
    pub exe: Option<String>,
}

impl Ancestor {
    /// The executable's file name — `zsh` out of `/bin/zsh` — else the pid.
    /// A `/bin/zsh -c source …shell-snapshots…` line is 200 characters of
    /// noise; `zsh` is the reading.
    pub fn short_name(&self) -> String {
        match &self.exe {
            Some(exe) => {
                let base = exe.rsplit('/').next().unwrap_or(exe);
                match base.is_empty() {
                    true => format!("pid {}", self.pid),
                    false => base.to_string(),
                }
            }
            None => format!("pid {}", self.pid),
        }
    }
}

/// What the walk needs of the operating system. The live implementation is
/// [`Live`]; the tests hand it a table.
pub trait ProcTable {
    fn ppid(&self, pid: i32) -> Option<i32>;
    fn path(&self, pid: i32) -> Option<String>;
}

/// The real process tree.
pub struct Live;

impl ProcTable for Live {
    fn ppid(&self, pid: i32) -> Option<i32> {
        sys::pid_ppid(pid)
    }
    fn path(&self, pid: i32) -> Option<String> {
        sys::pid_path(pid)
    }
}

/// The ancestors of `pid`, nearest first, stopping at `launchd` (pid 1).
///
/// Bounded by [`MAX_DEPTH`] and guarded against a cycle: a pid that repeats
/// ends the walk, because a loop in the tree is a kernel that has surprised us
/// and not a reason to spin on the audio thread's timescale.
pub fn chain<T: ProcTable>(table: &T, pid: i32) -> Vec<Ancestor> {
    let mut out = Vec::new();
    let mut seen = vec![pid];
    let mut at = pid;
    while out.len() < MAX_DEPTH {
        let Some(parent) = table.ppid(at) else { break };
        if parent <= 0 || seen.contains(&parent) {
            break;
        }
        out.push(Ancestor {
            pid: parent,
            exe: table.path(parent),
        });
        if parent == 1 {
            break;
        }
        seen.push(parent);
        at = parent;
    }
    out
}

/// The ancestors of `pid` from the real process tree.
pub fn live_chain(pid: i32) -> Vec<Ancestor> {
    chain(&Live, pid)
}

/// `afplay ← zsh ← claude` — the child's own name is *not* included, since the
/// record already carries it. `keep` caps how many ancestors are shown.
pub fn render(chain: &[Ancestor], keep: usize) -> String {
    chain
        .iter()
        .take(keep)
        .map(Ancestor::short_name)
        .collect::<Vec<_>>()
        .join(" ← ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process tree as `(pid, ppid, path)` rows.
    struct Table(Vec<(i32, i32, Option<&'static str>)>);

    impl ProcTable for Table {
        fn ppid(&self, pid: i32) -> Option<i32> {
            self.0
                .iter()
                .find(|(p, _, _)| *p == pid)
                .map(|(_, pp, _)| *pp)
        }
        fn path(&self, pid: i32) -> Option<String> {
            self.0
                .iter()
                .find(|(p, _, _)| *p == pid)
                .and_then(|(_, _, path)| path.map(str::to_string))
        }
    }

    /// The case that started this: the chain is what names the culprit, and the
    /// child is left out because the record already says `afplay`.
    #[test]
    fn the_chain_is_nearest_first_and_stops_at_launchd() {
        let t = Table(vec![
            (87533, 80740, Some("/usr/bin/afplay")),
            (80740, 76777, Some("/opt/homebrew/bin/claude")),
            (76777, 1, Some("/bin/zsh")),
            (1, 0, Some("/sbin/launchd")),
        ]);
        let c = chain(&t, 87533);
        assert_eq!(
            c.iter().map(Ancestor::short_name).collect::<Vec<_>>(),
            ["claude", "zsh", "launchd"]
        );
        assert_eq!(render(&c, 2), "claude ← zsh");
        assert_eq!(render(&c, 9), "claude ← zsh ← launchd");
    }

    /// A parent that exited between the audio event and the walk still counts —
    /// its pid is the evidence even when its path is gone.
    #[test]
    fn an_unreadable_parent_is_still_a_link_in_the_chain() {
        let t = Table(vec![
            (500, 400, Some("/usr/bin/thing")),
            (400, 1, None),
            (1, 0, Some("/sbin/launchd")),
        ]);
        let c = chain(&t, 500);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].short_name(), "pid 400");
        assert_eq!(c[1].short_name(), "launchd");
    }

    /// A process whose parent cannot be read at all yields an empty chain
    /// rather than a guess.
    #[test]
    fn a_process_with_no_readable_parent_has_no_chain() {
        let t = Table(vec![]);
        assert!(chain(&t, 999).is_empty());
        assert_eq!(render(&[], 3), "");
    }

    /// A cycle ends the walk instead of spinning.
    #[test]
    fn a_cycle_in_the_tree_ends_the_walk() {
        let t = Table(vec![(10, 11, Some("/a")), (11, 10, Some("/b"))]);
        let c = chain(&t, 10);
        assert_eq!(c.iter().map(|a| a.pid).collect::<Vec<_>>(), [11]);
    }

    /// A tree deeper than the cap is truncated, not followed forever.
    #[test]
    fn the_walk_is_bounded() {
        let rows: Vec<_> = (1..40).map(|p| (p, p + 1, Some("/x"))).collect();
        assert_eq!(chain(&Table(rows), 1).len(), MAX_DEPTH);
    }

    /// `launchd` itself, and anything below pid 1, has no parent worth naming.
    #[test]
    fn launchd_is_the_top() {
        let t = Table(vec![(1, 0, Some("/sbin/launchd"))]);
        assert!(chain(&t, 1).is_empty());
    }

    /// The live tree agrees with the fake one on this very process — no audio,
    /// no devices, just the tree the kernel already has.
    #[test]
    fn the_live_walk_reaches_launchd_from_this_test() {
        let me = std::process::id() as i32;
        let c = live_chain(me);
        assert!(!c.is_empty(), "this test has a parent");
        assert_eq!(
            c.last().map(|a| a.pid),
            Some(1),
            "the walk reaches launchd: {c:?}"
        );
        assert!(c.iter().all(|a| a.pid > 0));
    }
}
