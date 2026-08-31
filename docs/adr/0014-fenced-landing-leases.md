# Landing leases carry fencing epochs

The landing lease is a TTL claim in SQLite; the moves it guards — a landing, an undo, a fold — publish jj operations and git refs. A holder that outlived its TTL (a stalled process, a long import) kept publishing while a rival legitimately claimed the same point, and jj's operation reconciliation resolves conflicting working-copy pointers by keeping one side: a journaled line move could silently vanish. Every claim now opens a numbered tenancy — the point's epoch bumps — and every leased line move runs a fence at its last pure moment, before the first externally visible write. The fence renews the tenancy its epoch names and refuses once a newer claim superseded it. Expiry alone is a non-event: an honest holder that ran long renews and proceeds; only supersession aborts, with nothing published. Release keeps the lease row as the epoch's high-water mark, so no earlier fence can ever validate again — the fencing idea the hosted ownership record already uses (ADR-0013), kept as one pattern.

## Considered options

- Renewal heartbeats without epochs: rejected — a heartbeat narrows the race, but a stalled holder still publishes after a rival claims; only a fenced token refuses it deterministically.
- A conditional operation commit (fencing checked by the store): rejected — jj's operation store offers no conditional publish, and forking the engine to add one buys microseconds.

## Consequences

- A window remains between the fence and the first write — microseconds against the seconds the guarded phases take. A process stalled exactly there can still publish; the window is named and shared by every fencing design whose resource cannot check the token itself.
- A superseded landing or undo refuses by name (`LeaseSuperseded`) and a rerun completes what remains. A superseded fold skips exactly like a held point: the winner already folded, and the next operation probes afresh.
- The lease row survives release, and `user_version` 6 adds its epoch column. A binary from before the fence would still claim through the pre-epoch SQL, bypassing supersession; stores stamped newer than a binary's version now refuse to open, so the next schema change fails closed. Against already-shipped pre-fence binaries the residual stands, accepted: workspaces live on one machine and installs move in lockstep through `atelier update`.
- Reloading a store at its operation head merges divergent operation heads whoever loads it, superseded or not; the merge is deterministic and actor-independent, so the fence does not gate loads — it gates what a tenancy may publish about a line.
