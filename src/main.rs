mod config;
mod journal;
mod money;
mod reconcile;
mod server;
mod ynab;

use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::server::YnabServer;

#[tokio::main]
async fn main() -> Result<()> {
    // stdout is the MCP channel; every log line goes to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let config = Config::from_env()?;
    let client = ynab::Client::new(&config.access_token, config.plan_id.clone())?;
    tracing::info!(plan_id = %config.plan_id, allow_writes = config.allow_writes, "starting ynab-mcp");

    let journal = journal::Journal::new(config.journal_path.clone());
    tracing::info!(journal = %config.journal_path.display(), "write journal");
    let service = YnabServer::new(client, journal, config.allow_writes)
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("serve error: {e:?}"))?;
    service.waiting().await?;
    Ok(())
}
