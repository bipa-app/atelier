# ADR-0011: Undo — a landed request steps back off its lines

Status: accepted (2026-08-16)

## Context

ADR-0001 promised undo; the CLI contract has carried `atelier undo` since the
sketch. The fan-out model (ADR-0009) made the question precise: a landing
touches N lines, syncs origins, moves bookmarks, and closes a session — what
does one undo mean, and what does it refuse?

## Decision

1. **Undo takes a landing request.** `atelier undo <request>` is the whole
   vocabulary. The journal's other acts are not undoable, each for its own
   reason, and the refusal says it: snapshots amend forward (edit again);
   gate acts move forward (approve again, or open a new request); syncs
   reconcile with `atelier sync --force`; init and attach are structural,
   not operations on content. Only a request in the `landed` state undoes.
2. **Per-line step-back, idempotent.** For each recorded landing, in reverse
   landing order (mounts by name reversed, root last), under that line's
   landing lease:
   - the line's head **is** the landed snapshot → step back to its parent;
   - the head is already the parent → this line is already undone → skip;
   - anything else → the line moved past the landing → refuse, naming the
     source and the newer head ("undo that landing first").
   A stepped line's landing record is deleted — the fact no longer holds,
   so a retry works what remains and a re-approval lands the line anew
   instead of skipping it as already landed. Skip-or-step plus
   un-recording makes a mid-way failure retryable with no new
   bookkeeping — the same recorded-fact posture the landing fan-out
   established. The landed snapshot stays in history; undo moves the
   line, never erases what it carried.
3. **The gate re-opens.** After every line steps back: the request moves
   `landed → open` (guarded — the write names its expected prior state), its
   approvals are dismissed (an undo is a new decision point, exactly like a
   new snapshot), and the session moves `landed → open` — its working copy
   still holds the change, so the work is immediately re-landable.
4. **The world follows the line.** The colocated git HEAD and the landed
   bookmark step back with each line (plain `git push` never publishes an
   undone head as the newest state), and a folder source re-mirrors under
   the ADR-0010 fingerprint guard — an origin that moved parks, journaled,
   never overwritten.
5. **The journal records the undo** — one act per stepped line, scoped like
   the land acts (`undo … r1 <restored-head>`), attributed to the actor who
   asked.

## Consequences

- Editing never takes a lease (ADR-0007), so a watch snapshot can move a
  line between undo's preflight and its step. The step itself is guarded
  inside the engine — a moved line refuses there too; the preflight only
  makes the common case fail before anything moves.
- Undoing r1 after r2 landed on the same line refuses; undoing r2 first
  restores r1's landed head, and r1 becomes undoable — undo composes
  backward through the stack, one landing at a time.
- A skipped line journals nothing: the act that stepped it was already
  recorded by the attempt that did the stepping.
