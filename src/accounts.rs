//! What the proxy holds per credential: OAuth logins read from their files, API keys from
//! the config, and each plan account's live state.

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::{
    config::{ApiKeyConfig, hex, read_secret},
    files,
    pool::{Reading, Reset},
};

/// One OAuth login file, kept whole so a rewrite loses no field another tool wrote.
#[derive(Clone, Debug)]
pub struct Login {
    pub path: PathBuf,
    pub fields: Map<String, Value>,
    pub modified: Option<SystemTime>,
}

impl Login {
    pub fn read(path: &Path) -> anyhow::Result<Self> {
        let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let fields = serde_json::from_slice(&std::fs::read(path)?)?;
        Ok(Self { path: path.to_path_buf(), fields, modified })
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        files::write_json(&self.path, &self.fields)?;
        self.modified = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        Ok(())
    }

    pub fn get(&self, key: &str) -> &str {
        self.fields.get(key).and_then(Value::as_str).map_or("", str::trim)
    }

    pub fn set(&mut self, key: &str, value: impl Into<Value>) {
        self.fields.insert(key.into(), value.into());
    }

    pub fn kind(&self) -> &str {
        self.get("type")
    }

    pub fn disabled(&self) -> bool {
        self.fields.get("disabled").and_then(Value::as_bool).unwrap_or(false)
    }

    /// When the access token expires (unix ms).
    pub fn expires_at(&self) -> Option<i64> {
        files::parse_ms(self.get("expired"))
    }

    /// When the login itself runs out and needs a new sign-in (unix ms).
    pub fn sign_in_until(&self) -> Option<i64> {
        files::parse_ms(self.get("refresh_expired"))
    }

    /// The file name, which management calls use as the credential's id.
    pub fn name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// Every login in `dir`, skipping files that aren't one.
    pub fn read_dir(dir: &Path) -> Vec<Self> {
        let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
        let mut logins: Vec<Self> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .filter_map(|p| Self::read(&p).ok())
            .filter(|l| !l.kind().is_empty())
            .collect();
        logins.sort_by(|a, b| a.path.cmp(&b.path));
        logins
    }
}

/// A Claude plan account in the pool: its login and what the proxy knows about its limits.
#[derive(Debug)]
pub struct Account {
    pub login: Login,
    pub reading: Reading,
    /// The last reading written to the history, so it records only real moves.
    pub logged: Reading,
    pub cloud: Option<Cloud>,
    pub resets: Vec<Reset>,
    pub at_limit: Option<AtLimit>,
    pub resets_read_at: i64,
    pub plan: Option<Plan>,
    /// Anthropic asked the sampler to back off until then.
    pub sample_after: i64,
    pub needs_reauth: bool,
    /// The account sits out new requests until then, after an error it caused.
    pub cool_until: i64,
    pub claim: Option<Claim>,
    pub spent: Option<Spent>,
}

impl Account {
    pub fn new(login: Login) -> Self {
        Self {
            login,
            reading: Reading::default(),
            logged: Reading::default(),
            cloud: None,
            resets: Vec::new(),
            at_limit: None,
            resets_read_at: 0,
            plan: None,
            sample_after: 0,
            needs_reauth: false,
            cool_until: 0,
            claim: None,
            spent: None,
        }
    }

    /// The account's id: its Claude account UUID, or its file name before the profile is read.
    pub fn id(&self) -> String {
        match self.login.get("account_uuid") {
            "" => self.login.name(),
            id => id.to_string(),
        }
    }

    pub fn label(&self) -> &str {
        match self.login.get("email") {
            "" => self.login.get("account_uuid"),
            email => email,
        }
    }

    /// The reading with a week that still shows usage from before a spent reset replaced by
    /// the week the reset started. Anthropic's usage endpoint can trail a spend by a while.
    pub fn settled(&self, mut r: Reading, now: i64) -> (Reading, bool) {
        if let Some(s) = &self.spent
            && now < s.until
            && r.week.is_some_and(|w| w.used >= s.week)
        {
            r.week = Some(crate::pool::Window { used: 0.0, resets_at: s.week_ends });
            return (r, true);
        }
        (r, false)
    }

    /// Takes a new reading if it is newer than the one held.
    pub fn observe(&mut self, r: Reading, now: i64) {
        if r.at > self.reading.at {
            self.reading = self.settled(r, now).0;
        }
    }
}

/// A cloud credit balance on the account.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Cloud {
    pub limit: Option<f64>,
    pub used: Option<f64>,
    pub remaining: Option<f64>,
    pub expires_at: Option<i64>,
    pub sampled_at: i64,
}

/// Whether Anthropic offers a reset at the limit right now.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AtLimit {
    pub available: bool,
    pub next_at: Option<i64>,
}

/// The account's subscription, from its profile.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub tier: Option<String>,
    pub status: Option<String>,
    pub since: Option<i64>,
    pub at: i64,
}

/// A reset claim Anthropic hasn't settled, retried under the same request id so a retry
/// can't spend a second reset.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Claim {
    pub grant: String,
    pub request_id: String,
    pub sent: bool,
    #[serde(skip)]
    pub retry_at: i64,
    #[serde(skip)]
    pub told: bool,
}

/// A reset the pool spent, which the usage endpoint may not show yet.
#[derive(Clone, Debug)]
pub struct Spent {
    pub week: f64,
    pub week_ends: i64,
    pub until: i64,
}

/// An API key with credit.
#[derive(Debug)]
pub struct ApiKey {
    pub name: String,
    /// The id CLIProxyAPI gave this key, so records keep joining across the switch.
    pub id: String,
    pub key: String,
    pub prefix: String,
    pub priority: i64,
    pub cool_until: i64,
}

impl ApiKey {
    pub fn load(config: &ApiKeyConfig) -> anyhow::Result<Self> {
        let key = read_secret(&config.key_file)?;
        Ok(Self {
            name: config.name.clone(),
            id: api_key_id(&key, &config.prefix),
            key,
            prefix: config.prefix.clone(),
            priority: config.priority,
            cool_until: 0,
        })
    }
}

/// `claude:apikey:<hash>`, as CLIProxyAPI derives it from the key and its settings.
pub fn api_key_id(key: &str, prefix: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"claude:apikey");
    for part in [key, "", "", prefix, ""] {
        hash.update([0]);
        hash.update(part.trim().as_bytes());
    }
    format!("claude:apikey:{}", &hex(&hash.finalize())[..12])
}
