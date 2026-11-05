//! Keeping OAuth logins signed in, and adding new ones from any device: Claude by pasting
//! the code its hosted page shows, ChatGPT by entering a device code.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    accounts::{Account, Login},
    app::{App, Target},
    config::hex,
    files,
    pool::{HOUR, MINUTE, SECOND},
};

const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_SCOPE: &str = "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const CLAUDE_PASTE_PAGE: &str = "https://platform.claude.com/oauth/code/callback";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_DEVICE_PAGE: &str = "https://auth.openai.com/codex/device";
const CODEX_DEVICE_CALLBACK: &str = "https://auth.openai.com/deviceauth/callback";
/// Refresh a token this long before it expires.
const REFRESH_AHEAD: i64 = 4 * HOUR;
const SIGN_IN_FOR: i64 = 15 * MINUTE;

#[derive(Debug)]
pub enum RefreshError {
    /// The refresh token is gone for good; the account needs a new sign-in.
    SignedOut(String),
    Failed(String),
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefreshError::SignedOut(why) => write!(f, "signed out: {why}"),
            RefreshError::Failed(why) => write!(f, "{why}"),
        }
    }
}

/// A fresh token for an account whose token Anthropic just refused, or None. The account
/// owner refreshes it; any other process rereads the file the owner keeps current.
pub async fn renew(app: &App, account: &str, failed: &Target) -> Option<String> {
    let Target::Plan { token: refused, .. } = failed else { return None };
    let current = |app: &App| {
        let state = app.state();
        state.accounts.iter().find(|a| a.id() == account).map(|a| a.login.clone())
    };
    if !app.config.account_owner {
        app.rescan();
        return current(app).map(|l| l.get("access_token").to_string()).filter(|t| t != refused);
    }
    let path = current(app)?.path;
    match refresh_if(app, &path, |l| l.get("access_token") == refused).await {
        Ok(login) => Some(login.get("access_token").to_string()),
        Err(err) => {
            eprintln!("refresh {account}: {err}");
            None
        }
    }
}

/// Refreshes every login that expires within four hours. Only the account owner runs this.
pub async fn refresh_due(app: &App) {
    let due = |l: &Login| !l.disabled() && l.expires_at().is_none_or(|at| at - files::now_ms() <= REFRESH_AHEAD);
    let paths: Vec<PathBuf> = {
        let state = app.state();
        let claude = state.accounts.iter().filter(|a| !a.needs_reauth).map(|a| &a.login);
        claude.chain(state.others.iter()).filter(|l| due(l)).map(|l| l.path.clone()).collect()
    };
    for path in paths {
        if let Err(err) = refresh_if(app, &path, due).await {
            eprintln!("refresh {}: {err}", path.display());
        }
    }
}

/// Refreshes the login at `path` if `needed` still says so once no other refresh is running,
/// so a refresh token is never spent twice. Returns the login as it stands after.
pub async fn refresh_if(app: &App, path: &Path, needed: impl FnOnce(&Login) -> bool) -> Result<Login, RefreshError> {
    let _one = app.refreshing.lock().await;
    let login = {
        let state = app.state();
        state.accounts.iter().map(|a| &a.login).chain(state.others.iter()).find(|l| l.path == path).cloned()
    };
    let Some(mut login) = login else { return Err(RefreshError::Failed("the login is gone".into())) };
    if !needed(&login) {
        return Ok(login);
    }
    match refresh(app, &mut login).await {
        Ok(()) => {
            store(app, login.clone());
            Ok(login)
        }
        Err(err) => {
            if let RefreshError::SignedOut(_) = err {
                mark_signed_out(app, &login);
            }
            Err(err)
        }
    }
}

/// Puts a refreshed or new login in its place, on disk and in the pool.
fn store(app: &App, mut login: Login) {
    if let Err(err) = login.save() {
        eprintln!("save {}: {err}", login.path.display());
        return;
    }
    let mut state = app.state();
    if login.kind() == "claude" {
        match state.accounts.iter_mut().find(|a| a.login.path == login.path) {
            Some(a) => {
                a.login = login;
                a.needs_reauth = false;
                a.cool_until = 0;
            }
            None => state.accounts.push(Account::new(login)),
        }
    } else {
        state.others.retain(|l| l.path != login.path);
        state.others.push(login);
    }
    drop(state);
    app.changed.notify_one();
}

