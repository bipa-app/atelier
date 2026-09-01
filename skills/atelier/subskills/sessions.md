# Work through sessions

Use this branch for edits, builds, tests, and document reads.

## Choose the shortest session that preserves the work

One command or one coding-agent run:

```sh
atelier run --actor-name "coding-agent" --actor-kind agent \
  --summary "Fix request parsing and verify its boundary tests" -- <command> [args...]
```

Add `--land` when the command may self-approve and successful completion is
enough to land. A failed command leaves the session open with its edits
versioned.

Several commands or direct file-tool use:

```sh
atelier session open --actor-name "coding-agent" --actor-kind agent \
  --summary "Fix request parsing and verify its boundary tests"
# Work only in the printed working-copy path.
atelier session diff <session>
atelier land <session>
```

The working copy is an ordinary directory. Editors, language servers, builds,
and test runners need no Atelier-specific adapter.

Agents using the CLI must pass both actor flags when they open a session. The
session persists that actor, so later snapshots, requests, self-approval, and
landing keep the agent attribution even when those commands run under the
workstation owner's config.

## MCP session loop

1. Call `manifest`.
2. Call `open_session` with `actor_name`, `actor_kind = "agent"`, and one
   honest `instruction_summary`.
3. Edit the returned `working_copy` with native tools, or use MCP `read` and
   `write`.
4. Call session `diff` before asking to land.
5. Call `land` when self-approval applies; otherwise call `request_land` and
   leave the request for an approver.
6. Call `journal` to verify attribution.

MCP `write` snapshots immediately. Native edits snapshot at the next session
`diff`, `request_land`, `land`, `approve`, or `abandon`; there is no separate
commit step.

Paths include a mount prefix: `api/src/main.rs` addresses `src/main.rs` in the
`api` source. MCP `read` returns format projections where a package exists;
prefer it over raw bytes for documents such as `.docx`.
