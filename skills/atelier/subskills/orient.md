# Orient and attribute

Use this branch before acting in an existing workspace.

## Find the contract

1. Locate the workspace root: it is the nearest ancestor containing
   `.atelier/`.
2. Run `atelier manifest` first. Treat its sources, discipline, sessions, and
   requests as current state rather than inferring them from files.
3. Use `atelier status` when you need per-source heads and live coordination
   state. Use `atelier journal` for acts and intent; use `atelier history` for
   content snapshots.

## Act as the real actor

The CLI reads `ATELIER_CONFIG_HOME/config.toml`, then
`~/.config/atelier/config.toml`:

```toml
[actor]
name = "build-agent"
kind = "agent"
```

Give each agent its own scoped `ATELIER_CONFIG_HOME` for acting commands. MCP
and HTTP calls instead carry `actor_name` and `actor_kind = "agent"` per call.
The journal records that actor, so borrowing the machine owner's identity
would make the history false.

The same config may carry the owner's publishing identity:

```toml
[git]
name = "Repository Owner"
email = "owner@example.com"

[git.signing]
backend = "gpg"
key = "OWNER_KEY_ID"
```

Actor identity answers who directed the Atelier act. Git identity answers who
commits the landed source. Keep those roles distinct.

## Read state rather than guessing

- `atelier sessions`: durable work in progress.
- `atelier requests`: landing decisions and their states.
- `atelier journal`: who did what and why.
- `atelier history [mount]`: content snapshots for every line or one source.
- MCP `read`: a windowed text projection; `.docx` projects as Markdown and
  rich diffs name document structure.
