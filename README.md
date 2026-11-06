# broadcom-proxy

One address for all your Claude subscriptions. Point Claude Code at it and each request goes out on the plan with the most room left, so several Pro or Max accounts work like one bigger one.

It's a small Rust port of the Claude account pool in [AVGVSTVS96/CLIProxyAPI](https://github.com/AVGVSTVS96/CLIProxyAPI), a fork of [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI), and reads CLIProxyAPI's login files, so the two can share them. It forwards what Claude Code sends and swaps only the credential: no client disguise, no prompt rewriting, no TLS games.

```
claude ─┐                        ┌─▶ Max account A
claude ─┼─▶ broadcom-proxy ──────┼─▶ Max account B
claude ─┘   127.0.0.1:8317       └─▶ Pro account C   (then API keys)
```

## Start

```sh
cargo install --git https://github.com/AVGVSTVS96/broadcom-proxy
broadcom-proxy login     # once per Claude account: open the link, paste the code
broadcom-proxy           # serves on 127.0.0.1:8317
```

The first run writes a client key to `~/.config/broadcom-proxy/client-keys`. Give it to Claude Code:

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:8317 \
ANTHROPIC_CUSTOM_HEADERS="X-Api-Key: $(head -1 ~/.config/broadcom-proxy/client-keys)" \
claude
```

## How it picks an account

1. **Plans first.** Accounts under their cutoffs (90% of the five-hour limit, 98% of the week) come first: one holding a reset, then one with comfortable room per running agent, then the one whose week would otherwise reset unused, then the most room. Agent SDK and `claude -p` sessions count 1.7× against the five-hour limit, since they drain it faster.
2. **Sessions stay put.** A Claude Code session keeps its account, so its prompt cache stays warm, until that account passes a cutoff, or nears one while a calm account is free.
3. **Failover before the first byte.** When an account hits its limit or is signed out, the request goes to the next one in the same call, and that account sits out until it can serve again. An overloaded one just passes the request on.
4. **Then free credit, then paid extra usage.** Once no plan has room, the pool spends what costs nothing before what costs money:
   - **Free credit**: API keys marked `free`, drawing on credit you don't pay for, like the monthly API credit a Max or Ultra plan includes, claimed into an API organization with no card on file. Anthropic counts that credit for the API, the Agent SDK and `claude -p`, not interactive Claude Code, and a key with no balance answers "credit balance is too low", so the pool moves on.
   - **Paid extra usage**, last: a plan past its limits, which Anthropic serves only as extra usage on an account that has it turned on in Claude's settings, then paid API keys, each only while the plan account it names has extra usage on. That switch is the owner's yes to paying; while it's off, nothing paid is spent.
5. **Held resets.** When every account has used its week, the pool spends a held weekly reset, if one has any.

## Config

Optional. Without a file at `~/.config/broadcom-proxy/config.json` (or the path given as the argument), every default applies. A config with every field is in [config.example.json](config.example.json).

| Field | Default | What it is |
|---|---|---|
| `listen` | `127.0.0.1:8317` | Address to serve on, unless systemd passes a socket |
| `accountOwner` | `true` | Refreshes tokens, writes logins, spends resets. Only one process per login directory may: refresh tokens rotate |
| `clientKeysFile` | `~/.config/broadcom-proxy/client-keys` | Keys clients send, one per line; written on first run |
| `managementKeyFile` | `~/.config/broadcom-proxy/management-key` | Bearer key for the management API; written on first run |
| `credentialsDir` | `~/.config/broadcom-proxy/auths` | One JSON file per login, in CLIProxyAPI's format |
| `stateDir` | `~/.local/state/broadcom-proxy` | Receipts, limit history, session moves |
| `apiKeys` | none | `[{"name", "keyFile", "free", "account"}]`; `free: true` for a key on free credit, spent right after the plans; any other key is paid, spent last and only while `account`, the email of a plan login, has extra usage on |
| `policy` | `{"sessionCutoff": 90, "weeklyCutoff": 98, "agentsPerAccount": 100}` | Cutoffs, also settable over the API |
| `agentSdkFactor` | `1.7` | How much faster Agent SDK sessions drain the five-hour limit |
| `resetCommand` | none | Run as `sh -c '<command> "$1"'` with a line about each reset spent or refused |
| `accountsFile` | none | Where to write the account list, for tools that name accounts by id |

## Keep it running

`systemd/` has a user socket that holds the port across restarts, so clients queue instead of failing, and a service that gives requests in flight 10 minutes to finish on stop:

```sh
cp systemd/* ~/.config/systemd/user/
systemctl --user enable --now broadcom-proxy.socket
```

## Coming from CLIProxyAPI

Set `credentialsDir` to CLIProxyAPI's auth directory (`~/.cli-proxy-api` unless you changed it). Stop CLIProxyAPI before starting this one with `accountOwner: true`, or run this one with `accountOwner: false` beside it to compare. Going back is starting CLIProxyAPI on the same files.

## What it writes and serves

- `receipts.ndjson`: one line per upstream attempt, with Anthropic's usage object as sent. Also limit history, session moves and resets spent, all append-only in the state directory.
- `/v8/management/...` with the management key: accounts, pool standing as a live SSE stream, policy, sign-ins from any device, reset spending. [BROADCOM](https://github.com/AVGVSTVS96/broadcom) is a dashboard built on it.

How it works inside, and why: [DESIGN.md](DESIGN.md).

Pooling several subscriptions is between you and Anthropic's terms for your plans.

MIT licensed.