fn mark_signed_out(app: &App, login: &Login) {
    let mut state = app.state();
    if let Some(a) = state.accounts.iter_mut().find(|a| a.login.path == login.path) {
        a.needs_reauth = true;
    }
    drop(state);
    app.changed.notify_one();
}

/// Trades the login's refresh token for a new access token and writes the result into it.
async fn refresh(app: &App, login: &mut Login) -> Result<(), RefreshError> {
    let now = files::now_ms();
    let refresh_token = login.get("refresh_token").to_string();
    if refresh_token.is_empty() {
        return Err(RefreshError::SignedOut("no refresh token".into()));
    }
    let request = if login.kind() == "codex" {
        app.http.post(format!("{}/oauth/token", app.ends.openai_auth)).header("accept", "application/json").form(&[
            ("client_id", CODEX_CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh_token),
            ("scope", "openid profile email"),
        ])
    } else {
        claude_token_request(app).json(&json!({"client_id": CLAUDE_CLIENT_ID, "grant_type": "refresh_token", "refresh_token": refresh_token, "scope": CLAUDE_SCOPE}))
    };
    let response =
        request.timeout(Duration::from_secs(30)).send().await.map_err(|e| RefreshError::Failed(e.to_string()))?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or_default();
    if !status.is_success() {
        let reason = body.pointer("/error").map(Value::to_string).unwrap_or_default();
        return Err(if reason.contains("invalid_grant") {
            RefreshError::SignedOut(reason)
        } else {
            RefreshError::Failed(format!("{status} {reason}"))
        });
    }
    let tokens: Tokens = serde_json::from_value(body).map_err(|e| RefreshError::Failed(e.to_string()))?;
    tokens.apply(login, now);
    if login.kind() == "claude" {
        fill_profile(app, login).await;
    }
    Ok(())
}

fn claude_token_request(app: &App) -> reqwest::RequestBuilder {
    // Anthropic's token endpoint sees Claude Code's own HTTP client on this client id.
    app.http
        .post(format!("{}/v1/oauth/token", app.ends.claude_platform))
        .header("accept", "application/json, text/plain, */*")
        .header("user-agent", "axios/1.15.2")
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<i64>,
    refresh_token_expires_in: Option<i64>,
    #[serde(default)]
    account: Option<Value>,
    #[serde(default)]
    organization: Option<Value>,
}

impl Tokens {
    fn apply(self, login: &mut Login, now: i64) {
        login.set("access_token", self.access_token);
        if let Some(t) = self.refresh_token.filter(|t| !t.is_empty()) {
            login.set("refresh_token", t);
        }
        if let Some(t) = self.id_token.filter(|t| !t.is_empty()) {
            login.set("id_token", t);
        }
        if let Some(s) = self.expires_in {
            login.set("expired", files::local(now + s * SECOND));
        }
        if let Some(s) = self.refresh_token_expires_in.filter(|&s| s > 0) {
            login.set("refresh_expired", files::local(now + s * SECOND));
        }
        let text = |v: &Option<Value>, key: &str| {
            v.as_ref().and_then(|v| v.get(key)).and_then(Value::as_str).map(String::from)
        };
        for (field, value) in [
            ("account_uuid", text(&self.account, "uuid")),
            ("email", text(&self.account, "email_address")),
            ("organization_uuid", text(&self.organization, "uuid")),
            ("organization_name", text(&self.organization, "name")),
        ] {
            if let Some(v) = value.filter(|v| !v.is_empty()) {
                login.set(field, v);
            }
        }
        login.set("last_refresh", files::local(now));
    }
}

