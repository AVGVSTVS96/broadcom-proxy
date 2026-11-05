use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{files, pool::Policy};

/// The proxy's one config file. Secrets live in their own files, named here.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounts_file: Option<PathBuf>,
    #[serde(default)]
    pub api_keys: Vec<ApiKeyConfig>,
    #[serde(default)]
    pub policy: Policy,
    /// How much faster Agent SDK and `claude -p` sessions drain the five-hour limit.
    #[serde(default = "default_sdk_factor")]
    pub agent_sdk_factor: f64,
    /// Run with a line about each held reset spent or refused, as `sh -c '<command> "$1"'`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_command: Option<String>,
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

fn default_sdk_factor() -> f64 {
    1.7
}

impl Config {
    pub fn load(path: &Path) -> Result<(Self, String)> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
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
        let mut doc: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
        doc["policy"] = serde_json::to_value(policy)?;
        let text = serde_json::to_string_pretty(&doc)? + "\n";
        files::write_atomic(path, text.as_bytes())?;
        Ok(revision(text.as_bytes()))
    }
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
