# atelier

Versioned workspaces for humans and AI agents.

atelier answers "what is a workspace in the age of agents": a named, versioned body of work content — code, spreadsheets, contracts, docs — with its own history, journal, profile, and policy, served to humans and agents through the same contracts.

- **Everything is versioned.** No unversioned state; every edit becomes a snapshot. Jujutsu's model is the engine, git is the boundary — every workspace is a real git repo you can clone, pull, and push.
- **Documents diff like code.** One diff library, a fidelity ladder (binary → projected text → rich), and format support shipped as packages. First package: docx → markdown.
- **Actions are recorded.** History records content states; the journal records acts and intent — who, in which session, on whose instruction, with what approval.
- **Agents are first-class.** Sessions, working copies, leases, landing, and a manifest, exposed over MCP and the `ws` CLI.

Status: pre-alpha, under active design.

Start with [`CONTEXT.md`](CONTEXT.md) (the domain glossary), [`docs/adr/`](docs/adr/) (decisions), and [`plans/`](plans/) (PRD and current plan).

License: Apache-2.0
