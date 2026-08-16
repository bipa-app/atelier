# ADR-0010: Sync-back on landing — folder origins mirror the shared line

Status: accepted (2026-08-16)

## Context

Every config atelier writes says `sync = "two-way"`, and until now that was a
lie: attach imports once, and the origin folder diverges from the workspace
forever. Git sources got their out-flow with bookmark motion (plain `git push`
from the mount). Folder sources — the documents-and-contracts story — had no
way home.

## Decision

1. **Out-flow on landing.** When a landing advances a line whose source is a
   local folder (the root import included), the landed tree mirrors back to
   the origin path: files written, executable bits kept, symlinks recreated,
   anything the tree lacks removed. Engine-internal names (`.atelier`, `.jj`,
   `.git`) are never touched at any depth.
2. **A fingerprint guards the origin.** At attach and after every sync the
   origin's content digest is recorded. A sync only overwrites an origin that
   still matches the recorded fingerprint; an origin edited out-of-band parks
   the sync — journaled (`sync_parked`), never silent, never destructive —
   and the landing stands regardless. A sync failure (an unwritable origin)
   parks the same way: the mirror is a degradation surface, the landing is
   not.
3. **`atelier sync [source] [--force]` reconciles.** Retry exports again under
   the same guard; `--force` overwrites deliberately after a human decides.
   Both journal their act. Workspaces attached before this decision have no
   recorded fingerprint, so their first sync parks; `--force` seeds the state.
4. **In-flow stays out.** Origin edits do not flow into the workspace; that is
   the v2 sync-adapter story. Git sources refuse `atelier sync` by name —
   bookmark motion is their out-flow.

## Consequences

- The fingerprint check and the mirror are not atomic against a concurrent
  origin edit; for local folders on human timescales this window is accepted.
  The journal records what the sync believed.
- Per-source sync state (fingerprint, last-synced snapshot) lives in the
  coordination store; derived, rebuildable, never in history.
- A parked sync leaves the origin exactly as the human left it — resolving is
  a human act (`--force`), not a merge. Two-way merge of origin edits is
  explicitly not attempted.

## Considered: an async runtime (tokio) for the mirror

Rejected for v1. The core is synchronous by design — jj-lib's futures are
driven with `pollster` at each seam, and the HTTP face chose a runtime-free
server for the same reason. The mirror is sequential local file IO over
doc-scale folders: a runtime buys no measurable speed, costs a dependency,
and parallel syncs would make journal orderings racy. Running the sync under
the landing lease also serializes concurrent writers of one origin — a
correctness property an async fire-and-forget would give up. Remote sync
adapters (v2: S3, Drive) are where an async runtime earns its place; that
decision belongs to the adapter ADR, not this one.
