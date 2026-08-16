# Contributing to atelier

atelier's product is trust: a diff you cannot trust is worse than no diff. Everything below serves that.

Read before touching code: [`CONTEXT.md`](CONTEXT.md) — the domain vocabulary; use these words. [`docs/api.md`](docs/api.md) — behavior contracts and edge-case rulings. [`docs/adr/`](docs/adr/) — decisions and why. [`AGENTS.md`](AGENTS.md) — the full development rules; they bind humans and agents alike.

## Quality gates

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI ([`.github/workflows/gates.yml`](.github/workflows/gates.yml)) runs exactly these; green gates are the bar for every change. The toolchain is pinned in `rust-toolchain.toml`. The lint bar is clippy `all` + `pedantic` with `unwrap_used`, `panic`, `todo`, `unimplemented` restricted and `unsafe_code` denied; lint policy lives in `Cargo.toml` and `clippy.toml`, never in `#[allow]` attributes.

## The contracts every change keeps

1. **Determinism is contract** (ADR-0003). Same input + same package version → byte-identical output, forever.
2. **Degradation is never silent.** A failure either errors or lands in the journal while output falls to a lower fidelity rung.
3. **Malformed input errors, never shortens.** Partial output silently fabricates diffs.
4. **Projections are injective.** Two distinct documents must never project alike.
5. **Packages raise fidelity, never gate it.** A missing, failing, or panicking package drops the rung; it never aborts a diff.

## Writing a format package

Format support ships as packages over one diff library (ADR-0003): each format's projector, differ, and later merger is its own independently versioned crate. The package interface is the ecosystem's public ABI. This is the seam outside contributions are most welcome at.

### The trait

`FormatPackage` lives in `crates/diff-core/src/package.rs`:

```rust
pub trait FormatPackage: Send + Sync {
    /// The package's stable identity: name + semver. Projections carry it;
    /// caches key on it — a version bump is a new projection.
    fn id(&self) -> PackageId;

    /// How confidently this package claims the document; `None` when it
    /// does not handle it. Equal claims tie-break by package id.
    fn detect(&self, path: &str, bytes: &[u8]) -> Option<Confidence>;

    /// Render the document to its deterministic text projection.
    fn project(&self, bytes: &[u8]) -> Result<Projection, PackageError>;

    /// The rich diff in the format's own terms, or `None` while the
    /// package ships no differ — the ladder falls back to projected text.
    fn diff(&self, before: &[u8], after: &[u8]) -> Option<Result<Vec<Delta>, PackageError>>;
}
```

The projector is mandatory; the differ is optional and can arrive later.

### The rules a package lives by

- **Deterministic, forever.** Same bytes under the same package version produce the same projection: no wall-clock, no randomness, no map iteration order in any output path.
- **Refuse malformed input.** Truncated, out-of-range, or spec-violating input returns `PackageError` — never a shortened projection.
- **Project injectively.** Distinct documents must project distinctly. When escaping, escape the escape character first; encode structure in a channel content cannot reach.
- **Fail loudly, degrade gracefully.** Return `PackageError` and the ladder falls to the text or binary rung, journaled as `package_failed`. The core catches panics at the package boundary so a bug degrades fidelity instead of killing the process — but treat that boundary as a net, not a feature.

### Steps

1. Create `crates/format-<name>/` modeled on [`crates/format-docx`](crates/format-docx): `publish = false`, workspace lints, a dependency on `atelier-diff-core`.
2. Implement `FormatPackage` for a unit struct.
3. Register it in `builtin_packages()` (`crates/core/src/workspace.rs`); detection order does not matter — confidence and the id tie-break decide.
4. Add the crate to the workspace `members` list.

### Tests a package needs

- **Golden projection**: a fixture document projects to an exact, literal expected string — and the same bytes twice produce byte-identical projections.
- **Refusals**: malformed fixtures (truncated, out-of-range, spec-violating) each return an error, asserted by name.
- **Distinctness pins**: where a rule exists so two inputs stay distinguishable, assert both exact outputs *and* `assert_ne!` between them.
- Never re-derive an expected value by calling the code under test; that test is tautological.

## Testing philosophy

1. Never change test expectations to make tests pass — fix the code.
2. Test behavior, not coverage; trust the type system.
3. e2e tests assert exact full output, never `contains`.
4. Use production code in tests; never build helpers that mirror production code.
5. Real documents stay local: `fixtures/real/` is gitignored, and the ignored `real_documents` test asserts structure, never content.

## Delivery

1. Branch `feat/<slug>` off `main`; rebase, never merge — linear history.
2. Atomic commits, one logical unit each: imperative subject with crate scope (`feat(format-docx): …`), body saying what and why.
3. Run the gates before every push.
4. The PR description carries a review guide: entry point → how far the change reaches → which state machine it builds, fixes, or protects.

License: Apache-2.0. By contributing you agree your contributions are licensed under it.
