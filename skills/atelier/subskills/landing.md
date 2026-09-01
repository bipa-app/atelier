# Land and recover

Use this branch for gate transitions and recovery.

## Normal transitions

- `atelier land <session>` snapshots current session edits, creates or
  refreshes the request, and self-approves where policy allows.
- MCP `request_land` leaves a request pending when a human holds the gate.
- `atelier approve <request>` advances a pending request whose approvals now
  satisfy policy.
- `atelier reject <request> --reason "…"` records a refusal with its reason.
- `atelier session abandon <session>` closes unfinished work without landing;
  its snapshots remain in history.
- `atelier undo <request>` steps a landed request back to each landed line's
  parent and re-opens the request for a new decision.

One landing fans out across the root and touched mounts under separate fenced
leases. A retry skips sources that already landed.

## Parked recovery

`Parked` is a state, not a command failure. A conflicted source parks while
other sources may land.

1. Open the session's existing working copy.
2. Resolve the contested content there. Keep independent work; concede or
   rewrite only the overlap.
3. Review `atelier session diff <session>`.
4. Run `atelier land <session>` again. This snapshots the resolution, re-opens
   the gate, and approves in one transition.
5. Read `atelier requests` and `atelier journal` to confirm the remaining
   source landed.

A bare `approve` on a still-parked request refuses because no new snapshot
carries the resolution. Editing the shared workspace would bypass both the
session and the record.

If a documented transition behaves differently, preserve the exact state,
command, output, and version, then follow [`feedback.md`](feedback.md).
