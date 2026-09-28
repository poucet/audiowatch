//! The op-side traits: reversible primitives ([`Op`]), the state they land
//! on ([`State`]), and per-operation metadata ([`OpMeta`], implemented by
//! `simply-op-macros`' `#[derive(Op)]`).

/// One reversible primitive mutation: the ground truth undo/redo replays.
pub trait Op: Clone {
    /// The mechanically derived inverse — applying `op` then `op.invert()`
    /// is a no-op on the state.
    fn invert(&self) -> Self;
}

/// The undoable state ops apply to, plus the resource-reclaim hook.
///
/// When the op type implements [`Op`], `apply_inverse` is one line
/// (`self.apply(&op.invert())`); a consumer whose op type is foreign to it
/// (orphan rule blocks the [`Op`] impl) supplies the inversion here instead.
pub trait State<O> {
    /// Apply one op forward.
    fn apply(&mut self, op: &O);

    /// Apply one op's inverse (undo direction).
    fn apply_inverse(&mut self, op: &O);

    /// A transaction holding `op` is leaving the tree **forever** (pruned) —
    /// release whatever the op reserved (ids, handles). Called exactly once
    /// per op, never for transactions still reachable by undo/redo/goto.
    fn reclaim(&mut self, _op: &O) {}
}

/// Per-operation metadata for the journal/transaction machinery — what
/// `#[derive(Op)]` (`simply-op-macros`) generates from `#[op(...)]`
/// attributes on an operation enum's variants (or a struct):
///
/// - `#[op(mutates)]` / `#[op(read)]` (default) — whether executing the
///   operation changes state: mutating ops enter history transactions and
///   request journals; reads never do.
/// - `#[op(label = "connect {from}->{to}")]` — a display-label template
///   interpolating the operation's own fields.
pub trait OpMeta {
    /// Whether this operation mutates state (enters history / the journal).
    fn mutates(&self) -> bool;

    /// Display label from the `#[op(label = "...")]` template; defaults to
    /// the operation's (snake_case) name.
    fn label(&self) -> String;
}
