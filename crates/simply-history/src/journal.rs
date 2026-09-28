//! A session **request journal**: the record/replay substrate.
//!
//! Where the history tree stores *primitive ops* (what changed), the journal
//! stores the *requests that were asked* (typed operation values, e.g. an
//! api's generated request enum), in acceptance order with a monotonic
//! sequence number. Opt-in (`Journal::default()` is off), suspendable while
//! a replay re-drives recorded requests so they aren't recorded twice.

/// Sequence-numbered request log. `R` is the consumer's typed request value
/// (kept whole — rendering/serialization is the consumer's).
#[derive(Clone, Debug)]
pub struct Journal<R> {
    entries: Vec<(u64, R)>,
    next_seq: u64,
    recording: bool,
    suspended: bool,
}

impl<R> Default for Journal<R> {
    fn default() -> Self {
        Self { entries: Vec::new(), next_seq: 0, recording: false, suspended: false }
    }
}

impl<R> Journal<R> {
    /// A journal that records from the start.
    pub fn recording() -> Self {
        Self { recording: true, ..Self::default() }
    }

    /// Whether records are currently being kept (on and not suspended).
    pub fn is_recording(&self) -> bool {
        self.recording && !self.suspended
    }

    /// Turn recording on/off (the opt-in). Entries already recorded stay.
    pub fn set_recording(&mut self, on: bool) {
        self.recording = on;
    }

    /// Suspend/resume recording — a replay re-drives recorded requests
    /// through the normal path without journaling them again (suspension can
    /// span awaits, so it's a flag rather than a closure scope).
    pub fn set_suspended(&mut self, suspended: bool) {
        self.suspended = suspended;
    }

    /// Record one accepted request; returns its sequence number, or `None`
    /// when not recording. Sequence numbers are monotonic and never reused.
    pub fn record(&mut self, request: R) -> Option<u64> {
        if !self.is_recording() {
            return None;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.push((seq, request));
        Some(seq)
    }

    /// Every recorded entry, oldest first.
    pub fn entries(&self) -> &[(u64, R)] {
        &self.entries
    }

    /// The entries at or after `from_seq` — what a replay re-drives.
    pub fn entries_from(&self, from_seq: u64) -> &[(u64, R)] {
        let start = self.entries.partition_point(|(seq, _)| *seq < from_seq);
        &self.entries[start..]
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_by_default_and_opt_in() {
        let mut journal = Journal::default();
        assert_eq!(journal.record("a"), None);
        journal.set_recording(true);
        assert_eq!(journal.record("b"), Some(0));
        assert_eq!(journal.record("c"), Some(1));
        assert_eq!(journal.entries(), &[(0, "b"), (1, "c")]);
    }

    #[test]
    fn suspension_skips_recording_and_restores() {
        let mut journal = Journal::recording();
        journal.record("a");
        journal.set_suspended(true);
        assert!(!journal.is_recording());
        assert_eq!(journal.record("replayed"), None);
        journal.set_suspended(false);
        assert_eq!(journal.record("b"), Some(1), "seq not burned by suspended records");
        assert_eq!(journal.len(), 2);
    }

    #[test]
    fn entries_from_partitions_on_seq() {
        let mut journal = Journal::recording();
        for s in ["a", "b", "c"] {
            journal.record(s);
        }
        assert_eq!(journal.entries_from(0).len(), 3);
        assert_eq!(journal.entries_from(1), &[(1, "b"), (2, "c")]);
        assert!(journal.entries_from(9).is_empty());
    }
}
