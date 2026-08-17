# ADR-0013: Hosted workspaces — the celld pattern, not the celld runtime

Status: accepted (2026-08-16, ratified by Luiz) — implementation: slices H1–H3

> Revised after reading celld's source (the first draft reasoned from its
> README): the SQLite replication and fencing are not celld-internal — they
> are `rustyriver`, an embeddable published crate this ADR now adopts.

## Context

The PRD names a hosted / multi-node runtime, with celld as a candidate.
celld (denoland/celld) runs Cloudflare Workers and Durable Objects on your
own machines: each object is its own SQLite database, addressed by name,
replicated to an S3-compatible or GCS bucket you own; nodes coordinate
through that bucket alone — no control plane, no consensus; a cell no node
holds is inactive and costs nearly nothing.

atelier's workspace is exactly that shape of state: one SQLite database
(journal and coordination), one jj operation store, one git object store —
per workspace, self-contained, bucket-friendly. The question this ADR
answers: why not just use celld?

## The structural fact

celld hosts V8 isolates executing Wrangler bundles — JavaScript,
TypeScript, and WASM against the Workers API. It does not host programs.
atelier's cell body is jj-lib, gix, and rusqlite over a real POSIX
filesystem, and the product's load-bearing promise is that a session's
working copy is a **real directory** where agents run native tools
(`atelier run -- cargo test`). Inside a Workers isolate there is no
filesystem, no file locking, no subprocess — compiling the engine to WASM
does not conjure them. The slice of atelier that could live in a Durable
Object (the coordination rows) is the trivial slice; the slices that
cannot (the engine, the working copies) are the product.

Forcing it anyway would reduce celld to a lock service: a JS object per
workspace granting leases to real ateliers running on real machines — an
entire V8/Wrangler deployment to obtain a mutex the bucket grants directly
with one conditional write.

## Decision (proposed)

Borrow the pattern, skip the runtime:

1. **One workspace = one cell.** The unit of replication and ownership is
   the workspace: its coordination/journal database, jj operation store,
   and git object store. Working copies are *not* replicated — they are
   derived state that rematerializes from history on the owning node,
   exactly as session working copies materialize today.
2. **Ownership and fencing are celld's own design, adopted wholesale**
   (docs/fencing.md in denoland/celld). One ownership record per
   workspace, written conditionally (create, then compare-and-swap);
   every activation advances a fencing epoch; the data path writes
   **plainly under an epoch-prefixed key** (`cells/<ws>/…/e<epoch>/`) —
   the fence is the epoch in the key, not a condition on every request,
   so a deposed node's writes land in a superseded lineage and cost the
   hot path nothing. Acknowledgement re-reads the ownership record, so a
   stale node cannot make a promise the surviving lineage does not keep.
3. **The SQLite store replicates through `rustyriver`** — celld's own
   embeddable crate (published on crates.io, Apache-2.0): streaming
   WAL-to-LTX replication of a SQLite database to object storage with
   point-in-time restore and the lease fencing above, a Rust
   reimplementation of Litestream v0.5. We embed it in the atelier
   daemon rather than writing replication ourselves — the wheel exists,
   extracted, and maintained by the celld team. The jj and git object
   stores are content-addressed, not SQLite: they replicate as plain
   uploads plus an epoch-prefixed head pointer, under the same epoch
   from the same ownership record. Replication runs on the owning node
   at its own pace, batched (docs/style.md).
4. **Serving is claiming.** Any node with bucket credentials and a token
   can claim a workspace, hydrate its stores, serve it (the existing HTTP
   face, ADR-0006 unchanged: transports add reach, never capability), and
   release. An unclaimed workspace is objects in a bucket and costs
   nothing — celld's economic property, inherited without its runtime.
5. **celld stays a candidate at the edge, not the core.** If hosted
   atelier ever wants alarms, WebSocket fan-out of journal events, or
   webhook delivery at the edge, a celld worker subscribed to the bucket
   is a fine peripheral. Nothing in this design blocks that; nothing
   requires it.

## Considered and rejected

- **Run atelier inside celld (WASM).** Rejected: no filesystem, no file
  locks, no subprocesses in the Workers runtime; jj working copies and
  `atelier run` are structurally impossible there.
- **celld as the lock/coordination service only.** Rejected: adds a V8
  runtime, a Wrangler bundle, and a second language to deploy and debug,
  to deliver one conditional PUT the bucket already grants us.
- **A control plane (database or coordinator service).** Rejected on
  celld's own evidence: the bucket alone suffices for single-writer
  ownership at workspace granularity, and no control plane means nothing
  extra to operate, shard, or lose.
- **Writing the SQLite replication ourselves.** Rejected after reading
  celld's code: `crates/ltx` is exactly this wheel, already extracted as
  the embeddable `rustyriver` crate — conformance-tested backends over
  `object_store`, point-in-time restore, the fencing design above. An
  earlier draft of this ADR sketched a backup-API-plus-generations
  scheme; the published crate supersedes it. The risk taken on: the
  crate is weeks old at 0.1.0 — pinned, and hosted work does not start
  until it proves itself under our own conformance tests.

## Consequences

- Replication lag is the RPO for node loss: a crashed owner loses at most
  the operations since the last replication. The journal makes the gap
  observable — the bucket's journal generation names exactly what the
  world has seen.
- Lease TTL plus fencing generations handle crashed owners: the next
  claimant waits out the TTL, and any stale writes refuse on generation
  mismatch — the guarded-write rule (AGENTS.md rule 9) at bucket scale.
- Single-writer-per-workspace means hosted v1 serves a workspace from one
  node at a time — the same reality as today's one machine, with failover.
  Multi-node concurrent serving would need lease sharding per landing
  point and is explicitly out of scope.
- This ADR gates implementation cards; none exist until it is ratified.
