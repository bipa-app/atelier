# Wiring coding agents to atelier

atelier is adopted when agents *work inside a workspace* — not beside one. `attach`
copies; sync-back is a later slice. So the workspace is the working location: attach
your project as a mount (a git repo is adopted with its history and stays a real
repo), work through sessions, land through the gate. Branch motion on landing — so
plain `git push` from a mount carries the newest shared line — is tracked as
[tracker]; until it lands, the newest landing rides the colocated working-copy
commit and a push publishes only the history beneath it.

Every harness below needs the same three things:

1. **A workspace.** `atelier init` in an empty directory, then
   `atelier attach <repo> --mount <name>` per project.
2. **The MCP server.** One process per workspace: `atelier serve --mcp-stdio`
   (one client) or `atelier serve --http` (many clients, `POST /mcp`).
3. **The instruction layer** — the paragraph and loop below. Tools alone do not
   change agent behavior; the instruction does.

## The loop every agent follows

```
manifest                      read this first: sources, discipline, live state
open_session                  your working copy + your change; never edit the shared line
write / edit under it         paths scope by mount: "backend/src/api.rs"
diff                          your change across every source, highest fidelity
land                          request + self-approval where policy allows
  (or request_land and wait   when a human holds the gate)
journal                       who did what, attributed, always
```

Editing never takes a lease; landing always passes the gate. A parked landing is a
value, not an error: resolve in the session and approve again — what landed stands.

## Claude Code

`.mcp.json` at the workspace root (Claude Code starts the server itself):

```json
{
  "mcpServers": {
    "atelier": {
      "command": "atelier",
      "args": ["serve", "--mcp-stdio"]
    }
  }
}
```

## codex

`~/.codex/config.toml`:

```toml
[mcp_servers.atelier]
command = "atelier"
args = ["serve", "--mcp-stdio"]
```

## Oh My Pi (omp)

`.omp/config.json` in the workspace (or the global config):

```json
{
  "mcpServers": {
    "atelier": { "command": "atelier", "args": ["serve", "--mcp-stdio"] }
  }
}
```

## harness

harness's daemon speaks MCP natively; register the workspace server in the daemon's MCP
configuration the same way. The deeper integration — harness sessions whose working
copies are atelier session working copies — is tracked as dogfood slice D3.

## The AGENTS.md paragraph

Paste into any project worked through atelier:

> This project lives in an atelier workspace. Do not edit the shared line: call
> `manifest` first, then `open_session` and work inside your session's working
> copy — every edit is versioned and attributed. Paths scope by mount
> (`<mount>/path/inside`). When the work is done, `land` it; if the gate parks
> your landing, resolve inside the session and approve again. Check `journal`
> when you need to know who did what.

## Serving over HTTP

`atelier serve --http` binds `127.0.0.1:7423` and serves MCP streamable HTTP at
`POST /mcp` plus a REST slice under `/v1`. Loopback only unless `--allow-remote`
(auth is a dedicated pre-exposure slice — do not expose yet).
