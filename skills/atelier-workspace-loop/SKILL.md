---
name: atelier-workspace-loop
description: Work inside an atelier workspace (versioned, attributed, gated) instead of editing files directly - use when a project directory contains .atelier/, when the user says "use atelier", "atelier workspace", "land this", or when building or editing docs or code that should carry a journal trail
version: 0.1.0
---

# The atelier workspace loop

atelier is a versioned-workspace substrate for humans and agents. A directory
containing `.atelier/` is a workspace: **never edit its files directly** — the
shared line only moves through the landing gate. Work through a session.

The `atelier` binary must be on PATH (build from the repo:
`cargo build -p atelier-ws`).

## Detect and orient

- A workspace: `.atelier/` exists at the root. Orient with `atelier manifest`
  (sources, discipline, live state, open sessions and requests) — read it first.
- The configured actor comes from `ATELIER_CONFIG_HOME/config.toml` (else
  `~/.config/atelier/config.toml`):
  `[actor] name = "…" kind = "human|agent|automation"`.

## The fastest deep integration: run yourself inside a session

```sh
atelier run --summary "one honest line on what and why" -- <any command>
```

`run` opens a session, executes the command inside the session's working copy
(a real directory - native file tools, builds, and tests all work), snapshots
what it edited, prints the session diff, and leaves the session holding the
change with the land hint. `--land` lands on success. A failing command keeps
the session open with its work versioned.

## The loop (MCP or CLI)

Serve MCP when driving as an agent: `atelier serve --mcp-stdio` (one client
per process, cwd = workspace). Tools: `manifest`, `open_session`, `read`,
`write`, `diff`, `request_land`, `approve`, `reject`, `land`,
`landing_requests`, `journal`, `abandon`.

1. `manifest` — read first.
2. `open_session` (actor_name, actor_kind: "agent", instruction_summary — one
   honest line on what and why; it becomes the journal's attribution).
3. The response carries `working_copy` — a real directory. Either `write` over
   MCP or edit files in that directory with normal tools; every atelier
   command auto-snapshots outstanding edits.
4. Paths scope by mount: `backend/src/api.rs` addresses the `backend` source.
   Mounts are real git repos (adopted, history preserved).
5. `diff` (session) — review your change across every source.
6. `land` — request plus self-approval where policy allows. One land fans out
   per source: the root and each touched mount land under their own lease.
   - `Parked` is a value, not an error: a source conflicted. Resolve **inside
     the session** (concede the contested lines, keep your work elsewhere),
     then `approve` the same request again — what landed stands, the retry
     lands only what remains.
7. `journal` — verify attribution; every act is recorded.

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
- Never fabricate instruction summaries — they are the attribution record.
- `atelier journal` / `atelier history` / `atelier requests` answer "who did
  what", "what changed", "what awaits a gate" — prefer them over guessing.
