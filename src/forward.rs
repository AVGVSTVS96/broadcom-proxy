//! The wire: each request goes out as the client sent it, with only the credential swapped,
//! and the answer comes back byte for byte. A request fails over to the next credential
//! only before the first byte reaches the client, and only for errors its credential caused.

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{
    app::{App, Inbound, Target},
    files,
    pool::{MINUTE, SECOND},
};

/// What the proxy does after an upstream error.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The request's own fault: the client gets it as is.
    Answer,
    /// The credential failed: sit it out until then and try the next.
    Cool(i64),
    /// Upstream trouble: try the next credential.
    Next,
    /// The OAuth token was refused: renew it and try the same account once more.
    Renew,
}

/// Which account-level errors a response carries, from its status and body.
pub fn classify(target: &Target, status: StatusCode, headers: &HeaderMap, body: &[u8], now: i64) -> Verdict {
    let text = String::from_utf8_lossy(body).to_lowercase();
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("");
    let seconds = |name: &str| header(name).parse::<i64>().ok();
    match status.as_u16() {
        401 if matches!(target, Target::Plan { .. }) => Verdict::Renew,
        401..=403 => Verdict::Cool(now + 30 * MINUTE),
        400 if text.contains("credit balance is too low") => Verdict::Cool(now + MINUTE),
        400 if text.contains("identity verification") => Verdict::Cool(now + 30 * MINUTE),
        429 => {
            let rejected = |name: &str| header(&format!("anthropic-ratelimit-unified-{name}status")) == "rejected";
            let mut until = now + seconds("retry-after").unwrap_or(10) * SECOND;
            if rejected("") || rejected("5h-") || rejected("7d-") {
                for window in ["5h-", "7d-", ""] {
                    if rejected(window) || window.is_empty() {
                        let reset = seconds(&format!("anthropic-ratelimit-unified-{window}reset")).unwrap_or(0);
                        until = until.max(reset * SECOND);
                    }
                }
            }
            Verdict::Cool(until.max(now + 10 * SECOND))
        }
        408 | 500..=599 => Verdict::Next,
        _ => Verdict::Answer,
    }
}

/// The parts of a request body routing reads.
#[derive(Deserialize, Default)]
struct Peek {
    #[serde(default)]
    model: String,
    metadata: Option<PeekMetadata>,
}

#[derive(Deserialize)]
struct PeekMetadata {
    user_id: Option<String>,
}

#[derive(Deserialize)]
struct UserId {
    session_id: Option<String>,
}

pub fn inbound(headers: &HeaderMap, body: &[u8]) -> Inbound {
    let peek: Peek = serde_json::from_slice(body).unwrap_or_default();
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim);
    let session = header("x-claude-code-session-id")
        .map(str::to_lowercase)
        .or_else(|| {
            let user = peek.metadata?.user_id?;
            serde_json::from_str::<UserId>(&user).ok()?.session_id
        })
        .filter(|s| is_uuid(s));
    Inbound { model: peek.model, session, entrypoint: entrypoint(header("user-agent").unwrap_or("")) }
}

/// The entrypoint in `claude-cli/<version> (external, <entrypoint>, …)`, or "".
pub fn entrypoint(user_agent: &str) -> String {
    let Some(rest) = user_agent.strip_prefix("claude-cli/") else { return String::new() };
    let Some((_, inside)) = rest.split_once('(') else { return String::new() };
    let Some(rest) = inside.trim_start().strip_prefix("external,") else { return String::new() };
    rest.split([',', ')']).next().unwrap_or("").trim().to_lowercase()
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_digit() || ('a'..='f').contains(&c) }
        })
}

/// Request headers that describe the connection or the client's own credential, not the request.
const DROP_REQUEST: [&str; 13] = [
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "accept-encoding",
    "authorization",
    "x-api-key",
];

/// Response headers that describe the upstream connection or an encoding already undone.
const DROP_RESPONSE: [&str; 8] =
    ["connection", "keep-alive", "transfer-encoding", "te", "trailer", "upgrade", "content-length", "content-encoding"];

pub fn upstream_headers(headers: &HeaderMap, target: &Target) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(headers.len() + 1);
    for (name, value) in headers {
        if !DROP_REQUEST.contains(&name.as_str()) {
            out.append(name.clone(), value.clone());
        }
    }
    match target {
        Target::Plan { token, .. } => {
            out.insert(
                "authorization",
                HeaderValue::from_str(&format!("Bearer {token}")).expect("token is a header value"),
            );
        }
        Target::Key { key, .. } => {
            out.insert("x-api-key", HeaderValue::from_str(key).expect("key is a header value"));
            let betas: Vec<String> = out
                .get_all("anthropic-beta")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(','))
                .map(str::trim)
                .filter(|b| !b.is_empty() && !b.starts_with("oauth-") && !b.starts_with("claude-code-"))
                .map(String::from)
                .collect();
            out.remove("anthropic-beta");
            if !betas.is_empty() {
                out.insert(
                    "anthropic-beta",
                    HeaderValue::from_str(&betas.join(",")).expect("betas came from a header"),
                );
            }
        }
    }
    out
}

