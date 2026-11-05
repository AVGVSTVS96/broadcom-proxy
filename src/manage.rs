//! The management API under /v8/management: the live stream BROADCOM reads, account
//! history, policy, credentials, sign-in and held resets. Shapes match BROADCOM's
//! `lib/proxy/schema.ts`.

use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    app::App,
    config::Config,
    files,
    oauth::{self, Provider},
    pool::{self, FIVE_HOURS, Policy, Reading, Row, WEEK, Window},
    sampler,
};

pub fn router(app: Arc<App>) -> Router<Arc<App>> {
    Router::new()
        .route("/deck/events", get(events))
        .route("/deck/accounts", get(accounts))
        .route("/deck/accounts/policy", post(policy))
        .route("/credentials", get(credentials).delete(delete_credentials))
        .route("/credentials/status", patch(credential_status))
        .route("/credentials/refresh", post(refresh_credential))
        .route("/oauth/auth-url", get(sign_in_start))
        .route("/oauth/status", get(sign_in_status))
        .route("/oauth/callback", post(sign_in_callback))
        .route("/oauth/session", delete(sign_in_cancel))
        .route("/pool/resets/{account}/spend", post(spend_reset))
        .layer(middleware::from_fn_with_state(app, authorize))
}

async fn authorize(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let bearer =
        request.headers().get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    if bearer.map(str::trim) != Some(app.management_key.as_str()) {
        return fail(StatusCode::UNAUTHORIZED, "This needs the management key.");
    }
    next.run(request).await
}

fn fail(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}

fn no_store(value: Value) -> Response {
    ([("cache-control", "no-store")], Json(value)).into_response()
}

fn done() -> Response {
    no_store(json!({}))
}

fn owner_only(app: &App) -> Option<Response> {
    (!app.config.account_owner)
        .then(|| fail(StatusCode::CONFLICT, "This proxy doesn't own the accounts, so it leaves their logins alone."))
}

/// Keeps the live stream current after anything changes, with a heartbeat every 20 seconds.
/// The sampler's minute ticks count as changes, so counts that age stay current.
pub async fn run_live(app: Arc<App>) {
    let mut beat = tokio::time::interval(Duration::from_secs(20));
    loop {
        tokio::select! {
            () = app.changed.notified() => tokio::time::sleep(Duration::from_millis(250)).await,
            _ = beat.tick() => {
                app.live.heartbeat();
                continue;
            }
        }
        let (policy, revision) = {
            let state = app.state();
            (state.policy, state.revision.clone())
        };
        let (accounts, pool) = snapshot(&app, policy, &revision);
        let mut entries: Vec<(String, Value)> =
            accounts.into_iter().map(|a| (format!("a:{}", a["id"].as_str().unwrap_or("")), a)).collect();
        entries.push(("pool".into(), pool));
        app.live.publish(&entries);
    }
}

async fn events(State(app): State<Arc<App>>) -> Response {
    app.changed.notify_one();
    let rx = app.live.subscribe();
    let stream =
        futures_util::stream::unfold(
            rx,
            |mut rx| async move { rx.recv().await.map(|b| (Ok::<_, std::io::Error>(b), rx)) },
        );
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .expect("static headers")
}

