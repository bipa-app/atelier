# Durable session with native tools

```sh
cd ~/workspaces/contracts
atelier manifest
atelier session open --actor-name "coding-agent" --actor-kind agent \
  --summary "Update renewal terms and verify the rendered diff"
```

Suppose Atelier prints `s7` and a working-copy path. Use that exact path:

```sh
cd /printed/working-copy
$EDITOR legal/renewal.docx
cargo test -p contract-checks

cd ~/workspaces/contracts
atelier session diff s7
atelier land s7
atelier journal
```

For supported documents, MCP `read` and `diff` expose text/rich projections;
use them to review paragraphs, cells, and emphasis rather than comparing ZIP
bytes.
