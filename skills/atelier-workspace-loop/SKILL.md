---
name: atelier-workspace-loop
description: Work inside an atelier workspace (versioned, attributed, gated) instead of editing files directly. Use whenever a directory contains .atelier/, whenever the user says "use atelier", "atelier workspace", "open a session", "land this", or asks who changed something and why — and whenever an agent's edits should be versioned, attributed, and reviewable, even if atelier is not named. Also covers setting up a workspace, serving it to agents (MCP, HTTP), and hosted serving from a bucket.
version: 0.3.0
---

# The atelier workspace loop

atelier is a versioned-workspace substrate for humans and agents. A directory
containing `.atelier/` is a workspace: **never edit its files directly** — the
shared line only moves through the landing gate, and a direct edit dodges the
attribution record the whole product exists to keep. Work through a session.

Install if `atelier` is not on PATH (`atelier update` refreshes a script
install later):

```sh
curl -fsSL https://atelier-ws.dev/install.sh | sh   # or: cargo install atelier-ws
```

## Detect and orient

- A workspace: `.atelier/` exists at the root. Orient with `atelier manifest`
  (sources, discipline, live state, open sessions and requests) — read it first;
  it tells you the loop this workspace expects.
- The configured actor comes from `ATELIER_CONFIG_HOME/config.toml` (else
  `~/.config/atelier/config.toml`):
  `[actor] name = "…" kind = "human|agent|automation"`. Act as yourself, not
  as the machine's default human: over MCP/HTTP pass your own `actor_name` /
  `actor_kind: "agent"` per call; on the CLI point `ATELIER_CONFIG_HOME` at
  your own config for the acting commands. The journal records whoever the
  act names — attribution is the product.

## The fastest deep integration: run yourself inside a session

```sh
atelier run --summary "one honest line on what and why" -- <any command>
```

`run` opens a session, executes the command inside the session's working copy
(a real directory — native file tools, builds, and tests all work), snapshots
what it edited, prints the session diff, and leaves the session holding the
change with the land hint. `--land` lands on success. A failing command keeps
the session open with its work versioned — nothing is lost, nothing lands.

## The loop (MCP or CLI)

For a long-lived CLI session whose working copy any normal file tool can edit:

```sh
atelier session open --summary "one honest line on what and why"
# Edit the printed working-copy path.
atelier session diff <session>
atelier land <session>                 # or: atelier session abandon <session>
```

Serve MCP when driving as an agent: `atelier serve --mcp-stdio` (one client
per process, cwd = workspace). Tools: `manifest`, `status`, `open_session`,
`read`, `write`, `diff`, `request_land`, `approve`, `reject`, `land`,
`landing_requests`, `journal`, `abandon`, `undo`, `sync`, `pull`. The CLI
opens long-lived sessions with `session open`, prints their working copies,
and exposes `session diff`, `land`, and `session abandon`; session-scoped
`read` and `write` remain MCP/HTTP-only.

1. `manifest` — read first.
2. `open_session` (actor_name, actor_kind: "agent", instruction_summary — one
   honest line on what and why; it becomes the journal's attribution).
3. The response carries `working_copy` — a real directory. `write` snapshots
   immediately. After normal file edits, the next session `diff`,
   `request_land`, `land`, `approve`, or `abandon` snapshots them; there is no
   separate "commit".
4. Paths scope by mount: `backend/src/api.rs` addresses the `backend` source.
   Mounts are real git repos (adopted, history preserved).
5. `diff` (session) — review your change across every source before landing.
6. `land` — request plus self-approval where policy allows; when a human holds
   the gate, `request_land` and stop — the request waits for their `approve`.
   One land fans out per source: the root and each touched mount land under
   their own lease.
7. `journal` — verify attribution; every act is recorded.

Sessions are durable: `atelier sessions` lists them, an open session resumes
where it stood, and `atelier session abandon <session>` closes one without
landing (its work stays in history). A landed request steps back with `undo` —
the lines return to the landed snapshot's parent and the request re-opens for
a new decision.

### Parked is a value, not an error

A conflicted source parks; the other sources land. Resolve **inside the
session's working copy** (concede the contested lines, keep your work
elsewhere), then land the session again: `atelier land <session>` (or the
`land` tool) snapshots the resolution, re-opens the gate, and approves in one
step. A bare `approve` on a still-parked request refuses by design — the
gate only re-opens once a new snapshot carries the resolution. What landed
stands; the retry lands only what remains. Never resolve by editing shared
files directly.

### Reading documents

`read` returns text projections for documents a format package understands —
a `.docx` reads as markdown, and `diff` renders rich deltas in the format's
own terms (cells, paragraphs, emphasis). Reads are windowed; the response
carries a continuation offset when more remains. Prefer `read` over raw byte
tools for any document in a workspace.

## Serving a workspace

- `atelier serve --mcp-stdio` — one MCP client, stdio.
- `atelier serve --http` — MCP streamable HTTP at `POST /mcp` plus the same
  verbs as REST under `/v1`, many clients, loopback by default; `--token`
  makes every request carry a bearer, and binding beyond loopback requires
  `--allow-remote` and a token.
- `atelier serve --http --hosted s3://bucket/prefix` — a hosted workspace:
  claim the bucket's ownership record, hydrate the stores (an empty directory
  fills from the bucket; a present workspace seeds a fresh one), serve, and
  release on shutdown. `--take-over` seizes from a node that died holding the
  record. Credentials ride `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`;
  MinIO/R2 via `?endpoint=`.

## Workspace setup (once per project)

```sh
mkdir ws && cd ws && atelier init
atelier attach ~/path/to/repo --mount backend   # git repos are adopted, never imported
```

The workspace is the working location from then on. Every landing moves the
adopted branch (or the `atelier` bookmark), so publishing is plain git:
`git -C <mount> push origin <branch>`.

## Rules

- Never edit the workspace's shared files outside a session.
- Never run mutating git commands (`checkout`, `commit`, `rebase`, `reset`)
  inside a mount: the engine's store is colocated there, and foreign git ops
  can track or delete it, destroying the workspace. Plain `git push` from a
  mount is the one safe git verb — it is how landings publish.
- Never fabricate instruction summaries — they are the attribution record.
- `atelier journal` / `atelier history` / `atelier requests` answer "who did
  what", "what changed", "what awaits a gate" — prefer them over guessing.
