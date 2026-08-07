# Plan: atelier v1 — tracer bullet

> **Source PRD**: `plans/prd-atelier.md`
> **Linear project**: https://linear.app/bipa/project/atelier-6df1165b35cf (team ENG · PRD ENG-9651 · M0–M4 = ENG-9652…9656)
> **Status**: Approved
> **Owner**: Luiz Parreira

## Why this exists

Bipa has built three ad-hoc agent workspaces (Buzz's R2 git server, bip's workspaces, satoshi-holmes case files) and each rebuilds the same substrate badly: docs don't diff, actions aren't attributed, concurrency is improvised. atelier extracts that substrate into one open-source tool. v1 is a single tracer bullet — one workspace, one agent, one doc, one diff — proving the whole domain model end-to-end: auto-snapshot engine, diff ladder, agent session with landing, and the journal. Everything later (profiles, remote sources, rich differs, hosted runtime) widens this path; nothing replaces it.

## Project mappings

```yaml
project: atelier
remote: none yet            # GitHub org decision (bipa-app vs personal) pending
local_default_path: ~/bip/projects/atelier
git_strategy: worktree
base_branch: main
branch_naming: feat/<slug>
```

## Architectural decisions

Recorded as ADRs in `docs/adr/`; vocabulary in `CONTEXT.md`. Durable across all phases:

- Everything versioned; VCS model is the content model (ADR-0001).
- jj model as engine via pinned jj-lib, git backend, colocated `.git`; LFS sources fail loudly at attach (ADR-0002).
- One diff library (`diff-core`): Diff = addressed Deltas; fidelity ladder binary → projected text → rich; `FormatPackage` trait with mandatory projector, optional differ; packages as independent crates (ADR-0003).
- Projections are derived artifacts, cached by (blob, package version), never committed.
- Journal is append-only and distinct from history; instruction capture = summary + run reference by default, verbatim per profile (ADR-0004).
- Editing never needs a lease; landing always does. Optimistic concurrency by default.
- Crate layout (all `publish = false`): `core`, `diff-core`, `format-docx`, `surface`, `cli` (binary `ws`). anyhow + tracing. No mod.rs.
- Delivery surfaces: one core, three faces — the SDK (core library) is the product; `ws` CLI, MCP (stdio in v1, streamable HTTP post-v1), and REST are thin shells over one API; no shell-only behavior (ADR-0006).
- Storage: content = git object store via jj (ADR-0002); journal = SQLite beside the repo, never inside it (ADR-0005); projections = content-addressed derived cache, evictable.
- Agent surface verbs: `open_session`, `write`, `diff`, `land`, `journal` (MCP; HTTP transports are the M5 backlog slice, gated on M2).

## Out of scope

- Remote sources (git remote sync, S3/Drive/SFTP folders) — v2; folder-attach proves the source concept first.
- Gates, approvals, and non-default profiles — policy machinery arrives with the second profile.
- Rich differs (xlsx, pdf) — additive by design of the ladder.
- Hosted / multi-node runtime (celld / Cloudflare DO) — local-first daemon first; the domain never depends on it.
- WASM plugin ABI — in-process crates until third-party packages exist.
- crates.io publishing; GitHub org + remote push — pending the org decision.

## Holistic scope

- **Compliance**: N/A for v1 (no Bipa data). The journal + verbatim capture design (ADR-0004) is the future compliance surface.
- **Legal**: Apache-2.0 committed at repo root. No CLA for now.
- **Security**: agent surface binds localhost only in v1; no network exposure.
- **Business model / pricing**: none — open source.
- **GTM / marketing**: M4 ships the OSS floor (README quickstart, CI badge, contribution surface). Public announcement is out of scope until after v1.
- **Data / observability**: the journal is the product's own observability; process logs via tracing.
- **Infrastructure**: none beyond GitHub Actions CI (added in M4).

## Phases

---

### Phase M0: Round-trip skeleton

- **Type**: AFK
- **Target date**: 2026-08-14
- **User stories covered**: 1, 3, 5, 8
- **Blocked by**: None
- **Parallelizable with**: nothing (everything else builds on it)

#### What to build

The complete degraded loop in one slice: `ws init` creates a workspace, `ws attach` binds a local folder source, any `ws` command snapshots outstanding edits (jj auto-snapshot), `ws diff` reports changes at the binary rung (added/removed/changed paths), and `ws journal` shows every act attributed to an actor. Cargo workspace bootstrap (5 crates, quality gates) happens inside this slice, not as a separate phase.

#### Acceptance criteria

