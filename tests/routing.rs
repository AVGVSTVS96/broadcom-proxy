//! The proxy end to end against a stand-in for Anthropic that answers per credential.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use broadcom_proxy::{
    app::{App, Endpoints},
    config::Config,
};
use serde_json::{Value, json};

const SESSION: &str = "51933568-42b8-4c8e-bc44-2bed00731a66";
const CREDIT_LOW: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API."}}"#;

/// What the stand-in saw: the credential each call came in on and the body it carried.
#[derive(Clone, Debug)]
struct Seen {
    credential: String,
    authorization: bool,
    body: Value,
}

#[derive(Default)]
struct Upstream {
    seen: Mutex<Vec<Seen>>,
    /// How each credential answers; anything not listed answers 200.
    answers: Mutex<HashMap<String, (u16, String, Vec<(String, String)>)>>,
}

impl Upstream {
    fn answer(&self, credential: &str, status: u16, body: &str) {
        self.answer_with(credential, status, body, &[]);
    }

    fn answer_with(&self, credential: &str, status: u16, body: &str, headers: &[(&str, &str)]) {
        let headers = headers.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect();
        self.answers.lock().unwrap().insert(credential.into(), (status, body.into(), headers));
    }

    fn served(&self) -> Vec<String> {
        std::mem::take(&mut *self.seen.lock().unwrap()).into_iter().map(|s| s.credential).collect()
    }
}

