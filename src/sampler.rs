//! Every minute: reread the credentials directory, read each account's limits, held resets
//! and plan from Anthropic when they're stale, and spend a held reset when the pool needs it.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    accounts::{AtLimit, Claim, Cloud, Plan, Spent},
    app::{App, log_reading},
    files,
    pool::{self, HOUR, MINUTE, Reading, Reset, ResetPlan, Window},
};

const STALE_AFTER: i64 = 20 * MINUTE;
const CLOUD_EVERY: i64 = 5 * MINUTE;
const RESETS_EVERY: i64 = HOUR;
const PLAN_EVERY: i64 = 24 * HOUR;
const FRESH_RESETS: i64 = 2 * MINUTE;
const CLAIM_BACKOFF: i64 = 5 * MINUTE;
const SETTLE: i64 = HOUR;

pub async fn anthropic_get(app: &App, token: &str, path: &str) -> reqwest::Result<reqwest::Response> {
    app.http
        .get(format!("{}{path}", app.ends.anthropic))
        .bearer_auth(token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .timeout(Duration::from_secs(15))
        .send()
        .await
}

struct Job {
    account: String,
    token: String,
    usage: bool,
    plan: bool,
}

pub async fn sample(app: &App) {
    app.rescan();
    let now = files::now_ms();
    let jobs: Vec<Job> = {
        let state = app.state();
        state
            .accounts
            .iter()
            .filter(|a| !a.login.disabled() && a.sample_after <= now)
            .filter_map(|a| {
                let usage = now - a.reading.at > STALE_AFTER
                    || a.cloud.as_ref().is_some_and(|c| now - c.sampled_at > CLOUD_EVERY)
                    || now - a.resets_read_at > RESETS_EVERY;
                let plan = a.plan.as_ref().is_none_or(|p| now - p.at > PLAN_EVERY);
                let token = a.login.get("access_token").to_string();
                ((usage || plan) && !token.is_empty()).then(|| Job { account: a.id(), token, usage, plan })
            })
            .collect()
    };
    for job in jobs {
        if job.plan {
            read_plan(app, &job.account, &job.token, now).await;
        }
        if job.usage {
            read_usage(app, &job.account, &job.token, now).await;
        }
    }
    if app.config.account_owner {
        spend_due(app, now).await;
    }
    write_accounts(app);
    app.changed.notify_one();
}

async fn read_plan(app: &App, account: &str, token: &str, now: i64) {
    let Ok(response) = anthropic_get(app, token, "/api/oauth/profile").await else { return };
    if !response.status().is_success() {
        return;
    }
    let Ok(profile) = response.json::<Value>().await else { return };
    let text = |p: &str| profile.pointer(p).and_then(Value::as_str).map(String::from);
    let plan = Plan {
        tier: text("/organization/rate_limit_tier"),
        status: text("/organization/subscription_status"),
        since: text("/organization/subscription_created_at").and_then(|t| files::parse_ms(&t)),
        at: now,
    };
    let _ = files::write_json(&app.state_dir().join("plans").join(format!("{account}.json")), &plan);
    if let Some(a) = app.state().account_mut(account) {
        a.plan = Some(plan);
    }
}

#[derive(Deserialize)]
struct Usage {
    five_hour: Option<UsageWindow>,
    seven_day: Option<UsageWindow>,
    iguana_necktie: Option<CloudAnswer>,
    cedar_ember: Option<Grants>,
    juniper_tide: Option<AtLimitAnswer>,
}

#[derive(Deserialize)]
struct UsageWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

impl UsageWindow {
    fn window(&self) -> Option<Window> {
        Some(Window {
            used: self.utilization?,
            resets_at: self.resets_at.as_deref().and_then(files::parse_ms).unwrap_or(0),
        })
    }
}

#[derive(Deserialize)]
struct CloudAnswer {
    limit_dollars: Option<f64>,
    used_dollars: Option<f64>,
    remaining_dollars: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct Grants {
    #[serde(default)]
    grants: Vec<Value>,
}

#[derive(Deserialize)]
struct AtLimitAnswer {
    eligible: bool,
    available: bool,
    next_available_at: Option<String>,
}

/// Reads an account's limits and held resets from Anthropic's usage endpoint.
pub async fn read_usage(app: &App, account: &str, token: &str, now: i64) {
    let response = anthropic_get(app, token, "/api/oauth/usage?cedar_ember=1&at_wall=1&skip_spend=1").await;
    let Ok(response) = response else { return };
    let status = response.status().as_u16();
    let retry =
        response.headers().get("retry-after").and_then(|v| v.to_str().ok()?.parse::<i64>().ok()).filter(|&s| s > 0);
    let body = if status == 200 { response.json::<Usage>().await.ok() } else { None };
    let mut state = app.state();
    let Some(a) = state.account_mut(account) else { return };
    a.needs_reauth = status == 401;
    if status == 429 {
        a.sample_after = now + retry.map_or(15 * MINUTE, |s| s * 1000);
    }
    let Some(body) = body else { return };
    a.cloud = body.iguana_necktie.map(|c| Cloud {
        limit: c.limit_dollars,
        used: c.used_dollars,
        remaining: c.remaining_dollars,
        expires_at: c.resets_at.as_deref().and_then(files::parse_ms),
        sampled_at: now,
    });
    let reading = Reading {
        five_hour: body.five_hour.as_ref().and_then(UsageWindow::window),
        week: body.seven_day.as_ref().and_then(UsageWindow::window),
        at: now,
    };
    let (reading, stale) = a.settled(reading, now);
    a.reading = reading;
    log_reading(app.state_dir(), a);
    if !stale {
        a.resets = body.cedar_ember.map(|c| c.grants).unwrap_or_default().iter().filter_map(grant).collect();
    }
    a.at_limit = body
        .juniper_tide
        .filter(|j| j.eligible)
        .map(|j| AtLimit { available: j.available, next_at: j.next_available_at.as_deref().and_then(files::parse_ms) });
    a.resets_read_at = now;
    let file = json!({"held": a.resets, "atLimit": a.at_limit, "at": now});
    drop(state);
    let _ = files::write_json(&app.state_dir().join("resets").join(format!("{account}.json")), &file);
}

/// A held reset from one of Anthropic's grants. A grant that doesn't say otherwise needs
/// the limit, as Claude Code reads it.
fn grant(g: &Value) -> Option<Reset> {
    let left = g.get("resets_left")?.as_f64().filter(|&l| l > 0.0)? as i64;
    let text = |k: &str| g.get(k).and_then(Value::as_str).unwrap_or("");
    let flag = |k: &str| g.get(k).and_then(Value::as_bool);
    let clears: Vec<&str> = g
        .get("clears")
        .and_then(Value::as_array)
        .map(|c| c.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let kind = if flag("gives_back_weekly") == Some(true) {
        "fable".to_string()
    } else if !text("type").is_empty() {
        text("type").to_string()
    } else {
        let session = clears.contains(&"five_hour");
        let other = clears.iter().any(|c| *c != "five_hour");
        match (session, other) {
            (true, false) => "session".into(),
            (false, true) => "weekly".into(),
            _ => "full".into(),
        }
    };
    Some(Reset {
        id: text("id").into(),
        kind,
        left,
        expires_at: files::parse_ms(text("ends_at")),
        paused: flag("paused").unwrap_or(false),
        usable_now: flag("usable_now").unwrap_or(false),
        needs_limit: flag("use_requires_limit").unwrap_or(true),
        weekly: clears.contains(&"seven_day"),
    })
}

/// The reset plan for every account holding a weekly reset, ranked as the pool stands now.
pub fn reset_plans(app: &App, now: i64) -> Vec<(String, ResetPlan)> {
    let state = app.state();
    let enabled: Vec<_> = state.accounts.iter().filter(|a| !a.login.disabled()).collect();
    let views = state.views(&enabled, now, app.config.agent_sdk_factor);
    let rows = pool::rank(&views, &state.policy, now, 1.0);
    pool::plan_resets(&views, &rows, now, files::when).into_iter().map(|p| (enabled[p.index].id(), p)).collect()
}

async fn spend_due(app: &App, now: i64) {
    load_claims(app);
    for (account, plan) in reset_plans(app, now) {
        if let Some(why) = plan.why.clone() {
            spend(app, &account, plan, &why, now).await;
        }
    }
}

/// Claims a held reset after rereading the account's grants to make sure Anthropic still
/// offers it, and reports whether it was spent.
pub async fn spend(app: &App, account: &str, mut plan: ResetPlan, why: &str, now: i64) -> bool {
    let (token, org, fresh, label) = {
        let state = app.state();
        let Some(a) = state.accounts.iter().find(|a| a.id() == account) else { return false };
        if a.sample_after > now || a.claim.as_ref().is_some_and(|c| c.retry_at > now) {
            return false;
        }
        (
            a.login.get("access_token").to_string(),
            a.login.get("organization_uuid").to_string(),
            now - a.resets_read_at <= FRESH_RESETS,
            a.label().to_string(),
        )
    };
    if token.is_empty() || org.is_empty() {
        return false;
    }
    if !fresh {
        read_usage(app, account, &token, now).await;
        match held(app, account, &plan.grant.id, now) {
            Some(grant) => plan.grant = grant,
            None => return false,
        }
    }
    let Some((claim, resent)) = claim(app, account, &mut plan, why, now) else { return false };

    let (status, answer) = claim_reset(app, &token, &org, &claim).await;
    let text = |k: &str| answer.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let result = text("result");
    let spent = result == "reset" || (result == "already_used" && resent);
    let settled = !result.is_empty() && result != "unavailable" && status != 429 && status < 500;
    let resets_left = answer.get("resets_left").and_then(Value::as_i64);
    files::append(
        app.state_dir(),
        "resets-spent.ndjson",
        &json!({"at": files::utc(now), "account": account, "email": label, "grant": plan.grant.id, "kind": plan.grant.kind,
                "requestId": claim.request_id, "status": status, "result": result, "reason": text("reason"), "spent": spent,
                "resetsLeft": resets_left, "cleared": answer.get("cleared"), "weeklyResetsAt": answer.get("weekly_resets_at"), "why": why}),
    );
    let mut retry = now + CLAIM_BACKOFF;
    if let Some(at) = files::parse_ms(&text("cooldown_until")) {
        retry = retry.max(at);
    }
    let (told, week_ends) = {
        let mut state = app.state();
        let Some(a) = state.account_mut(account) else { return false };
        let told = a.claim.as_ref().is_some_and(|c| c.told);
        let mut week_ends = a.reading.week.map_or(now + pool::WEEK, |w| w.resets_at);
        if spent {
            week_ends = files::parse_ms(&text("weekly_resets_at")).unwrap_or(week_ends);
            a.spent = Some(Spent { week: a.reading.week.map_or(0.0, |w| w.used), week_ends, until: now + SETTLE });
            a.reading = Reading {
                five_hour: a.reading.five_hour,
                week: Some(Window { used: 0.0, resets_at: week_ends }),
                at: now,
            };
            for g in &mut a.resets {
                if g.id == plan.grant.id {
                    g.left = resets_left.unwrap_or(g.left - 1);
                }
            }
            a.resets.retain(|g| g.left > 0);
            a.resets_read_at = 0;
            a.claim = None;
            a.cool_until = 0;
        } else if settled {
            a.resets_read_at = 0;
            a.claim = Some(Claim {
                grant: plan.grant.id.clone(),
                request_id: uuid::Uuid::new_v4().to_string(),
                sent: false,
                retry_at: retry,
                told: true,
            });
        } else if let Some(c) = a.claim.as_mut() {
            c.retry_at = retry;
            c.told = true;
        }
        save_claims(app, &state);
        (told, week_ends)
    };
    if spent {
        tell(
            app,
            format!(
                "Spent {label}'s weekly reset: {why}. Its week is back to 0%; next weekly reset {}.",
                files::when(week_ends)
            ),
        );
    } else if !told {
        tell(
            app,
            format!(
                "Tried to spend {label}'s weekly reset ({why}); Anthropic answered {result:?} ({}, HTTP {status}). Retrying every few minutes.",
                text("reason")
            ),
        );
    }
    app.changed.notify_one();
    spent
}

/// The account's grant `id`, if it still holds it live.
fn held(app: &App, account: &str, id: &str, now: i64) -> Option<Reset> {
    let state = app.state();
    let a = state.accounts.iter().find(|a| a.id() == account)?;
    a.resets.iter().find(|g| g.id == id && g.live(now)).cloned()
}

/// The claim to send for the plan's grant, and whether it was sent before. A claim already
/// sent for another grant the account still holds goes out again under its own request id.
/// None while Anthropic says the reset can't be used yet.
fn claim(app: &App, account: &str, plan: &mut ResetPlan, why: &str, now: i64) -> Option<(Claim, bool)> {
    let mut state = app.state();
    let a = state.account_mut(account)?;
    let label = a.label().to_string();
    if let Some(c) = a.claim.as_ref().filter(|c| c.sent && c.grant != plan.grant.id)
        && let Some(g) = a.resets.iter().find(|g| g.id == c.grant && g.live(now))
    {
        plan.grant = g.clone();
    }
    if a.claim.as_ref().is_none_or(|c| c.grant != plan.grant.id) {
        let request_id = uuid::Uuid::new_v4().to_string();
        a.claim = Some(Claim { grant: plan.grant.id.clone(), request_id, sent: false, retry_at: 0, told: false });
    }
    let claim = a.claim.as_mut()?;
    if !plan.grant.usable_now {
        let told = std::mem::replace(&mut claim.told, true);
        claim.retry_at = now + CLAIM_BACKOFF;
        drop(state);
        if !told {
            tell(
                app,
                format!(
                    "Wanted to spend {label}'s weekly reset ({why}), but Anthropic says it can't be used right now. Retrying every few minutes."
                ),
            );
        }
        return None;
    }
    let resent = std::mem::replace(&mut claim.sent, true);
    let claim = claim.clone();
    save_claims(app, &state);
    Some((claim, resent))
}

/// Spends a held reset the way Claude Code's /limit-reset does.
async fn claim_reset(app: &App, token: &str, org: &str, claim: &Claim) -> (u16, Value) {
    let response = app
        .http
        .post(format!("{}/api/organizations/{org}/reset_rate_limits", app.ends.anthropic))
        .bearer_auth(token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .json(&json!({"program": "cedar_ember", "grant_id": claim.grant, "request_id": claim.request_id}))
        .timeout(Duration::from_secs(30))
        .send()
        .await;
    match response {
        Ok(r) => {
            let status = r.status().as_u16();
            (status, if status == 200 { r.json().await.unwrap_or_default() } else { Value::Null })
        }
        Err(_) => (0, Value::Null),
    }
}

fn tell(app: &App, line: String) {
    println!("pool: {line}");
    let Some(command) = app.config.reset_command.clone() else { return };
    tokio::spawn(async move {
        let run =
            tokio::process::Command::new("sh").arg("-c").arg(format!("{command} \"$1\"")).arg("sh").arg(&line).output();
        match tokio::time::timeout(Duration::from_secs(60), run).await {
            Ok(Ok(out)) if out.status.success() => {}
            other => eprintln!("reset-command failed: {other:?}"),
        }
    });
}

/// Reads claims a previous run sent that Anthropic hadn't settled, once.
fn load_claims(app: &App) {
    static LOADED: std::sync::Once = std::sync::Once::new();
    LOADED.call_once(|| {
        let Ok(bytes) = std::fs::read(app.state_dir().join("reset-claims.json")) else { return };
        let Ok(claims) = serde_json::from_slice::<std::collections::HashMap<String, Claim>>(&bytes) else { return };
        let mut state = app.state();
        for (account, claim) in claims {
            if let Some(a) = state.account_mut(&account) {
                a.claim = Some(claim);
            }
        }
    });
}

fn save_claims(app: &App, state: &crate::app::State) {
    let claims: serde_json::Map<String, Value> = state
        .accounts
        .iter()
        .filter_map(|a| Some((a.id(), serde_json::to_value(a.claim.as_ref().filter(|c| c.sent)?).ok()?)))
        .collect();
    let _ = files::write_json(&app.state_dir().join("reset-claims.json"), &claims);
}

/// Keeps the account list other tools use to name accounts current.
fn write_accounts(app: &App) {
    let Some(path) = &app.config.accounts_file else { return };
    let state = app.state();
    let mut list: Vec<Value> = state
        .accounts
        .iter()
        .filter(|a| !a.login.get("account_uuid").is_empty())
        .map(|a| {
            let tier = a.plan.as_ref().and_then(|p| p.tier.as_deref()).unwrap_or("");
            let tier = tier.trim_start_matches("default_claude_").replace('_', " ");
            json!({"id": a.id(), "email": a.login.get("email"), "tier": tier, "needsReauth": a.needs_reauth || a.login.disabled()})
        })
        .collect();
    drop(state);
    if list.is_empty() {
        return;
    }
    list.sort_by(|a, b| a["email"].as_str().cmp(&b["email"].as_str()));
    let doc = json!({"accounts": list});
    if std::fs::read(path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).as_ref() != Some(&doc) {
        let _ = files::write_json(path, &doc);
    }
}

/// When the pool spent each account's resets (unix ms), oldest first.
pub fn resets_spent(app: &App) -> std::collections::HashMap<String, Vec<i64>> {
    let mut out: std::collections::HashMap<String, Vec<i64>> = std::collections::HashMap::new();
    let text = std::fs::read_to_string(app.state_dir().join("resets-spent.ndjson")).unwrap_or_default();
    for line in text.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else { continue };
        if (row["result"] == "reset" || row["spent"] == true)
            && let (Some(account), Some(at)) = (row["account"].as_str(), row["at"].as_str().and_then(files::parse_ms))
        {
            out.entry(account.into()).or_default().push(at);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grant_needs_the_limit_unless_it_says_otherwise() {
        let g = grant(&json!({"id": "g", "resets_left": 1, "clears": ["seven_day"]})).unwrap();
        assert!(g.needs_limit && g.weekly);
        assert_eq!(g.kind, "weekly");
        assert!(!grant(&json!({"id": "g", "resets_left": 1, "use_requires_limit": false})).unwrap().needs_limit);
        assert!(grant(&json!({"id": "g", "resets_left": 0})).is_none());
        assert_eq!(grant(&json!({"resets_left": 1, "gives_back_weekly": true})).unwrap().kind, "fable");
    }
}