/// Every account as the dashboard shows it, in rank order with turned-off ones last, and
/// where new sessions go.
pub fn snapshot(app: &App, policy: Policy, revision: &str) -> (Vec<Value>, Value) {
    let spent = sampler::resets_spent(app);
    let now = files::now_ms();
    let plans: HashMap<String, String> =
        sampler::reset_plans(app, now).into_iter().map(|(id, p)| (id, p.plan)).collect();
    let state = app.state();
    let factor = app.config.agent_sdk_factor;
    let (enabled, disabled): (Vec<_>, Vec<_>) = state.accounts.iter().partition(|a| !a.login.disabled());
    let views = state.views(&enabled, now, factor);
    let mut ranked: Vec<(&crate::accounts::Account, Row, Reading)> = pool::rank(&views, &policy, now, 1.0)
        .into_iter()
        .map(|r| (enabled[r.index], r.clone(), views[r.index].reading))
        .collect();
    for a in &disabled {
        let views = state.views(&[a], now, factor);
        let row = pool::rank(&views, &policy, now, 1.0).remove(0);
        ranked.push((a, row, views[0].reading));
    }

    let accounts: Vec<Value> = ranked
        .iter()
        .enumerate()
        .map(|(rank, (a, row, reading))| {
            let id = a.id();
            let seated = state.seats.values().filter(|s| s.account == id).count();
            let window = |w: Option<Window>, length: i64, cutoff: f64| match w {
                None => json!({"used": null, "cutoff": cutoff, "resetsAt": null, "sampledAt": null, "elapsed": null}),
                Some(w) => {
                    let elapsed = (w.resets_at != 0)
                        .then(|| ((1.0 - (w.resets_at - reading.at) as f64 / length as f64).clamp(0.0, 1.0) * 1000.0).round() / 1000.0);
                    json!({"used": w.used, "cutoff": cutoff, "resetsAt": (w.resets_at != 0).then_some(w.resets_at), "sampledAt": reading.at, "elapsed": elapsed})
                }
            };
            let mut account = json!({
                "id": id,
                "label": a.label(),
                "plan": plan_name(a.plan.as_ref().and_then(|p| p.tier.as_deref())),
                "since": a.plan.as_ref().and_then(|p| p.since),
                "signInUntil": a.login.sign_in_until(),
                "rank": rank + 1,
                "standing": standing(a.login.disabled(), row, rank),
                "reasons": reasons(a.login.disabled(), row, &policy),
                "fiveHour": window(reading.five_hour, FIVE_HOURS, policy.session_cutoff),
                "week": window(reading.week, WEEK, policy.weekly_cutoff),
                "working": row.working,
                "idle": seated.saturating_sub(row.working),
                "heldResets": a.resets.iter().map(|r| json!({"kind": r.kind, "left": r.left, "expiresAt": r.expires_at})).collect::<Vec<_>>(),
                "resetsSpent": spent.get(&id).cloned().unwrap_or_default(),
                "cloud": a.cloud,
                "atLimit": a.at_limit,
            });
            if let Some(plan) = plans.get(&id) {
                account["resetPlan"] = plan.clone().into();
            }
            account
        })
        .collect();

    let first = ranked.first().filter(|(a, _, _)| !a.login.disabled());
    let decision = match first {
        Some((a, row, _)) if row.usable => format!("New sessions go to {}.", a.label()),
        Some((a, row, _)) if row.open => {
            format!("Every account is past a cutoff; new sessions go to {}, which has the most room left.", a.label())
        }
        _ => "No Claude account can take a new session.".into(),
    };
    let pool = json!({
        "decision": decision,
        "next": first.filter(|(_, row, _)| row.usable).map(|(a, _, _)| a.id()),
        "notes": [],
        "order": ranked.iter().map(|(a, _, _)| a.id()).collect::<Vec<_>>(),
        "policy": policy,
        "revision": revision,
    });
    (accounts, pool)
}

fn plan_name(tier: Option<&str>) -> String {
    let Some(tier) = tier.filter(|t| !t.is_empty()) else { return String::new() };
    let t = tier.to_lowercase();
    let near = |a: &str, b: &str| {
        t.find(a).is_some_and(|i| t[i + a.len()..].trim_start_matches(|c: char| !c.is_alphanumeric()).starts_with(b))
    };
    if near("max", "20x") {
        "Max 20x".into()
    } else if near("max", "5x") {
        "Max 5x".into()
    } else if t.contains("max") {
        "Max".into()
    } else if t.contains("pro") {
        "Pro".into()
    } else {
        tier.into()
    }
}

fn standing(disabled: bool, row: &Row, rank: usize) -> &'static str {
    match () {
        () if row.needs_reauth || disabled => "signed-out",
        () if rank == 0 && row.usable => "next",
        () if row.usable && row.comfortable => "open",
        () if row.usable || row.open => "tight",
        () => "limited",
    }
}

fn reasons(disabled: bool, row: &Row, policy: &Policy) -> Vec<String> {
    let number = |v: f64| Value::from(v).to_string();
    if row.needs_reauth {
        return vec!["Signed out: sign in again.".into()];
    }
    if disabled {
        return vec!["Turned off.".into()];
    }
    if !row.usable {
        return vec![if row.five >= policy.session_cutoff {
            format!("Past the {}% five-hour cutoff.", number(policy.session_cutoff))
        } else {
            format!("Past the {}% weekly cutoff.", number(policy.weekly_cutoff))
        }];
    }
    let mut out = Vec::new();
    if row.holds_reset {
        out.push("Holds a limit reset, so its week gets used first and the reset clears a whole one.".into());
    }
    out.push(
        if row.comfortable { "Has room for another working session." } else { "Crowded for its five-hour room." }
            .into(),
    );
    out
}

