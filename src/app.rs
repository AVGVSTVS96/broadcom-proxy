//! The proxy's shared state and the decisions that read or change it: which credentials
//! serve a request, what a response says about an account, and where sessions sit.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Mutex, MutexGuard},
};

use axum::http::HeaderMap;
use serde_json::json;
use tokio::sync::Notify;

use crate::{
    accounts::{Account, ApiKey, Login},
    config::{Config, read_secret},
    files,
    live::Stream,
    oauth::SignIns,
    pool::{self, Cause, HOUR, MINUTE, Policy, Reading, View, Window},
};

/// Where the proxy reaches Anthropic and OpenAI.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub anthropic: String,
    pub claude_platform: String,
    pub claude_ai: String,
    pub openai_auth: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            anthropic: "https://api.anthropic.com".into(),
            claude_platform: "https://platform.claude.com".into(),
            claude_ai: "https://claude.ai".into(),
            openai_auth: "https://auth.openai.com".into(),
        }
    }
}

pub struct App {
    pub config_path: PathBuf,
    pub config: Config,
    pub ends: Endpoints,
    pub http: reqwest::Client,
    pub client_keys: Vec<String>,
    pub management_key: String,
    pub state: Mutex<State>,
    /// Wakes the live stream when something it shows may have changed.
    pub changed: Notify,
    pub live: Stream,
    pub sign_ins: SignIns,
    /// One token refresh at a time, so a token is never refreshed twice.
    pub refreshing: tokio::sync::Mutex<()>,
}

pub struct State {
    pub policy: Policy,
    pub revision: String,
    pub accounts: Vec<Account>,
    /// Logins the pool doesn't route (ChatGPT), kept signed in.
    pub others: Vec<Login>,
    pub keys: Vec<ApiKey>,
    pub seats: HashMap<String, Seat>,
}

/// The account a Claude Code session runs on.
#[derive(Clone, Debug)]
pub struct Seat {
    pub account: String,
    pub entrypoint: String,
    pub seen: i64,
}

/// A credential a request can go out on, with what it needs to authenticate.
#[derive(Clone, Debug)]
pub enum Target {
    Key { id: String, name: String, key: String },
    Plan { id: String, token: String },
}

impl Target {
    pub fn id(&self) -> &str {
        match self {
            Target::Key { id, .. } | Target::Plan { id, .. } => id,
        }
    }
}

/// What routing needs to know about a request.
#[derive(Clone, Debug, Default)]
pub struct Inbound {
    pub model: String,
    pub session: Option<String>,
    pub entrypoint: String,
}

/// The credentials to try for a request, in order, and the body's model name for each kind.
#[derive(Debug, Default)]
pub struct Route {
    pub targets: Vec<Target>,
    /// The model with its credit prefix stripped, when the client asked for one.
    pub unprefixed: Option<String>,
    /// Why the session's account isn't first, if it had one.
    pub moved: Option<Cause>,
}

const SEAT_DAY: i64 = 24 * HOUR;
const WORKING: i64 = 2 * MINUTE;
const READING_EVERY: i64 = 15 * MINUTE;

