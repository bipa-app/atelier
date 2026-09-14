# Explicit publishing-author policy

Some source repositories require the configured human identity as both Git author
and committer. Atelier can preserve that contract while its journal records which
agent or automation acted and on whose instruction.

`[git] author = "publisher"` makes new engine commits use the configured publishing
identity as author. The default, `author = "actor"`, keeps ADR-0015's behavior.
Both policies keep the publishing committer and configured signing. Neither changes
the actor, session ownership, journal records, landing approvals, or adopted history.

This adds an explicit option to ADR-0015, which rejected making publisher authorship
the only behavior. Actor authorship remains the default; a repository owner who
chooses publisher authorship uses the journal as the record of agent activity.
Changing policy affects later engine writes, not existing imported commits.

The policy is one enum in the existing identity configuration. It adds no storage
schema, network request, history scan, or dependency. An unknown value refuses
configuration parsing before the workspace writes a commit.
