# MCP coding agent

Configure one stdio server with the workspace as its current directory:

```json
{
  "mcpServers": {
    "atelier": {
      "command": "atelier",
      "args": ["serve", "--mcp-stdio"],
      "cwd": "/absolute/path/to/workspace"
    }
  }
}
```

Then use the native tool loop:

1. `manifest`.
2. `open_session` with the agent's own `actor_name`,
   `actor_kind: "agent"`, and an honest `instruction_summary`.
3. Edit the returned `working_copy`, or use `read`/`write`.
4. `diff` the session.
5. `land` when self-approval applies; otherwise `request_land`.
6. `journal` and verify the actor, session, and instruction summary.

Use one MCP server process per client. Session-scoped `read` and `write` are
MCP/HTTP operations; the CLI exposes the working copy instead.