/// Fills a Claude login's account, email and organization from its profile.
async fn fill_profile(app: &App, login: &mut Login) {
    let token = login.get("access_token").to_string();
    let Ok(response) = crate::sampler::anthropic_get(app, &token, "/api/oauth/profile").await else { return };
    let Ok(profile) = response.json::<Value>().await else { return };
    let text = |p: &str| profile.pointer(p).and_then(Value::as_str).filter(|v| !v.is_empty()).map(String::from);
    for (field, pointer) in [
        ("account_uuid", "/account/uuid"),
        ("email", "/account/email"),
        ("organization_uuid", "/organization/uuid"),
        ("organization_name", "/organization/name"),
    ] {
        if let Some(v) = text(pointer) {
            login.set(field, v);
        }
    }
}

/// Sign-ins started from the dashboard, by state.
#[derive(Default)]
pub struct SignIns(Mutex<HashMap<String, SignIn>>);

#[derive(Clone, Debug)]
struct SignIn {
    provider: Provider,
    verifier: String,
    started: i64,
    status: Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
}

/// Where a sign-in stands.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Status {
    Pending,
    Ok { credential: String, email: String },
    Error { error: String },
}

/// What the dashboard shows to start a sign-in.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Started {
    state: String,
    url: String,
    /// `code`: open the url, sign in, paste the code shown. `device`: open the url and enter `user_code`.
    flow: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_code: Option<String>,
}

impl SignIns {
    fn put(&self, state: &str, sign_in: SignIn) {
        let mut all = self.0.lock().expect("sign-ins lock");
        let now = files::now_ms();
        all.retain(|_, s| now - s.started < SIGN_IN_FOR);
        all.insert(state.into(), sign_in);
    }

    fn get(&self, state: &str) -> Option<SignIn> {
        self.0.lock().expect("sign-ins lock").get(state).cloned()
    }

    fn finish(&self, state: &str, status: Status) {
        if let Some(s) = self.0.lock().expect("sign-ins lock").get_mut(state) {
            s.status = status;
        }
    }

    pub fn status(&self, state: &str) -> Status {
        self.get(state).map_or(Status::Error { error: "unknown or expired sign-in".into() }, |s| s.status)
    }
}