fn client_headers(headers: &HeaderMap) -> HeaderMap {
    headers
        .iter()
        .filter(|(name, _)| !DROP_RESPONSE.contains(&name.as_str()) && name.as_str() != "set-cookie")
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect()
}

fn error(status: StatusCode, kind: &str, message: &str) -> Response {
    (status, axum::Json(json!({"type": "error", "error": {"type": kind, "message": message}}))).into_response()
}

pub async fn messages(State(app): State<Arc<App>>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    forward(&app, Method::POST, &uri, &headers, body, true).await
}

pub async fn count_tokens(State(app): State<Arc<App>>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    forward(&app, Method::POST, &uri, &headers, body, false).await
}

pub async fn models(State(app): State<Arc<App>>, uri: Uri, headers: HeaderMap) -> Response {
    forward(&app, Method::GET, &uri, &headers, Bytes::new(), false).await
}

/// Sends the request out on each credential the route lists until one answers.
/// `receipts` marks requests whose usage is recorded and whose session is seated.
async fn forward(app: &App, method: Method, uri: &Uri, headers: &HeaderMap, body: Bytes, receipts: bool) -> Response {
    if !app.client_allowed(headers) {
        return error(StatusCode::UNAUTHORIZED, "authentication_error", "This proxy needs one of its client keys.");
    }
    let started = files::now_ms();
    let inbound = inbound(headers, &body);
    let route = app.route(&inbound, started);
    let path = uri.path_and_query().map_or(uri.path(), |p| p.as_str());
    let url = format!("{}{path}", app.ends.anthropic);
    let unprefixed = route.unprefixed.as_ref().map(|model| {
        let mut doc: Value = serde_json::from_slice(&body).unwrap_or_default();
        doc["model"] = model.clone().into();
        Bytes::from(serde_json::to_vec(&doc).unwrap_or_default())
    });
    let body = unprefixed.unwrap_or(body);
    let mut last = None;
    for mut target in route.targets {
        let mut renewed = false;
        loop {
            let now = files::now_ms();
            let request =
                app.http.request(method.clone(), &url).headers(upstream_headers(headers, &target)).body(body.clone());
            let response = match request.send().await {
                Ok(response) => response,
                Err(err) => {
                    eprintln!("upstream {}: {err}", target.id());
                    last = Some(error(StatusCode::BAD_GATEWAY, "api_error", "The proxy couldn't reach Anthropic."));
                    break;
                }
            };
            let status = response.status();
            let response_headers = response.headers().clone();
            if let Target::Plan { id, .. } = &target {
                app.observe(id, &response_headers, now);
            }
            let receipt = receipts.then(|| Receipt::new(&inbound, &target, status, &response_headers, now));
            if status.is_success() {
                if let Target::Plan { id, .. } = &target
                    && receipts
                {
                    app.seat(&inbound, id, route.moved, now);
                }
                log(&method, path, &target, status, &inbound, &response_headers, started);
                let stream =
                    Tap::new(response.bytes_stream(), receipt, app.state_dir().to_path_buf(), &response_headers);
                let mut out = Response::new(Body::from_stream(stream));
                *out.status_mut() = status;
                *out.headers_mut() = client_headers(&response_headers);
                return out;
            }
            let bytes = response.bytes().await.unwrap_or_default();
            if let Some(mut receipt) = receipt {
                receipt.read_body(&bytes, false);
                receipt.write(app.state_dir());
            }
            log(&method, path, &target, status, &inbound, &response_headers, started);
            let mut answer = Response::new(Body::from(bytes.clone()));
            *answer.status_mut() = status;
            *answer.headers_mut() = client_headers(&response_headers);
            match classify(&target, status, &response_headers, &bytes, now) {
                Verdict::Answer => return answer,
                Verdict::Renew if !renewed => {
                    renewed = true;
                    if let Some(token) = crate::oauth::renew(app, target.id(), &target).await {
                        target = Target::Plan { id: target.id().to_string(), token };
                        continue;
                    }
                    app.cool(&target, now + 30 * MINUTE);
                }
                Verdict::Renew => app.cool(&target, now + 30 * MINUTE),
                Verdict::Cool(until) => app.cool(&target, until),
                Verdict::Next => {}
            }
            last = Some(answer);
            break;
        }
    }
    last.unwrap_or_else(|| {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded_error",
            "No account in the pool can take this request right now.",
        )
    })
}

