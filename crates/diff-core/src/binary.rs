use std::collections::{BTreeMap, BTreeSet};

use crate::model::{Address, Delta, DeltaKind, Diff, Fidelity};

/// The binary rung: diff two file listings by content id alone.
///
/// Each listing maps a workspace-relative path to the content id of the bytes
/// at that path. A path present on one side only is `Added`/`Removed`; a path
/// on both sides with a different id is `Changed`; an unchanged id yields no
/// delta. Deltas come back sorted by path, so the same inputs always produce
/// the same diff. `Moved` is never produced here — it needs the engine's
/// rename detection, not a content-id comparison.
#[must_use]
pub fn diff_listings(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Diff {
    let paths: BTreeSet<&String> = before.keys().chain(after.keys()).collect();

    let mut deltas = Vec::new();
    for path in paths {
        let delta = match (before.get(path), after.get(path)) {
            (Some(old), None) => Some(binary_delta(path, DeltaKind::Removed, Some(old), None)),
            (None, Some(new)) => Some(binary_delta(path, DeltaKind::Added, None, Some(new))),
            (Some(old), Some(new)) if old != new => {
                Some(binary_delta(path, DeltaKind::Changed, Some(old), Some(new)))
            }
            _ => None,
        };
        if let Some(delta) = delta {
            deltas.push(delta);
        }
    }

    Diff { deltas }
}

fn binary_delta(
    path: &str,
    kind: DeltaKind,
    before: Option<&String>,
    after: Option<&String>,
) -> Delta {
    Delta {
        address: Address::new(path),
        kind,
        fidelity: Fidelity::Binary,
        before: before.cloned(),
        after: after.cloned(),
        lines: Vec::new(),
        package: None,
        summary: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(path, id)| ((*path).to_owned(), (*id).to_owned()))
            .collect()
    }

    #[test]
    fn added_path_yields_added_delta() {
        let before = listing(&[]);
        let after = listing(&[("a.txt", "id1")]);

        let diff = diff_listings(&before, &after);

        assert_eq!(diff.deltas.len(), 1);
        let delta = &diff.deltas[0];
        assert_eq!(delta.address, Address::new("a.txt"));
        assert_eq!(delta.kind, DeltaKind::Added);
        assert_eq!(delta.fidelity, Fidelity::Binary);
        assert_eq!(delta.before, None);
        assert_eq!(delta.after, Some("id1".to_owned()));
        assert!(delta.lines.is_empty());
    }

    #[test]
    fn removed_path_yields_removed_delta() {
        let before = listing(&[("a.txt", "id1")]);
        let after = listing(&[]);

        let diff = diff_listings(&before, &after);

        assert_eq!(diff.deltas.len(), 1);
        let delta = &diff.deltas[0];
        assert_eq!(delta.kind, DeltaKind::Removed);
        assert_eq!(delta.fidelity, Fidelity::Binary);
        assert_eq!(delta.before, Some("id1".to_owned()));
        assert_eq!(delta.after, None);
    }

    #[test]
    fn changed_id_yields_changed_delta() {
        let before = listing(&[("a.txt", "id1")]);
        let after = listing(&[("a.txt", "id2")]);

        let diff = diff_listings(&before, &after);

        assert_eq!(diff.deltas.len(), 1);
        let delta = &diff.deltas[0];
        assert_eq!(delta.kind, DeltaKind::Changed);
        assert_eq!(delta.fidelity, Fidelity::Binary);
        assert_eq!(delta.before, Some("id1".to_owned()));
        assert_eq!(delta.after, Some("id2".to_owned()));
    }

    #[test]
    fn mixed_changes_come_back_sorted_by_path() {
        let before = listing(&[("gone.txt", "g1"), ("keep.txt", "k1"), ("edit.txt", "e1")]);
        let after = listing(&[("keep.txt", "k1"), ("edit.txt", "e2"), ("new.txt", "n1")]);

        let diff = diff_listings(&before, &after);

        let observed: Vec<(&str, DeltaKind)> = diff
            .deltas
            .iter()
            .map(|d| (d.address.as_str(), d.kind))
            .collect();
        assert_eq!(
            observed,
            vec![
                ("edit.txt", DeltaKind::Changed),
                ("gone.txt", DeltaKind::Removed),
                ("new.txt", DeltaKind::Added),
            ]
        );
    }

    #[test]
    fn identical_listings_yield_no_deltas() {
        let before = listing(&[("a.txt", "id1"), ("b.txt", "id2")]);
        let after = listing(&[("a.txt", "id1"), ("b.txt", "id2")]);

        let diff = diff_listings(&before, &after);

        assert!(diff.deltas.is_empty());
    }

    #[test]
    fn both_empty_yields_no_deltas() {
        let diff = diff_listings(&listing(&[]), &listing(&[]));

        assert!(diff.deltas.is_empty());
    }
}