fn random(bytes: usize) -> String {
    let mut buf = vec![0; bytes];
    rand::rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

pub async fn start(app: &std::sync::Arc<App>, provider: Provider) -> anyhow::Result<Started> {
    let state = random(24);
    let now = files::now_ms();
    match provider {
        Provider::Claude => {
            let verifier = random(32);
            let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
            let url = reqwest::Url::parse_with_params(
                &format!("{}/oauth/authorize", app.ends.claude_ai),
                [
                    ("code", "true"),
                    ("client_id", CLAUDE_CLIENT_ID),
                    ("response_type", "code"),
                    ("redirect_uri", CLAUDE_PASTE_PAGE),
                    ("scope", CLAUDE_SCOPE),
                    ("code_challenge", &challenge),
                    ("code_challenge_method", "S256"),
                    ("state", &state),
                ],
            )?;
            app.sign_ins.put(&state, SignIn { provider, verifier, started: now, status: Status::Pending });
            Ok(Started { state, url: url.into(), flow: "code", user_code: None })
        }
        Provider::Codex => {
            let answer: Value = app
                .http
                .post(format!("{}/api/accounts/deviceauth/usercode", app.ends.openai_auth))
                .json(&json!({"client_id": CODEX_CLIENT_ID}))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let text = |k: &str| answer.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let (device, code) = (
                text("device_auth_id"),
                [text("user_code"), text("usercode")].into_iter().find(|c| !c.is_empty()).unwrap_or_default(),
            );
            anyhow::ensure!(!device.is_empty() && !code.is_empty(), "OpenAI gave no device code");
            let every = answer
                .get("interval")
                .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
                .unwrap_or(5)
                .max(1);
            app.sign_ins
                .put(&state, SignIn { provider, verifier: String::new(), started: now, status: Status::Pending });
            tokio::spawn(wait_for_device(app.clone(), state.clone(), device, code.clone(), every));
            Ok(Started { state, url: CODEX_DEVICE_PAGE.into(), flow: "device", user_code: Some(code) })
        }
    }
}

/// Trades the code Claude's page showed for tokens and adds the account to the pool.
pub async fn finish_claude(app: &App, state: &str, pasted: &str) -> Status {
    let Some(sign_in) = app.sign_ins.get(state).filter(|s| s.provider == Provider::Claude) else {
        return Status::Error { error: "unknown or expired sign-in".into() };
    };
    let (code, pasted_state) = pasted.trim().split_once('#').unwrap_or((pasted.trim(), state));
    let body = json!({
        "grant_type": "authorization_code", "code": code, "redirect_uri": CLAUDE_PASTE_PAGE,
        "client_id": CLAUDE_CLIENT_ID, "code_verifier": sign_in.verifier, "state": pasted_state,
    });
    let status = match exchange(app, claude_token_request(app).json(&body), "claude").await {
        Ok(status) => status,
        Err(err) => Status::Error { error: err.to_string() },
    };
    app.sign_ins.finish(state, status.clone());
    status
}

async fn wait_for_device(app: std::sync::Arc<App>, state: String, device: String, code: String, every: i64) {
    let deadline = files::now_ms() + SIGN_IN_FOR;
    let status = loop {
        tokio::time::sleep(Duration::from_secs(every as u64)).await;
        if files::now_ms() > deadline {
            break Status::Error { error: "The device code expired before it was entered.".into() };
        }
        let poll = app
            .http
            .post(format!("{}/api/accounts/deviceauth/token", app.ends.openai_auth))
            .json(&json!({"device_auth_id": device, "user_code": code}))
            .send()
            .await;
        let Ok(response) = poll else { continue };
        if matches!(response.status().as_u16(), 403 | 404) {
            continue;
        }
        let Ok(answer) = response.error_for_status().map(reqwest::Response::json::<Value>) else {
            break Status::Error { error: "OpenAI refused the device sign-in.".into() };
        };
        let Ok(answer) = answer.await else { break Status::Error { error: "OpenAI's answer was unreadable.".into() } };
        let text = |k: &str| answer.get(k).and_then(Value::as_str).unwrap_or("");
        let request = app.http.post(format!("{}/oauth/token", app.ends.openai_auth)).form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CODEX_CLIENT_ID),
            ("code", text("authorization_code")),
            ("redirect_uri", CODEX_DEVICE_CALLBACK),
            ("code_verifier", text("code_verifier")),
        ]);
        break exchange(&app, request, "codex").await.unwrap_or_else(|err| Status::Error { error: err.to_string() });
    };
    app.sign_ins.finish(&state, status);
}

/// Sends a code exchange, writes the login it returns and adds it to the pool.
async fn exchange(app: &App, request: reqwest::RequestBuilder, kind: &str) -> anyhow::Result<Status> {
    let now = files::now_ms();
    let response = request.timeout(Duration::from_secs(30)).send().await?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "The code exchange failed ({status}): {}",
        body.pointer("/error").map(Value::to_string).unwrap_or_default()
    );
    let tokens: Tokens = serde_json::from_value(body)?;
    let mut login = Login { path: PathBuf::new(), fields: serde_json::Map::new(), modified: None };
    login.set("type", kind);
    tokens.apply(&mut login, now);
    let name = if kind == "claude" {
        fill_profile(app, &mut login).await;
        claude_file_name(&login)
    } else {
        codex_fields(&mut login);
        codex_file_name(&login)
    };
    anyhow::ensure!(!login.get("email").is_empty(), "The sign-in returned no email.");
    let existing = {
        let state = app.state();
        let account = login.get("account_uuid");
        state
            .accounts
            .iter()
            .map(|a| &a.login)
            .chain(state.others.iter())
            .find(|l| l.kind() == kind && !account.is_empty() && l.get("account_uuid") == account || l.name() == name)
            .cloned()
    };
    login.path = app.config.credentials_dir.join(&name);
    if let Some(old) = existing {
        let mut fields = old.fields;
        fields.extend(login.fields);
        login = Login { path: old.path, fields, modified: old.modified };
    } else if kind == "claude" {
        let mut device = [0u8; 32];
        rand::rng().fill_bytes(&mut device);
        login.set("claude_device_ids", json!([hex(&device)]));
        login.set("disabled", false);
    }
    let email = login.get("email").to_string();
    let credential = login.name();
    store(app, login);
    Ok(Status::Ok { credential, email })
}

