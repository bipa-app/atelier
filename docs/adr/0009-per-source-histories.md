# Sources keep their own histories; the workspace is the umbrella

One piece of work routinely spans several projects — a backend, its SDK, the
infra that deploys them — and each project is its own repository with its own
history that must stay pushable. A workspace therefore attaches many sources,
each mounted at its own subdirectory and each carrying its own engine: its own
jj repository, its own history, its own shared line, its own landing point.
What stays singular is the umbrella the actors work in — one journal, one
coordination store, one session concept, one gate — so one instruction
produces one attributable trail across every project it touches.

The workspace root remains an engine of its own — source zero — versioning the
content that belongs to the work rather than to any project: plans, notes,
cross-project documents. Mount directories are outside the root's boundary the
way engine internals are.

A session materializes a working copy per source and carries one change per
source it touches. Its one landing request fans out at apply time: one lease
per source's landing point, sources applied in deterministic mount order,
each landing or parking on its own line. Cross-source landing is not atomic
and does not pretend to be — a source that parks leaves the session open with
the others landed, every outcome journaled per source, exactly the parked
semantics ADR-0007 already defines. Addresses scope by mount
(`app/src/main.rs`), the channel the diff ladder already uses.

## Considered options

- One history, many mounts (all sources imported into a single workspace
  history, synced back at mount boundaries): rejected — each project's real
  history would receive synthetic sync commits instead of the session's own
  change, and cloning the workspace yields a megarepo rather than the
  projects themselves.
- Linked workspaces (one history per workspace, sessions federated across
  workspaces): rejected — the journal fragments across stores, and the
  actors' place of work stops being one workspace.

## Consequences

- A v1 workspace is the degenerate case: root engine, zero mounts. Nothing
  changes for it; multi-source is additive.
- Snapshot ids, histories, and diffs are per-source; every cross-source read
  model (diff, history, watch) aggregates with mount-scoped addresses.
- The landing lease point is per source (`landing/<mount>`); the one-lease
  invariant holds per line, not per workspace.
- `GateOutcome` grows per-source outcomes; a partial landing is a value, not
  an error, and the journal records each source's act.
- A git-repo source is adopted, never imported: its history is preserved and
  the mount stays a real repository plain git can push. Git-LFS sources still
  refuse loudly at attach (ADR-0002).
