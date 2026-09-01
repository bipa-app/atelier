---
name: atelier
description: Operate Atelier workspaces so agent work is versioned, attributed, reviewed, and landed through the workspace gate. Use whenever a directory contains `.atelier/`, the user says Atelier, asks to open or resume a session, land or undo work, attach or sync a source, inspect who changed something, serve a workspace over MCP/HTTP, or host one from object storage. Also use when coding or document work should carry an Atelier journal trail even if the user does not name the tool. Includes evidence-backed GitHub reporting for Atelier bugs, wrong documentation, and concrete workflow improvements.
compatibility: Requires the Atelier CLI; GitHub feedback reporting uses an authenticated gh CLI.
---

# Atelier

Invoke this skill as `/use:atelier`.

Atelier is a versioned workspace for humans and agents. In a directory that
contains `.atelier/`, change the shared line through a session. Direct edits
bypass the attribution and landing contracts the workspace exists to keep.

Install once with `curl -fsSL https://atelier-ws.dev/install.sh | sh` or
`cargo install atelier-ws`. Refresh a script install with `atelier update`.

## Route to the relevant subskill
Resolve every bundled path relative to this `SKILL.md`, never relative to the
workspace. In harnesses that support skill URIs, use `skill://atelier/...`.


Read only the branches the task reaches:

| Need | Read |
|---|---|
| Detect a workspace, inspect it, or set actor and publishing identity | [`subskills/orient.md`](subskills/orient.md) |
| Initialize a workspace; attach, pull, or sync sources | [`subskills/sources.md`](subskills/sources.md) |
| Edit, build, test, or read documents inside a session | [`subskills/sessions.md`](subskills/sessions.md) |
| Request, approve, land, recover a parked request, abandon, or undo | [`subskills/landing.md`](subskills/landing.md) |
| Configure MCP/HTTP or host a workspace from object storage | [`subskills/serving.md`](subskills/serving.md) |
| Report a bug, wrong documentation, or a workflow improvement | [`subskills/feedback.md`](subskills/feedback.md) |

## Default coding-agent loop

1. Run `atelier manifest` from the workspace root. It names the sources,
   discipline, live sessions, and gate state.
2. Open a session with one honest sentence on what and why. CLI agents pass
   `--actor-name "coding-agent" --actor-kind agent`; use `atelier run ... --`
   for one command or `atelier session open ...` for a durable working copy.
3. Work only in the printed session working copy. Normal file tools, builds,
   and tests work there.
4. Review with `atelier session diff <session>` or the MCP `diff` tool.
5. Run `atelier land <session>` when policy allows self-approval. When a human
   holds the gate, request landing and leave the request pending.
6. Read `atelier journal` to verify the act and its attribution.

The MCP loop uses the same states: `manifest` → `open_session` → edit the
returned `working_copy` → `diff` → `land` or `request_land` → `journal`.

## Product feedback is part of the loop

When Atelier itself behaves unexpectedly, contradicts its documentation, or
makes a repeated workflow harder than it needs to be, read
[`subskills/feedback.md`](subskills/feedback.md) before finishing. Search the
issue tracker, then open an evidence-backed issue in `bipa-app/atelier` or add
new evidence to an existing issue. This turns agent friction into product
input instead of silent workarounds.

## Ready-to-run material

- [`examples/one-shot-coding.md`](examples/one-shot-coding.md) — run a coding
  agent inside one versioned session.
- [`examples/durable-session.md`](examples/durable-session.md) — edit with
  normal tools across several commands.
- [`examples/mcp-agent.md`](examples/mcp-agent.md) — wire an MCP client and use
  the native tool loop.
- [`examples/multi-source-recovery.md`](examples/multi-source-recovery.md) —
  land across mounts and recover a parked source.
- `scripts/collect-diagnostics.sh` — collect safe environment facts for an
  issue without workspace content or credentials.
- `scripts/open-feedback-issue.sh` — search for duplicates, then open a
  suggestion, bug, or documentation issue with `gh`.
