//! The history tree itself: arena, current pointer, MRU branches,
//! undo/redo/goto, prune, serde. See the crate docs for the model.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::op::State;

/// Stable handle of one history-tree node. `HistoryId::ROOT` is the root
/// sentinel — the initial state, carrying no transaction. Ids are never
/// reused, even after pruning.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HistoryId(pub usize);

impl HistoryId {
    pub const ROOT: HistoryId = HistoryId(0);
}

impl std::fmt::Display for HistoryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The history atom the tree stores: the consumer's own transaction record —
/// its ops (ground truth for replay), a display label, and whatever metadata
/// it carries (author, scope, provenance, …). Serialization of the record is
/// entirely the consumer's, so wire formats survive extraction verbatim.
pub trait Transaction: Clone {
    type Op;

    /// The primitive ops this transaction recorded, in application order.
    fn ops(&self) -> &[Self::Op];

    /// Display label ("connect osc→filter", "add 4 nodes").
    fn label(&self) -> &str;
}

#[derive(Clone, Debug)]
struct HistoryNode<T> {
    /// `None` only on the root sentinel.
    tx: Option<T>,
    parent: Option<usize>,
    children: Vec<usize>,
    /// The child redo follows from here — the most recently used branch.
    active: Option<usize>,
}

/// A read-only view of one tree node, for history UIs.
pub struct HistoryEntry<'a, T> {
    pub id: HistoryId,
    pub parent: Option<HistoryId>,
    /// `None` for the root sentinel (the initial state).
    pub tx: Option<&'a T>,
    pub is_current: bool,
    /// The child redo would follow from this node.
    pub active_child: Option<HistoryId>,
}

/// Default [`History::set_cap`] value — generous; pruning only reclaims
/// abandoned branches, never the path to the current position.
pub const DEFAULT_HISTORY_CAP: usize = 1000;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    bound(serialize = "T: Serialize", deserialize = "T: DeserializeOwned"),
    into = "HistoryRepr<T>",
    try_from = "HistoryRepr<T>"
)]
pub struct History<T: Transaction> {
    /// Arena; `None` marks pruned slots so ids stay stable.
    nodes: Vec<Option<HistoryNode<T>>>,
    current: usize,
    cap: usize,
}

impl<T: Transaction> Default for History<T> {
    fn default() -> Self {
        Self {
            nodes: vec![Some(HistoryNode {
                tx: None,
                parent: None,
                children: Vec::new(),
                active: None,
            })],
            current: HistoryId::ROOT.0,
            cap: DEFAULT_HISTORY_CAP,
        }
    }
}

