# atelier API design

> Status: draft for review · Vocabulary: `CONTEXT.md` · Ground rules: ADR-0006 (one core, three faces — every capability lands in the SDK first; CLI, MCP, and HTTP are thin shells with no shell-only behavior).

The API speaks the glossary, never the engine: `Workspace`, `Source`, `Snapshot`, `Change`, `Working Copy`, `Session`, `Lease`, `Landing`, `Journal`, `Diff`, `Delta`. jj types never cross the public boundary.

## 1. SDK (Rust) — the product

### Types

```rust
Workspace                    // handle to an open workspace
Source      { id, kind: RemoteGit|LocalGit|LocalFolder|RemoteFolder, endpoint, mount, sync_policy }
SyncPolicy  { ImportOnce | Mirror | TwoWay }
Snapshot    { id, actor, at, parents }
Change      { id, description, snapshots }        // stable identity across rewrites
WorkingCopy { path, change }                      // a real directory on disk
Session     { id, actor, instruction, working_copy, state: Open|Landed|Abandoned }
Instruction { summary, run_ref, verbatim: Option<String> }   // persistence per profile (ADR-0004)
Actor       { id, kind: Human|Agent|Automation, name }
Diff        { fidelity: Rich|Text|Binary, deltas: Vec<Delta> }
Delta       { address, kind: Added|Removed|Changed|Moved, before, after, summary }
JournalEntry{ at, actor, act, session, refs }     // append-only
Manifest    // rendered self-description: identity, profile, sources, discipline, current state
```

### Surface (grouped)

```rust
// Lifecycle
Workspace::init(path, opts) -> Workspace
Workspace::open(path) -> Workspace
ws.attach(SourceSpec) -> Source            // v1: one LocalFolder source; API shaped for many (mount points)
ws.detach(source_id)
ws.status() -> Status                      // head, dirty paths, open sessions, sources
ws.manifest() -> Manifest                  // the read model an actor consumes first

// Sessions (the agent write path)
ws.open_session(actor, Instruction) -> Session    // always: own WorkingCopy + own Change
session.snapshot() -> Snapshot                    // explicit; any SDK op also auto-snapshots first
session.diff() -> Diff                            // change vs shared line
session.land() -> Landed | Parked(Conflict)       // acquires landing lease internally
session.abandon()                                 // work stays in history; session closed
ws.sessions() -> Vec<Session>                     // durable; survives process restarts

// History & recovery
ws.log(range) -> Vec<Snapshot>
ws.diff_between(a, b, paths) -> Diff
ws.undo(op_ref)                                   // op-log undo, journaled as its own act

// Journal (read; writes are internal to every mutating op)
ws.journal(JournalQuery) -> Vec<JournalEntry>     // by actor, session, path, time range

// Documents
ws.project(path_or_blob) -> Projection            // cached by (blob, package, version)
registry.packages() -> Vec<PackageId>             // detection: highest confidence wins; ties break by package id

// Watch (M3)
ws.watch(opts) -> impl Stream<Item = WatchEvent>  // shells decide foreground/daemon
```

### Leases are internal

`land` is the only serialization point; the lease is acquired inside it and is **not** a public verb. Exposed read-only via `status()` for observability. Editing never leases (glossary rule).

### Errors (typed; the contract includes failure)

`NotAWorkspace` · `AlreadyAttached` · `NestedWorkspace` · `LfsSourceUnsupported` (ADR-0002, attach-time) · `NoActorConfigured` · `SessionNotFound` · `SessionClosed` · `LeaseHeld { holder, expires }` · `LandParkedOnConflict { change, conflicts }` · `FileTooLarge { path, limit }` · `PackageFailed { package, fell_back_to }` · `JournalLocked`

## 2. MCP (agent face)

Tools, all thin over the SDK; explicit ids in every call (no connection-session state, so reconnects are safe):

| Tool | Args | Returns |
|---|---|---|
| `manifest` | — | workspace self-description (read this first) |
| `open_session` | actor, instruction{summary, run_ref, verbatim?} | session_id, working_copy_path, change_id |
| `read` | session_id, path | content (fs-sandboxed agents cannot read the working copy directly) |
| `write` | session_id, path, content | snapshot_id |
| `diff` | session_id (or from/to) | Diff (rendered per fidelity) |
| `land` | session_id | landed{snapshot} \| parked{conflicts} |
| `journal` | query | entries |
| `abandon` | session_id | ok |

stdio transport in M2; MCP streamable HTTP in M5 (same tools — reach, not capability).

## 3. HTTP REST (M5, programmatic face)

