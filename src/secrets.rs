//! Token storage in the OS secret store: macOS Keychain via `security`, Linux
//! secret-service via `secret-tool`. The token travels over stdin, never argv.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

pub const SERVICE: &str = "ynab-mcp";

pub fn store_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macOS Keychain"
    } else {
        "secret-service (secret-tool)"
    }
}

/// Ok(None) when the store has no item; Err when the store itself is unusable.
pub fn get() -> Result<Option<String>> {
    let output = if cfg!(target_os = "macos") {
        Command::new("security")
            .args(["find-generic-password", "-s", SERVICE, "-w"])
            .stderr(Stdio::piped())
            .output()
            .context("run `security`")?
    } else {
        match Command::new("secret-tool")
            .args(["lookup", "service", SERVICE])
            .stderr(Stdio::piped())
            .output()
        {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!("secret-tool is not installed (package libsecret-tools / libsecret)")
            }
            Err(e) => return Err(e).context("run `secret-tool`"),
        }
    };
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() && !token.is_empty() {
        return Ok(Some(token));
    }
    if output.status.success() {
        tracing::warn!(
            "{} returned an empty token; treating as not found",
            store_name()
        );
        return Ok(None);
    }
    // security: 44 = item not found. secret-tool: 1 = not found (but also other failures).
    if output.status.code() == Some(44) || output.status.code() == Some(1) {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        tracing::info!(code = ?output.status.code(), stderr, "{}: no item for {SERVICE}", store_name());
        return Ok(None);
    }
    bail!(
        "{} lookup failed: {}",
        store_name(),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

pub fn set(token: &str) -> Result<()> {
    if cfg!(windows) {
        bail!("no secret store integration on Windows; use `ynab-mcp setup --token-in-config`");
    }
    // The macOS path builds a `security -i` command line from the token, so refuse anything
    // that could break out of it. YNAB tokens are plain ASCII with no whitespace or quotes.
    if token
        .chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\'' || c == '\\' || !c.is_ascii_graphic())
    {
        bail!(
            "token contains whitespace, quotes, or non-printable characters; that is not a YNAB token"
        );
    }
    let account = match std::env::var("USER") {
        Ok(u) if !u.trim().is_empty() => u,
        _ => {
            tracing::warn!("USER is not set; storing the token under account \"ynab\"");
            "ynab".to_string()
        }
    };
    let (program, args, stdin_body) = if cfg!(target_os = "macos") {
        (
            "security",
            vec!["-i".to_string()],
            format!("add-generic-password -a {account} -s {SERVICE} -U -w {token}\n"),
        )
    } else {
        (
            "secret-tool",
            vec![
                "store".to_string(),
                "--label=ynab-mcp personal access token".to_string(),
                "service".to_string(),
                SERVICE.to_string(),
            ],
            token.to_string(),
        )
    };
    let mut child = Command::new(program)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run `{program}`"))?;
    child
        .stdin
        .take()
        .context("open stdin")?
        .write_all(stdin_body.as_bytes())
        .context("write token to stdin")?;
    let output = child.wait_with_output().context("wait for secret store")?;
    if !output.status.success() {
        bail!(
            "{} store failed: {}",
            store_name(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
