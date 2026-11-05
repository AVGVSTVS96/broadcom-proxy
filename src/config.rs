use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{files, pool::Policy};

/// The proxy's one config file, every field optional. Secrets live in their own files, named here.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Config {
    pub listen: SocketAddr,
    /// The one process that refreshes OAuth tokens, writes credential files and spends held
    /// resets. A refresh token rotates when used, so a second refresher logs accounts out.
    pub account_owner: bool,
    /// Keys clients may use, one per line.
    pub client_keys_file: PathBuf,
    pub management_key_file: PathBuf,
    /// One JSON file per OAuth login, in CLIProxyAPI's format.
    pub credentials_dir: PathBuf,
    pub state_dir: PathBuf,
    /// The account list other tools read to name accounts by id.
    pub accounts_file: Option<PathBuf>,
    pub api_keys: Vec<ApiKeyConfig>,
    pub policy: Policy,
    /// How much faster Agent SDK and `claude -p` sessions drain the five-hour limit.
    pub agent_sdk_factor: f64,
    /// Run with a line about each held reset spent or refused, as `sh -c '<command> "$1"'`.
    pub reset_command: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: ([127, 0, 0, 1], 8317).into(),
            account_owner: true,
            client_keys_file: "~/.config/broadcom-proxy/client-keys".into(),
            management_key_file: "~/.config/broadcom-proxy/management-key".into(),
            credentials_dir: "~/.config/broadcom-proxy/auths".into(),
            state_dir: "~/.local/state/broadcom-proxy".into(),
            accounts_file: None,
            api_keys: Vec::new(),
            policy: Policy::default(),
            agent_sdk_factor: 1.7,
            reset_command: None,
        }
    }
}

/// An API key, the pool's last resort: tried only after every plan account, and only while
/// `account` has extra usage turned on.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApiKeyConfig {
    pub name: String,
    pub key_file: PathBuf,
    /// The email of the plan login whose extra-usage setting decides whether this key may spend.
    pub account: String,
}

impl Config {
    /// Reads the config at `path`; a missing file means every default.
    pub fn load(path: &Path) -> Result<(Self, String)> {
        let text = read_or_empty(path)?;
        let mut config: Self = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.policy.validate().map_err(anyhow::Error::msg)?;
        for p in [
            &mut config.client_keys_file,
            &mut config.management_key_file,
            &mut config.credentials_dir,
            &mut config.state_dir,
        ] {
            *p = expand_home(p);
        }
        if let Some(p) = &mut config.accounts_file {
            *p = expand_home(p);
        }
        for key in &mut config.api_keys {
            key.key_file = expand_home(&key.key_file);
        }
        Ok((config, revision(text.as_bytes())))
    }

    /// Saves a new policy into the file at `path`, keeping everything else as written, and
    /// returns the new revision.
    pub fn save_policy(path: &Path, policy: Policy) -> Result<String> {
        let mut doc: serde_json::Value = serde_json::from_str(&read_or_empty(path)?)?;
        doc["policy"] = serde_json::to_value(policy)?;
        let text = serde_json::to_string_pretty(&doc)? + "\n";
        files::write_atomic(path, text.as_bytes())?;
        Ok(revision(text.as_bytes()))
    }
}

fn read_or_empty(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok("{}".into()),
        text => text.with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes a new random key to `path` unless a file is already there, and says whether it did.
pub fn ensure_secret(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    let mut key = [0u8; 24];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut key);
    files::write_atomic(path, format!("{}\n", hex(&key)).as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// The config's revision: a hash of its bytes, quoted like an `ETag`.
pub fn revision(bytes: &[u8]) -> String {
    format!("\"{}\"", &hex(&Sha256::digest(bytes))[..16])
}

pub fn read_secret(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text.trim().to_string())
}

pub fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => Path::new(&home).join(rest),
        _ => path.to_path_buf(),
    }
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}
