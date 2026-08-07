# Coordination is a port: SQLite locally, a durable-object cell when distributed

Serialization points — the landing lease, journal writes, request state — go through one coordination port with exactly two kinds of implementation. Locally: SQLite transactions (WAL) — proven, multi-process-correct, already in the stack. When a workspace becomes shared or hosted: a single-writer durable-object cell (celld, or Cloudflare DO — the workspace-as-cell model), whose per-cell SQLite maps 1:1 onto the local journal. We never build bespoke coordination: no hand-rolled lock files, no custom consensus, no invented distributed leases.

## Considered options

- In-process lease (the original M2 sketch): rejected — the CLI and the MCP server running at once already splits it into two lease-worlds.
- Bespoke distributed lock service: rejected on principle — use what exists: SQLite now, durable-object cells later.

## Consequences

- The port's SQLite implementation ships in M2; the cell implementation arrives with a hosted milestone and changes no domain code.
- Content scales the same way through jj's pluggable backend (bucket storage later); coordination and content scale independently.