/// One line per upstream attempt, with the account's limits as Anthropic just reported them.
fn log(
    method: &Method,
    path: &str,
    target: &Target,
    status: StatusCode,
    inbound: &Inbound,
    headers: &HeaderMap,
    started: i64,
) {
    let credential = match target {
        Target::Key { name, .. } => name.as_str(),
        Target::Plan { id, .. } => id.as_str(),
    };
    let utilization = |window: &str| {
        headers
            .get(format!("anthropic-ratelimit-unified-{window}-utilization"))
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-")
    };
    println!(
        "{method} {} {} credential={credential} model={} entrypoint={} 5h={} 7d={} ms={}",
        path.split('?').next().unwrap_or(path),
        status.as_u16(),
        inbound.model,
        inbound.entrypoint,
        utilization("5h"),
        utilization("7d"),
        files::now_ms() - started
    );
}

/// One line in receipts.ndjson: one upstream attempt and the usage Anthropic reported for it.
/// The fields match CLIProxyAPI's usage events; `usage` is Anthropic's own object, verbatim.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    source: &'static str,
    id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    response_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    upstream_request_id: String,
    account_id: String,
    credential: String,
    failed: bool,
    status: u16,
    timestamp: String,
    harness: &'static str,
    model: String,
    project: String,
    repository: Option<String>,
    session_id: String,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    reasoning_tokens: i64,
    cost_usd: f64,
    cost_known: bool,
    service_tier: Option<String>,
    #[serde(rename = "cacheWrite1hTokens")]
    cache_write_1h_tokens: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    entrypoint: String,
    usage: Map<String, Value>,
}

impl Receipt {
    fn new(inbound: &Inbound, target: &Target, status: StatusCode, headers: &HeaderMap, now: i64) -> Self {
        let attempt = uuid::Uuid::new_v4().to_string();
        let model = inbound.model.rsplit('/').next().unwrap_or_default().to_string();
        let request_id = ["request-id", "x-request-id"]
            .iter()
            .find_map(|n| headers.get(*n).and_then(|v| v.to_str().ok()))
            .unwrap_or_default();
        Self {
            source: "proxy",
            id: format!("proxy:{attempt}:{model}"),
            response_id: String::new(),
            upstream_request_id: request_id.to_string(),
            account_id: target.id().to_string(),
            credential: match target {
                Target::Key { name, .. } => name.clone(),
                Target::Plan { .. } => "plan".into(),
            },
            failed: !status.is_success(),
            status: status.as_u16(),
            timestamp: files::utc(now),
            harness: "claude",
            project: if inbound.entrypoint.is_empty() { "Proxy requests".into() } else { inbound.entrypoint.clone() },
            repository: None,
            session_id: inbound.session.clone().unwrap_or_else(|| format!("proxy:{attempt}")),
            model,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            cost_usd: 0.0,
            cost_known: false,
            service_tier: None,
            cache_write_1h_tokens: 0,
            entrypoint: inbound.entrypoint.clone(),
            usage: Map::new(),
        }
    }

    /// Takes the message id, model and usage from a JSON answer or one SSE event's data.
    fn read_body(&mut self, data: &[u8], event: bool) {
        let Ok(Value::Object(doc)) = serde_json::from_slice::<Value>(data) else { return };
        let message = if event { doc.get("message").and_then(Value::as_object).unwrap_or(&doc) } else { &doc };
        if let Some(id) = message.get("id").and_then(Value::as_str) {
            self.response_id = id.into();
        }
        if let Some(model) = message.get("model").and_then(Value::as_str) {
            self.model = model.into();
        }
        if let Some(Value::Object(usage)) = message.get("usage").or_else(|| doc.get("usage")) {
            self.usage.extend(usage.clone());
        }
    }

    fn write(&mut self, dir: &std::path::Path) {
        let n = |key: &str| self.usage.get(key).and_then(Value::as_i64).unwrap_or(0);
        self.input_tokens = n("input_tokens");
        self.output_tokens = n("output_tokens");
        self.cache_read_tokens = n("cache_read_input_tokens");
        self.cache_write_tokens = n("cache_creation_input_tokens");
        let nested = |path: &str| self.usage.pointer_get(path);
        self.reasoning_tokens = nested("/output_tokens_details/thinking_tokens").unwrap_or(0);
        self.cache_write_1h_tokens = nested("/cache_creation/ephemeral_1h_input_tokens").unwrap_or(0);
        self.service_tier =
            ["speed", "service_tier"].iter().find_map(|k| self.usage.get(*k).and_then(Value::as_str)).map(String::from);
        files::append(dir, "receipts.ndjson", self);
    }
}

