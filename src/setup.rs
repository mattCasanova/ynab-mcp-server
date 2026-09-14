//! `ynab-mcp setup`: token in, verified against YNAB, stored in the OS secret store,
//! config written, and the Claude Code registration line printed.

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result, bail};

use crate::config::FileConfig;
use crate::{paths, secrets, ynab};

pub struct SetupOptions {
    /// Store the token in config.toml (mode 600) instead of the OS secret store.
    pub token_in_config: bool,
}

fn ask(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .context("read stdin")?;
    Ok(line.trim().to_string())
}

fn ask_yes_no(prompt: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    let answer = ask(&format!("{prompt} {hint}: "))?;
    Ok(match answer.to_ascii_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        "n" | "no" => false,
        other => bail!("expected y or n, got {other:?}"),
    })
}

pub async fn run(opts: SetupOptions) -> Result<()> {
    let config_path = paths::config_file()?;
    let mut file = FileConfig::load(&config_path)?.unwrap_or_default();

    println!("ynab-mcp setup");
    println!(
        "  Create a Personal Access Token in YNAB: Account Settings -> Developer Settings -> New Token."
    );
    let token = rpassword::prompt_password("  Paste the token (input hidden): ")
        .context("read token")?
        .trim()
        .to_string();
    if token.is_empty() {
        bail!("no token entered");
    }

    print!("  Checking with YNAB... ");
    io::stdout().flush()?;
    let client = ynab::Client::new(&token, "last-used".to_string())?;
    let plans = client.plans().await.context("token rejected by YNAB")?;
    println!("ok, {} plan(s):", plans.len());
    for (i, p) in plans.iter().enumerate() {
        println!("    {}. {}  ({})", i + 1, p.name, p.id);
    }
    let choice = ask("  Plan number to use, or Enter for last-used: ")?;
    let plan_id = if choice.is_empty() {
        "last-used".to_string()
    } else {
        let n: usize = choice.parse().context("plan choice must be a number")?;
        plans
            .get(
                n.checked_sub(1)
                    .context("plan choice must be 1 or higher")?,
            )
            .map(|p| p.id.clone())
            .with_context(|| format!("no plan number {n}"))?
    };

    let allow_writes = ask_yes_no(
        "  Allow writes (create_transactions and undo tools)? You can flip this later in the config.",
        false,
    )?;

    let mut stored_in_config = opts.token_in_config || cfg!(windows);
    if !stored_in_config {
        print!("  Storing the token in the {}... ", secrets::store_name());
        io::stdout().flush()?;
        match secrets::set(&token) {
            Ok(()) => println!("ok"),
            Err(e) => {
                println!("failed: {e:#}");
                stored_in_config = ask_yes_no(
                    "  Store it in config.toml instead (file is made mode 600)?",
                    false,
                )?;
                if !stored_in_config {
                    bail!("token not stored; nothing written");
                }
            }
        }
    }

    file.plan_id = Some(plan_id);
    file.allow_writes = Some(allow_writes);
    file.access_token = if stored_in_config { Some(token) } else { None };
    file.save(&config_path)?;
    println!("  Wrote {}", config_path.display());
    println!("  Journal: {}", paths::default_journal()?.display());

    let exe = std::env::current_exe().context("locate this binary")?;
    println!();
    println!("Register with Claude Code:");
    println!("  claude mcp add --scope user ynab -- {}", exe.display());
    println!();
    println!("Then start a new session and ask for the `status` tool.");
    Ok(())
}
