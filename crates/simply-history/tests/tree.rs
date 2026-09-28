//! The generic tree exercised flux-free: a counter state with additive ops —
//! push/undo/redo walks, branching + goto, prune reclaim, replay_onto, and
//! the serde envelope.
//!
//! Convention (mirrors how a real reducer drives the tree): ops are applied
//! to the state *live*, as the edit happens; `push` records the already-
//! applied transaction. Undo/redo/goto are the only paths that replay.

use simply_history::{History, HistoryId, Op, State, Transaction};

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct AddOp(i64);

impl Op for AddOp {
    fn invert(&self) -> Self {
        AddOp(-self.0)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Counter {
    value: i64,
    reclaimed: Vec<i64>,
}

impl State<AddOp> for Counter {
    fn apply(&mut self, op: &AddOp) {
        self.value += op.0;
    }

    fn apply_inverse(&mut self, op: &AddOp) {
        self.apply(&op.invert());
    }

    fn reclaim(&mut self, op: &AddOp) {
        self.reclaimed.push(op.0);
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Tx {
    label: String,
    ops: Vec<AddOp>,
}

impl Transaction for Tx {
    type Op = AddOp;

    fn ops(&self) -> &[AddOp] {
        &self.ops
    }

    fn label(&self) -> &str {
        &self.label
    }
}

/// Perform an edit the way a reducer does: apply the ops live, then record
/// the transaction. Returns the new node's id.
fn edit(history: &mut History<Tx>, state: &mut Counter, label: &str, deltas: &[i64]) -> HistoryId {
    for &d in deltas {
        state.apply(&AddOp(d));
    }
    history
        .push(Tx { label: label.into(), ops: deltas.iter().copied().map(AddOp).collect() }, state)
}

#[test]
fn push_undo_redo_walk_a_linear_trunk() {
    let mut history = History::<Tx>::default();
    let mut state = Counter::default();
    edit(&mut history, &mut state, "a", &[1]);
    let b = edit(&mut history, &mut state, "b", &[10, 100]);
    assert_eq!(state.value, 111);
    assert_eq!(history.current(), b);
    assert_eq!(history.undo_labels(), vec!["b", "a"], "nearest first");
    assert!(history.redo_labels().is_empty());

    let undone = history.undo(&mut state).expect("b undoes");
    assert_eq!(undone.label(), "b");
    assert_eq!(state.value, 1, "b's two ops inverted in reverse");
    assert_eq!(history.redo_labels(), vec!["b"]);

    history.undo(&mut state).expect("a undoes");
    assert_eq!(state.value, 0);
    assert_eq!(history.current(), HistoryId::ROOT);
    assert!(history.undo(&mut state).is_none(), "nothing to undo at the root");

    let redone = history.redo(&mut state).expect("a redoes");
    assert_eq!(redone.label(), "a");
    history.redo(&mut state).expect("b redoes");
    assert_eq!(state.value, 111, "the full trunk replays forward");
    assert!(history.redo(&mut state).is_none(), "nothing to redo at the tip");
}

#[test]
fn editing_after_undo_branches_and_goto_reaches_every_future() {
    let mut history = History::<Tx>::default();
    let mut state = Counter::default();
    let a = edit(&mut history, &mut state, "a", &[1]);
    history.undo(&mut state);
    let b = edit(&mut history, &mut state, "b", &[100]); // sibling branch of a
    assert_eq!(state.value, 100);
    assert_eq!(history.entries().count(), 3, "root + two sibling branches");

    assert!(history.goto(a, &mut state));
    assert_eq!(state.value, 1, "goto crosses the branch point to a's future");
    assert!(history.goto(b, &mut state));
    assert_eq!(state.value, 100);
    assert!(history.goto(HistoryId::ROOT, &mut state));
    assert_eq!(state.value, 0);
    assert!(!history.goto(HistoryId(404), &mut state), "unknown ids are refused");
    assert_eq!(state.value, 0, "a refused goto moves nothing");
}

#[test]
fn prune_reclaims_abandoned_leaves_only() {
    let mut history = History::<Tx>::default();
    let mut state = Counter::default();
    let a = edit(&mut history, &mut state, "a", &[7]);
    history.undo(&mut state);
    edit(&mut history, &mut state, "b", &[100]);
    history.set_cap(1);
    let c = edit(&mut history, &mut state, "c", &[1000]);

    assert!(history.entry(a).is_none(), "the abandoned a leaf was pruned");
    assert_eq!(state.reclaimed, vec![7], "a's ops were handed to reclaim");
    assert_eq!(history.current(), c, "the current path is never pruned");
    assert_eq!(history.undo_labels(), vec!["c", "b"]);

    // Ids are positional and never reused: the next push skips the hole.
    let d = edit(&mut history, &mut state, "d", &[1]);
    assert!(d.0 > c.0, "pruned slots stay holes; ids stay monotonic");
}

#[test]
fn replay_onto_reconstructs_every_node_from_the_root() {
    let mut history = History::<Tx>::default();
    let mut state = Counter::default();
    edit(&mut history, &mut state, "a", &[1]);
    edit(&mut history, &mut state, "b", &[10]);
    history.undo(&mut state);
    edit(&mut history, &mut state, "c", &[100]); // branch under a

    let ids: Vec<HistoryId> = history.entries().map(|e| e.id).collect();
    for id in ids {
        let mut fresh = Counter::default();
        assert!(history.replay_onto(&mut fresh, id), "live node replays");
        assert!(history.goto(id, &mut state), "goto agrees");
        assert_eq!(fresh.value, state.value, "replay == goto at node {id}");
    }
    assert!(!history.replay_onto(&mut Counter::default(), HistoryId(404)));
}

#[test]
fn serde_round_trips_the_tree_and_stamps_versions() {
    let mut history = History::<Tx>::default();
    let mut state = Counter::default();
    edit(&mut history, &mut state, "a", &[1]);
    history.undo(&mut state);
    edit(&mut history, &mut state, "b", &[100]);

    let value = serde_json::to_value(&history).unwrap();
    assert_eq!(value["version"], 1, "major format version");
    assert_eq!(value["minor"], simply_history::FORMAT_MINOR_VERSION);

    let back: History<Tx> = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(&back).unwrap(), value, "image survives verbatim");
    assert_eq!(back.current(), history.current());
    assert_eq!(back.undo_labels(), history.undo_labels());

    // The restored tree is live: undo walks the same path.
    let mut replayed = Counter::default();
    assert!(back.replay_onto(&mut replayed, back.current()));
    assert_eq!(replayed.value, 100);
}

#[test]
fn invalid_reprs_are_rejected_readably() {
    let e = serde_json::from_value::<History<Tx>>(serde_json::json!({
        "version": 9,
        "nodes": [{ "parent": null, "tx": null, "active": null }],
        "current": 0,
    }))
    .unwrap_err();
    assert!(e.to_string().contains("format version 9"), "{e}");

    let e = serde_json::from_value::<History<Tx>>(serde_json::json!({
        "version": 1, "nodes": [], "current": 0,
    }))
    .unwrap_err();
    assert!(e.to_string().contains("invalid saved history"), "{e}");
}