trait PointerGet {
    fn pointer_get(&self, path: &str) -> Option<i64>;
}

impl PointerGet for Map<String, Value> {
    fn pointer_get(&self, path: &str) -> Option<i64> {
        let (first, rest) = path.trim_start_matches('/').split_once('/')?;
        self.get(first)?.pointer(&format!("/{rest}"))?.as_i64()
    }
}

/// The response body on its way to the client, read along the way for the receipt. The
/// receipt is written when the stream ends or the client goes away, whichever comes first.
struct Tap<S> {
    inner: S,
    receipt: Option<Receipt>,
    dir: std::path::PathBuf,
    sse: bool,
    line: Vec<u8>,
    /// The current SSE line isn't one the receipt needs.
    skipping: bool,
    body: Vec<u8>,
    stopped: bool,
}

/// SSE lines carrying a message's id and usage start with this, after `data: `.
const MESSAGE_EVENT: &[u8] = br#"{"type":"message_"#;
const MAX_JSON_BODY: usize = 32 << 20;

impl<S> Tap<S> {
    fn new(inner: S, receipt: Option<Receipt>, dir: std::path::PathBuf, headers: &HeaderMap) -> Self {
        let sse = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| t.starts_with("text/event-stream"));
        Self { inner, receipt, dir, sse, line: Vec::new(), skipping: false, body: Vec::new(), stopped: !sse }
    }

    fn feed(&mut self, chunk: &[u8]) {
        let Some(receipt) = self.receipt.as_mut() else { return };
        if !self.sse {
            if self.body.len() + chunk.len() <= MAX_JSON_BODY {
                self.body.extend_from_slice(chunk);
            }
            return;
        }
        for &byte in chunk {
            if byte == b'\n' {
                if !self.skipping
                    && let Some(data) = self.line.strip_prefix(b"data: ")
                {
                    if data.starts_with(br#"{"type":"message_stop""#) {
                        self.stopped = true;
                    }
                    receipt.read_body(data, true);
                }
                self.line.clear();
                self.skipping = false;
            } else if !self.skipping {
                self.line.push(byte);
                let probe = 6 + MESSAGE_EVENT.len();
                if self.line.len() == probe && !self.line.ends_with(MESSAGE_EVENT) {
                    self.skipping = true;
                    self.line.clear();
                }
            }
        }
    }
}

impl<S> Drop for Tap<S> {
    fn drop(&mut self) {
        let Some(mut receipt) = self.receipt.take() else { return };
        if !self.sse {
            receipt.read_body(&self.body, false);
        }
        if !self.stopped {
            receipt.failed = true;
        }
        receipt.write(&self.dir);
    }
}

impl<S: Stream<Item = reqwest::Result<Bytes>> + Unpin> Stream for Tap<S> {
    type Item = reqwest::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let next = Pin::new(&mut self.inner).poll_next(cx);
        if let Poll::Ready(Some(Ok(chunk))) = &next {
            let chunk = chunk.clone();
            self.feed(&chunk);
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderName;

    const NOW: i64 = 1_800_000_000_000;

    fn plan() -> Target {
        Target::Plan { id: "a".into(), token: "t".into() }
    }

    fn key() -> Target {
        Target::Key { id: "k".into(), name: "k".into(), key: "sk".into() }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(k, v)| (HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap()))
            .collect()
    }

    #[test]
    fn classifies_upstream_errors() {
        let none = HeaderMap::new();
        let credit = br#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API."}}"#;
        let identity = br#"{"type":"error","error":{"type":"invalid_request_error","message":"Identity verification is required"}}"#;
        let cases: [(&Target, u16, &[u8], Verdict); 9] = [
            (&key(), 400, credit, Verdict::Cool(NOW + MINUTE)),
            (&plan(), 400, identity, Verdict::Cool(NOW + 30 * MINUTE)),
            (&plan(), 400, b"prompt is too long", Verdict::Answer),
            (&plan(), 401, b"", Verdict::Renew),
            (&key(), 401, b"", Verdict::Cool(NOW + 30 * MINUTE)),
            (&plan(), 403, b"", Verdict::Cool(NOW + 30 * MINUTE)),
            (&plan(), 404, b"", Verdict::Answer),
            (&plan(), 529, b"", Verdict::Next),
            (&plan(), 500, b"", Verdict::Next),
        ];
        for (target, status, body, want) in cases {
            assert_eq!(classify(target, StatusCode::from_u16(status).unwrap(), &none, body, NOW), want, "{status}");
        }
    }

