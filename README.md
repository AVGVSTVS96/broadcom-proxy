# broadcom-proxy

A Claude account pool. Point Claude Code at it and every request goes out on the account with the most room: API credit first, then subscription plans ranked by their five-hour and weekly limits, each session kept on its account until that account nears a cutoff.

It forwards what Claude Code sends and swaps only the credential. No client disguise, no prompt rewriting, no TLS games.

```sh
cargo install --path .
cp config.example.json ~/.config/broadcom-proxy/config.json   # then edit
broadcom-proxy ~/.config/broadcom-proxy/config.json
```

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:8317 \
ANTHROPIC_CUSTOM_HEADERS="X-Api-Key: <a client key>" claude
```

- **Routing**: credit-first, ranked plans, session affinity, failover before the first byte. See [DESIGN.md](DESIGN.md).
- **Accounts**: OAuth logins are one JSON file each (CLIProxyAPI's format); add Claude and ChatGPT logins from any device through the management API. Exactly one running proxy should have `accountOwner: true`: it refreshes tokens, which rotate.
- **Records**: `receipts.ndjson` (one line per upstream attempt, with Anthropic's usage), limit history, session moves and held resets spent, as append-only files in the state directory.
- **Management**: `/v8/management/...` with a bearer key, including a live SSE stream of every account's standing.
- **systemd**: `systemd/` has a socket unit that holds the port across restarts and a service that drains in-flight streams on stop.

MIT licensed.
