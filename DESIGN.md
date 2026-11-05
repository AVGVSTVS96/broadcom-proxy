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
6. **Record**: one receipt per upstream attempt, one reading when a limit moves, one seat when a session lands on or leaves an account.
7. **Serve** accounts, pool, sign-in and policy over HTTP, with a live stream.

Usage collection (transcripts, other machines, GitHub artifacts, pricing, the usage views, live sessions) is not the proxy's job. It lives in the usage service, which reads the receipts.

## The wire: honest passthrough

Every client on the pool is a real Claude Code binary (CLI, Agent SDK, or Claude Code under hex, microscope or T3 Code). The proxy forwards what the client sent and changes only what routing requires:

- **OAuth account**: `Authorization` becomes the account's bearer token; the client's own `Authorization` and `X-Api-Key` never leave. Body bytes untouched.
- **API key**: `x-api-key` set to the key, `Authorization` dropped, and the OAuth-only betas (`oauth-*`, `claude-code-*`) dropped. Body bytes untouched, except a `<prefix>/` model name loses its prefix.
- **Responses** stream back byte for byte (decompressed), with upstream headers minus hop-by-hop ones and `set-cookie`, so `anthropic-ratelimit-unified-*` and `request-id` reach the client.
- The proxy's own calls (sampler, token refresh, reset claims, sign-in) say `User-Agent: broadcom-proxy/<version>`. Anthropic's usage, profile and token endpoints answer it the same as Claude Code's own client (checked in the shadow run).

It doesn't disguise clients. CLIProxyAPI rewrites non-Claude-Code clients to look like Claude Code on a Mac: it renames tools, replaces the system prompt, rewrites `metadata.user_id`, forges the billing-header signature and spoofs the TLS handshake. Our clients are Claude Code, so none of that is needed, and it's ~3k lines this proxy doesn't carry.

One consequence, seen in the shadow run: Anthropic answers an OAuth request without Claude Code's system prompt with `429 rate_limit_error "Error"`, no limit headers. That's what Claude Code's own startup quota probe (`max_tokens: 1`, "quota") gets, through the proxy and direct alike, and what a plain non-Claude-Code request on a plan account gets. The proxy passes it back as the request's own.

## Routing

Same decisions as the Go fork's pool, carried over deliberately (`src/pool.rs`, pure functions, table tests from the Go fork's `pool_test.go` and `pool_resets_test.go`):

- **Credit first**: a bare model goes to the API keys first, highest priority first, as the bare model. A key that answers "credit balance is too low" cools for a minute and the request falls through to the next key, then the plans, in the same call. `<prefix>/<model>` (e.g. `included/claude-opus-5-5`) goes only to keys with that prefix. No per-key model lists: any model is tried on credit.
- **Rank plans** (`pool::rank`): usable (5-hour < session cutoff, week < weekly cutoff, signed in) first; among usable, holders of a reset first, then comfortable room per agent, then known, then urgency to use the week before it resets, then room. Unusable: open ones by five-hour use, then soonest free. Agent SDK and `claude -p` sessions weigh `agentSdkFactor` (1.7) on the five-hour room.
- **Session affinity**: key is `X-Claude-Code-Session-Id`, else `metadata.user_id.session_id` (UUIDs only). The seat is the binding: a session stays on its account while seen in the last 24 h, until `pool::move_away` says it's over a cutoff (any usable account takes it) or near one with a calm account free. A seated account that's cooling or turned off sends the session to the next ranked one.
- **Failover, before the first byte only**, one pass over the credentials in order (no retry rounds: Claude Code retries on its own):

| upstream | what happens |
|---|---|
| 2xx | streamed back, seat recorded |
| 400 "credit balance is too low" | key cools 1 min, next credential |
| 400 "identity verification" | account cools 30 min, next credential |
| 401 on a plan | renew the token and retry once (owner refreshes; a non-owner rereads the file), else cool 30 min, next |
| 401–403 otherwise | cools 30 min, next credential |
| 429 on a plan with a rejected unified window | account cools until that window resets (and at least `retry-after`, 10 s floor), next |
| 429 on a plan without one | the request's own, back to the client |
| 429 on a key | key cools for `retry-after` (10 s floor), next |
| 408, 5xx, 529 | next credential, no cooldown: the trouble isn't the account's |
| anything else | the request's own, back to the client |

When every credential fails, the client gets the last answer; when none is available, a 503 `overloaded_error`.

## Data

