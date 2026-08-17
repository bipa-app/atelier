# One core, three faces: SDK first; CLI, MCP, and HTTP are thin shells

atelier ships as a Rust SDK (the core library), an MCP/HTTP server, and the `atelier` CLI. The SDK is the product: every capability lands in the library first, and the shells are thin bindings over the same API — no behavior may exist only in a shell. MCP serves agents (stdio locally, streamable HTTP remotely); REST serves plain programmatic clients; the CLI serves humans and scripts.

## Considered options

- CLI-first, extract a library later: rejected — extraction after the fact is the classic painful path, and the embedders this project exists for (internal agent harnesses and services) need the library from day one.
- MCP-only surface: rejected — scripts, services, and a future UI should not need an MCP client to read a diff.

## Consequences

- Public API discipline starts at M0: SDK types speak the glossary (Workspace, Snapshot, Change, Session, Lease), never engine internals.
- v1 ships MCP over stdio (M2); the HTTP transports (MCP streamable HTTP + REST) are a follow-up slice gated on M2 — same verbs, second transport.
- Shell parity is a test concern: a capability's CLI, MCP, and HTTP forms must exercise one core path.
