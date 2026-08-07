# Everything in a workspace is versioned; the version-control model is the content model

Workspaces hold any knowledge work — code, docs, spreadsheets — edited concurrently by humans and agents, and every change must diff, attribute, and audit. We decided there is no unversioned state in a workspace: every write becomes a snapshot in one version-control content model (snapshots, changes, refs), and folder and document workspaces ride the same model as code. One history serves diffing, attribution, and audit; agents never decide what to save; "folder" and "repo" stop being different kinds of thing.

## Considered options

- Unversioned working area with explicit saves (git's own model): rejected — the save step is the thing humans forget and agents fumble, and it leaves audit gaps.
- Database/CRDT content model with version export: rejected — it forfeits git interop, the one ecosystem requirement fixed from the start.

## Consequences

- Auto-snapshot is universal, including folder sources edited outside the tool.
- Large binaries must be first-class in the content store from day one.
- Scratch work needs retention policy (cheap history truncation), never an "unversioned" escape hatch.