impl App {
    pub fn new(config_path: PathBuf, config: Config, revision: String, ends: Endpoints) -> anyhow::Result<Self> {
        let client_keys = read_secret(&config.client_keys_file)?
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        let management_key = read_secret(&config.management_key_file)?;
        let keys = config.api_keys.iter().map(ApiKey::load).collect::<anyhow::Result<_>>()?;
        let state = State {
            policy: config.policy,
            revision,
            accounts: Vec::new(),
            others: Vec::new(),
            keys,
            seats: HashMap::new(),
        };
        let app = Self {
            config_path,
            ends,
            http: reqwest::Client::builder()
                .user_agent(concat!("broadcom-proxy/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(std::time::Duration::from_secs(10))
                .read_timeout(std::time::Duration::from_secs(600))
                .build()?,
            client_keys,
            management_key,
            state: Mutex::new(state),
            changed: Notify::new(),
            live: Stream::default(),
            sign_ins: SignIns::default(),
            refreshing: tokio::sync::Mutex::new(()),
            config,
        };
        app.rescan();
        Ok(app)
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("state lock")
    }

    pub fn state_dir(&self) -> &std::path::Path {
        &self.config.state_dir
    }

    /// Brings the accounts in line with the credentials directory: new logins join, changed
    /// files are reread, removed ones leave. Live state stays with its account.
    pub fn rescan(&self) {
        let logins = Login::read_dir(&self.config.credentials_dir);
        let mut state = self.state();
        let mut accounts = std::mem::take(&mut state.accounts);
        let mut others = Vec::new();
        let mut kept = Vec::new();
        for login in logins {
            if login.kind() != "claude" {
                others.push(login);
                continue;
            }
            match accounts.iter().position(|a| a.login.path == login.path) {
                Some(i) => {
                    let mut account = accounts.swap_remove(i);
                    if account.login.modified != login.modified {
                        if account.login.get("access_token") != login.get("access_token") {
                            account.needs_reauth = false;
                        }
                        account.login = login;
                    }
                    kept.push(account);
                }
                None => kept.push(Account::new(login)),
            }
        }
        state.accounts = kept;
        state.others = others;
        drop(state);
        self.changed.notify_one();
    }

    pub fn client_allowed(&self, headers: &HeaderMap) -> bool {
        let bearer = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
        let api_key = headers.get("x-api-key").and_then(|v| v.to_str().ok());
        [bearer, api_key].into_iter().flatten().any(|k| self.client_keys.iter().any(|c| c == k.trim()))
    }

    pub fn weight(&self, entrypoint: &str) -> f64 {
        if matches!(entrypoint, "sdk-cli" | "sdk-ts" | "sdk-py") { self.config.agent_sdk_factor } else { 1.0 }
    }

    /// Credit first: a bare model goes to API keys with credit, highest priority first, then
    /// to the plans in rank order, the session's own account first unless it should move.
    /// `<prefix>/<model>` goes only to the keys with that prefix.
    pub fn route(&self, inbound: &Inbound, now: i64) -> Route {
        let state = self.state();
        let mut route = Route::default();
        let (prefix, bare) = inbound.model.split_once('/').unwrap_or(("", &inbound.model));
        let prefixed = !prefix.is_empty() && state.keys.iter().any(|k| k.prefix == prefix);
        let mut keys: Vec<&ApiKey> =
            state.keys.iter().filter(|k| k.cool_until <= now && (!prefixed || k.prefix == prefix)).collect();
        keys.sort_by_key(|k| std::cmp::Reverse(k.priority));
        route.targets.extend(keys.into_iter().map(|k| Target::Key {
            id: k.id.clone(),
            name: k.name.clone(),
            key: k.key.clone(),
        }));
        if prefixed {
            route.unprefixed = Some(bare.to_string());
            return route;
        }

        let available: Vec<&Account> =
            state.accounts.iter().filter(|a| !a.login.disabled() && a.cool_until <= now).collect();
        let weight = self.weight(&inbound.entrypoint);
        let views = state.views(&available, now, self.config.agent_sdk_factor);
        let rows = pool::rank(&views, &state.policy, now, weight);
        let mut order: Vec<usize> = rows.iter().map(|r| r.index).collect();
        if let Some(seat) = inbound.session.as_ref().and_then(|s| state.seats.get(s)) {
            match available.iter().position(|a| a.id() == seat.account) {
                Some(mine) => {
                    route.moved = pool::move_away(&rows, mine, &state.policy, weight);
                    order.retain(|&i| i != mine);
                    if route.moved.is_some() {
                        order.push(mine);
                    } else {
                        order.insert(0, mine);
                    }
                }
                None => route.moved = Some(Cause::Limit),
            }
        }
        route.targets.extend(order.into_iter().map(|i| {
            let a = available[i];
            Target::Plan { id: a.id(), token: a.login.get("access_token").to_string() }
        }));
        route
    }

    /// Records that a session's request was served by `account`: a `start` line the first
    /// time, a move line with its cause when the account changes.
    pub fn seat(&self, inbound: &Inbound, account: &str, moved: Option<Cause>, now: i64) {
        let Some(session) = &inbound.session else { return };
        let mut state = self.state();
        let previous = state.seats.get(session).map(|s| s.account.clone());
        if previous.as_deref() != Some(account) {
            let mut row = json!({"at": files::utc(now), "sessionId": session, "accountId": account, "event": "start"});
            if !inbound.entrypoint.is_empty() {
                row["entrypoint"] = inbound.entrypoint.clone().into();
            }
            if let Some(from) = &previous {
                row["event"] = "failure".into();
                row["from"] = from.clone().into();
                row["cause"] = moved.unwrap_or(Cause::Limit).as_str().into();
            }
            files::append(self.state_dir(), "attribution.ndjson", &row);
        }
        state.seats.insert(
            session.clone(),
            Seat { account: account.into(), entrypoint: inbound.entrypoint.clone(), seen: now },
        );
        state.seats.retain(|_, s| now - s.seen <= SEAT_DAY);
        drop(state);
        self.changed.notify_one();
    }

    /// Reads an account's limits from Anthropic's `anthropic-ratelimit-unified-*` headers.
    pub fn observe(&self, account: &str, headers: &HeaderMap, now: i64) {
        let Some(reading) = header_reading(headers, now) else { return };
        let mut state = self.state();
        if let Some(a) = state.account_mut(account) {
            a.observe(reading, now);
            log_reading(self.state_dir(), a);
        }
        drop(state);
        self.changed.notify_one();
    }

    /// Takes a credential out of rotation until `until`.
    pub fn cool(&self, target: &Target, until: i64) {
        let mut state = self.state();
        match target {
            Target::Key { id, .. } => {
                if let Some(k) = state.keys.iter_mut().find(|k| &k.id == id) {
                    k.cool_until = k.cool_until.max(until);
                }
            }
            Target::Plan { id, .. } => {
                if let Some(a) = state.account_mut(id) {
                    a.cool_until = a.cool_until.max(until);
                }
            }
        }
        drop(state);
        self.changed.notify_one();
    }
}

impl State {
    pub fn account_mut(&mut self, id: &str) -> Option<&mut Account> {
        self.accounts.iter_mut().find(|a| a.id() == id)
    }

    /// What ranking needs about each account, with sessions working on it in the last two minutes.
    pub fn views<'a>(&self, accounts: &[&'a Account], now: i64, factor: f64) -> Vec<View<'a>> {
        accounts
            .iter()
            .map(|a| {
                let id = a.id();
                let (working, load) = self.seats.values().filter(|s| s.account == id && now - s.seen <= WORKING).fold(
                    (0, 0.0),
                    |(n, load), s| {
                        let weight =
                            if matches!(s.entrypoint.as_str(), "sdk-cli" | "sdk-ts" | "sdk-py") { factor } else { 1.0 };
                        (n + 1, load + weight)
                    },
                );
                View {
                    label: a.label(),
                    reading: a.settled(a.reading, now).0.live(now),
                    resets: &a.resets,
                    needs_reauth: a.needs_reauth,
                    working,
                    load,
                }
            })
            .collect()
    }
}

