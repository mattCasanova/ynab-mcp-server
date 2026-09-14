mod analytics;
mod analytics_tools;
mod config;
mod diag;
mod journal;
mod money;
mod paths;
mod reconcile;
mod secrets;
mod server;
mod setup;
mod ynab;

use std::path::Path;

use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};

use crate::config::Config;
use crate::server::YnabServer;

const USAGE: &str = "\
ynab-mcp — MCP server for YNAB

Usage:
  ynab-mcp                      run the MCP server on stdio (what Claude Code launches)
  ynab-mcp setup                store your token, pick a plan, write the config
  ynab-mcp setup --token-in-config
                                same, but keep the token in config.toml (mode 600)
                                instead of the OS secret store
  ynab-mcp report               print a redacted diagnostic report and a prefilled
                                GitHub issue link (nothing is sent; you open the link)
  ynab-mcp --version | --help

Config: ~/.config/ynab-mcp/config.toml   (env vars YNAB_ACCESS_TOKEN, YNAB_PLAN_ID,
        YNAB_MCP_ALLOW_WRITES, YNAB_MCP_JOURNAL override it)
";

#[tokio::main]
async fn main() -> Result<()> {
    // stdout is the MCP channel; logs go to stderr and, redacted, to the local log file.
    let log_path = diag::init_logging();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install ring as the TLS provider once at startup");

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => serve(log_path.as_deref()).await,
        ["setup"] => {
            setup::run(setup::SetupOptions {
                token_in_config: false,
            })
            .await
        }
        ["setup", "--token-in-config"] => {
            setup::run(setup::SetupOptions {
                token_in_config: true,
            })
            .await
        }
        ["report"] => report(log_path.as_deref()),
        ["--version" | "-V"] => {
            println!("ynab-mcp {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        ["--help" | "-h"] => {
            print!("{USAGE}");
            Ok(())
        }
        other => {
            eprintln!("unknown arguments: {}\n\n{USAGE}", other.join(" "));
            std::process::exit(2);
        }
    }
}

fn report(log_path: Option<&std::path::Path>) -> Result<()> {
    let input = report_input();
    let body = diag::build_report(&input, log_path)?;
    println!("{body}");
    println!("File it (opens a prefilled GitHub issue; review before submitting):");
    println!(
        "  {}",
        diag::issue_url("ynab-mcp: <short description>", &body)
    );
    Ok(())
}

/// Everything the report needs, tolerant of a broken config so `report` works when serve won't.
fn report_input() -> diag::ReportInput {
    let config_path = paths::config_file().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "cannot resolve config path");
        std::path::PathBuf::from("<unknown>")
    });
    let journal_path = paths::default_journal().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "cannot resolve journal path");
        std::path::PathBuf::from("<unknown>")
    });
    match Config::load() {
        Ok(c) => diag::ReportInput {
            token_source: Some(format!("{:?}", c.token_source)),
            allow_writes: Some(c.allow_writes),
            config_error: None,
            config_path: c.config_path,
            journal_path: c.journal_path,
        },
        Err(e) => diag::ReportInput {
            token_source: None,
            allow_writes: None,
            config_error: Some(format!("{e:#}")),
            config_path,
            journal_path,
        },
    }
}

async fn serve(log_path: Option<&std::path::Path>) -> Result<()> {
    let config = Config::load()?;
    let client = ynab::Client::new(&config.access_token, config.plan_id.clone())?;
    tracing::info!(
        plan_id = %config.plan_id,
        allow_writes = config.allow_writes,
        token_source = ?config.token_source,
        config = %config.config_path.display(),
        journal = %config.journal_path.display(),
        "starting ynab-mcp"
    );
    let journal = journal::Journal::new(config.journal_path.clone());
    let meta = server::ServerMeta {
        token_source: format!("{:?}", config.token_source),
        config_path: config.config_path.clone(),
        log_path: log_path.map(Path::to_path_buf),
    };
    let service = YnabServer::new(client, journal, config.allow_writes, meta)
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("serve error: {e:?}"))?;
    service.waiting().await?;
    Ok(())
}
