/// The rung a diff is carried at: the format-independent fidelity ladder.
///
/// The floor is [`Fidelity::Binary`] (changed-or-not on opaque bytes); format
/// packages raise it to [`Fidelity::Text`] (projected text) and
/// [`Fidelity::Rich`] (deltas in the format's own terms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fidelity {
    Binary,
    Text,
    Rich,
}

/// What kind of change one [`Delta`] records.
///
/// `Moved` is part of the model but is never produced at the binary rung:
/// rename detection belongs to the engine, not to a content-id comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Added,
    Removed,
    Changed,
    Moved,
}

/// Where a delta lands, in the format's own terms.
///
/// At the binary rung this is a workspace-relative path. Richer addresses —
/// a cell, a clause, a paragraph — arrive with format packages later.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Address(pub String);

impl Address {
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One addressed difference inside a [`Diff`].
///
/// `before` and `after` hold content ids, not content: the identity of the
/// bytes on each side, absent when the side does not exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delta {
    pub address: Address,
    pub kind: DeltaKind,
    pub before: Option<String>,
    pub after: Option<String>,
    pub summary: Option<String>,
}

/// The difference between two versions, carried at the highest fidelity its
/// format package allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub fidelity: Fidelity,
    pub deltas: Vec<Delta>,
}
