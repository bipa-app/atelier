# Instruction capture in the journal is profile-decided

The journal must answer "on whose instruction did this act happen", but verbatim prompts carry PII and secrets, and most workspaces should not become prompt archives. By default the journal records, per session, an instruction summary plus a reference to the originating run; profiles that answer to auditors (Compliance, Legal) require the instruction verbatim. One mechanism; the policy lives where policy already lives — the profile.

## Consequences

- The journal schema carries both fidelities from day one: summary, external reference, optional verbatim body.
- Changing a workspace's profile changes capture going forward, never retroactively.
- References point at systems we do not control; a dangling reference is an accepted risk of the default, and one reason audit-grade profiles demand verbatim.
