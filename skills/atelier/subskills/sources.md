# Set up and move sources

Use this branch for workspace creation and source movement.

## Initialize and attach

```sh
mkdir workspace
cd workspace
atelier init
atelier attach ~/work/api --mount api
atelier attach ~/work/web --mount web
```

A local source without `--mount` imports into the root line. A named mount
keeps its own engine and history. Git mounts preserve history and remain real
repositories; every landing moves the adopted branch, or the `atelier`
bookmark when no branch was checked out.

Before copying a local Git source, `attach` prints its HEAD, branch, tracked
modification count, untracked file count, and estimated untracked bytes. It
refuses dirty sources by default. Prefer a clean clone; use `--allow-dirty`
only when every reported change belongs in the workspace.

`attach` copies what Atelier can version: it applies ignore rules by directory,
skips nested repositories, preserves symlinks, and keeps the root repository's
`.git`. Build caches and linked worktrees should therefore stay outside the
workspace copy. A different result is product feedback; capture it through
[`feedback.md`](feedback.md).

## Move content across the source boundary

- `atelier pull [source]` folds bucket-side changes into a mounted remote
  source as one attributed snapshot.
- `atelier sync [source]` mirrors a local folder source's shared line back to
  its origin.
- `atelier sync [source] --force` overwrites an origin that changed out of
  band. Use it only after reviewing both lines; ordinary sync parks instead
  of discarding either side.
- `git -C <mount> push origin <branch>` publishes a landed Git mount. This is
  the ordinary Git verb at the boundary.

Prefer Atelier verbs inside mounts. An out-of-band commit or branch move folds
into the line on the next Atelier command as an attributed pull; a conflict
refuses by name rather than guessing which content wins.
