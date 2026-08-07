# Every landing passes a gate; the shared line stays conflict-free

Landing is not a direct write. A session opens a landing request — the diff, its requester, its approvals — and the change lands only when the workspace's gate is satisfied. Humans and agents approve alike, and every request, approval, and rejection is a journal act. The final apply serializes on the landing lease and rebases; a conflict parks the request instead of landing, because the shared line never carries a conflicted state — conflict markers inside a binary document are meaningless.

## Considered options

- Direct land with lease only, no gate: rejected — the user model is PR-shaped (work is proposed, reviewed, landed), and audit profiles need the approval chain as first-class record, not an afterthought.
- Landing conflicted states (jj-native behavior): rejected — fine for code, nonsense for docx. Conflicts live on changes; resolution is a follow-up session.

## Consequences

- `land` is sugar: request + self-approve where policy allows, so a single-actor workspace keeps a one-verb flow. Default profile: one approval, self-approval allowed; audit profiles tighten both.
- New snapshots on a change dismiss prior approvals under the default policy (audit profiles: always).
- Concurrent landings become concurrent open requests; only the final apply serializes.