Append-only ndjson and small JSON snapshots in the state directory, in the Go fork's names and shapes, so a rollback, the old deck and the usage service read them unchanged and nothing needs importing:

| File | One line per | Read by |
|---|---|---|
| `receipts.ndjson` (new) | upstream attempt: CLIProxyAPI's usage-event fields (`id`, `responseId`, `upstreamRequestId`, `accountId`, `failed`, `timestamp`, `model`, `project`, `sessionId`, `entrypoint`, token counts, `serviceTier`, `cacheWrite1hTokens`) plus `status`, `credential` and Anthropic's own `usage` object verbatim (advisor iterations included) | usage service, credits |
| `usage-history.ndjson` | limit move ≥0.5 pt, or 15 min while readings arrive | Limits page, `/deck/accounts` |
| `attribution.ndjson` | session landing on or moving between accounts, with cause | Sessions page, `/deck/accounts` |
| `resets-spent.ndjson` | held reset spent or refused | Accounts page |

Snapshots, written with tmp + rename: `plans/<id>.json`, `resets/<id>.json`, `reset-claims.json` (a sent claim keeps its request id across restarts), and the account list `accounts.json`. Credentials stay where they are, one JSON file per OAuth login in CLIProxyAPI's format, rewritten whole (unknown fields kept) with tmp + rename. API-key credential ids are CLIProxyAPI's (`claude:apikey:<hash>`), so receipts and `credits.json` keep joining.

In memory the proxy keeps only live state: accounts, cooldowns, seats, latest readings. Nothing grows with history.

## API

JSON over HTTP under `/v8/management`, bearer management key. Shapes match BROADCOM's `lib/proxy/schema.ts` (the contract; a test checks the live keys against it).

- `GET  /deck/events`: SSE. `snapshot` of live state on connect, `patch` with changed keys (`a:<account>`, `pool`; null = removed), a heartbeat every 20 s. `s:` and `app:` keys belong to the usage service.
- `GET  /deck/accounts`: last week of limit readings and last day of session moves (`label` null: session names live in the usage service).
- `POST /deck/accounts/policy`: preview, or save when `revision` (the config file's hash) still matches.
- `GET  /credentials`, `PATCH /credentials/status {id, disabled}`, `DELETE /credentials?id=`, `POST /credentials/refresh {id}`.
- `POST /oauth/start {provider: "claude"|"codex"}` → `{state, url, flow, userCode?}`; `GET /oauth/status?state=`; `POST /oauth/finish {state, code}` for Claude. Both flows work from any device: Claude's hosted page shows a code to paste, ChatGPT's device page takes a code. A new login joins the pool and the stream the moment it's saved; signing in an existing account again replaces its tokens in its own file.
- `POST /pool/resets/{account}/spend`.

Anything that writes credentials, refreshes or spends is refused (409) unless the proxy is the account owner.

## Config

One camelCase JSON file (`config.example.json`): listen address, `accountOwner`, client and management key files, credentials and state directories, the account list path, API keys (name, key file, prefix, priority), pool policy, `agentSdkFactor`, reset command. Secrets live in their own files, never inline.

`accountOwner` makes this the one process that refreshes tokens, writes credential files and spends resets. Refresh tokens rotate when used, so a second refresher logs accounts out; a shadow runs with it off and rereads credential files the owner keeps current.

The credentials directory is rescanned every minute and whenever a token is refused. A policy saved through the API takes effect at once; other config edits take a restart, which the socket makes seamless.

## Running

- systemd socket activation (the socket unit holds the port across restarts), graceful drain on SIGTERM: in-flight streams finish, new connections queue on the socket. Without a passed socket it binds `listen`.
- One binary, tokio + axum + reqwest on rustls. No TLS fingerprint games.
- Logs: one line per upstream attempt to the journal (credential, model, entrypoint, the 5h/7d utilization Anthropic reported, Anthropic's error message on failure); no request dumps.

## Moving off the Go proxy

1. **Shadow** (done Nov 5): a spare loopback port, `accountOwner: false`, real credentials read-only. Real Claude Code requests on every account and key, each client kind.
2. **Cut over** (on hold: the Go proxy stays live): stop Go, start Rust on the socket with `accountOwner: true`, same credential files and state directory. Go would move to a loopback port with no credentials for what the old dashboard still needs; forwarding those paths to it and posting receipts into Go's usage store aren't built, since the cutover is off. Rollback: re-enable the Go unit on the socket, same credential files.
3. **Usage service** replaces the Go process; then Go is retired.
