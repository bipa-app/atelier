# atelier API design

> Status: v2, decisions of 2026-08-07 folded in · Vocabulary: `CONTEXT.md` · Ground rules: ADR-0006 (one core, three faces), ADR-0007 (gated landing), ADR-0008 (coordination port).

The API speaks the glossary, never the engine: `Workspace`, `Source`, `Boundary`, `Snapshot`, `Change`, `Working Copy`, `Session`, `Landing Request`, `Approval`, `Journal`, `Diff`, `Delta`. jj types never cross the public boundary.

## 1. SDK (Rust) — the product

### Types

```rust
Workspace                    // handle to an open workspace
Source        { id, kind: RemoteGit|LocalGit|LocalFolder|RemoteFolder, endpoint, mount, sync_policy }
SyncPolicy    { ImportOnce | Mirror | TwoWay }
Snapshot      { id, actor, at, parents }
Change        { id, description, snapshots }        // stable identity across rewrites
WorkingCopy   { path, change }                      // a real directory on disk
Session       { id, actor, instruction, working_copy, state: Open|Landed|Abandoned }
Instruction   { summary, run_ref, verbatim: Option<String> }   // persistence per profile (ADR-0004)
Actor         { id, kind: Human|Agent|Automation, name }
LandingRequest{ id, change, requester, approvals, state: Open|Approved|Landed|Parked|Rejected|Abandoned }
Approval      { actor, at }
Diff          { fidelity: Rich|Text|Binary, deltas: Vec<Delta> }
Delta         { address, kind: Added|Removed|Changed|Moved, before, after, summary }
JournalEntry  { at, actor, act, session, refs }     // append-only
Manifest      // rendered self-description: identity, profile, sources, boundary, discipline, state
```

### Surface (grouped)

```rust
// Lifecycle
Workspace::init(path, opts) -> Workspace
Workspace::open(path) -> Workspace
ws.attach(SourceSpec) -> Source            // v1: one LocalFolder source; API shaped for many (mounts)
ws.detach(source_id)
ws.status() -> Status                      // head, dirty paths, open sessions/requests, sources
ws.manifest() -> Manifest                  // the read model an actor consumes first

// Sessions (the agent write path)
ws.open_session(actor, Instruction) -> Session    // always: own WorkingCopy + own Change
session.snapshot() -> Snapshot                    // explicit; any SDK op auto-snapshots first
session.diff() -> Diff                            // change vs shared line
session.request_land() -> LandingRequest          // opens the gate's object; never lands directly
session.abandon()                                 // work stays in history; session closed
ws.sessions() -> Vec<Session>                     // durable; survive process restarts

// Landing requests (ADR-0007)
ws.landing_requests(filter) -> Vec<LandingRequest>
lr.approve(actor) -> GateState                    // agents and humans approve alike
lr.reject(actor, reason)
// gate satisfied => apply runs: lease -> rebase -> advance, or Parked(Conflict)
session.land() -> Landed | PendingApproval(LandingRequest) | Parked(Conflict)
                                                  // sugar: request + self-approve where policy allows

// History & recovery
ws.log(range) -> Vec<Snapshot>
ws.diff_between(a, b, paths) -> Diff
ws.undo(op_ref)                                   // op-log undo, journaled as its own act

// Journal (read; writes are internal to every mutating op)
ws.journal(JournalQuery) -> Vec<JournalEntry>     // by actor, session, path, time, act kind

// Documents & reading (artifact-style; see §2.1)
ws.read(Address, Window, View) -> ReadResult
ws.project(Address) -> Projection                 // cached by (blob, package, version)
registry.packages() -> Vec<PackageId>

// Watch (M3) — blocking loop; external edits become attributed snapshots
// through the same snapshot path every operation uses; a catch-up scan at
// start owns edits made while no watcher ran; `stop` returns the loop.
ws.watch(debounce, on_event: FnMut(&WatchEvent), stop: &WatchStop)
```

### Coordination is a port (ADR-0008)

The landing lease, request state, and journal writes go through one coordination port. Local implementation: SQLite (WAL) — atomic claim in a transaction, TTL column, correct across CLI + server processes. Hosted implementation later: a single-writer durable-object cell (celld / Cloudflare DO), one cell per workspace. No bespoke coordination, ever. Leases stay internal — `approve`/`land` acquire them; `status()` exposes them read-only.

