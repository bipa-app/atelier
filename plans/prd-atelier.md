# PRD: atelier — versioned workspaces for humans and agents

> **Status**: Approved (planning session 2026-08-07)
> **Canonical vocabulary**: `CONTEXT.md` · **Decisions**: `docs/adr/0001`–`0004`

## Problem

Every team building with agents hand-rolls the same thing: a place for agents to work. Bipa alone has built three — Buzz (R2 + a git server per project), bip's own workspaces, and a case-file attempt for the satoshi-holmes fraud analyst. Each rebuilds versioned storage, coordination, and attribution, and each falls short the same ways: documents don't diff, agent actions aren't attributed or auditable, concurrent actors collide ad hoc, and only code — never the spreadsheet or the contract — gets real version control.

## Solution

An open-source toolkit — a Rust SDK, an MCP/HTTP server, and the `atelier` CLI, all thin faces over one core library — where a workspace is a named, versioned body of any work content, shared by humans and agents through the same contracts:

- Attach a workspace to a source — remote git repo, local git repo, local folder, remote folder — with explicit sync policy.
- Every edit auto-snapshots (jj engine on the git boundary): nothing unversioned, everything undoable, every workspace a real git repo.
- Documents diff at the highest fidelity available: rich deltas, projected text, or binary — format support ships as independent packages.
- A journal records acts and intent (who, session, instruction, approval), distinct from content history.
- Agents get a uniform surface: open a session (own working copy + change), edit, diff, land through a lease, all journaled — over MCP.
- Profiles (Finance, Legal, Code, …) type workspaces as composable bundles of conventions, projectors, policies, and tools.

## User stories

1. As a user, I want to create a workspace from a local folder, so that my files gain history without me managing git.
2. As a user, I want to attach a remote git repo two-way, so that a workspace can collaborate with the git universe.
3. As a user, I want everything versioned automatically, so that nothing is ever lost and nothing depends on remembering to save.
4. As an agent, I want `open_session` to give me my own working copy and change, so that I never collide with other actors.
5. As an agent, I want every edit snapshotted with my identity, so that my work is attributable.
6. As an agent, I want to land through a lease, so that advancing the shared line is safe and serialized.
7. As an agent, I want the manifest to tell me the workspace's rules first, so that I act correctly without tribal knowledge.
8. As a user, I want undo for any agent mistake, so that delegation is never fatal.
9. As a user, I want conflicts recorded instead of blocking, so that disagreement becomes a task, not an outage.
10. As an analyst, I want docx changes shown as readable diffs, so that a contract edit reviews like a code change.
11. As a compliance officer, I want the journal to answer who/what/on-whose-instruction/approved-by, so that audits read from the record.
12. As a compliance workspace owner, I want verbatim instruction capture, so that the audit trail is complete where required.
13. As an OSS contributor, I want to add a format package as its own crate, so that fidelity for my format doesn't wait on the core team.
14. As a human teammate, I want to clone any workspace with plain git, so that adoption requires no new tool.

## Implementation decisions

- Engine: jj model (auto-snapshot, changes, first-class conflicts, op log) via pinned jj-lib; git backend; colocated `.git` (ADR-0002). Git-LFS sources fail loudly at attach.
- Content model: version control IS the content model; no unversioned state (ADR-0001).
- Diffing: one `diff-core` library owning the Diff/Delta model and the fidelity ladder; `FormatPackage` trait (projector mandatory, differ optional); packages as independent crates (ADR-0003). Projections are derived and cached by (blob, package, version), never committed.
- Journal: append-only; instruction as summary + run reference by default, verbatim per profile (ADR-0004).
- Concurrency: editing never leases; landing always does. Optimistic by default; pessimistic paths per profile policy.
- Crates (all `publish = false`; crates.io names `atelier`/`atelier_core` are taken — publish prefix decided later): `core`, `diff-core`, `format-docx`, `surface` (MCP + HTTP), `cli` (binary `atelier`).
- Delivery surfaces: one core, three faces — SDK first; CLI, MCP (stdio in v1, streamable HTTP after), and REST are thin shells with no shell-only behavior (ADR-0006).
- Storage: content = git object store via jj (ADR-0002); journal = SQLite beside the repo, never inside it (ADR-0005); projections = derived, content-addressed, evictable cache.
- Rust throughout; anyhow for errors, tracing for logs; Rust 2018+ module layout (no mod.rs).

## Testing decisions

- Every milestone proves itself with an end-to-end CLI round-trip test, not unit tests alone.
- Projector determinism via golden tests (byte-identical output for same blob + package version).
- Concurrency (lease/land) covered by a two-actor integration test.
- Quality gates on every PR: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`.

## Out of scope (v1)

- Remote sources (git remotes sync, S3/Drive folders) — folder-attach ships first; remote adapters are v2.
- Gates and approvals; profiles beyond the default — policy machinery lands with the second profile.
- Rich differs (xlsx, pdf) — the ladder makes them additive later.
- Hosted / multi-node runtime (celld or Cloudflare DO deployment) — local-first daemon first.
- WASM plugin ABI — in-process crates until third-party packages exist.
- crates.io publishing (names `atelier`/`atelier_core` are taken; publish prefix decided later).