/// `claude-<hash of org>-<email>.json`, as CLIProxyAPI names Claude logins.
fn claude_file_name(login: &Login) -> String {
    let identity = [login.get("organization_uuid"), login.get("account_uuid")].into_iter().find(|s| !s.is_empty());
    match identity {
        Some(id) => format!("claude-{}-{}.json", &hex(&Sha256::digest(id.as_bytes()))[..8], login.get("email")),
        None => format!("claude-{}.json", login.get("email")),
    }
}

/// Reads the ChatGPT account, email and plan from the id token.
fn codex_fields(login: &mut Login) {
    let claims = login
        .get("id_token")
        .split('.')
        .nth(1)
        .and_then(|p| URL_SAFE_NO_PAD.decode(p.trim_end_matches('=')).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or_default();
    let text = |p: &str| claims.pointer(p).and_then(Value::as_str).unwrap_or("").to_string();
    login.set("email", text("/email"));
    login.set("account_id", text("/https:~1~1api.openai.com~1auth/chatgpt_account_id"));
    let plan = text("/https:~1~1api.openai.com~1auth/chatgpt_plan_type");
    login.set("plan_type", if plan.is_empty() { "free".into() } else { plan });
}

/// `codex-<hash of account>-<email>-<plan>.json`, as CLIProxyAPI names ChatGPT logins.
fn codex_file_name(login: &Login) -> String {
    let plan: String =
        login.get("plan_type").chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_lowercase();
    match login.get("account_id") {
        "" => format!("codex-{}-{plan}.json", login.get("email")),
        id => format!("codex-{}-{}-{plan}.json", &hex(&Sha256::digest(id.as_bytes()))[..8], login.get("email")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login(fields: Value) -> Login {
        Login { path: PathBuf::new(), fields: serde_json::from_value(fields).unwrap(), modified: None }
    }

    #[test]
    fn names_files_the_way_cliproxyapi_does() {
        let claude = login(json!({"email": "a@example.com", "organization_uuid": "org"}));
        let hash = &hex(&Sha256::digest(b"org"))[..8];
        assert_eq!(claude_file_name(&claude), format!("claude-{hash}-a@example.com.json"));
        let codex = login(json!({"email": "a@example.com", "account_id": "acc", "plan_type": "Pro"}));
        let hash = &hex(&Sha256::digest(b"acc"))[..8];
        assert_eq!(codex_file_name(&codex), format!("codex-{hash}-a@example.com-pro.json"));
    }

    #[test]
    fn a_refresh_keeps_the_old_refresh_token_when_none_comes_back() {
        let mut l = login(json!({"type": "claude", "access_token": "old", "refresh_token": "keep", "other": 1}));
        let tokens: Tokens = serde_json::from_value(json!({"access_token": "new", "expires_in": 28800})).unwrap();
        tokens.apply(&mut l, 1_800_000_000_000);
        assert_eq!(l.get("access_token"), "new");
        assert_eq!(l.get("refresh_token"), "keep");
        assert_eq!(l.fields["other"], 1);
        assert_eq!(l.expires_at(), Some(1_800_000_000_000 + 28_800_000));
    }

    #[test]
    fn reads_the_chatgpt_account_from_the_id_token() {
        let claims = json!({"email": "a@example.com", "https://api.openai.com/auth": {"chatgpt_account_id": "acc", "chatgpt_plan_type": "plus"}});
        let token = format!("x.{}.y", URL_SAFE_NO_PAD.encode(claims.to_string()));
        let mut l = login(json!({"id_token": token}));
        codex_fields(&mut l);
        assert_eq!((l.get("email"), l.get("account_id"), l.get("plan_type")), ("a@example.com", "acc", "plus"));
    }
}
