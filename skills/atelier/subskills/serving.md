# Serve and host

Use this branch when an agent connects through MCP/HTTP or a workspace lives
in object storage.

## Local agent surfaces

```sh
atelier serve --mcp-stdio
```

One stdio process serves one MCP client. Start it with the workspace as `cwd`.
The tool surface includes `manifest`, `status`, `open_session`, `read`,
`write`, `diff`, `request_land`, `approve`, `reject`, `land`,
`landing_requests`, `journal`, `abandon`, `undo`, `sync`, and `pull`.

```sh
atelier serve --http
```

HTTP serves streamable MCP at `POST /mcp` and the same contract under `/v1`.
Loopback is the default. A non-loopback bind needs both `--allow-remote` and a
bearer `--token`; keep the token out of commands, logs, and issue reports.

## Hosted workspace

```sh
atelier serve --http --hosted 's3://bucket/prefix'
```

The server claims the ownership record, hydrates the workspace, replicates
while serving, and releases on clean shutdown. A present workspace seeds a
fresh record; an empty directory hydrates from an existing one. Use
`--take-over` only after proving the named holder died, because it fences that
holder from later writes.

S3 credentials come from `AWS_ACCESS_KEY_ID` and
`AWS_SECRET_ACCESS_KEY`. S3-compatible stores such as MinIO or R2 use the
bucket URL's `?endpoint=` parameter. Never copy credentials or signed URLs
into journal summaries or GitHub issues.

See [`../examples/mcp-agent.md`](../examples/mcp-agent.md) for client wiring.
