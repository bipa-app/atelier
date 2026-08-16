# atelier

Versioned workspaces for humans and AI agents.

atelier answers "what is a workspace in the age of agents": a named, versioned body of work content — code, spreadsheets, contracts, docs — with its own history, journal, profile, and policy, served to humans and agents through the same contracts.

- **Everything is versioned.** No unversioned state; every edit becomes a snapshot. Jujutsu's model is the engine, git is the boundary — every workspace is a real git repo you can clone, pull, and push.
- **Documents diff like code.** One diff library, a fidelity ladder (binary → projected text → rich), and format support shipped as packages. First package: docx → markdown.
- **Actions are recorded.** History records content states; the journal records acts and intent — who, in which session, on whose instruction, with what approval.
- **Agents are first-class.** Sessions, working copies, leases, landing, and a manifest, exposed over MCP and the `atelier` CLI.

Status: pre-alpha, under active design.

## Quickstart

Build the `atelier` binary (the toolchain is pinned in `rust-toolchain.toml`):

```sh
git clone https://github.com/bipa-app/atelier
cargo install --path atelier/crates/cli
```

Tell atelier who you are, then work in a fresh directory. Every `atelier` command snapshots outstanding edits first — there is no save step:

```sh
mkdir -p ~/.config/atelier
cat > ~/.config/atelier/config.toml <<'EOF'
[actor]
name = "you"
kind = "human"
EOF

mkdir demo && cd demo
atelier init
echo "atelier keeps every edit" > notes.txt
atelier journal
echo "no save button, no lost work" >> notes.txt
atelier diff
```

The journal names who did what and when; the diff reads at the highest fidelity the format allows — a changed .docx prints a markdown line diff, never "binary files differ".

From there:

- `atelier watch` — external edits (Finder, any editor) become attributed snapshots within seconds.
- `atelier attach <folder>` — bind an existing folder as the workspace's source.
- `atelier serve --mcp-stdio` — serve the workspace to agents over MCP: sessions, diffs, gated landing, journal.
- `atelier sessions` / `atelier requests` / `atelier approve <id>` — review and land an agent's change.

## Contributing

CI runs the same gates you run locally: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. Start with [CONTRIBUTING.md](CONTRIBUTING.md) — including how to ship support for a new document format as its own package.

Read next: [`CONTEXT.md`](CONTEXT.md) (the domain glossary), [`docs/adr/`](docs/adr/) (decisions), and [`plans/`](plans/) (PRD and current plan).

License: Apache-2.0
