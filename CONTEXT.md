# Agent Workspaces

The domain of workspaces that humans and AI agents share: versioned bodies of work content of any kind — code, spreadsheets, contracts, docs — plus the record of who did what in them, and the contracts agents need to work in them.

## Language

### Core

**Workspace**:
A named, versioned body of work content with its own history, journal, profile, and policy, served to humans and agents through the same contracts.
_Avoid_: project, repo, folder (those are sources), environment

**Source**:
An external origin a workspace is attached to: a git repository (remote or local) or a folder (local or remote). A workspace can outlive, diverge from, and re-sync with its source.
_Avoid_: origin, upstream, mount

**Sync Policy**:
The rule for how content moves between a workspace and its source: import once, pull-only mirror, or two-way.
_Avoid_: sync mode, replication

**Boundary**:
The set of paths a workspace versions. An ignored path is outside the workspace, not unversioned state inside it; changing the boundary is a journaled act.
_Avoid_: ignore list, exclusions

**Snapshot**:
One immutable whole-workspace state in history, attributed to an actor.
_Avoid_: version, revision, save

**Profile**:
The composable bundle that types a workspace: content conventions, projectors, policies, and agent tools. Finance, Legal, and Code ship as built-in profiles; users compose their own.
_Avoid_: workspace type, template, mode

### Documents

**Format Package**:
The independently shipped unit of support for one document format: it projects, diffs, and later merges that format. Absence never blocks a workspace — fidelity drops to text or binary.
_Avoid_: plugin, codec, converter

**Projection**:
A deterministic text rendering of a non-text document, produced by its format package; the universal face for search and for fallback diffs. The original document is always kept.
_Avoid_: extraction, conversion, preview

**Projector**:
The function in a format package that renders a document to its projection.
_Avoid_: converter, parser

**Differ**:
The function in a format package that computes a rich diff between two documents of its format.
_Avoid_: diff engine

**Diff**:
The difference between two versions of a document, expressed at the highest fidelity its format package allows: rich deltas, projected text, or binary changed/unchanged.
_Avoid_: patch, changeset

**Delta**:
One addressed difference inside a rich diff: where in the document, in the format's own terms — a cell, a clause, a paragraph — what kind of change, and the before and after.
_Avoid_: hunk, edit

### Actors & work

**Actor**:
A human, agent, or automation with its own identity. Every snapshot and journal entry names its actor.
_Avoid_: user (humans only), member

**Session**:
One actor's bounded run of work in a workspace. Journal entries group under it.
_Avoid_: run, task

**Change**:
The stable identity of one unit of work as it evolves through snapshots; it survives rewrites. Sessions produce changes.
_Avoid_: branch, PR, patch

**Working Copy**:
One actor's editable set of files backed by a workspace; every edit in it becomes a snapshot automatically.
_Avoid_: worktree, checkout, clone, workspace (jj's sense of the word)

**Conflict**:
A recorded, non-blocking disagreement between changes, kept in history and resolved later as its own act.
_Avoid_: merge conflict (implies blocking)

**Landing**:
The act of advancing a workspace's shared line to include a change, passing its gates.
_Avoid_: merge, push, ship

**Landing Request**:
A change's application to land on the shared line: the diff, its requester, and its approvals, open until it lands, parks, or is abandoned.
_Avoid_: pull request, merge request, MR

**Lease**:
A time-boxed exclusive claim on a scarce point of a workspace — a landing point, a shared working copy, or a policy-guarded path — held by one actor at a time. Editing needs no lease; landing does.
_Avoid_: lock

### Record & governance

**Journal**:
The append-only record of acts in a workspace: who did what, in which session, on whose instruction, with what approval. History records content states; the journal records actions and intent.
_Avoid_: audit log (one use of it, not the thing), activity feed, event log

**Instruction**:
The task or prompt that drove a session's acts. The journal records a summary plus a reference to the originating run by default; verbatim where the profile demands it.
_Avoid_: prompt, task (both narrower)

**Policy**:
The rules a profile enforces on a workspace: what may be written where, by whom, kept how long.
_Avoid_: settings, config

**Gate**:
A policy point where an act needs an approval from an authorized actor before it lands.
_Avoid_: check, review step

**Approval**:
A recorded grant by an authorized actor — human or agent — that helps a landing request pass its gate.
_Avoid_: review (broader), sign-off

### Agent contract

**Agent Surface**:
The contract every workspace exposes to agents: content access, diffs, search, journal, leases, and the manifest. "Agent ready" means this surface exists whole, for every workspace, whatever its source or profile.
_Avoid_: API (an implementation of it), integration

**Manifest**:
The workspace's self-description, the first thing an actor reads: what this workspace is, its profile, conventions, and current state.
_Avoid_: README (a human doc), metadata
