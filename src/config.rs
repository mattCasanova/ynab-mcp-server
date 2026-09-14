use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};

pub struct Config {
    pub access_token: String,
    pub plan_id: String,
    pub allow_writes: bool,
    pub journal_path: PathBuf,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let access_token = env::var("YNAB_ACCESS_TOKEN")
            .context("YNAB_ACCESS_TOKEN is not set; see scripts/ynab-mcp.sh")?;
        if access_token.trim().is_empty() {
            anyhow::bail!("YNAB_ACCESS_TOKEN is empty");
        }
        let plan_id = env::var("YNAB_PLAN_ID").unwrap_or_else(|_| "last-used".to_string());
        let allow_writes = matches!(
            env::var("YNAB_MCP_ALLOW_WRITES").as_deref(),
            Ok("1") | Ok("true")
        );
        let journal_path = crate::journal::Journal::default_path()?;
        Ok(Self {
            access_token,
            plan_id,
            allow_writes,
            journal_path,
        })
    }
}
