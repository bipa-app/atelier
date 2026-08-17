# Plan: atelier dogfood — local agents work inside workspaces

> **Why**: battle-test before any publish decision. The product's own PRD names three earlier internal agent-workspace attempts; the test is whether atelier replaces them for the agents on this machine.
> **Status**: Approved (2026-08-16)
> **Owner**: Luiz Parreira

## The adoption model

`attach` copies; sync-back is not built. So adoption does not mean "atelier shadows the repos agents already sit in" — it means **agents work inside atelier workspaces**. That works today because mounts are real, pushable git repos (N1): attach the project, work in the mount through sessions, land through the gate, push from the mount with plain git. The workspace is the working location.

Three layers make an agent actually use it:

1. **Discovery** — the `manifest` tool: the first thing an actor reads, telling it what this workspace is, its sources, its discipline, and the loop it must follow. Without it, MCP tools are just verbs with no story.
2. **Wiring** — MCP registration per harness: Claude Code (`.mcp.json`), codex (`mcpServers` config), OMP (mcp servers), and any MCP-native daemon. One kit documents them all, copy-paste exact.
3. **Habit** — an instruction layer that makes the session loop the default: a skill (OMP + Claude Code) and an AGENTS.md paragraph for dogfood projects that says: this project is worked through atelier; open a session, never edit the shared line.

## Phases

### D1: manifest — the read model agents consume first
`ws.manifest()` rendering identity, sources with mounts and kinds, discipline (landing policy, instruction capture), live state (per-source heads, open sessions and requests), and the agent loop convention. `atelier manifest` and the MCP `manifest` tool return it verbatim — one render, three faces.

### D2: integration kit — wire every local harness
`docs/integrations.md`: exact MCP registration for Claude Code, codex, OMP, and MCP-native daemons; the AGENTS.md paragraph a dogfood project pastes; the skill text agents load. Ships with a live proof: an agent session over MCP edits an adopted real repo and lands.

### D3: dogfood harness sessions on atelier workspaces
The in-house coding harness gives sessions worktrees today; the dogfood is a harness session whose working copy is an atelier session working copy, journal and gate included. The harness side lives in its own repository; this card tracks the atelier-side needs it surfaces.

### D4: the feedback loop
Dogfood pain lands as cards in the project tracker the day it is felt, tagged to this project. Known candidates already: sync-back for folder sources, bookmark motion on landing (landed work should advance the source's branch), `status` read model, session working copies outside `.atelier` for build-tool friendliness.

## Out of scope

- Publishing, announcement — the point of this plan is to earn them.
- Remote sources, hosted runtime — unchanged.
