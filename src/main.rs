mod analytics;
mod analytics_tools;
mod cache;
mod config;
mod csvio;
mod diag;
mod journal;
mod money;
mod paths;
mod pdf_import;
mod pdf_text;
mod reconcile;
mod secrets;
mod server;
mod setup;
mod statement;
mod transfer_tools;
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
  ynab-mcp pdf-to-csv <statement.pdf> <out.csv> [--account NAME|LAST4] [--year YYYY] [--overwrite]
                                turn a bank statement PDF into a Date,Description,Amount CSV
                                on this machine; the PDF text never leaves it
  ynab-mcp cache clear          delete the cached month data (safe; it is refetched)
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
        ["pdf-to-csv", rest @ ..] => pdf_to_csv_cli(rest),
        ["cache", "clear"] => {
            let dir = paths::cache_dir()?;
            let mut removed = 0;
            if dir.exists() {
                for plan in std::fs::read_dir(&dir)? {
                    let plan_dir = plan?.path().join("months");
                    removed += cache::MonthCache::new(plan_dir, 1).clear()?;
                }
            }
            println!(
                "removed {removed} cached month file(s) under {}",
                dir.display()
            );
            Ok(())
        }
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

fn pdf_to_csv_cli(args: &[&str]) -> Result<()> {
    let mut positional: Vec<&str> = Vec::new();
    let mut account: Option<&str> = None;
    let mut year: Option<i32> = None;
    let mut overwrite = false;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--account" => {
                account = Some(
                    args.get(i + 1)
                        .copied()
                        .ok_or_else(|| anyhow::anyhow!("--account needs a value"))?,
                );
                i += 2;
            }
            "--year" => {
                year = Some(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow::anyhow!("--year needs a value"))?
                        .parse()
                        .map_err(|_| anyhow::anyhow!("--year must be a number"))?,
                );
                i += 2;
            }
            "--overwrite" => {
                overwrite = true;
                i += 1;
            }
            other if other.starts_with("--") => anyhow::bail!("unknown flag {other}\n\n{USAGE}"),
            other => {
                positional.push(other);
                i += 1;
            }
        }
    }
    let [pdf, csv] = positional.as_slice() else {
        anyhow::bail!("pdf-to-csv needs <statement.pdf> <out.csv>\n\n{USAGE}");
    };
    let summary = pdf_import::convert(
        &pdf_import::resolve(pdf)?,
        &pdf_import::resolve(csv)?,
        &pdf_import::Options {
            account,
            year,
            overwrite,
        },
    )?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
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
        cache_root: paths::cache_dir()?,
        cache_ttl_days: config.cache_ttl_days,
    };
    let service = YnabServer::new(client, journal, config.allow_writes, meta)
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("serve error: {e:?}"))?;
    service.waiting().await?;
    Ok(())
}
