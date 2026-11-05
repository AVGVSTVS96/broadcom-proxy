//! What only the account owner does, against a stand-in for Anthropic's and OpenAI's sign-in,
//! usage and reset endpoints: refreshing tokens, spending held resets and adding logins.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use broadcom_proxy::{
    app::{App, Endpoints},
    config::Config,
    files, oauth, sampler,
};
use serde_json::{Value, json};

#[derive(Default)]
struct Upstream {
    /// Refresh tokens spent, in order.
    refreshes: Mutex<Vec<String>>,
    /// Request ids of reset claims, in order.
    claims: Mutex<Vec<String>>,
    /// How long the token and claim endpoints take.
    delay: Mutex<Duration>,
    /// What the claim endpoint answers: status and body.
    claim_answer: Mutex<(u16, Value)>,
    /// Whether the held reset is spent, which zeroes the week and empties the grants.
    reset_spent: Mutex<bool>,
    device_polls: Mutex<u32>,
}

async fn upstream(State(up): State<Arc<Upstream>>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let bearer = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").trim_start_matches("Bearer ");
    let json_body: Value = serde_json::from_slice(&body).unwrap_or_default();
    let text = String::from_utf8_lossy(&body).into_owned();
    let form = |key: &str| text.split('&').find_map(|kv| kv.strip_prefix(&format!("{key}="))).unwrap_or("").to_string();
    let delay = *up.delay.lock().unwrap();
    let now = files::now_ms();
    let ok = |v: Value| axum::Json(v).into_response();
    match uri.path() {
        "/v1/oauth/token" if json_body["grant_type"] == "refresh_token" => {
            let spent = json_body["refresh_token"].as_str().unwrap_or("").to_string();
            let n = {
                let mut all = up.refreshes.lock().unwrap();
                all.push(spent.clone());
                all.len()
            };
            tokio::time::sleep(delay).await;
            if spent == "dead" {
                return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": "invalid_grant"}))).into_response();
            }
            ok(json!({"access_token": format!("fresh-{n}"), "refresh_token": format!("r-{n}"), "expires_in": 28800}))
        }
        "/v1/oauth/token" => {
            if json_body["code"] != "good-code" || json_body["code_verifier"].as_str().unwrap_or("").is_empty() {
                return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": "invalid_grant"}))).into_response();
            }
            ok(json!({"access_token": "token-c", "refresh_token": "r-c", "expires_in": 28800,
                      "account": {"uuid": "c", "email_address": "c@example.com"}, "organization": {"uuid": "org-c"}}))
        }
        "/v1/messages" if bearer.starts_with("stale") => (StatusCode::UNAUTHORIZED, "{}").into_response(),
        "/v1/messages" => {
            ok(json!({"id": "msg_1", "usage": {"input_tokens": 1, "output_tokens": 1}, "served": bearer}))
        }
        "/api/oauth/usage" => {
            let spent = *up.reset_spent.lock().unwrap();
            let week_ends = files::utc(now + 3 * 24 * 3_600_000);
            let grants = if spent {
                json!([])
            } else {
                json!([{"id": "g1", "resets_left": 1, "clears": ["seven_day"], "usable_now": true,
                        "ends_at": files::utc(now + 14 * 24 * 3_600_000)}])
            };
            let week = if spent { 0.0 } else { 100.0 };
            ok(json!({"five_hour": {"utilization": 0.0, "resets_at": null},
                      "seven_day": {"utilization": week, "resets_at": week_ends},
                      "cedar_ember": {"grants": if bearer == "token-a" { grants } else { json!([]) }}}))
        }
        path if path.ends_with("/reset_rate_limits") => {
            up.claims.lock().unwrap().push(json_body["request_id"].as_str().unwrap_or("").into());
            tokio::time::sleep(delay).await;
            let (status, answer) = up.claim_answer.lock().unwrap().clone();
            if answer["result"] == "reset" {
                *up.reset_spent.lock().unwrap() = true;
            }
            (StatusCode::from_u16(status).unwrap(), axum::Json(answer)).into_response()
        }
        "/api/accounts/deviceauth/usercode" => {
            ok(json!({"device_auth_id": "dev-1", "user_code": "ABCD", "interval": 1}))
        }
        "/api/accounts/deviceauth/token" => {
            let polls = {
                let mut n = up.device_polls.lock().unwrap();
                *n += 1;
                *n
            };
            if polls < 2 {
                return StatusCode::FORBIDDEN.into_response();
            }
            ok(json!({"authorization_code": "device-code", "code_verifier": "v"}))
        }
        "/oauth/token" if form("code") == "device-code" => {
            let claims = json!({"email": "d@example.com",
                                "https://api.openai.com/auth": {"chatgpt_account_id": "acc-d", "chatgpt_plan_type": "plus"}});
            let id_token = format!("x.{}.y", URL_SAFE_NO_PAD.encode(claims.to_string()));
            ok(
                json!({"access_token": "codex-token", "refresh_token": "codex-r", "id_token": id_token, "expires_in": 3600}),
            )
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

struct Owner {
    app: Arc<App>,
    proxy: String,
    up: Arc<Upstream>,
    ends: Endpoints,
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

/// Writes a Claude login for account `name`; `extra` fields go in as they are.
fn login(dir: &Path, name: &str, token: &str, extra: &Value) -> PathBuf {
    let path = dir.join("auths").join(format!("claude-{name}.json"));
    let mut file = json!({"type": "claude", "access_token": token, "refresh_token": format!("r-{name}"),
                          "account_uuid": name, "organization_uuid": format!("org-{name}"),
                          "email": format!("{name}@example.com"), "expired": "2099-01-01T00:00:00Z"});
    file.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    write(&path, &file.to_string());
    path
}

/// An owning proxy over whatever logins are already in `dir`.
async fn owner(dir: tempfile::TempDir) -> Owner {
    let at = |p: &str| dir.path().join(p);
    write(&at("client-keys"), "client-key\n");
    write(&at("management-key"), "admin-key\n");
    let config = json!({
        "listen": "127.0.0.1:0", "accountOwner": true, "clientKeysFile": at("client-keys"),
        "managementKeyFile": at("management-key"), "credentialsDir": at("auths"), "stateDir": at("state"),
    });
    write(&at("config.json"), &config.to_string());
    let up = Arc::new(Upstream::default());
    *up.claim_answer.lock().unwrap() = (200, json!({"result": "reset", "resets_left": 0}));
    let stand_in = serve(Router::new().fallback(upstream).with_state(up.clone())).await;
    let ends = Endpoints {
        anthropic: stand_in.clone(),
        claude_platform: stand_in.clone(),
        claude_ai: stand_in.clone(),
        openai_auth: stand_in,
    };
    let (config, revision) = Config::load(&at("config.json")).unwrap();
    let app = Arc::new(App::new(at("config.json"), config, revision, ends.clone()).unwrap());
    let proxy = serve(broadcom_proxy::router(app.clone())).await;
    Owner { app, proxy, up, ends, dir, http: reqwest::Client::new() }
}

impl Owner {
    /// The same directories and stand-in under a new process, as after a restart.
    async fn restart(&mut self) {
        let path = self.dir.path().join("config.json");
        let (config, revision) = Config::load(&path).unwrap();
        self.app = Arc::new(App::new(path, config, revision, self.ends.clone()).unwrap());
        self.proxy = serve(broadcom_proxy::router(self.app.clone())).await;
    }

    async fn send(&self) -> (u16, Value) {
        let response = self
            .http
            .post(format!("{}/v1/messages", self.proxy))
            .header("x-api-key", "client-key")
            .json(&json!({"model": "claude-x", "max_tokens": 1, "messages": []}))
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.json().await.unwrap_or_default())
    }

    async fn manage(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut request =
            self.http.request(method, format!("{}/v8/management{path}", self.proxy)).bearer_auth("admin-key");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        (response.status().as_u16(), response.json().await.unwrap_or_default())
    }
}

fn read(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[tokio::test]
async fn a_refused_token_is_refreshed_once_for_every_request_waiting_on_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = login(dir.path(), "a", "stale", &json!({"note": "kept"}));
    let o = owner(dir).await;
    *o.up.delay.lock().unwrap() = Duration::from_millis(200);

    let (first, second) = tokio::join!(o.send(), o.send());
    assert_eq!((first.0, second.0), (200, 200));
    assert_eq!(first.1["served"], "fresh-1");
    assert_eq!(second.1["served"], "fresh-1");
    assert_eq!(*o.up.refreshes.lock().unwrap(), ["r-a"]);
    let file = read(&path);
    assert_eq!((file["access_token"].as_str(), file["refresh_token"].as_str()), (Some("fresh-1"), Some("r-1")));
    assert_eq!(file["note"], "kept");
    assert_eq!(file["email"], "a@example.com");
}

#[tokio::test]
async fn a_refresh_lands_on_the_login_as_it_stands_and_never_brings_back_a_removed_one() {
    let dir = tempfile::tempdir().unwrap();
    let kept = login(dir.path(), "a", "token-a", &json!({"expired": "2000-01-01T00:00:00Z", "note": "kept"}));
    let removed = login(dir.path(), "b", "token-b", &json!({}));
    let o = owner(dir).await;
    *o.up.delay.lock().unwrap() = Duration::from_millis(300);

    let refreshing = tokio::spawn({
        let app = o.app.clone();
        async move { oauth::refresh_due(&app).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (status, _) = o
        .manage(reqwest::Method::PATCH, "/credentials/status", Some(json!({"name": "claude-a.json", "disabled": true})))
        .await;
    assert_eq!(status, 200);
    refreshing.await.unwrap();
    let file = read(&kept);
    assert_eq!(file["access_token"], "fresh-1");
    assert_eq!(file["disabled"], true);
    assert_eq!(file["note"], "kept");

    let refreshing = tokio::spawn({
        let app = o.app.clone();
        let path = removed.clone();
        async move { oauth::refresh_if(&app, &path, |_| true).await.map(|_| ()) }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (status, _) =
        o.manage(reqwest::Method::DELETE, "/credentials", Some(json!({"names": ["claude-b.json"]}))).await;
    assert_eq!(status, 200);
    assert!(refreshing.await.unwrap().is_err());
    assert!(!removed.exists());
    assert!(o.app.state().accounts.iter().all(|a| a.login.path != removed));
}

#[tokio::test]
async fn a_dead_refresh_token_signs_the_account_out_and_a_signed_out_account_still_gets_refreshed() {
    let dir = tempfile::tempdir().unwrap();
    login(dir.path(), "a", "token-a", &json!({"refresh_token": "dead", "expired": "2000-01-01T00:00:00Z"}));
    login(dir.path(), "b", "token-b", &json!({"expired": "2000-01-01T00:00:00Z"}));
    let o = owner(dir).await;
    o.app.state().accounts.iter_mut().find(|a| a.id() == "b").unwrap().needs_reauth = true;

    oauth::refresh_due(&o.app).await;
    let (_, list) = o.manage(reqwest::Method::GET, "/credentials", None).await;
    let file = |name: &str| list["files"].as_array().unwrap().iter().find(|f| f["name"] == name).unwrap().clone();
    assert_eq!(file("claude-a.json")["requires_sign_in"], true);
    assert_eq!(file("claude-a.json")["status"], "error");
    assert!(file("claude-b.json").get("requires_sign_in").is_none());
    assert_eq!(file("claude-b.json")["status"], "active");
}

#[tokio::test]
async fn a_claim_goes_out_once_and_keeps_its_request_id_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    login(dir.path(), "a", "token-a", &json!({}));
    login(dir.path(), "b", "token-b", &json!({}));
    let mut o = owner(dir).await;
    *o.up.claim_answer.lock().unwrap() = (503, Value::Null);

    sampler::sample(&o.app).await;
    sampler::sample(&o.app).await;
    let sent = o.up.claims.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "a claim Anthropic hasn't settled waits before going out again");
    let saved: Value = read(&o.dir.path().join("state/reset-claims.json"));
    assert_eq!(saved["a"]["requestId"], sent[0].as_str());

    *o.up.claim_answer.lock().unwrap() = (200, json!({"result": "reset", "resets_left": 0}));
    o.restart().await;
    sampler::sample(&o.app).await;
    let sent = o.up.claims.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1], "the retry after a restart reuses the request id");
    let a = o.app.state().accounts.iter().find(|a| a.id() == "a").map(|a| (a.claim.is_none(), a.reading.week)).unwrap();
    assert!(a.0);
    assert!(a.1.unwrap().used < f64::EPSILON);
}

#[tokio::test]
async fn the_dashboard_and_the_pool_spend_one_reset_between_them() {
    let dir = tempfile::tempdir().unwrap();
    login(dir.path(), "a", "token-a", &json!({}));
    login(dir.path(), "b", "token-b", &json!({}));
    let o = owner(dir).await;
    *o.up.delay.lock().unwrap() = Duration::from_millis(200);

    let ((status, answer), ()) =
        tokio::join!(o.manage(reqwest::Method::POST, "/pool/resets/a/spend", None), sampler::sample(&o.app));
    assert_eq!(status, 200);
    assert_eq!(o.up.claims.lock().unwrap().len(), 1, "{answer}");
    let (_, again) = o.manage(reqwest::Method::POST, "/pool/resets/a/spend", None).await;
    assert_eq!(again["spent"], false);
    assert_eq!(o.up.claims.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_claude_sign_in_from_another_device_writes_the_login_and_joins_the_pool() {
    let dir = tempfile::tempdir().unwrap();
    let o = owner(dir).await;
    let (status, started) =
        o.manage(reqwest::Method::GET, "/oauth/auth-url?provider=claude&browser=remote&is_webui=true", None).await;
    assert_eq!(status, 200);
    assert_eq!(started["flow"], "code");
    let state = started["state"].as_str().unwrap().to_string();
    let url = reqwest::Url::parse(started["url"].as_str().unwrap()).unwrap();
    let query = |k: &str| url.query_pairs().find(|(key, _)| key == k).map(|(_, v)| v.into_owned());
    assert_eq!(query("state").as_deref(), Some(state.as_str()));
    assert!(query("code_challenge").is_some());

    let status_of =
        async |o: &Owner| o.manage(reqwest::Method::GET, &format!("/oauth/status?state={state}"), None).await.1;
    assert_eq!(status_of(&o).await, json!({"status": "wait"}));
    let (refused, body) = o
        .manage(
            reqwest::Method::POST,
            "/oauth/callback",
            Some(json!({"provider": "claude", "code": "bad", "state": state})),
        )
        .await;
    assert_eq!(refused, 400);
    assert!(body["error"].as_str().unwrap().contains("invalid_grant"));
    let (status, body) = o
        .manage(
            reqwest::Method::POST,
            "/oauth/callback",
            Some(json!({"provider": "claude", "code": "good-code", "state": state})),
        )
        .await;
    assert_eq!((status, body), (200, json!({})));
    assert_eq!(status_of(&o).await, json!({"status": "ok"}));

    let (_, list) = o.manage(reqwest::Method::GET, "/credentials", None).await;
    let file = &list["files"][0];
    let mut keys: Vec<&str> = file.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["auth_index", "disabled", "email", "name", "provider", "status"]);
    assert_eq!((file["provider"].as_str(), file["email"].as_str()), (Some("claude"), Some("c@example.com")));
    let saved = read(&o.dir.path().join("auths").join(file["name"].as_str().unwrap()));
    assert_eq!((saved["access_token"].as_str(), saved["disabled"].as_bool()), (Some("token-c"), Some(false)));
    assert_eq!(o.send().await.1["served"], "token-c");

    let (_, cancelled) = o.manage(reqwest::Method::DELETE, &format!("/oauth/session?state={state}"), None).await;
    assert_eq!(cancelled, json!({}));
    assert_eq!(status_of(&o).await["status"], "error");
}

#[tokio::test]
async fn a_chatgpt_sign_in_with_a_device_code_writes_the_login() {
    let dir = tempfile::tempdir().unwrap();
    let o = owner(dir).await;
    let (_, started) = o.manage(reqwest::Method::GET, "/oauth/auth-url?provider=codex", None).await;
    assert_eq!((started["flow"].as_str(), started["user_code"].as_str()), (Some("device"), Some("ABCD")));
    let state = started["state"].as_str().unwrap();
    let mut status = Value::Null;
    for _ in 0..50 {
        status = o.manage(reqwest::Method::GET, &format!("/oauth/status?state={state}"), None).await.1;
        if status["status"] != "wait" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(status, json!({"status": "ok"}));
    let (_, list) = o.manage(reqwest::Method::GET, "/credentials", None).await;
    let file = &list["files"][0];
    assert_eq!((file["provider"].as_str(), file["email"].as_str()), (Some("codex"), Some("d@example.com")));
    let saved = read(&o.dir.path().join("auths").join(file["name"].as_str().unwrap()));
    assert_eq!((saved["account_id"].as_str(), saved["access_token"].as_str()), (Some("acc-d"), Some("codex-token")));
}
