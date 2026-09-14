//! XDG-style user paths, used on macOS and Linux alike so docs stay the same on both.

use std::path::PathBuf;

use anyhow::{Context, Result};

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

pub fn config_dir() -> Result<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("APPDATA")
            .map(|d| PathBuf::from(d).join("ynab-mcp"))
            .context("APPDATA is not set");
    }
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir).join("ynab-mcp")),
        _ => Ok(home()?.join(".config/ynab-mcp")),
    }
}

pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

pub fn data_dir() -> Result<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("LOCALAPPDATA")
            .map(|d| PathBuf::from(d).join("ynab-mcp"))
            .context("LOCALAPPDATA is not set");
    }
    match std::env::var_os("XDG_DATA_HOME") {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir).join("ynab-mcp")),
        _ => Ok(home()?.join(".local/share/ynab-mcp")),
    }
}

pub fn default_journal() -> Result<PathBuf> {
    Ok(data_dir()?.join("journal.jsonl"))
}

/// Expand a leading `~/` so config files can use it.
pub fn expand_tilde(path: &str) -> Result<PathBuf> {
    match path.strip_prefix("~/") {
        Some(rest) => Ok(home()?.join(rest)),
        None => Ok(PathBuf::from(path)),
    }
}