### Errors (typed; the contract includes failure)

`NotAWorkspace` · `AlreadyAttached` · `NestedWorkspace` · `LfsSourceUnsupported` (attach-time, ADR-0002) · `NoActorConfigured` · `SessionNotFound` · `SessionClosed` · `RequestNotFound` · `ApprovalNotAuthorized` · `SelfApprovalForbidden` (profile) · `ApprovalsDismissed { new_snapshot }` · `LeaseHeld { holder, expires }` · `LandParkedOnConflict { request, conflicts }` · `FileTooLarge { path, limit }` · `PackageFailed { package, fell_back_to }` · `WindowTooLarge { max }` · `AddressNotFound { available }`

## 2. MCP (agent face)

Tools, thin over the SDK; explicit ids in every call (reconnect-safe):

| Tool | Args | Returns |
|---|---|---|
| `manifest` | — | self-description (read this first) |
| `open_session` | actor, instruction{summary, run_ref, verbatim?} | session_id, working_copy_path, change_id |
| `read` | address, window?, view?, extract? | windowed content + continuation, or extracted field (§2.1) |
| `write` | session_id, path, content | snapshot_id |
| `diff` | session_id (or from/to), path? | Diff, rendered per fidelity; large diffs windowed (§2.1) |
| `request_land` | session_id | request_id, gate state |
| `approve` / `reject` | request_id, reason? | gate state → landed \| pending \| parked |
| `land` | session_id | landed \| pending_approval{request_id} \| parked{conflicts} |
| `landing_requests` | filter | open/parked requests with diffs summaries |
| `journal` | query | entries |
| `abandon` | session_id | ok |

stdio in M2; MCP streamable HTTP in M5 — same tools, reach not capability.

### 2.1 Read protocol (after oh-my-pi's blob/artifact architecture)

oh-my-pi splits two stores on purpose — content-addressed blobs (dedup, global, immortal) vs session-scoped outputs (short local ids, append-only, retrieval-friendly) — and atelier inherits the same split natively: the engine's object store IS the blob store.

- **Content addresses** — `blob:sha256:<hash>` (the object store: deduplicating, idempotent, outlives every session) · `path@<snapshot>` (immutable) · `path` (live working copy, via session_id). Immutable addresses are agent-cacheable forever.
- **Session outputs** — an oversized tool result (a big Diff, a long journal answer) spills to a session-scoped output with a short id: inline shows head + tail + `[... elided ...]` + the output's address; window through it with `read`. Ids allocate scan-before-use, so a resumed session never clobbers earlier outputs.
- **Every read is windowed.** Default ~50KB; response carries `{content, window: {start, end, total}, next?}`. No unbounded responses exist on the surface.
- **Projection is the default view for non-text.** `read report.docx` returns its markdown projection; `view=raw` returns bytes (base64, windowed). A projection's address is stable: (blob, package, version).
- **Structured extraction.** Reads of structured payloads (manifest, journal answers, request lists) accept `extract` — a JSON path/query, omp's `agent://…?q=` idea — so an agent fetches one field, not a document. Extraction and windowing are mutually exclusive, as in omp.
- **Errors teach the next move.** `AddressNotFound` lists the available outputs and nearest addresses, the way omp's resolvers list available artifact ids on a miss.

## 3. HTTP (M5, programmatic face) — shipped shape

One process serves one workspace (`ws serve --http`), so paths carry no
workspace segment. MCP streamable HTTP and REST share the server — and the
one dispatch behind it.

```
POST /mcp                             MCP streamable HTTP: one JSON-RPC message per POST,
                                      answered as JSON; notifications → 202; GET → 405
                                      (no server-initiated streams in v1)
GET  /v1/diff                         the workspace diff, text/plain — the exact lines `ws diff` prints
POST /v1/sessions                     open (actor, instruction)
PUT  /v1/sessions/{s}/files/{path}    write; body is the content
GET  /v1/sessions/{s}/diff
POST /v1/sessions/{s}/land
GET  /v1/journal?limit=
```

Statuses: 400 broken protocol · 422 domain refusal (`{"error": …}`) · 404/405 routing.
The remaining sketch endpoints (files read, request-land/approve/reject, abandon, status +
manifest) arrive with their read models.