    #[test]
    fn a_rejected_window_cools_the_account_until_it_resets() {
        let reset = NOW / 1000 + 7200;
        let h = headers(&[
            ("retry-after", "30"),
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-reset", &reset.to_string()),
        ]);
        assert_eq!(classify(&plan(), StatusCode::TOO_MANY_REQUESTS, &h, b"", NOW), Verdict::Cool(reset * 1000));
        let plain = headers(&[("retry-after", "3")]);
        assert_eq!(classify(&key(), StatusCode::TOO_MANY_REQUESTS, &plain, b"", NOW), Verdict::Cool(NOW + 10_000));
    }

    #[test]
    fn reads_the_entrypoint_from_the_user_agent() {
        assert_eq!(entrypoint("claude-cli/2.1.318 (external, sdk-cli)"), "sdk-cli");
        assert_eq!(entrypoint("claude-cli/2.1.318 (external, hex, agent-sdk/0.2)"), "hex");
        assert_eq!(entrypoint("curl/8.0"), "");
    }

    #[test]
    fn finds_the_session_in_the_header_or_the_metadata() {
        let id = "51933568-42b8-4c8e-bc44-2bed00731a66";
        let h = headers(&[("x-claude-code-session-id", &id.to_uppercase())]);
        assert_eq!(inbound(&h, b"{}").session.as_deref(), Some(id));
        let body = json!({"model": "m", "metadata": {"user_id": json!({"session_id": id}).to_string()}}).to_string();
        assert_eq!(inbound(&HeaderMap::new(), body.as_bytes()).session.as_deref(), Some(id));
        assert_eq!(inbound(&headers(&[("x-claude-code-session-id", "nope")]), b"{}").session, None);
    }

    #[test]
    fn swaps_only_the_credential() {
        let client = headers(&[
            ("authorization", "Bearer client-token"),
            ("x-api-key", "proxy-key"),
            ("anthropic-beta", "claude-code-20250219,oauth-2025-04-20,effort-2025-11-24"),
            ("x-app", "cli"),
            ("content-length", "10"),
        ]);
        let out = upstream_headers(&client, &plan());
        assert_eq!(out["authorization"], "Bearer t");
        assert!(out.get("x-api-key").is_none());
        assert_eq!(out["anthropic-beta"], "claude-code-20250219,oauth-2025-04-20,effort-2025-11-24");
        assert_eq!(out["x-app"], "cli");

        let out = upstream_headers(&client, &key());
        assert_eq!(out["x-api-key"], "sk");
        assert!(out.get("authorization").is_none());
        assert_eq!(out["anthropic-beta"], "effort-2025-11-24");
    }

    #[test]
    fn the_tap_reads_usage_from_a_stream_split_anywhere() {
        let stream = concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-x","usage":{"input_tokens":10,"cache_read_input_tokens":5,"cache_creation":{"ephemeral_1h_input_tokens":3}}}}"#,
            "\n\nevent: content_block_delta\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            "\n\nevent: message_delta\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7,"output_tokens_details":{"thinking_tokens":2}}}"#,
            "\n\nevent: message_stop\n",
            r#"data: {"type":"message_stop"}"#,
            "\n\n"
        );
        let dir = tempfile::tempdir().unwrap();
        for split in [1, 7, 64, stream.len()] {
            let inbound = Inbound { model: "included/claude-x".into(), ..Inbound::default() };
            let receipt = Receipt::new(&inbound, &plan(), StatusCode::OK, &HeaderMap::new(), NOW);
            let sse = headers(&[("content-type", "text/event-stream; charset=utf-8")]);
            let mut tap = Tap::new((), Some(receipt), dir.path().to_path_buf(), &sse);
            for chunk in stream.as_bytes().chunks(split) {
                tap.feed(chunk);
            }
            drop(tap);
        }
        let text = std::fs::read_to_string(dir.path().join("receipts.ndjson")).unwrap();
        for line in text.lines() {
            let r: Value = serde_json::from_str(line).unwrap();
            assert_eq!(r["responseId"], "msg_1");
            assert_eq!(r["inputTokens"], 10);
            assert_eq!(r["outputTokens"], 7);
            assert_eq!(r["reasoningTokens"], 2);
            assert_eq!(r["cacheWrite1hTokens"], 3);
            assert_eq!(r["failed"], false);
        }
        assert_eq!(text.lines().count(), 4);
    }
}
