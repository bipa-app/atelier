# ADR-0012: Remote sources — buckets attach, land, and mirror like folders

Status: accepted (2026-08-16)

## Context

The documents story does not live on local disks: contracts, spreadsheets,
and exports live in buckets. The PRD scoped remote sources to v2; ADR-0010
ruled that an async runtime "earns its place" in remote sync adapters and
deferred the decision here. celld (denoland/celld — self-hosted Durable
Objects coordinating through a bucket alone) was considered as the vehicle
and is not one: it is a compute runtime, not a filesystem gateway. Its
bucket-only coordination *pattern* is the hosted/multi-node design's
concern (tracked separately), not a way to read objects.

## Decision

1. **One adapter over `object_store`.** The Arrow project's crate speaks
   S3-compatible, GCS, Azure, and local `file://` through one trait. A new
   crate, `atelier-source-remote`, wraps it behind a **synchronous seam**:
   it owns a contained current-thread tokio runtime and exposes blocking
   functions (`download_all`, `fingerprint`, `mirror`). The core's
   execution model stays synchronous; the runtime lives and dies inside
   the adapter. `file://` makes every code path testable without a
   network; real buckets are the same path with credentials from the
   environment (`AWS_*`, and the provider equivalents).
2. **A remote source is a mounted source.** `atelier attach s3://bucket/prefix
   --mount docs` lists and downloads the objects into the mount, which gets
   its own engine and history like any folder mount. A remote root import
   refuses by name in v1. The URL persists in config as the source path;
   `SourceKind::Remote` names the kind, so every decision point matches on
   it explicitly.
3. **The fingerprint guard, remote edition.** The recorded fingerprint is a
   digest over the sorted object listing — key, ETag, size — captured at
   attach and after every mirror. A sync only writes a bucket whose listing
   still matches; one changed out-of-band parks the sync, journaled, never
   overwritten; `atelier sync --force` overwrites deliberately and reseeds.
   Exactly ADR-0010's posture with the ETag standing in for content bytes.
4. **Out-flow mirrors the landed tree.** On landing (and on `atelier sync`),
   the landed snapshot exports to a scratch directory (the proven
   `export_tree` mirror) and the adapter reconciles the bucket against it:
   upload added and changed objects, delete removed ones, engine-internal
   names never travel. In-flow — colleagues uploading to the bucket — is
   R2 (`atelier pull`), not this slice; until then a moved bucket parks
   syncs, which is the correct refusal, not a gap.

## Consequences

- The listing-compare-then-write window is the ADR-0010 window over a
  network: accepted and journaled. Per-object conditional puts (If-Match
  on ETag) are a hardening note once provider support is uniform.
- ETags are opaque and provider-defined (multipart uploads change them);
  the guard never interprets them, only compares listings to listings.
- The adapter is the workspace's first networked dependency: `object_store`
  plus a contained tokio. The dependency budget rationale (docs/style.md)
  is met — the alternative is hand-rolling four providers' signing and
  paging, and the crate is the ecosystem's standard.
- Large buckets download whole at attach in v1. The caps that bound local
  content apply per file after download; a paging/windowed attach is future
  work the journal will motivate with real usage.
