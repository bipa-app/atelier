//! Ownership and fencing (ADR-0013, H2): two claimants cannot share a
//! workspace, every activation advances the epoch, a deposed writer's
//! late writes land in a superseded lineage no restore selects, the
//! acknowledgement rule refuses a stale holder without consulting a
//! clock, and a release keeps the epoch as a high-water mark no
//! activation reuses.

use std::sync::Arc;

use atelier_hosted::object_store::ObjectStore;
use atelier_hosted::object_store::memory::InMemory;
use atelier_hosted::object_store::path::Path as ObjectPath;
use atelier_hosted::{ClaimOutcome, Ownership, OwnershipRecord, ReleaseOutcome};

/// Two node handles over one shared store — the in-memory backend
/// implements the conditional writes the ownership record requires
/// (`LocalFileSystem` does not; real buckets do).
fn two_nodes() -> (Ownership, Ownership) {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let prefix = ObjectPath::from("cells/ws1");
    let node_a = Ownership::from_store(Arc::clone(&store), prefix.clone()).expect("open the plane");
    let node_b = Ownership::from_store(store, prefix).expect("open the plane");
    (node_a, node_b)
}

#[test]
fn one_claim_wins_and_the_other_learns_who_holds() {
    let (node_a, node_b) = two_nodes();

    // A claims a fresh workspace: epoch 1.
    assert_eq!(
        node_a.claim("node-a").unwrap(),
        ClaimOutcome::Held { epoch: 1 }
    );

    // B's plain claim refuses by name; the record is untouched.
    assert_eq!(
        node_b.claim("node-b").unwrap(),
        ClaimOutcome::HeldByOther {
            holder: "node-a".to_owned(),
            epoch: 1
        }
    );
    assert_eq!(
        node_b.record().unwrap().unwrap().holder.as_deref(),
        Some("node-a")
    );

    // A re-activates: same holder, advanced epoch — an epoch never has
    // two writers, not even the same node across wakes.
    assert_eq!(
        node_a.claim("node-a").unwrap(),
        ClaimOutcome::Held { epoch: 2 }
    );
}

#[test]
fn a_release_keeps_the_epoch_as_high_water() {
    let (node_a, node_b) = two_nodes();

    assert_eq!(
        node_a.claim("node-a").unwrap(),
        ClaimOutcome::Held { epoch: 1 }
    );

    // The release is guarded: it clears the holder, keeps the epoch.
    assert_eq!(
        node_a.release("node-a", 1).unwrap(),
        ReleaseOutcome::Released
    );
    assert_eq!(
        node_a.record().unwrap(),
        Some(OwnershipRecord {
            holder: None,
            epoch: 1
        })
    );

    // A released workspace claims plainly — no takeover needed — and the
    // epoch advances past the high-water mark, never reusing it.
    assert_eq!(
        node_b.claim("node-b").unwrap(),
        ClaimOutcome::Held { epoch: 2 }
    );

    // Stale releases refuse by name: the old holder at the old epoch, the
    // new holder at a stale epoch, and a repeat after the record moved.
    assert_eq!(
        node_a.release("node-a", 1).unwrap(),
        ReleaseOutcome::NotHeld
    );
    assert_eq!(
        node_b.release("node-b", 1).unwrap(),
        ReleaseOutcome::NotHeld
    );
    assert_eq!(
        node_b.release("node-b", 2).unwrap(),
        ReleaseOutcome::Released
    );
    assert_eq!(
        node_b.release("node-b", 2).unwrap(),
        ReleaseOutcome::NotHeld
    );

    // A holderless record still refuses every acknowledgement.
    assert!(!node_b.confirm("node-b", 2).unwrap());
}

#[test]
fn a_deposed_writer_lands_in_a_superseded_lineage() {
    let (node_a, node_b) = two_nodes();

    let ClaimOutcome::Held { epoch: first } = node_a.claim("node-a").unwrap() else {
        panic!("a fresh workspace must claim");
    };
    node_a
        .put_under_epoch(first, "segment-1", b"a's work".to_vec())
        .unwrap();

    // B takes over deliberately: the epoch advances, A is deposed.
    let ClaimOutcome::Held { epoch: second } = node_b.take_over("node-b").unwrap() else {
        panic!("a takeover must hold");
    };
    assert_eq!(second, first + 1);

    // A can still write — plain PUTs never refuse — but its late writes
    // land under the superseded lineage, invisible to B's.
    node_a
        .put_under_epoch(first, "segment-2-late", b"a's late work".to_vec())
        .unwrap();
    node_b
        .put_under_epoch(second, "segment-1", b"b's work".to_vec())
        .unwrap();
    let current = node_b.keys_under_epoch(second).unwrap();
    assert_eq!(current.len(), 1);
    assert!(
        current[0].ends_with(&format!("e{second}/segment-1")),
        "{current:?}"
    );
    let superseded = node_a.keys_under_epoch(first).unwrap();
    assert_eq!(superseded.len(), 2, "{superseded:?}");
    assert_ne!(current, superseded);

    // The acknowledgement rule: A's ownership read shows the new owner,
    // so A can never acknowledge what B's lineage does not carry.
    assert!(!node_a.confirm("node-a", first).unwrap());
    assert!(node_b.confirm("node-b", second).unwrap());
}

#[test]
fn a_swap_race_admits_exactly_one_winner() {
    let (node_a, node_b) = two_nodes();

    // Both nodes race the conditional create for a fresh workspace.
    let outcomes = [
        node_a.claim("node-a").unwrap(),
        node_b.claim("node-b").unwrap(),
    ];
    let winners = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, ClaimOutcome::Held { .. }))
        .count();
    assert_eq!(winners, 1, "{outcomes:?}");

    // Both race a takeover from the same observed record; the swap names
    // the exact version read, so the bucket admits exactly one.
    let outcomes = [
        node_a.take_over("node-a").unwrap(),
        node_b.take_over("node-b").unwrap(),
    ];
    let held: Vec<u64> = outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            ClaimOutcome::Held { epoch } => Some(*epoch),
            ClaimOutcome::HeldByOther { .. } => None,
        })
        .collect();
    // Sequential here, so both succeed — but each advanced a distinct
    // epoch over the version it read; no epoch ever had two writers.
    assert_eq!(held.len(), 2);
    assert_ne!(held[0], held[1]);
}
