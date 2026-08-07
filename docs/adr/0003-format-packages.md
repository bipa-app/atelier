# Format support ships as packages over one diff library

Rich document diffs are the product's differentiator, but formats are many and each is deep. We build one diff library that owns the format-independent diff model — diffs made of addressed deltas — and a fidelity ladder (binary → projected text → rich), and ship each format's support (projector, differ, later merger) as its own independently versioned package. The first package is docx, projector-only: docx → markdown.

## Considered options

- One monolithic diff engine: rejected — it couples every format's release cadence and closes the door on outside contributors, which the open-source goal needs open.
- Rich-only, no text fallback: rejected — a workspace must diff every document from day one; packages raise fidelity, they never gate it.

## Consequences

- Determinism is contract: same document and package version → same projection and same diff. Outputs carry the package version; caches key on both.
- The package interface is the ecosystem's public ABI. Changing it after packages exist in the wild is the expensive thing — design it before the second package, not the fifth.
- Packages are in-process libraries first (Rust crates). A sandboxed plugin boundary (WASM) becomes worth building only when third-party packages arrive.