```
POST /v1/workspaces                       init/attach
GET  /v1/workspaces/{ws}                  status + manifest
POST /v1/workspaces/{ws}/sessions         open (actor, instruction)
GET  /v1/workspaces/{ws}/sessions/{s}/files/{path}
PUT  /v1/workspaces/{ws}/sessions/{s}/files/{path}
POST /v1/workspaces/{ws}/sessions/{s}/land
POST /v1/workspaces/{ws}/sessions/{s}/abandon
GET  /v1/workspaces/{ws}/diff?from=&to=&path=
GET  /v1/workspaces/{ws}/journal?actor=&since=&path=
```

Localhost bind by default; non-localhost requires an explicit flag; auth is a dedicated pre-exposure slice.

## 4. CLI (human face)

`ws init` · `ws attach <src>` · `ws status` · `ws manifest` · `ws log` · `ws diff [--from --to] [path]` · `ws journal [--actor --since]` · `ws sessions [--abandon <id>]` · `ws land <session>` · `ws watch` · `ws undo <op>` · `ws serve [--mcp-stdio | --http]`

## 5. Configuration

Two artifacts with different natures:

- **Config is input.** Global `~/.config/atelier/config.toml` (who am I) ⊂ workspace `.atelier/config.toml` (what this workspace is) ⊂ per-invocation flags. Precedence: invocation > workspace > global.
- **Manifest is output.** `ws manifest` renders config + live state (head, sources, discipline, open sessions). Never hand-edited.

```toml
# global
[actor]           name = "luiz"        kind = "human"

# workspace
[workspace]       name = "deals-q3"    profile = "default"
[snapshot]        max_file_size = "50MB"   debounce_ms = 500
[boundary]        ignore = [".DS_Store", ".env*", "node_modules/"]
[journal]         instruction_fidelity = "summary"   # profile-driven; "verbatim" in audit profiles
[[source]]        kind = "local-folder"  path = "."  sync = "two-way"  mount = "/"
[packages]        pins = { "format-docx" = "0.1" }
```

Schema versioning from day one: `schema = 1` in config; SQLite `user_version` for the journal; MCP tools carry a `version` in `manifest`.

## 6. Edge-case catalog

### Sessions & landing
- **Shared line moved since session start** → land rebases (jj auto-rebase). Clean → advance. Conflict → **the land parks**: the Conflict is recorded on the change, the session stays open, the journal records the parked attempt, and the caller gets `LandParkedOnConflict`. **Invariant: the shared line is always conflict-free** — a materialized conflict inside a docx would be nonsense; conflicts live on changes, resolution is a follow-up task.
- **Concurrent lands** → exactly one lease holder; the loser gets `LeaseHeld { holder, expires }` and retries after. Refuse, don't queue (observable, simple, fair enough at v1 scale).
- **Crash mid-session** → sessions are durable rows + real directories; `ws sessions` lists them; reopen by id; stale sessions warn after TTL but are never auto-deleted (ADR-0001: work is never lost silently).
- **Lease holder crashes** → TTL expiry frees it; the parked land is retryable.
- **Working copy manually deleted** → session flagged broken; change and snapshots survive in history.

### Snapshots, boundary & watch
- **Huge file** → `FileTooLarge` skip: recorded in the journal, file listed in status as outside-boundary until config raises the limit. Snapshot of everything else proceeds.
- **Secrets (.env)** → default ignores. Sharpened rule: **ignores define the content boundary, not unversioned state** — an ignored path is *outside the workspace*; ADR-0001 applies inside the boundary. Boundary changes are journaled acts.
- **Editor atomic-save (write-temp-rename)** and event storms (`npm install`) → debounce + batch into one snapshot; default ignores carry the usual offenders.
- **Symlinks / empty dirs / case-insensitive fs** → follow git semantics (they are the boundary contract); document, don't fight.

### Attach
- Folder already a git repo → that's the LocalGit source kind: preserve history, colocate. Already a jj repo → adopt. **Inside another workspace → refuse** (`NestedWorkspace`). LFS in a git source → refuse loudly (ADR-0002). Attach same source twice → `AlreadyAttached`.

### Documents & packages
- **Corrupt/encrypted docx** → projector error never fails the diff: fall to binary rung, `PackageFailed { fell_back_to }` recorded in the journal.
- **Package panic** → caught at the package boundary (`catch_unwind`); a bad package degrades fidelity, never kills the process.
- **Two packages claim a file** → highest detect-confidence wins; ties break deterministically by package id.
- **Rename + edit** → delta kind `Moved` with content diff; follows the engine's rename detection.

### Journal & multi-process
- **CLI and server running at once** → SQLite WAL handles concurrent writers for the journal. But an in-memory lease would split into two lease-worlds — so **the lease is a SQLite row** (atomic claim in a transaction, TTL column), multi-process-correct from day one. This amends M2's "in-process TTL lease" wording.
- **No actor configured** → every mutating op refuses with `NoActorConfigured` + setup hint. No anonymous acts, ever.
- **Instruction hygiene** → caller supplies `Instruction`; the profile decides what persists (default drops `verbatim`). The journal records what it was given — attribution is the caller's honesty plus the actor identity.
```
