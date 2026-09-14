//! Settings come from three layers. Highest wins:
//! 1. environment variables (YNAB_ACCESS_TOKEN, YNAB_PLAN_ID, YNAB_MCP_ALLOW_WRITES, YNAB_MCP_JOURNAL)
//! 2. ~/.config/ynab-mcp/config.toml
//! 3. built-in defaults
//!
//! The token additionally falls back to the OS secret store when neither layer has it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{paths, secrets};

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// Plan (budget) id, or "last-used".
    pub plan_id: Option<String>,
    pub allow_writes: Option<bool>,
    /// Journal path; `~/` is expanded.
    pub journal: Option<String>,
    /// Fallback when the OS secret store is not available. The file must be mode 600.
    pub access_token: Option<String>,
    /// Days to keep closed months in the on-disk cache. 0 disables. Default 30.
    pub cache_ttl_days: Option<i64>,
}

impl FileConfig {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let config: Self =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        if config.access_token.is_some() {
            ensure_private(path)?;
        }
        Ok(Some(config))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serialize config")?;
        std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
        set_private(path)
    }
}

#[cfg(unix)]
fn ensure_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        bail!(
            "{} holds access_token but is readable by others (mode {:o}); run: chmod 600 {}",
            path.display(),
            mode & 0o777,
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 600 {}", path.display()))
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> Result<()> {
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    Env,
    ConfigFile,
    SecretStore,
}

pub struct Config {
    pub access_token: String,
    pub token_source: TokenSource,
    pub plan_id: String,
    pub allow_writes: bool,
    pub journal_path: PathBuf,
    pub config_path: PathBuf,
    pub cache_ttl_days: i64,
}

fn env_flag(name: &str) -> Option<bool> {
    match std::env::var(name).ok()?.trim() {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        other => {
            tracing::warn!(var = name, value = other, "unrecognized boolean; ignoring");
            None
        }
    }
}

fn env_string(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_path = paths::config_file()?;
        let file = match FileConfig::load(&config_path)? {
            Some(f) => f,
            None => {
                tracing::info!(path = %config_path.display(), "no config file; using env vars and defaults");
                FileConfig::default()
            }
        };
        Self::merge(file, config_path)
    }

    fn merge(file: FileConfig, config_path: PathBuf) -> Result<Self> {
        let (access_token, token_source) = if let Some(t) = env_string("YNAB_ACCESS_TOKEN") {
            (t, TokenSource::Env)
        } else if let Some(t) = file.access_token.clone() {
            (t, TokenSource::ConfigFile)
        } else {
            match secrets::get()? {
                Some(t) => (t, TokenSource::SecretStore),
                None => bail!(
                    "no YNAB token found. Tried: YNAB_ACCESS_TOKEN, access_token in {}, and the {}. \
                     Run `ynab-mcp setup`.",
                    config_path.display(),
                    secrets::store_name()
                ),
            }
        };
        let plan_id = env_string("YNAB_PLAN_ID")
            .or(file.plan_id)
            .unwrap_or_else(|| "last-used".to_string());
        let allow_writes = env_flag("YNAB_MCP_ALLOW_WRITES")
            .or(file.allow_writes)
            .unwrap_or(false);
        let journal_path = match env_string("YNAB_MCP_JOURNAL").or(file.journal) {
            Some(p) => paths::expand_tilde(&p)?,
            None => paths::default_journal()?,
        };
        let cache_ttl_days = match std::env::var("YNAB_MCP_CACHE_TTL_DAYS") {
            Ok(v) => match v.trim().parse::<i64>() {
                Ok(n) if n >= 0 => n,
                _ => bail!("YNAB_MCP_CACHE_TTL_DAYS must be a non-negative integer, got {v:?}"),
            },
            Err(_) => file.cache_ttl_days.unwrap_or(30),
        };
        if cache_ttl_days < 0 {
            bail!("cache_ttl_days must be 0 or more, got {cache_ttl_days}");
        }
        Ok(Self {
            access_token,
            token_source,
            plan_id,
            allow_writes,
            journal_path,
            config_path,
            cache_ttl_days,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_keys() {
        let err = toml::from_str::<FileConfig>("plan_id = \"x\"\nwrites = true\n").unwrap_err();
        assert!(err.to_string().contains("writes"));
    }

    #[test]
    fn parses_every_field() {
        let c: FileConfig =
            toml::from_str("plan_id = \"abc\"\nallow_writes = true\njournal = \"~/j.jsonl\"\n")
                .unwrap();
        assert_eq!(c.plan_id.as_deref(), Some("abc"));
        assert_eq!(c.allow_writes, Some(true));
        assert_eq!(c.journal.as_deref(), Some("~/j.jsonl"));
        assert!(c.access_token.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn config_with_token_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ynab-mcp-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "access_token = \"secret\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(FileConfig::load(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            FileConfig::load(&path)
                .unwrap()
                .unwrap()
                .access_token
                .as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn save_round_trips_and_is_private() {
        let dir = std::env::temp_dir().join(format!("ynab-mcp-save-{}", std::process::id()));
        let path = dir.join("nested/config.toml");
        let c = FileConfig {
            plan_id: Some("p".into()),
            allow_writes: Some(false),
            journal: None,
            access_token: None,
            cache_ttl_days: None,
        };
        c.save(&path).unwrap();
        let back = FileConfig::load(&path).unwrap().unwrap();
        assert_eq!(back.plan_id.as_deref(), Some("p"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