impl<T: Transaction> History<T> {
    /// Cap the number of live transactions; excess prunes oldest abandoned
    /// leaves (reclaiming their ops' resources) on the next push. A pure
    /// linear trunk is never truncated — it is all undo-reachable.
    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap;
    }

    pub fn current(&self) -> HistoryId {
        HistoryId(self.current)
    }

    /// Whether the tree holds no transactions — just the root sentinel.
    pub fn is_empty(&self) -> bool {
        self.nodes.iter().flatten().count() == 1
    }

    /// The transaction a tree node holds (`None` for the root sentinel or a
    /// pruned/unknown id).
    pub fn entry(&self, id: HistoryId) -> Option<&T> {
        self.node(id.0)?.tx.as_ref()
    }

    /// Every live tree node, in creation (= chronological) order.
    pub fn entries(&self) -> impl Iterator<Item = HistoryEntry<'_, T>> {
        self.nodes.iter().enumerate().filter_map(|(i, slot)| {
            let node = slot.as_ref()?;
            Some(HistoryEntry {
                id: HistoryId(i),
                parent: node.parent.map(HistoryId),
                tx: node.tx.as_ref(),
                is_current: i == self.current,
                active_child: node.active.map(HistoryId),
            })
        })
    }

    /// Labels undo would walk through, nearest first (current → root).
    pub fn undo_labels(&self) -> Vec<&str> {
        let mut labels = Vec::new();
        let mut at = self.current;
        while let Some(node) = self.node(at) {
            if let Some(tx) = &node.tx {
                labels.push(tx.label());
            }
            let Some(parent) = node.parent else { break };
            at = parent;
        }
        labels
    }

    /// Labels redo would walk through, nearest first (the active descent).
    pub fn redo_labels(&self) -> Vec<&str> {
        let mut labels = Vec::new();
        let mut at = self.current;
        while let Some(child) = self.node(at).and_then(|n| n.active) {
            let Some(node) = self.node(child) else { break };
            labels.push(node.tx.as_ref().expect("non-root node holds a transaction").label());
            at = child;
        }
        labels
    }

    fn node(&self, id: usize) -> Option<&HistoryNode<T>> {
        self.nodes.get(id)?.as_ref()
    }

    fn node_mut(&mut self, id: usize) -> &mut HistoryNode<T> {
        self.nodes[id].as_mut().expect("live history node")
    }

    /// Append `tx` as a new branch under the current position and move onto
    /// it. Nothing is discarded — a divergent redo future stays as a sibling
    /// branch — but the size cap may prune oldest abandoned leaves (whose
    /// ops are handed to [`State::reclaim`]).
    pub fn push(&mut self, tx: T, state: &mut impl State<T::Op>) -> HistoryId {
        let id = self.nodes.len();
        self.nodes.push(Some(HistoryNode {
            tx: Some(tx),
            parent: Some(self.current),
            children: Vec::new(),
            active: None,
        }));
        let parent = self.node_mut(self.current);
        parent.children.push(id);
        parent.active = Some(id);
        self.current = id;
        self.prune(state);
        HistoryId(id)
    }

    /// Step to the parent, replaying the current transaction's ops inverted
    /// in reverse. Returns the undone transaction.
    pub fn undo(&mut self, state: &mut impl State<T::Op>) -> Option<&T> {
        let from = self.current;
        let parent = self.node(from)?.parent?;
        self.replay_up(from, state);
        // Redo retraces this branch even if a sibling was used more recently.
        self.node_mut(parent).active = Some(from);
        self.current = parent;
        self.node(from).and_then(|n| n.tx.as_ref())
    }

    /// Step onto the most recently used child, replaying its ops forward.
    /// Returns the redone transaction.
    pub fn redo(&mut self, state: &mut impl State<T::Op>) -> Option<&T> {
        let child = self.node(self.current)?.active?;
        self.replay_down(child, state);
        self.current = child;
        self.node(child).and_then(|n| n.tx.as_ref())
    }

    /// Jump to any live tree node: undo up to the common ancestor, then redo
    /// down `target`'s branch, marking it active along the way. `false` for
    /// an unknown or pruned id.
    pub fn goto(&mut self, target: HistoryId, state: &mut impl State<T::Op>) -> bool {
        if self.node(target.0).is_none() {
            return false;
        }
        // Ancestor chains (self-inclusive, root-terminated) of both ends.
        let chain = |mut at: usize| {
            let mut path = vec![at];
            while let Some(parent) = self.node(at).and_then(|n| n.parent) {
                path.push(parent);
                at = parent;
            }
            path
        };
        let up = chain(self.current);
        let down = chain(target.0);
        let ancestor =
            *down.iter().find(|id| up.contains(id)).expect("tree nodes share at least the root");

        for &id in up.iter().take_while(|&&id| id != ancestor) {
            self.replay_up(id, state);
            let parent = self.node(id).and_then(|n| n.parent).expect("below the ancestor");
            self.node_mut(parent).active = Some(id);
        }
        let descent: Vec<usize> = down.iter().copied().take_while(|&id| id != ancestor).collect();
        for &id in descent.iter().rev() {
            self.replay_down(id, state);
            let parent = self.node(id).and_then(|n| n.parent).expect("below the ancestor");
            self.node_mut(parent).active = Some(id);
        }
        self.current = target.0;
        true
    }

    /// Replay node `id`'s ops inverted, in reverse (stepping state above it).
    fn replay_up(&self, id: usize, state: &mut impl State<T::Op>) {
        let tx = self.node(id).and_then(|n| n.tx.as_ref()).expect("non-root live node");
        for op in tx.ops().iter().rev() {
            state.apply_inverse(op);
        }
    }

    /// Replay node `id`'s ops forward (stepping state onto it).
    fn replay_down(&self, id: usize, state: &mut impl State<T::Op>) {
        let tx = self.node(id).and_then(|n| n.tx.as_ref()).expect("non-root live node");
        for op in tx.ops() {
            state.apply(op);
        }
    }

    /// Replay the root→`target` path forward onto `state`, which the caller
    /// vouches is at the **root** (initial) position. Read-only on the tree —
    /// the current position is untouched — and `false` for a pruned or
    /// unknown id. This is the proof (and the tool) that a saved op log is
    /// self-sufficient: no state beyond the root and the ops is needed to
    /// reach any node of the tree.
    pub fn replay_onto(&self, state: &mut impl State<T::Op>, target: HistoryId) -> bool {
        if self.node(target.0).is_none() {
            return false;
        }
        let mut path = Vec::new();
        let mut at = target.0;
        while let Some(parent) = self.node(at).and_then(|n| n.parent) {
            path.push(at);
            at = parent;
        }
        for &id in path.iter().rev() {
            self.replay_down(id, state);
        }
        true
    }

    /// While over the cap, remove the oldest abandoned leaf (never the
    /// current node, never the root) and hand its ops to
    /// [`State::reclaim`] — the single place reservation ends.
    fn prune(&mut self, state: &mut impl State<T::Op>) {
        loop {
            let live = self.nodes.iter().flatten().count() - 1; // sans root
            if live <= self.cap {
                return;
            }
            let victim = self.nodes.iter().enumerate().position(|(i, slot)| {
                matches!(slot, Some(node)
                    if node.children.is_empty() && i != self.current && i != HistoryId::ROOT.0)
            });
            let Some(victim) = victim else { return };
            let node = self.nodes[victim].take().expect("victim is live");
            let tx = node.tx.expect("non-root node holds a transaction");
            for op in tx.ops() {
                state.reclaim(op);
            }
            if let Some(parent) = node.parent {
                let parent = self.node_mut(parent);
                parent.children.retain(|&c| c != victim);
                if parent.active == Some(victim) {
                    parent.active = parent.children.last().copied();
                }
            }
        }
    }
}

