# The journal lives beside the repo, never inside it

A workspace's journal records intent — sessions, instructions (verbatim under audit profiles), approvals — and the tempting shortcut is to store it in the repo (git notes, or riding jj's op log) so it replicates with `git clone`. We keep it out: the journal is a SQLite database beside the repo, replicated only through the agent surface, under policy. Clone gives content, never intent.

## Considered options

- Git notes / a side ref: replicates for free — rejected. Verbatim instructions must never ride silently into every clone of a contract repo; notes are not queryable; journal retention cannot differ from content retention.
- jj's op log: rejected — engine-internal, prunable, and it speaks jj's vocabulary. The journal is a domain artifact with its own schema and retention.

## Consequences

- A plain-git collaborator sees history but no journal. That is correct by design: journal access implies the agent surface or the CLI, and their policy.
- Journal replication and backup are their own channel — which maps 1:1 onto a single-writer cell's SQLite in a future hosted mode.
- Retention and redaction can differ per profile without ever rewriting content history.