/// The last week of limit readings and the last day of session moves.
async fn accounts(State(app): State<Arc<App>>) -> Response {
    let now = files::now_ms();
    let read = |name: &str| std::fs::read_to_string(app.state_dir().join(name)).unwrap_or_default();
    let mut history: Vec<Value> = Vec::new();
    let mut at: HashMap<(String, &str), usize> = HashMap::new();
    for line in read("usage-history.ndjson").lines() {
        let Ok(r) = serde_json::from_str::<Value>(line) else { continue };
        let (Some(account), Some(when)) = (r["account"].as_str(), r["at"].as_f64()) else { continue };
        if when < (now - WEEK) as f64 {
            continue;
        }
        for kind in ["fiveHour", "week"] {
            let Some(used) = r[kind].as_f64() else { continue };
            let i = *at.entry((account.to_string(), kind)).or_insert_with(|| {
                history.push(json!({"account": account, "kind": kind, "points": []}));
                history.len() - 1
            });
            history[i]["points"].as_array_mut().expect("points").push(json!([when, used]));
        }
    }
    let mut moves = Vec::new();
    let mut last: HashMap<String, String> = HashMap::new();
    for line in read("attribution.ndjson").lines() {
        let Ok(r) = serde_json::from_str::<Value>(line) else { continue };
        let (Some(session), Some(to), Some(when)) =
            (r["sessionId"].as_str(), r["accountId"].as_str(), r["at"].as_str().and_then(files::parse_ms))
        else {
            continue;
        };
        if to.is_empty() {
            continue;
        }
        let before = last.insert(session.into(), to.into());
        if before.as_deref() == Some(to) || when < now - pool::HOUR * 24 {
            continue;
        }
        let cause = match &before {
            None => "start",
            Some(_) => r["cause"].as_str().filter(|c| !c.is_empty()).unwrap_or("limit"),
        };
        moves.push(json!({"at": when, "session": session, "label": null, "from": before, "to": to, "cause": cause}));
    }
    moves.sort_by_key(|m| std::cmp::Reverse(m["at"].as_i64().unwrap_or(0)));
    let seats = app.state().seats.len();
    no_store(json!({"history": history, "moves": moves, "hosts": [{"host": "server", "seats": seats, "seenAt": null}]}))
}

#[derive(Deserialize)]
struct PolicyRequest {
    policy: Policy,
    #[serde(default)]
    revision: Value,
    #[serde(default)]
    save: bool,
}

