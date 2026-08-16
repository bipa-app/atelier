use crate::package::PackageId;

/// The rung a delta is carried at: the format-independent fidelity ladder.
///
/// The floor is [`Fidelity::Binary`] (changed-or-not on opaque bytes); the
/// ladder raises a delta to [`Fidelity::Text`] (a line diff over projected
/// text) and, once a package ships a differ, [`Fidelity::Rich`] (deltas in
/// the format's own terms).
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

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What happened to one line at the text rung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Added,
    Removed,
    /// The synthetic marker after a changed line with no trailing newline
    /// — its own kind, so document content that happens to contain the
    /// marker text can never be mistaken for it.
    NoNewline,
}

/// One line of a text-rung comparison: what happened and the line's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub text: String,
}

/// One addressed difference inside a [`Diff`], carried at its own rung.
///
/// `before` and `after` hold content ids, not content: the identity of the
/// bytes on each side, absent when the side does not exist. `lines` carries
/// the text-rung comparison and is empty at the binary rung. `package`
/// names the format package whose projection or differ produced the delta —
/// outputs carry the package version (ADR-0003) — and is `None` at rungs no
/// package produced: the binary floor and plain-text raises. `summary` is a
/// rich delta's difference in the format's own terms — the affected text
/// and the property change — and `None` below the rich rung.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delta {
    pub address: Address,
    pub kind: DeltaKind,
    pub fidelity: Fidelity,
    pub before: Option<String>,
    pub after: Option<String>,
    pub lines: Vec<Line>,
    pub package: Option<PackageId>,
    pub summary: Option<String>,
}

impl Delta {
    /// This delta raised to the text rung, carrying its line comparison and
    /// the package that projected the compared text (`None` for plain text).
    #[must_use]
    pub fn at_text_rung(self, lines: Vec<Line>, package: Option<PackageId>) -> Self {
        Self {
            fidelity: Fidelity::Text,
            lines,
            package,
            ..self
        }
    }

    /// A rich-rung delta a format package produced: addressed in the
    /// format's own terms, its difference described by `summary`.
    #[must_use]
    pub fn rich(address: Address, summary: impl Into<String>, package: PackageId) -> Self {
        Self {
            address,
            kind: DeltaKind::Changed,
            fidelity: Fidelity::Rich,
            before: None,
            after: None,
            lines: Vec::new(),
            package: Some(package),
            summary: Some(summary.into()),
        }
    }
}

/// The differences between two versions: addressed deltas, each carried at
/// the highest fidelity the ladder could raise it to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub deltas: Vec<Delta>,
}