Localhost bind by default (`--bind ip:port`); binding beyond loopback requires
`--allow-remote`; auth is a dedicated pre-exposure slice.

## 4. CLI (human face)

`ws init` · `ws attach <src>` · `ws status` · `ws manifest` · `ws log` · `ws diff` · `ws journal` · `ws sessions` · `ws requests` · `ws approve <id>` · `ws reject <id>` · `ws land <session>` · `ws watch` · `ws undo <op>` · `ws serve [--mcp-stdio | --http]`

The human review flow is the v1 demo: agent `request_land`s → human runs `ws requests`, reads the docx diff as markdown, `ws approve` → change lands.

## 5. Configuration

- **Config is input.** Global `~/.config/atelier/config.toml` (who am I) ⊂ workspace `.atelier/config.toml` (what this workspace is) ⊂ per-invocation flags. Precedence: invocation > workspace > global.
- **Manifest is output.** `ws manifest` renders config + live state. Never hand-edited.

```toml
# global
[actor]     name = "luiz"   kind = "human"

# workspace
schema = 1
[workspace] name = "deals-q3"   profile = "default"
[snapshot]  max_file_size = "50MB"   debounce_ms = 500
[boundary]  ignore = [".DS_Store", ".env*", "node_modules/"]
[journal]   instruction_fidelity = "summary"        # "verbatim" in audit profiles
[landing]   approvals = 1   allow_self_approve = true   dismiss_approvals_on_new_snapshots = true
[[source]]  kind = "local-folder"  path = "."  sync = "two-way"  mount = "/"
[packages]  pins = { "format-docx" = "0.1" }
```

Schema versioning from day one: `schema = 1`; SQLite `user_version` for the journal; `manifest` reports surface version.

## 6. Edge-case catalog

### Landing requests & concurrency
- **Shared line moved; rebase conflicts** → the apply parks the request (`Parked`), Conflict recorded on the change, session stays open, journal records the parked attempt. **Invariant: the shared line is always conflict-free.**
- **New snapshots after approval** → approvals dismissed (default policy; audit profiles always); request returns to `Open`, journal records the dismissal.
- **Approver = requester** → allowed in default profile, `SelfApprovalForbidden` in audit profiles.
- **Concurrent applies** → one lease holder (SQLite claim); loser sees `LeaseHeld { holder, expires }` and retries. Requests queue naturally as open objects — only the apply serializes.
- **Approve a parked request** → refused; resolve the conflict (follow-up session), new snapshot re-opens the gate.
- **Crash mid-session / mid-apply** → sessions and requests are durable rows + real directories; `ws sessions` / `ws requests` list them; lease TTL frees a dead holder; nothing is auto-deleted.

### Snapshots, boundary & watch
- **Huge file** → `FileTooLarge` skip, journaled, path listed outside-boundary until config raises the limit; the rest of the snapshot proceeds.
- **Secrets (.env)** → default ignores. Rule: **ignores define the Boundary** — outside the workspace, not unversioned inside it (glossary: Boundary). Boundary changes are journaled acts.
- **Editor atomic-save, event storms** → debounce + batch into one snapshot; default ignores carry the usual offenders.
- **Symlinks / empty dirs / case-insensitive fs** → git semantics; document, don't fight.

### Attach
- Folder already a git repo → LocalGit source: preserve history, colocate. Already jj → adopt. Inside another workspace → `NestedWorkspace`. LFS git source → loud `LfsSourceUnsupported` (ADR-0002). Re-attach → `AlreadyAttached`.

### Documents & packages
- **Corrupt/encrypted doc** → never fails a diff: binary rung + `PackageFailed { fell_back_to }` journaled.
- **Package panic** → caught at the boundary; degrades fidelity, never kills the process.
- **Two packages claim a file** → highest detect confidence; ties break by package id.
- **Rename + edit** → `Moved` delta with content diff, engine rename detection.

### Journal & multi-process
- **CLI + server concurrently** → journal via SQLite WAL; lease and request state via the same coordination port (ADR-0008) — one lease-world across processes.
- **No actor configured** → mutating ops refuse (`NoActorConfigured`). No anonymous acts, ever.
- **Instruction hygiene** → caller supplies `Instruction`; profile decides persistence (default drops `verbatim`). The journal records what it was given.