/// The account's limits from a response's headers: utilization is a fraction, reset a unix time.
pub fn header_reading(headers: &HeaderMap, now: i64) -> Option<Reading> {
    let window = |name: &str| {
        let get = |field: &str| headers.get(format!("anthropic-ratelimit-unified-{name}-{field}"))?.to_str().ok();
        let used: f64 = get("utilization")?.parse().ok().filter(|u: &f64| u.is_finite())?;
        let reset: i64 = get("reset").and_then(|r| r.parse().ok()).unwrap_or(0);
        Some(Window { used: used * 100.0, resets_at: reset * 1000 })
    };
    let reading = Reading { five_hour: window("5h"), week: window("7d"), at: now };
    (reading.five_hour.is_some() || reading.week.is_some()).then_some(reading)
}

/// Appends a line to the limit history when either limit moved half a point, and at least
/// every 15 minutes while readings keep arriving.
pub fn log_reading(dir: &std::path::Path, a: &mut Account) {
    let r = a.reading;
    let used = |w: Option<Window>| w.map(|w| w.used);
    let same = |x: Option<f64>, y: Option<f64>| match (x, y) {
        (None, None) => true,
        (Some(x), Some(y)) => (x - y).abs() < 0.5,
        _ => false,
    };
    let logged = a.logged;
    if r.at <= logged.at
        || (same(used(r.five_hour), used(logged.five_hour))
            && same(used(r.week), used(logged.week))
            && r.at - logged.at < READING_EVERY)
    {
        return;
    }
    a.logged = r;
    let reset = |w: Option<Window>| w.map(|w| w.resets_at).filter(|&at| at > 0);
    files::append(
        dir,
        "usage-history.ndjson",
        &json!({"account": a.id(), "at": r.at, "fiveHour": used(r.five_hour), "week": used(r.week),
                "fiveHourResetsAt": reset(r.five_hour), "weekResetsAt": reset(r.week)}),
    );
}