async fn anthropic(State(up): State<Arc<Upstream>>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let header = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let credential = match header("x-api-key") {
        key if !key.is_empty() => key,
        _ => header("authorization").trim_start_matches("Bearer ").to_string(),
    };
    up.seen.lock().unwrap().push(Seen {
        credential: credential.clone(),
        authorization: !header("authorization").is_empty(),
        body: serde_json::from_slice(&body).unwrap_or_default(),
    });
    if let Some((status, body, extra)) = up.answers.lock().unwrap().get(&credential).cloned() {
        let mut response = (StatusCode::from_u16(status).unwrap(), body).into_response();
        for (k, v) in extra {
            response.headers_mut().insert(axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        return response;
    }
    if uri.path().ends_with("/count_tokens") {
        return axum::Json(json!({"input_tokens": 42})).into_response();
    }
    axum::Json(json!({"id": "msg_1", "type": "message", "model": "claude-x", "content": [],
                      "usage": {"input_tokens": 3, "output_tokens": 2}}))
    .into_response()
}

struct Pool {
    proxy: String,
    up: Arc<Upstream>,
    dir: tempfile::TempDir,
    http: reqwest::Client,
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Plan accounts `a` and `b` (tokens `token-a`, `token-b`) and credit keys `key-hi` and `key-lo`.
async fn pool(keys: &[(&str, i64)]) -> Pool {
    let dir = tempfile::tempdir().unwrap();
    let at = |p: &str| dir.path().join(p);
    write(&at("client-keys"), "client-key\n");
    write(&at("management-key"), "admin-key\n");
    for name in ["a", "b"] {
        login(&at("auths"), name, &format!("token-{name}"));
    }
    let api_keys: Vec<Value> = keys
        .iter()
        .map(|(key, priority)| {
            write(&at(key), key);
            json!({"name": key, "keyFile": at(key), "prefix": "included", "priority": priority})
        })
        .collect();
    let config = json!({
        "listen": "127.0.0.1:0", "accountOwner": false, "clientKeysFile": at("client-keys"),
        "managementKeyFile": at("management-key"), "credentialsDir": at("auths"), "stateDir": at("state"),
        "apiKeys": api_keys,
    });
    write(&at("config.json"), &config.to_string());

    let up = Arc::new(Upstream::default());
    let upstream = serve(Router::new().fallback(anthropic).with_state(up.clone())).await;
    let (config, revision) = Config::load(&at("config.json")).unwrap();
    let ends = Endpoints { anthropic: upstream.clone(), claude_platform: upstream.clone(), claude_ai: upstream.clone(), openai_auth: upstream };
    let app = Arc::new(App::new(at("config.json"), config, revision, ends).unwrap());
    let proxy = serve(broadcom_proxy::router(app)).await;
    Pool { proxy, up, dir, http: reqwest::Client::new() }
}

fn login(dir: &Path, name: &str, token: &str) -> PathBuf {
    let path = dir.join(format!("claude-{name}.json"));
    let file = json!({"type": "claude", "access_token": token, "refresh_token": "r", "account_uuid": name,
                      "email": format!("{name}@example.com"), "expired": "2099-01-01T00:00:00Z"});
    write(&path, &file.to_string());
    path
}

impl Pool {
    async fn send(&self, model: &str, session: Option<&str>) -> (u16, Value) {
        let mut request = self
            .http
            .post(format!("{}/v1/messages?beta=true", self.proxy))
            .header("x-api-key", "client-key")
            .header("authorization", "Bearer the-clients-own-claude-token")
            .header("user-agent", "claude-cli/2.1.318 (external, sdk-cli)")
            .json(&json!({"model": model, "max_tokens": 1, "messages": [{"role": "user", "content": "hi"}]}));
        if let Some(s) = session {
            request = request.header("x-claude-code-session-id", s);
        }
        let response = request.send().await.unwrap();
        (response.status().as_u16(), response.json().await.unwrap_or_default())
    }

    fn file(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.path().join("state").join(name)).unwrap_or_default()
    }
}

#[tokio::test]
async fn credit_goes_first_by_priority_then_falls_through_to_the_plans() {
    let p = pool(&[("key-lo", 100), ("key-hi", 300)]).await;
    assert_eq!(p.send("claude-x", None).await.0, 200);
    assert_eq!(p.up.served(), ["key-hi"]);

    p.up.answer("key-hi", 400, CREDIT_LOW);
    assert_eq!(p.send("claude-x", None).await.0, 200);
    assert_eq!(p.up.served(), ["key-hi", "key-lo"]);

    p.up.answer("key-lo", 400, CREDIT_LOW);
    assert_eq!(p.send("claude-x", None).await.0, 200);
    let served = p.up.served();
    assert_eq!(served.len(), 2, "key-hi still cools, so only key-lo is tried before a plan: {served:?}");
    assert_eq!(served[0], "key-lo");
    assert!(served[1].starts_with("token-"));

    assert_eq!(p.send("claude-x", None).await.0, 200);
    assert_eq!(p.up.served().len(), 1, "with every key cooling the plan is reached directly");
}

#[tokio::test]
async fn a_request_error_on_a_funded_key_goes_back_to_the_client() {
    let p = pool(&[("key-hi", 300)]).await;
    p.up.answer("key-hi", 400, r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long"}}"#);
    let (status, body) = p.send("claude-x", None).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["message"], "prompt is too long");
    assert_eq!(p.up.served(), ["key-hi"]);
}

#[tokio::test]
async fn keys_get_the_key_and_plans_get_their_token_never_the_clients() {
    let p = pool(&[("key-hi", 300)]).await;
    p.send("claude-x", None).await;
    p.up.answer("key-hi", 400, CREDIT_LOW);
    p.send("claude-x", None).await;
    let seen = std::mem::take(&mut *p.up.seen.lock().unwrap());
    assert!(!seen[0].authorization, "a key request carried an Authorization header");
    assert!(seen[2].credential.starts_with("token-"));
    assert!(seen.iter().all(|s| s.credential != "the-clients-own-claude-token" && s.credential != "client-key"));
}

#[tokio::test]
async fn a_prefixed_model_uses_only_credit_and_goes_out_bare() {
    let p = pool(&[("key-hi", 300)]).await;
    assert_eq!(p.send("included/claude-x", None).await.0, 200);
    let seen = std::mem::take(&mut *p.up.seen.lock().unwrap());
    assert_eq!(seen[0].body["model"], "claude-x");

    p.up.answer("key-hi", 400, CREDIT_LOW);
    assert_eq!(p.send("included/claude-x", None).await.0, 400, "credit-only requests never reach a plan");
    assert_eq!(p.up.served(), ["key-hi"]);
}

#[tokio::test]
async fn an_overloaded_account_fails_over_without_cooling() {
    let p = pool(&[]).await;
    p.up.answer("token-a", 529, r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#);
    p.up.answer("token-b", 529, r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#);
    let (status, _) = p.send("claude-x", None).await;
    assert_eq!(status, 529, "the client sees the last answer when every account fails");
    assert_eq!(p.up.served().len(), 2);
    p.up.answers.lock().unwrap().clear();
    assert_eq!(p.send("claude-x", None).await.0, 200);
}

#[tokio::test]
async fn a_rejected_limit_cools_the_account_and_the_session_moves() {
    let p = pool(&[]).await;
    assert_eq!(p.send("claude-x", Some(SESSION)).await.0, 200);
    let first = p.up.served().remove(0);
    let other = if first == "token-a" { "token-b" } else { "token-a" };
    let reset = (jiff::Timestamp::now().as_second() + 3600).to_string();
    p.up.answer_with(&first, 429, "{}", &[
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-reset", &reset),
    ]);
    assert_eq!(p.send("claude-x", Some(SESSION)).await.0, 200);
    assert_eq!(p.up.served(), [first.clone(), other.to_string()]);
    assert_eq!(p.send("claude-x", Some(SESSION)).await.0, 200);
    assert_eq!(p.up.served(), [other.to_string()], "the cooling account is skipped");

    let seats: Vec<Value> = p.file("attribution.ndjson").lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(seats.len(), 2, "one start and one move: {seats:?}");
    assert_eq!(seats[0]["event"], "start");
    assert_eq!(seats[0]["entrypoint"], "sdk-cli");
    assert_eq!((seats[1]["event"].as_str(), seats[1]["cause"].as_str()), (Some("failure"), Some("limit")));
}

#[tokio::test]
async fn a_session_stays_on_its_account_until_it_nears_a_cutoff() {
    let p = pool(&[]).await;
    p.send("claude-x", Some(SESSION)).await;
    let first = p.up.served().remove(0);
    for _ in 0..3 {
        p.send("claude-x", Some(SESSION)).await;
    }
    assert_eq!(p.up.served(), vec![first.clone(); 3]);

    let reset = (jiff::Timestamp::now().as_second() + 3600).to_string();
    p.up.answer_with(&first, 200, r#"{"id":"m","usage":{}}"#, &[
        ("anthropic-ratelimit-unified-5h-utilization", "0.85"),
        ("anthropic-ratelimit-unified-5h-reset", &reset),
    ]);
    p.send("claude-x", Some(SESSION)).await;
    p.send("claude-x", Some(SESSION)).await;
    let served = p.up.served();
    assert_eq!(served[0], first);
    assert_ne!(served[1], first, "an Agent SDK session at 85% moves to the calm account");
    let last: Value = serde_json::from_str(p.file("attribution.ndjson").lines().last().unwrap()).unwrap();
    assert_eq!(last["cause"], "early");
    assert!(p.file("usage-history.ndjson").contains("\"fiveHour\":85"));
}

#[tokio::test]
async fn a_refused_token_is_reread_from_its_file_when_another_process_owns_the_accounts() {
    let p = pool(&[]).await;
    p.up.answer("token-a", 401, r#"{"type":"error","error":{"type":"authentication_error","message":"OAuth token has expired."}}"#);
    p.up.answer("token-b", 401, r#"{"type":"error","error":{"type":"authentication_error","message":"OAuth token has expired."}}"#);
    login(&p.dir.path().join("auths"), "a", "token-a2");
    assert_eq!(p.send("claude-x", None).await.0, 200);
    let served = p.up.served();
    assert!(served.contains(&"token-a2".to_string()), "{served:?}");
    assert!(!p.file("receipts.ndjson").is_empty());
}

#[tokio::test]
async fn receipts_record_every_attempt() {
    let p = pool(&[("key-hi", 300)]).await;
    p.up.answer("key-hi", 400, CREDIT_LOW);
    p.send("claude-x", Some(SESSION)).await;
    let receipts: Vec<Value> = p.file("receipts.ndjson").lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(receipts.len(), 2);
    assert_eq!((receipts[0]["failed"].as_bool(), receipts[0]["status"].as_u64()), (Some(true), Some(400)));
    assert!(receipts[0]["accountId"].as_str().unwrap().starts_with("claude:apikey:"));
    assert_eq!(receipts[1]["responseId"], "msg_1");
    assert_eq!(receipts[1]["inputTokens"], 3);
    assert_eq!(receipts[1]["sessionId"], SESSION);
    assert_eq!(receipts[1]["entrypoint"], "sdk-cli");
}

#[tokio::test]
async fn a_client_without_a_proxy_key_is_turned_away() {
    let p = pool(&[]).await;
    let response = p.http.post(format!("{}/v1/messages", p.proxy)).json(&json!({"model": "m"})).send().await.unwrap();
    assert_eq!(response.status(), 401);
    assert!(p.up.served().is_empty());
    let hello = p.http.head(format!("{}/api/hello", p.proxy)).send().await.unwrap();
    assert_eq!(hello.status(), 200);
}

#[tokio::test]
async fn count_tokens_and_models_route_like_messages() {
    let p = pool(&[("key-hi", 300)]).await;
    let count = p
        .http
        .post(format!("{}/v1/messages/count_tokens?beta=true", p.proxy))
        .header("x-api-key", "client-key")
        .json(&json!({"model": "claude-x", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(count.json::<Value>().await.unwrap()["input_tokens"], 42);
    let models = p.http.get(format!("{}/v1/models", p.proxy)).header("x-api-key", "client-key").send().await.unwrap();
    assert_eq!(models.status(), 200);
    assert_eq!(p.up.served(), ["key-hi", "key-hi"]);
    assert!(p.file("receipts.ndjson").is_empty(), "only messages leave receipts");
}
