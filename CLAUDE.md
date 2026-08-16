# AGENTS.md — atelier development rules

atelier is the workspace substrate agents and humans do real work in. Its product is trust: a
diff you cannot trust is worse than no diff. These rules are **MANDATORY**.

Read before touching code: [`CONTEXT.md`](CONTEXT.md) (the vocabulary — use these words, avoid
the listed synonyms), [`docs/api.md`](docs/api.md) (behavior contracts and edge-case rulings),
[`docs/adr/`](docs/adr/) (decisions and why), the active plan in [`plans/`](plans/).

## Contracts (the product's laws)

1. **Determinism is contract** (ADR-0003). Same input + same package version → byte-identical
   output, forever. Caches key on (package id@version, content id) and entries never
   invalidate; derived artifacts never enter history. No wall-clock, no randomness, no map
   iteration order in any output path.
2. **Degradation is never silent.** A failure either errors or lands in the journal
   (`package_failed`, `file_too_large`) while output falls to a lower fidelity rung. No
   `unwrap_or*` / `ok()` defaulting that hides an error. A deliberate rendering default that
   matches Word's behavior is allowed only with a comment stating the rule at the decision
   site.
3. **Malformed input errors, never shortens.** Truncated, out-of-range, or spec-violating
   input must refuse — partial output silently fabricates diffs.
4. **Projections are injective.** Two distinct documents must never project alike. When
   escaping, escape the escape character first; encode structure in a channel content cannot
   reach.
5. **Packages raise fidelity, never gate it.** A missing, failing, or panicking package drops
   the rung (journaled); it never aborts a diff. Package calls sit behind a panic boundary.

## Code shape (MUST)

1. **Parse, don't validate.** Turn raw input into a bounded, typed value in one named function
   (`list_level`, `outline_level`) and pass the type through. Typed values end-to-end
   (`PackageId`, `Confidence`, `Act`); raw scalars only at transport boundaries.
2. **Match domain enums exhaustively — spell out every variant, never `_`.** A new variant
   must break the build at every decision point; that list is the review. `==`/`matches!`
   carry no such guarantee — protection has to come from a real `match`.
3. **Errors are failures; refusals and degradations are values.** Model expected outcomes as
   `Ok` variants plus journal acts. Never smuggle an outcome through `Err`.
4. **Prefer shallow code over clever code.** Fewer branches, fewer locals, fewer helper hops.
   No one-off private helpers unless they encode a real domain concept.
5. **Comments state what the code cannot say**: hidden constraints, spec citations, invariants
   (`// startOverride prevails over nested starts`). Doc comments give a function's contract.
   Never narrate steps, reference tasks/PRs/reviews, or transcribe conversation.
6. **Keep the diff at base shape.** Never rebind or reindent existing code to bolt a change
   on; if the change forces a structural rewrap, restructure the change.
7. **No `mod.rs`.** Modules are `module_name.rs`.
8. **No `#[allow]`.** Lint policy lives in `Cargo.toml` (`[workspace.lints]`) and
   `clippy.toml`. A principled per-site exception is `#[expect(lint, reason = "…")]` — the
   reason is mandatory and the lint must actually fire.
9. **Guard writes that move a state machine.** The write names the expected prior state in
   its predicate (`… SET state = 'landed' WHERE id = ? AND state = 'approved'`), so a stale
   writer fails instead of overwriting — the pattern the lease claim and approval dismissal
   already follow (`AND dismissed = 0`).
10. **No side effects inside a store transaction.** A transaction block reads and writes the
   store — never the filesystem, a package, or the network. Whatever must happen on commit
   happens after it.
11. **Direct params up to seven.** Past seven — or when same-typed params are easy to mix
   up — take a params struct. Typed values (rule 1) keep most signatures short first.

## Quality gates (MUST before landing)

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs exactly these ([`.github/workflows/gates.yml`](.github/workflows/gates.yml)). The
workspace is small and builds are fast — run gates whenever useful. A shared warm cache lives
at `CARGO_TARGET_DIR="$HOME/.cache/atelier-shared-target"`.

The lint bar is clippy `all` + `pedantic` with `unwrap_used`, `panic`, `todo`,
`unimplemented` restricted and `unsafe_code` denied. `clippy.toml` allows unwrap/expect/panic
inside `#[test]` functions only — helpers in `tests/*.rs` files are not covered; give them
`.expect("why")`.

## Testing philosophy (MUST)

1. **Never change test expectations to make tests pass** — fix the code.
2. **Test behavior, not coverage.** Skip trivial tests; trust the type system.
3. **e2e tests assert exact full output**, never `contains` — contains-checks prove little
   beyond exit codes.
4. **Assert literal expected values**, never a value re-derived by calling the code under
   test — that test is tautological.
5. **Use production code in tests; never build helpers that mirror production code.**
6. **Pin distinctness.** When a rule exists so two inputs stay distinguishable, the test
   asserts both exact outputs *and* `assert_ne!` between them.
7. **Cover state machine transitions**: the legal ones land, and each illegal one refuses by
   name — an untested transition is an untested promise.
8. **Real documents stay local.** `fixtures/real/` is gitignored; the ignored
   `real_documents` test asserts structure, never content — confidential fixtures and their
   text never enter the repository. Run with
   `cargo test -p atelier-cli --test real_documents -- --ignored`.

## Delivery (MUST)

1. Work in a worktree off `main` on a `feat/` branch — never commit to `main` directly; land
   by fast-forwarding `main`.
2. **Rebase, never merge.** Linear history; force-push with lease after rewrites.
3. Atomic commits, one logical unit each: imperative subject with crate scope
   (`feat(format-docx): …`), body saying what and why — not a diff restatement.
4. The PR description carries a review guide: entry point → how far the change reaches →
   which state machine it builds, fixes, or protects.

## Prose (docs, PR text, commit bodies)

Orwell, 1946 — these govern prose, never code or technical terms:

1. Never use a metaphor or figure of speech you are used to seeing in print.
2. Never use a long word where a short one will do.
3. If it is possible to cut a word out, always cut it out.
4. Never use the passive where you can use the active.
5. Never use a jargon word if you can think of an everyday English equivalent.
6. Break any of these rules sooner than say anything outright barbarous.
