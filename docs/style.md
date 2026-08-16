# atelier style

Adapted from [TigerBeetle's TIGER_STYLE.md](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md)
for a Rust CLI and library over jj, SQLite, and local filesystems. Design goals
in order: **safety, performance, developer experience**. Every Tiger Style rule
appears below with a ruling — adopted, adapted, or rejected — and the reason.
[`AGENTS.md`](../AGENTS.md) stays the short law; this document is the ruling on
each point and the rationale. Where they overlap, they agree; where they seem
to disagree, AGENTS.md wins and this file has a bug.

Zero technical debt is the standing policy: we do it right the first time,
because the second time may not transpire. A problem solved in design is many
times cheaper than one solved in production.

## Safety

### Control flow

- **No recursion** — *adopted.* Filesystem walks use an explicit work stack
  (bounded by entry count), never the call stack: recursion depth is attacker-
  or user-controlled through directory nesting. Event loops (`serve`'s request
  loop, `watch`) are the sanctioned infinite loops; each says so in its doc
  comment. Every other loop iterates a finite collection or a bounded window.
- **Only a minimum of excellent abstractions** — *adopted.* AGENTS.md rule 4
  (shallow over clever, no one-off helpers) is this rule. When splitting a
  function, push `if`s up and `for`s down: the parent owns control flow and
  state transitions, helpers stay leaf-pure.
- **Put a limit on everything** — *adopted.* Transport and content boundaries
  carry named caps (`BODY_SIZE_MAX`, `NEW_FILE_SIZE_MAX`, `LADDER_FILE_SIZE_MAX`,
  `READ_WINDOW_MAX`, journal/log render limits). Related caps assert their
  relationship at compile time. A new unbounded input is a review defect.
- **Compound conditions** — *adopted.* Split `a && b` decision points into
  nested branches when each case deserves its own handling or assertion;
  domain enums already force exhaustive `match` (AGENTS.md rule 2), which is
  the stronger form of the same idea.
- **State invariants positively** — *adopted.* `if index < length` with an
  `else`, not `if index >= length`.

### Types

- **Explicitly-sized types, avoid `usize`** — *adapted.* Storage, protocol,
  and time values are explicitly sized (`i64` milliseconds, `u64` byte sizes,
  `u32` approvals). `usize` stays where Rust's standard library demands it —
  indexing and collection lengths — and never crosses a serialization boundary.
  Rejecting `usize` wholesale would fight the language for no safety gain.

### Assertions

- **Assertions detect programmer errors; operating errors are handled** —
  *adopted, with the repo's own seam.* Refusals and degradations are values
  (`Ok` variants plus journal acts, AGENTS.md rule 3); errors are failures;
  `assert!` is for invariants no input should ever reach. Never assert on user
  input. The clippy `panic` restriction governs `panic!` in production paths;
  `assert!` on invariants is the sanctioned crash — corrupt code must not keep
  running.
- **Density: average two assertions per function** — *adapted.* Measured over
  state-moving code (engine, workspace, coordination, store), not over render
  helpers where the type system already carries the proof. Guarded writes
  (AGENTS.md rule 9) are assertions in SQL: the predicate names the expected
  prior state and a stale writer fails.
- **Pair assertions** — *adopted.* Enforce a property where it is produced and
  again where it is consumed: the sync fingerprint is recorded after every
  mirror and re-checked before the next; the lease is claimed by name and
  released by name.
- **Split compound assertions; single-line implication** — *adopted.*
  `assert!(a); assert!(b);` over `assert!(a && b)`.
- **Assert compile-time constant relationships** — *adopted.* `const _: () =
  assert!(...)` pins cap orderings (the ladder cap fits inside the snapshot
  cap; the read window fits inside the body cap).
- **Positive and negative space** — *adopted.* Tests already pin both: legal
  transitions land, each illegal one refuses by name (AGENTS.md testing rule
  7); distinctness pins carry `assert_ne!` (rule 6).
- **Assertions are not a substitute for understanding** — *adopted.* Build the
  mental model, encode it as assertions, explain it in comments, and let tests
  hunt the gap.

### Memory

- **Static allocation, nothing allocated after init** — *rejected, spirit
  adapted.* TigerBeetle is a long-lived database with a fixed budget; atelier
  is a short-lived CLI and a localhost server over user content of unknown
  shape. Wholesale static allocation would be theater. The spirit we keep:
  every buffer that holds external input is bounded by a named cap, content
  streams through windows rather than accumulating, and hot paths avoid
  gratuitous clones.
- **Declare at smallest scope, minimize variables in scope** — *adopted.*
- **Buffer bleeds** — *adapted.* Windows slice exact lengths; no
  partially-initialized buffers are ever exposed (Rust's initialization rules
  carry most of this).

### Functions

- **Hard limit of 70 lines per function** — *adopted and enforced.* clippy's
  `too_many_lines` at threshold 70 runs in the gates with warnings denied. Cut
  walls of code where the domain cuts: control flow stays in the parent,
  helpers encode real concepts (AGENTS.md rule 4 still applies — a split that
  invents a fake concept is worse than length). Test functions are the ruled
  exception: a test tells one story end to end (testing rules 3 and 7), and
  fragmenting a story to satisfy a line count would hide the transition being
  pinned. Where the lint fires on a test target it carries a file-level
  `#[expect]` with that reason.
- **Warnings at the strictest setting** — *already law.* clippy `all` +
  `pedantic`, `-D warnings`, no `#[allow]` (AGENTS.md rule 8).
- **Don't react directly to external events** — *adopted.* `watch` debounces
  edit storms into one snapshot; the HTTP server works one request at a time at
  its own pace; the landing lease serializes applies. Batch, don't context-
  switch.
- **All errors handled** — *already law.* No defaulting that hides an error
  (AGENTS.md contract 2); catastrophic-failure studies are why.
- **Explicit options at call sites** — *adopted.* Pass load-bearing options
  explicitly at the call site (jj snapshot/import options are); never lean on a
  library's defaults for behavior we promise.

## Performance

- **Napkin math in the design phase** — *adopted.* Back-of-envelope sketches
  belong in the ADR that decides a design (ADR-0010's doc-scale reasoning is
  the shape). The four resources in order — network, disk, memory, CPU —
  weighted by frequency.
- **Batch to amortize** — *adopted.* Already load-bearing: the watcher's
  debounce, jj snapshots folding many edits into one, the lease holding one
  apply over N per-source landings.
- **Control plane vs data plane** — *adapted.* Verbs (open, land, approve) are
  the control plane and stay chatty; content bytes are the data plane and move
  in bounded windows.
- **Predictable CPU, extract hot loops** — *adapted.* The hot paths are jj's
  and SQLite's; ours must not make them zig-zag. Diff and projection loops take
  primitive arguments and stay free of `self` where it costs nothing.

## Developer experience

### Naming

- **Get the nouns and verbs just right** — *already law.* `CONTEXT.md` is the
  glossary; code speaks it and avoids the listed synonyms. Don't overload one
  name with two meanings.
- **snake_case files and functions** — *Rust default, adopted.* No `mod.rs`
  (AGENTS.md rule 7).
- **No abbreviations** — *adapted.* Production identifiers spell words out.
  Sanctioned shorthands, each a proper noun or ecosystem convention: `jj`,
  `id`, `tx` (a rusqlite/jj transaction), `wc` only inside the engine where it
  mirrors jj's own `wc_commit` vocabulary. Test bodies may bind a `Workspace`
  as `ws` — the fixture idiom is louder than the letters. CLI flags are long
  form in scripts and docs.
- **Acronym capitalization (`VSRState`)** — *rejected.* Rust API guidelines
  and the ecosystem write `Cli`, `Http`; fighting rustc's expected idents
  buys nothing.
- **Units and qualifiers last, descending significance** — *adopted.*
  `lease_ttl_ms`, `expires_at_ms`, `BODY_SIZE_MAX`, `READ_WINDOW_MAX`. Group by
  prefix, qualify by suffix.
- **Same-length related names** — *adopted for pairs.* `source`/`target`, not
  `src`/`dst`.
- **Helper named after caller** — *adopted.* `land` / `land_async`,
  `export_tree` / `export_tree_async`.
- **Callbacks last in parameters** — *adopted* (`on_event` closes the watch
  signature).
- **Important things first in a file** — *adopted.* `main` first, then the
  order a reader needs; structs list fields, then types, then methods.
- **Nouns over participles** — *adopted;* read models are nouns (`manifest`,
  `status`, `journal`).
- **Options structs for confusable arguments** — *already law* (AGENTS.md
  rules 1 and 11: typed values first, params struct past seven or when
  same-typed arguments could swap).

### Comments and commits

- **Descriptive commit messages** — *already law* (AGENTS.md delivery rule 3;
  PR descriptions are not a substitute — they are invisible to `git blame`).
- **Say why, show workings** — *already law* (AGENTS.md rule 5; Orwell rules
  govern the prose).
- **Say how in tests** — *adopted.* Every `tests/*.rs` opens with a `//!`
  header stating the goal and method, so a reader can skip with confidence.
- **Comments are sentences** — *adopted.* Capital letter, full stop or colon;
  end-of-line comments may be phrases.

### Cache invalidation

- **No duplicate or aliased variables** — *adopted;* Rust's borrow checker
  enforces the hard cases, we avoid the soft ones (two names for one value).
- **Pass big arguments by reference** — *Rust default, adopted.*
- **Out-pointer initialization** — *rejected.* Zig-specific; Rust moves are
  the idiom and the compiler elides the copies that rule exists to avoid.
- **Check close to use (POCPOU)** — *adopted.* Compute a value where it is
  consumed. Where a gap is unavoidable it is named and accepted in an ADR
  (ADR-0010's fingerprint window).
- **Simple return types** — *already law* (outcomes are values; `GateOutcome`
  and `SyncOutcome` collapse dimensionality instead of smuggling states
  through `Err`).
- **Run to completion** — *adopted.* The core is synchronous; jj futures are
  driven to completion at each seam (ADR-0010 records why no runtime).

### Off-by-one

- **`index`, `count`, `size` are distinct concepts** — *adopted.* Units in
  names; windows carry `start`/`end`/`total` and slice exact lengths.
- **Show division intent** — *adopted.* `div_euclid` and friends over bare `/`
  where rounding is a scenario.

### Style by the numbers

- **Formatter always** — `cargo fmt --check` in the gates.
- **4-space indent, 100-column hard limit** — rustfmt defaults, enforced.
- **Braces on `if`** — rustfmt enforces.

## Dependencies

*Adapted from "zero dependencies".* atelier's engine **is** a dependency
(jj-lib) — that is the product bet, recorded in ADR-0002. The policy: every
dependency is a liability; adding one requires an ADR-grade why (what it
carries, what breaks when it breaks, why vendoring or writing it is worse).
Prefer the standard library; prefer what the tree already carries (sha2 over a
new hasher); never add a dependency for code that fits in a page.

## Tooling

*Adapted from "write scripts in Zig".* The one language is Rust: load-bearing
scripts become `cargo` invocations or Rust tests, not shell. CI runs exactly
the three gate commands a developer runs (AGENTS.md quality gates) — no
CI-only logic. The real-documents fixture test is the local-only exception and
is documented where it lives.