// ── serialization ───────────────────────────────────────────────────────

/// Major format version of the serialized tree — breaking changes only.
/// Additions (new metadata on the consumer's transaction record, defaulted
/// fields) bump [`FORMAT_MINOR_VERSION`] instead and stay readable.
pub const FORMAT_VERSION: u32 = 1;

/// Additive format generation: bumped when new (defaulted) fields appear in
/// the envelope or in typical transaction records. Readers accept any minor
/// — the field is informative, so tools can tell which writer produced a
/// file. History: 0 = the original flux tree; 1 = request provenance on
/// transaction records.
pub const FORMAT_MINOR_VERSION: u32 = 1;

/// The serialized image of the tree: a **positional** arena copy — index is
/// the [`HistoryId`], pruned slots serialize as `null` — plus the current
/// position. Children lists and MRU consistency are rebuilt/validated on
/// load; the cap is a runtime setting and is not persisted.
#[derive(Clone, Serialize, Deserialize)]
#[serde(bound(serialize = "T: Serialize", deserialize = "T: DeserializeOwned"))]
pub struct HistoryRepr<T> {
    version: u32,
    /// Additive generation ([`FORMAT_MINOR_VERSION`]); absent in old files.
    #[serde(default)]
    minor: u32,
    nodes: Vec<Option<HistoryNodeRepr<T>>>,
    current: usize,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(bound(serialize = "T: Serialize", deserialize = "T: DeserializeOwned"))]