- [ ] In a fresh temp dir: `ws init` then `ws attach <folder>` succeed; editing a file and running any `ws` command records a snapshot.
- [ ] `ws diff` between the two latest snapshots names the changed path (binary rung).
- [ ] `ws journal` lists the snapshot act attributed to the configured actor.
- [ ] An e2e test drives that round-trip; `cargo fmt --check`, `clippy --all-targets -- -D warnings`, `cargo test --workspace` are green.

#### Risks / unknowns

- jj-lib v0.x API surface for embedding (snapshot + op log) — spike early in the slice.
- Undo semantics exposed via op log may need to wait; journal read-path is the requirement here.

---

### Phase M1: Document diffs

- **Type**: AFK
- **Target date**: 2026-08-21
- **User stories covered**: 10, 13 (first package proves the seam)
- **Blocked by**: M0
- **Parallelizable with**: M2, M3

#### What to build

The fidelity ladder becomes real: `diff-core` gains projections and the text rung; `format-docx` ships as the first format package (projector only, docx → markdown). `ws diff` on a changed docx prints a readable markdown line diff; unknown formats keep the binary rung; plain text diffs as text.

#### Acceptance criteria

- [ ] Given two versions of a fixture .docx, `ws diff` prints a markdown line diff containing the edited sentence — never "binary files differ".
- [ ] Golden test: same docx blob + same package version → byte-identical projection.
- [ ] A file of unknown format still diffs at the binary rung; .md/.txt diff as plain text (ladder covered by tests).

#### Risks / unknowns

- OOXML → markdown determinism (ordering, tables) — constrain v1 scope to body/headings/lists/tables.

---

### Phase M2: Agent surface

- **Type**: AFK
- **Target date**: 2026-08-21
- **User stories covered**: 4, 5, 6, 7, 11
- **Blocked by**: M0
- **Parallelizable with**: M1, M3

#### What to build

The agent round-trip over MCP: `open_session` returns the agent its own working copy and change; the agent edits, calls `diff`, then `land` — which takes the in-process landing lease and advances the shared line; `journal` records the session with instruction summary + run reference. A scripted MCP client demos the whole loop.

#### Acceptance criteria

- [ ] A scripted MCP client opens a session, edits a file, lands — and the change is visible on the shared line via `ws log`.
- [ ] Two concurrent land requests: exactly one holds the lease; the other is refused or queued; a test proves no corruption.
- [ ] `ws journal` shows the session with instruction summary and run reference.
- [ ] The MCP `diff` tool returns the same diff `ws diff` shows for that change (both ends of the loop observed).

#### Risks / unknowns

- MCP server framing in Rust (stdio vs HTTP) — pick one for v1, keep `surface` thin.

---

### Phase M3: Live folder watch

- **Type**: AFK
- **Target date**: 2026-08-21
- **User stories covered**: 1, 3 (continuous form)
- **Blocked by**: M0
- **Parallelizable with**: M1, M2

#### What to build

`ws watch` turns auto-snapshot continuous: external edits (Finder, any editor) become attributed snapshots within seconds, no `ws` command needed. The journal shows them; stop/restart catches up cleanly.

#### Acceptance criteria

- [ ] With `ws watch` running, an external append to a file produces a snapshot within 5 seconds; `ws journal` shows the act attributed to the human actor.
- [ ] Stopping the watcher stops snapshots; restarting catches up edits made while stopped (tests for both).
- [ ] Engine internals (`.jj`, `.git`) are never snapshotted as content.

#### Risks / unknowns

- fs event debounce vs "within 5s" bound on macOS — tune with a test-controlled clock if needed.

---

### Phase M4: OSS floor

- **Type**: AFK
- **Target date**: 2026-08-28
- **User stories covered**: 13, 14 (adoption surface)
- **Blocked by**: M0 (repo layout); best landed after M1–M3
- **Parallelizable with**: M1, M2, M3 (CI part)

#### What to build

The repo becomes contributable: GitHub Actions CI running the quality gates, README quickstart that a stranger can run end-to-end, CONTRIBUTING with the format-package seam documented, license/manifest hygiene.

#### Acceptance criteria

- [ ] On a fresh clone, the CI script (fmt --check, clippy -D warnings, build, test) passes.
- [ ] The README quickstart block runs end-to-end in a temp dir, driven by a scripted test.
- [ ] LICENSE (Apache-2.0) at root; every crate carries `publish = false` and `license = "Apache-2.0"`.

#### Risks / unknowns

- None material; smallest slice.

---

## Sequencing summary

```
M0 ─┬─▶ M1 ─┐
    ├─▶ M2 ─┼─▶ (v1 done; announce decision + org/remote decision follow)
    ├─▶ M3 ─┘
    └─▶ M4 (CI early, polish last)
```

Post-v1 backlog: **M5 HTTP surface** (MCP streamable HTTP + REST — same verbs, second transport), gated on M2, per ADR-0006.
