# Wiring coding agents to atelier

atelier is adopted when agents *work inside a workspace* — not beside one. `attach`
copies; sync-back is a later slice. So the workspace is the working location: attach
your project as a mount (a git repo is adopted with its history and stays a real
repo), work through sessions, land through the gate. Every landing moves the
adopted branch (or the `atelier` bookmark), so plain `git push` from a mount
publishes the newest shared line.

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

## The deepest integration is no integration: `atelier run`

```sh
atelier run --summary "wire the retry path" -- claude
atelier run --summary "fix the flaky test" --land -- codex exec "…"
```

`run` opens a session and starts the command inside its working copy — a real
directory, so the harness's native file tools, builds, and tests all flow
through atelier with no interception. On exit it snapshots, prints the session
diff, and leaves the session holding the change (`--land` lands on success; a
failing command keeps its work versioned in the open session). This works for
every harness, including ones with no plugin system at all.

## The plugin: one repo, three installers

This repository is itself a passive plugin — the `atelier-workspace-loop`
skill plus, for Claude Code, a SessionStart hook that injects
`atelier manifest` as context when the project is a workspace (silent
anywhere else).

```sh
omp plugin install git@github.com:bipa-app/atelier     # Oh My Pi
pi install git:git@github.com:bipa-app/atelier         # pi
claude plugin install atelier@<marketplace>            # Claude Code, or:
claude --plugin-dir ~/work/atelier                     # local
```

codex has no plugin system; install the skill directly:

```sh
cp -r skills/atelier-workspace-loop "${CODEX_HOME:-$HOME/.codex}/skills/"
```

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

## bip

bip's daemon speaks MCP natively; register the workspace server in the daemon's MCP
configuration the same way. The deeper integration — bip sessions whose working
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
