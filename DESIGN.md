# broadcom-proxy

A Claude account pool. Agents send Claude API requests to one address; the proxy picks the account with the most room, forwards the request untouched apart from the credential, and records what happened. BROADCOM (the dashboard) and the usage service read what it records.

```
agents ──/v1──▶ broadcom-proxy ──▶ api.anthropic.com
                 │ accounts: OAuth logins + API keys
                 │ pool: who serves which session
                 │ sampler: limits, plans, held resets
                 │
                 ├─ writes: receipts, readings, seats (append-only ndjson)
                 └─ serves: small management API + live stream
                         ▲
        BROADCOM (Next.js) and the usage service read it
```

## What it does, and nothing else

1. **Forward** `POST /v1/messages`, `POST /v1/messages/count_tokens`, `GET /v1/models`. Answer `HEAD|GET /api/hello` and `/healthz` with 200.
2. **Pick an account** per request: API credit first, then plans ranked by room. Keep a session on its account until it should move.
3. **Keep accounts signed in**: refresh OAuth tokens before they expire, sign new ones in (Claude code-paste, ChatGPT device code).
4. **Measure limits**: read them from every response's headers, and sample each account's usage, plan and held resets.
5. **Spend held resets** when every account has used its week.
6. **Record**: one receipt per request, one reading when a limit moves, one seat when a session lands on or leaves an account.
7. **Serve** accounts, pool, sign-in and policy over HTTP, with a live stream.

Usage collection (transcripts, other machines, GitHub artifacts, pricing, the usage views) is not the proxy's job. It lives in the usage service, which reads the receipts.

## The wire: honest passthrough

Every client on the pool is a real Claude Code binary (CLI, Agent SDK, or Claude Code under hex, microscope or T3 Code). The proxy forwards what the client sent and changes only what routing requires:

- **OAuth account**: replace `Authorization` with the account's bearer token, drop the client's `X-Api-Key`. Body bytes untouched.
- **API key**: `x-api-key` set to the key, `Authorization` dropped, the `included/` model prefix stripped, and the OAuth-only betas (`oauth-*`, `claude-code-*`) dropped so the key is accepted.
- **Responses** stream back byte for byte, with upstream headers minus hop-by-hop ones, so `anthropic-ratelimit-unified-*` reaches the client.

It doesn't disguise clients. CLIProxyAPI rewrites non-Claude-Code clients to look like Claude Code on a Mac: it renames tools, replaces the system prompt, forges the billing-header signature and spoofs the TLS handshake. Our clients are Claude Code, so none of that is needed, and it's ~3k lines this proxy doesn't carry.

## Routing

Same decisions as the Go fork's pool (spec: what it does today, carried over deliberately):

- **Credit first**: a bare model goes to the highest-priority API key with credit, as `included/<model>`. A key that answers "credit balance is too low" cools for a minute, and the request falls through to the plans in the same call.
- **Rank plans**: usable (5-hour < session cutoff, week < weekly cutoff, signed in) first; among usable, holders of a reset first, then comfortable room per agent, then urgency to use the week before it resets.
- **Session affinity**: key is `X-Claude-Code-Session-Id`, else `metadata.user_id.session_id`. A session stays on its account until that account is over or close to a cutoff and a calmer one exists.
- **Failover before the first byte**: 401 → refresh once, then next account; identity-verification 400 → 403 to the client, account cools 30 min; 429 with a rejected unified window → account cools until that window resets; 5xx/529 → next account, 1 min cool. Other 4xx are the request's own and go straight back.

The ranking is a pure function `rank(&[AccountView], &Policy, now) -> Vec<Pick>` with table tests built from the Go fork's tests and today's real readings.

## Data

Append-only ndjson in the state directory, one file per kind, each line a typed record with a `v` version field:

| File | One line per | Read by |
|---|---|---|
| `receipts.ndjson` | finished request: time, session, entrypoint, account, credential, model, status, tokens (incl. cache), upstream request id | usage service, credits |
| `readings.ndjson` | limit change ≥0.5 pt or 15 min: account, window, used %, resets at, source (header/sample) | Limits page (8+ weeks kept) |
| `seats.ndjson` | session landing on or moving between accounts, with cause | Sessions page, attribution |
| `resets-spent.ndjson` | held reset spent or refused | Accounts page |

Plus small snapshot files written atomically: `accounts/<id>.json` (plan, held resets). Credentials stay where they are (one JSON file per OAuth login), written with tmp + rename. In memory the proxy keeps only live state: accounts, cooldowns, seats, latest readings. Nothing grows with history.

Today's Go-format files (`usage-history.ndjson`, `attribution.ndjson`) are imported once into the new ones.

## API

JSON over HTTP, bearer management key. Each endpoint has a JSON Schema and an example payload in `api/`, checked by tests.

- `GET  /v8/management/deck/events`: SSE. `snapshot` of live state on connect, `patch` with changed keys (`a:<account>`, `pool`; null = removed), `stale` tags.
- `GET  /v8/management/deck/accounts`: limit reading history and session moves.
- `GET  /v8/management/credentials`, `PATCH .../credentials/status`, `DELETE .../credentials`, `POST .../credentials/refresh`.
- `POST /v8/management/deck/accounts/policy`: preview or save cutoffs and agents per account.
- `POST /v8/management/oauth/start`, `GET .../oauth/status`, `POST .../oauth/finish`: Claude (paste code) and ChatGPT (device code). A new account joins the pool and the stream the moment it's saved.
- `POST /v8/management/pool/resets/:account/spend`.

Paths and shapes match what BROADCOM's `lib/proxy/schema.ts` reads today.

## Config

One JSON file, typed, reloaded on change: listen address, client keys, API-key credentials (name, prefix, priority), pool policy, reset command, state and credential directories. Secrets live in their own files, never inline.

## Running

- systemd socket activation (the socket unit holds the port across restarts), graceful drain on SIGTERM: in-flight streams finish, new connections queue on the socket.
- One binary, tokio + axum + hyper/reqwest on rustls. No TLS fingerprint games.
- Logs: one structured line per request to the journal; no request dumps.

## Moving off the Go proxy

1. **Shadow**: run on a spare port, read-only on credentials (no refresh, the Go proxy still owns refresh). Verify with real Claude Code requests from a test session against every path: OAuth, API key, credit fall-through, failover, count_tokens, models.
2. **Cut over**: stop Go, start Rust on the socket. Go moves to a loopback port with no credentials (so only one process ever refreshes a token), serving what the old dashboard still needs (usage views, workspace, `management.html`); the proxy forwards those paths to it and posts its receipts into Go's usage store as their own device, so usage and credit views stay whole. Confirm with a real request. Rollback: re-enable the Go unit on the socket, same credential files.
3. **Usage service** replaces the Go process; then Go is retired.
