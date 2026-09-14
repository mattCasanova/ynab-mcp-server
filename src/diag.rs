//! Local-only diagnostics. Warnings and errors go to a redacted log file next to the journal.
//! `ynab-mcp report` and the `diagnostic_report` tool turn that log into a redacted issue
//! report plus a prefilled GitHub new-issue link. Nothing is ever sent without the user
//! opening that link themselves.
//!
//! Logging policy: never log the token, payees, memos, amounts, or category names. Ids are
//! allowed at the call site but are redacted before they reach the file.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::paths;

pub const ISSUES_URL: &str = "https://github.com/mattCasanova/ynab-mcp/issues/new";
const REPORT_LOG_LINES: usize = 40;
const MAX_URL_BODY: usize = 6000;

pub fn log_path() -> Result<PathBuf> {
    Ok(paths::data_dir()?.join("ynab-mcp.log"))
}

/// stderr gets everything at the RUST_LOG level (default info); the file gets warn and above,
/// redacted. Returns the log path, or None with a stderr note if the file could not be opened.
pub fn init_logging() -> Option<PathBuf> {
    let filter = match EnvFilter::try_from_default_env() {
        Ok(f) => f,
        Err(e) => {
            if std::env::var_os("RUST_LOG").is_some() {
                eprintln!("ynab-mcp: RUST_LOG is not a valid filter ({e}); using info");
            }
            EnvFilter::new("info")
        }
    };
    let stderr_layer = fmt::layer()
        .with_writer(io::stderr)
        .with_ansi(false)
        .with_filter(filter);

    let file_layer = match open_log_file() {
        Ok((path, file)) => {
            let layer = fmt::layer()
                .with_writer(RedactingWriter::new(file))
                .with_ansi(false)
                .with_target(true)
                .with_filter(EnvFilter::new("warn"));
            (Some(path), Some(layer))
        }
        Err(e) => {
            eprintln!("ynab-mcp: cannot open the error log ({e:#}); logging to stderr only");
            (None, None)
        }
    };
    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer.1)
        .init();
    file_layer.0
}

fn open_log_file() -> Result<(PathBuf, File)> {
    let path = log_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    Ok((path, file))
}

/// Wraps the log file so every line is redacted before it touches disk.
struct RedactingWriter {
    file: Mutex<File>,
}

impl RedactingWriter {
    fn new(file: File) -> Self {
        Self {
            file: Mutex::new(file),
        }
    }
}

impl<'a> fmt::MakeWriter<'a> for RedactingWriter {
    type Writer = RedactingHandle<'a>;
    fn make_writer(&'a self) -> Self::Writer {
        RedactingHandle { writer: self }
    }
}

struct RedactingHandle<'a> {
    writer: &'a RedactingWriter,
}

impl Write for RedactingHandle<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        let redacted = redact(&text);
        let mut file = self
            .writer
            .file
            .lock()
            .map_err(|_| io::Error::other("log file lock poisoned"))?;
        file.write_all(redacted.as_bytes())?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        let mut file = self
            .writer
            .file
            .lock()
            .map_err(|_| io::Error::other("log file lock poisoned"))?;
        file.flush()
    }
}

fn is_hex(c: char) -> bool {
    c.is_ascii_hexdigit()
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => is_hex(c),
        })
}

fn looks_like_secret(s: &str) -> bool {
    s.len() >= 32 && s.chars().all(is_hex)
}

