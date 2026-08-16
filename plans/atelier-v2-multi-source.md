# Plan: atelier v2 — multi-source workspaces

> **Source PRD**: `plans/prd-atelier.md` (user stories 1, 2, 4, 6, 14) · **Decision**: `docs/adr/0009`
> **Linear project**: https://linear.app/bipa/project/atelier-6df1165b35cf (team ENG · N0–N3 cards)
> **Status**: Approved (model B ratified 2026-08-16)
> **Owner**: Luiz Parreira

## Why this exists

One piece of work routinely spans several projects — a backend, its SDK, the infra that deploys them — and each is its own repository whose history must stay pushable. v1 binds a workspace to one history. v2 makes the workspace the umbrella: many sources, each mounted with its own engine and shared line, under one journal, one session concept, one gate. One instruction, one attributable trail, N pushable repos.

## Project mappings

```yaml
project: atelier
remote: https://github.com/bipa-app/atelier
local_default_path: ~/bip/projects/atelier
git_strategy: worktree
base_branch: main
branch_naming: feat/<slug>
delivery: push feat branch, open a PR, squash-merge (linear history); never push main directly
```

## Architectural decisions

- Per ADR-0009: sources keep their own histories; the workspace root is source zero; sessions carry one change per touched source; one landing request fans out per source with per-mount leases (`landing/<mount>`), sequential in mount order, parking per source; addresses scope by mount.
- A v1 workspace is the degenerate case (root engine, zero mounts); multi-source is additive and no migration is needed.
- Config: `[[source]]` rows gain `name` (the mount); attach reuses the existing refusal catalog (`AlreadyAttached` becomes per-mount, `NestedWorkspace`, `LfsSourceUnsupported`).

## Out of scope

- Remote sources (git remote sync, S3/Drive) — still v2-later; local folders and local git repos only.
- Atomic cross-source landing — parking per source is the model, per ADR-0009.
- Per-source profiles/policies — one workspace policy still governs.

## Phases

---

### Phase N0: Source engines and mounts

- **Type**: AFK
- **Blocked by**: None

#### What to build

`atelier attach <path> --mount <name>` attaches additional local folders, each becoming its own engine at its mount; the root engine keeps versioning root content and ignores mounts. Auto-snapshot walks every engine; `atelier diff`, `atelier history`, and the journal aggregate with mount-scoped addresses.

#### Acceptance criteria

- [ ] Two folders attach at two mounts; edits in each produce snapshots in that source's own history (ids disjoint, histories independent — pinned with `assert_ne`).
- [ ] `atelier diff` after edits in both mounts shows both, addresses mount-scoped (`app/notes.txt`); root-level edits still diff unprefixed.
- [ ] A v1 workspace (zero mounts) behaves byte-identically to today (regression e2e).
- [ ] Attaching over an existing mount refuses (`AlreadyAttached`); a mount name that collides with root content refuses.

---

### Phase N1: Git-repo sources

- **Type**: AFK
- **Blocked by**: N0

#### What to build

Attaching a folder that is already a git repo adopts it: history preserved, jj colocated, the mount stays a real repo plain git pushes. LFS refuses loudly at attach.

#### Acceptance criteria

- [ ] Attaching a git repo with existing commits preserves them: `atelier history <mount>` lists the pre-attach commits beneath new snapshots.
- [ ] `git -C <mount> log` still works and sees atelier's snapshots; a plain `git push` from the mount succeeds against a local bare remote (story 14, per project).
- [ ] An LFS-using repo refuses at attach with `LfsSourceUnsupported`.

---

### Phase N2: Sessions across sources

- **Type**: AFK
- **Blocked by**: N0

#### What to build

`open_session` materializes a working copy per source (root included) under `.atelier/sessions/sN/<mount>/`; session reads/writes/diffs take mount-scoped paths; the session carries one change per touched source. MCP and REST route the same paths.

#### Acceptance criteria

- [ ] One session writes to two mounts and the root; `diff` (session) shows all three, mount-scoped; each touched source has its own change id (pinned distinct).
- [ ] An untouched source contributes nothing to the session diff.
- [ ] The MCP session loop and REST PUT/diff work with mount-scoped paths unchanged in shape.

---

### Phase N3: Fan-out landing

- **Type**: AFK
- **Blocked by**: N2

#### What to build

The session's one landing request applies per source: one lease per mount's landing point, sources in mount order, landing or parking each line independently. `GateOutcome` grows per-source outcomes; partial landings are values; every act journals per source. Watch (from v1) routes events to the owning engine.

#### Acceptance criteria

- [ ] A two-source session lands both lines through one request; both mounts' shared lines advance; the journal shows one land act per source under one session.
- [ ] With a conflict planted in source B only: A lands, B parks, the session stays open; resolving B and re-landing completes it — the exact partial-landing story, e2e.
- [ ] Two concurrent applies on different sources proceed in parallel (distinct lease points); on the same source, exactly one holds the lease.
- [ ] `atelier watch` snapshots edits in every mount into the right history (test).

---

## Sequencing summary

```
N0 ─┬─▶ N1 (git adoption)
    └─▶ N2 ─▶ N3 (fan-out landing)
```

N1 and N2 parallelize after N0; N3 closes the tracer bullet: one instruction, two projects, two landings, one journal.