struct HistoryNodeRepr<T> {
    /// `None` only on the root sentinel (slot 0).
    parent: Option<usize>,
    /// `None` only on the root sentinel.
    tx: Option<T>,
    /// The child redo follows from this node (the MRU branch).
    active: Option<usize>,
}

impl<T: Transaction> From<History<T>> for HistoryRepr<T> {
    fn from(history: History<T>) -> Self {
        HistoryRepr {
            version: FORMAT_VERSION,
            minor: FORMAT_MINOR_VERSION,
            nodes: history
                .nodes
                .into_iter()
                .map(|slot| {
                    slot.map(|n| HistoryNodeRepr { parent: n.parent, tx: n.tx, active: n.active })
                })
                .collect(),
            current: history.current,
        }
    }
}

/// Why a serialized history was rejected. Messages are user-facing — they
/// surface verbatim through the consumer's load errors.
#[derive(Debug)]
pub struct FormatError(pub String);

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FormatError {}

impl<T: Transaction> TryFrom<HistoryRepr<T>> for History<T> {
    type Error = FormatError;

    /// Structural validation: exactly the invariants the live tree
    /// maintains — a root sentinel at slot 0, every live non-root node
    /// holding a transaction and a live parent *older than itself* (pushes
    /// are chronological, which also rules out cycles), MRU links pointing
    /// at live children, and a live current position. Transaction *content*
    /// is trusted the way the surrounding document is — the same trust as
    /// the rest of the file.
    fn try_from(repr: HistoryRepr<T>) -> Result<Self, Self::Error> {
        let fail = |msg: String| FormatError(format!("invalid saved history: {msg}"));
        if repr.version != FORMAT_VERSION {
            return Err(FormatError(format!(
                "saved history uses format version {}; this build reads version \
                 {FORMAT_VERSION}",
                repr.version
            )));
        }
        let live = |id: usize| repr.nodes.get(id).is_some_and(|slot| slot.is_some());
        for (id, slot) in repr.nodes.iter().enumerate() {
            let Some(node) = slot else { continue };
            match (id, node.parent, &node.tx) {
                (0, None, None) => {}
                (0, _, _) => return Err(fail("the root sentinel carries a parent or ops".into())),
                (_, Some(parent), Some(_)) if parent < id && live(parent) => {}
                (_, Some(parent), Some(_)) => {
                    return Err(fail(format!("node {id} has a broken parent link ({parent})")))
                }
                (_, _, _) => return Err(fail(format!("node {id} is missing its parent or ops"))),
            }
            if let Some(child) = node.active {
                let child_of_this =
                    repr.nodes.get(child).and_then(|s| s.as_ref()).map(|n| n.parent);
                if child_of_this != Some(Some(id)) {
                    return Err(fail(format!("node {id}'s redo link ({child}) is no child")));
                }
            }
        }
        if !live(repr.current) {
            return Err(fail(format!("current position {} is no live node", repr.current)));
        }
        if !live(0) {
            return Err(fail("the tree has no root sentinel".into()));
        }

        let mut nodes: Vec<Option<HistoryNode<T>>> = repr
            .nodes
            .into_iter()
            .map(|slot| {
                slot.map(|n| HistoryNode {
                    tx: n.tx,
                    parent: n.parent,
                    children: Vec::new(),
                    active: n.active,
                })
            })
            .collect();
        // Rebuild children in ascending id order — push ids are monotonic,
        // so this reproduces the original (chronological) sibling order.
        for id in 0..nodes.len() {
            let Some(parent) = nodes[id].as_ref().and_then(|n| n.parent) else { continue };
            nodes[parent].as_mut().expect("validated live parent").children.push(id);
        }
        Ok(History { nodes, current: repr.current, cap: DEFAULT_HISTORY_CAP })
    }
}