/// Replace UUIDs with `<id>`, long hex runs with `<secret>`, and anything after `Bearer` with
/// `<secret>`. Works on runs of [0-9A-Fa-f-] so surrounding punctuation is preserved.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let mut after_bearer = false;
    let flush_run = |run: &mut String, out: &mut String| {
        if looks_like_uuid(run) {
            out.push_str("<id>");
        } else if looks_like_secret(run) {
            out.push_str("<secret>");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for word_or_sep in split_keep_separators(text) {
        if after_bearer && !word_or_sep.trim().is_empty() {
            out.push_str("<secret>");
            after_bearer = false;
            continue;
        }
        if word_or_sep == "Bearer" {
            out.push_str(word_or_sep);
            after_bearer = true;
            continue;
        }
        for c in word_or_sep.chars() {
            if is_hex(c) || c == '-' {
                run.push(c);
            } else {
                flush_run(&mut run, &mut out);
                out.push(c);
            }
        }
        flush_run(&mut run, &mut out);
    }
    out
}

fn split_keep_separators(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if start < i {
                parts.push(&text[start..i]);
            }
            parts.push(&text[i..i + c.len_utf8()]);
            start = i + c.len_utf8();
        }
    }
    if start < text.len() {
        parts.push(&text[start..]);
    }
    parts
}

pub struct ReportInput {
    pub token_source: Option<String>,
    pub allow_writes: Option<bool>,
    pub config_error: Option<String>,
    pub config_path: PathBuf,
    pub journal_path: PathBuf,
}

pub fn build_report(input: &ReportInput, log_path: Option<&Path>) -> Result<String> {
    let mut body = String::new();
    body.push_str(&format!("ynab-mcp {}\n", env!("CARGO_PKG_VERSION")));
    body.push_str(&format!(
        "os: {} {}\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    body.push_str(&format!(
        "config: {} ({})\n",
        if input.config_path.exists() {
            "present"
        } else {
            "absent"
        },
        input.config_path.display()
    ));
    if let Some(e) = &input.config_error {
        body.push_str(&format!("config error: {}\n", redact(e)));
    }
    if let Some(src) = &input.token_source {
        body.push_str(&format!("token source: {src}\n"));
    }
    if let Some(w) = input.allow_writes {
        body.push_str(&format!("writes: {w}\n"));
    }
    body.push_str(&format!(
        "journal: {}\n",
        if input.journal_path.exists() {
            "present"
        } else {
            "absent"
        }
    ));
    body.push_str("\nrecent warnings/errors (redacted):\n");
    match log_path {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => {
                let lines: Vec<&str> = text.lines().collect();
                let tail = &lines[lines.len().saturating_sub(REPORT_LOG_LINES)..];
                if tail.is_empty() {
                    body.push_str("(log is empty)\n");
                }
                for line in tail {
                    body.push_str(&redact(line));
                    body.push('\n');
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => body.push_str("(no log file yet)\n"),
            Err(e) => body.push_str(&format!("(could not read log: {e})\n")),
        },
        None => body.push_str("(error log is disabled)\n"),
    }
    body.push_str("\nWhat happened:\n\n(describe what you were doing and what you expected)\n");
    Ok(body)
}

pub fn issue_url(title: &str, body: &str) -> String {
    let body: String = body.chars().take(MAX_URL_BODY).collect();
    format!(
        "{ISSUES_URL}?title={}&body={}",
        percent_encode(title),
        percent_encode(&body)
    )
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_uuids_and_secrets_but_keeps_context() {
        let line = "undo: row a1b2c3d4-e5f6-7890-abcd-ef1234567890 gone (account 0123456789abcdef0123456789abcdef01234567) status=404";
        assert_eq!(
            redact(line),
            "undo: row <id> gone (account <secret>) status=404"
        );
    }

    #[test]
    fn redacts_bearer_tokens() {
        assert_eq!(
            redact("Authorization: Bearer abc.def-123 ok"),
            "Authorization: Bearer <secret> ok"
        );
    }

    #[test]
    fn leaves_ordinary_words_and_short_hex_alone() {
        assert_eq!(
            redact("deadbeef cafe 2026-09-14 plan_id=last-used"),
            "deadbeef cafe 2026-09-14 plan_id=last-used"
        );
    }

    #[test]
    fn percent_encodes_for_a_github_url() {
        assert_eq!(percent_encode("a b&c=d\n"), "a%20b%26c%3Dd%0A");
        assert!(issue_url("t", "b").starts_with(ISSUES_URL));
    }
}