/// Previews where new sessions would go under proposed cutoffs, or saves them when the
/// config is still at the revision the caller saw.
async fn policy(State(app): State<Arc<App>>, Json(body): Json<PolicyRequest>) -> Response {
    if let Err(message) = body.policy.validate() {
        return fail(StatusCode::BAD_REQUEST, message);
    }
    let revision = match &body.revision {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    let current = app.state().revision.clone();
    if !body.save {
        return no_store(json!({"pool": snapshot(&app, body.policy, &revision).1, "saved": false, "conflict": false}));
    }
    if revision != current {
        let policy = app.state().policy;
        return no_store(json!({"pool": snapshot(&app, policy, &current).1, "saved": false, "conflict": true}));
    }
    match Config::save_policy(&app.config_path, body.policy) {
        Ok(next) => {
            {
                let mut state = app.state();
                state.policy = body.policy;
                state.revision.clone_from(&next);
            }
            app.changed.notify_one();
            no_store(json!({"pool": snapshot(&app, body.policy, &next).1, "saved": true, "conflict": false}))
        }
        Err(err) => fail(StatusCode::INTERNAL_SERVER_ERROR, &format!("Couldn't save the policy: {err}")),
    }
}

/// Every OAuth login, as BROADCOM's Accounts page lists them.
async fn credentials(State(app): State<Arc<App>>) -> Response {
    let now = files::now_ms();
    let state = app.state();
    let logins = state
        .accounts
        .iter()
        .map(|a| (&a.login, a.id(), a.needs_reauth, a.cool_until))
        .chain(state.others.iter().map(|l| (l, l.name(), false, 0)));
    let list: Vec<Value> = logins
        .map(|(l, index, needs_reauth, cool_until)| {
            let mut file = json!({"auth_index": index, "name": l.name(), "provider": l.kind(), "disabled": l.disabled(),
                                  "status": if l.disabled() { "disabled" } else if needs_reauth { "error" } else { "active" }});
            if !l.get("email").is_empty() {
                file["email"] = l.get("email").into();
            }
            if needs_reauth {
                file["requires_sign_in"] = true.into();
                file["status_message"] = "Signed out: sign in again.".into();
            } else if cool_until > now {
                file["unavailable"] = true.into();
                file["status_message"] = format!("Cooling down until {}.", files::when(cool_until)).into();
            }
            file
        })
        .collect();
    no_store(json!({"files": list}))
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

#[derive(Deserialize)]
struct StatusRequest {
    name: String,
    disabled: bool,
}

async fn credential_status(State(app): State<Arc<App>>, Json(body): Json<StatusRequest>) -> Response {
    if let Some(refused) = owner_only(&app) {
        return refused;
    }
    let path = app.config.credentials_dir.join(&body.name);
    let fields = [("disabled".to_string(), Value::from(body.disabled))].into_iter().collect();
    match app.update_login(&path, fields) {
        Some(_) => done(),
        None => fail(StatusCode::NOT_FOUND, "No such credential."),
    }
}

#[derive(Deserialize)]
struct Names {
    names: Vec<String>,
}

/// Removes logins from the pool and their files, together, so nothing writes one back.
async fn delete_credentials(State(app): State<Arc<App>>, Json(body): Json<Names>) -> Response {
    if let Some(refused) = owner_only(&app) {
        return refused;
    }
    let mut state = app.state();
    for name in &body.names {
        let path = app.config.credentials_dir.join(name);
        let known = state.accounts.iter().map(|a| &a.login).chain(state.others.iter()).any(|l| l.path == path);
        if !known {
            return fail(StatusCode::NOT_FOUND, &format!("No credential named {name}."));
        }
        if let Err(err) = std::fs::remove_file(&path) {
            return fail(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
        }
        state.accounts.retain(|a| a.login.path != path);
        state.others.retain(|l| l.path != path);
    }
    drop(state);
    app.changed.notify_one();
    done()
}

async fn refresh_credential(State(app): State<Arc<App>>, Json(body): Json<Named>) -> Response {
    if let Some(refused) = owner_only(&app) {
        return refused;
    }
    let path = app.config.credentials_dir.join(&body.name);
    match oauth::refresh_if(&app, &path, |_| true).await {
        Ok(_) => done(),
        Err(err) => fail(StatusCode::BAD_GATEWAY, &err.to_string()),
    }
}

#[derive(Deserialize)]
struct StartQuery {
    provider: Provider,
    name: Option<String>,
}

/// Starts a sign-in that finishes on any device: Claude's code page, or ChatGPT's device code.
async fn sign_in_start(State(app): State<Arc<App>>, Query(q): Query<StartQuery>) -> Response {
    if let Some(refused) = owner_only(&app) {
        return refused;
    }
    match oauth::start(&app, q.provider, q.name.filter(|n| !n.is_empty())).await {
        Ok(started) => no_store(serde_json::to_value(started).unwrap_or_default()),
        Err(err) => fail(StatusCode::BAD_GATEWAY, &err.to_string()),
    }
}

#[derive(Deserialize)]
struct StateQuery {
    state: String,
}

async fn sign_in_status(State(app): State<Arc<App>>, Query(q): Query<StateQuery>) -> Response {
    no_store(serde_json::to_value(app.sign_ins.status(&q.state)).unwrap_or_default())
}

#[derive(Deserialize)]
struct Callback {
    provider: Provider,
    #[serde(default)]
    code: String,
    #[serde(default)]
    state: String,
    redirect_url: Option<String>,
}

/// The code from Claude's page, or the address it redirected to.
async fn sign_in_callback(State(app): State<Arc<App>>, Json(body): Json<Callback>) -> Response {
    if body.provider != Provider::Claude {
        return fail(StatusCode::BAD_REQUEST, "ChatGPT signs in with its device code; there's nothing to paste.");
    }
    let (mut code, mut state) = (body.code, body.state);
    if let Some(url) = body.redirect_url.as_deref().and_then(|u| reqwest::Url::parse(u).ok()) {
        for (key, value) in url.query_pairs() {
            match &*key {
                "code" => code = value.into_owned(),
                "state" => state = value.into_owned(),
                _ => {}
            }
        }
    }
    match oauth::finish_claude(&app, &state, &code).await {
        oauth::Status::Error { error } => fail(StatusCode::BAD_REQUEST, &error),
        _ => done(),
    }
}

async fn sign_in_cancel(State(app): State<Arc<App>>, Query(q): Query<StateQuery>) -> Response {
    app.sign_ins.cancel(&q.state);
    done()
}

/// Spends the account's held weekly reset that expires first, now.
async fn spend_reset(State(app): State<Arc<App>>, Path(account): Path<String>) -> Response {
    if let Some(refused) = owner_only(&app) {
        return refused;
    }
    let spent = sampler::spend(&app, &account, true, files::now_ms()).await;
    no_store(json!({"spent": spent}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_plans_from_their_tier() {
        assert_eq!(plan_name(Some("default_claude_max_20x")), "Max 20x");
        assert_eq!(plan_name(Some("default_claude_max_5x")), "Max 5x");
        assert_eq!(plan_name(Some("claude_pro")), "Pro");
        assert_eq!(plan_name(None), "");
    }
}
