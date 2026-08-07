# jj's model is the engine; git is the boundary

Workspaces need every edit versioned automatically (ADR-0001), safe concurrent actors, and undo — while staying first-class citizens of the git ecosystem. We adopt Jujutsu's model as the internal engine — working-copy-as-snapshot, stable change identities, first-class non-blocking conflicts, an operation log with undo, pluggable storage — embedded via jj-lib (Rust), on the git backend, so every workspace remains a real git repo that humans and tools clone, pull, and push.

## Considered options

- Plain git as the engine: rejected — the staging area, blocking conflicts, and the missing operation log push ADR-0001 and multi-actor safety onto our own discipline and bespoke machinery.
- A custom VCS model: rejected — years to rebuild what jj already proves, with git interop as an afterthought instead of a property.

## Consequences

- Git-LFS sources are unsupported by jj (jj-vcs/jj#80): attaching one must fail loudly at attach time.
- jj-lib is v0.x: pin it; expect churn at upgrades.
- Exit is cheap: on the git backend, snapshots are git commits. Leaving jj strands only change ids, the operation log, and unmaterialized conflicts — never content.
- In our language a jj "workspace" is a Working Copy; the glossary term Workspace never means jj's.
