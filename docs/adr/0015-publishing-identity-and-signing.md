# One publishing identity signs; actors stay authors

Engines synthesized a git identity per actor (`luiz@atelier.local`) and signed nothing, so every commit atelier published showed up unverified — hosts could neither match the address to an account nor check a signature. But a workspace already has exactly one identity a host *can* verify: its owner's. The `[git]` section of the actor config now names that publishing identity, and it becomes the committer of every commit the engine writes. The owning human authors as the identity; agents and automations keep authoring as themselves (`codex@atelier.local`), so their work stays attributed in git while the journal remains the attribution record. With `[git.signing]` configured, jj signs everything the engine writes with the identity's key — behavior `force`: agents author, the owner vouches. Hosts verify the committer's email against the signing key, which is exactly the model GitHub's own web-flow commits use: authored by whoever acted, committed and signed by the publisher.

## Considered options

- Sign as each actor: rejected — agents have no accounts and no keys a host would trust; an unverifiable signature buys nothing over none.
- Collapse authorship into the publishing identity: rejected — the workspace's product is attribution, and git's author field is the one channel other tools already read; the split (author = actor, committer = publisher) keeps both.
- Sign only at landing: rejected — landing rebases session commits, and a rebase preserves signatures only if they existed; signing at write keeps one rule with no special cases.

## Consequences

- `[git]` and `[git.signing]` live in the actor config home. An agent process with its own scoped config home publishes verified commits only when provisioned with the owner's `[git]` section — otherwise it falls back to the synthetic address, unsigned, exactly as before. One machine, one owner is the assumed shape.
- Signing behavior is `force`: every commit the engine writes or rewrites is signed by the publisher, whoever authored it. Adopted history is never rewritten by atelier (ADR-0002: adopt, never import), so foreign commits keep their own signatures.
- The ssh backend takes a key path (`~/` expands against `$HOME`); the gpg backend takes a key id. Verification-side configuration (allowed signers) belongs to git and the host, not to atelier.
- Without `[git]`, nothing changes: synthetic per-actor addresses, no signatures.
