# ynab-mcp

A small MCP server for [YNAB](https://www.ynab.com), written in Rust on the official
[`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) SDK. Read-only by construction;
one write tool exists and is only registered when you opt in.

Plan and use cases: `~/workspace/ynab/ynab-mcp-plan.md`.

## Tools

| Tool | Notes |
|---|---|
| `status` | plans on the token, plan in use, write mode, rate-limit usage |
| `list_accounts` | open accounts + balances (`include_closed`) |
| `list_categories` | groups → categories, current month numbers + goals (`include_hidden`) |
| `get_month` | one month, `YYYY-MM-01` or `current` |
| `list_transactions` | by account, category, date range, or `kind` = `uncategorized` / `unapproved` |
| `list_scheduled_transactions` | upcoming recurring transactions |
| `reconcile_account` | bank rows in → matched / missing in YNAB / in YNAB but not at the bank |
| `list_write_history` | every write batch any agent has made, with status open / partially_undone / undone |
| `diagnostic_report` | redacted local diagnostics + a prefilled GitHub issue link; sends nothing |
| `create_transactions` | **write, gated** by `YNAB_MCP_ALLOW_WRITES=1`; dedupe `import_id`, lands unapproved, journaled |
| `undo_batch`, `undo_last` | **write, gated.** Two-phase: preview first, then `confirm=true`. Flagged rows also need `force` |

Amounts are decimal strings in the plan currency; outflows are negative.

## Install

Prebuilt binaries for macOS (Apple Silicon and Intel), Linux x86_64, and Windows x86_64 are the
plan, with a one-line installer. With Rust installed:

```
cargo install ynab-mcp-server
```

The crate is `ynab-mcp-server` (the shorter name was taken); the binary is `ynab-mcp`.

## Setup

One command. It asks for your token, checks it against YNAB, lets you pick a plan, stores the
token in the OS secret store, writes the config, and prints the Claude Code registration line.

```
ynab-mcp setup
```

Then register with Claude Code (setup prints the exact line with the binary's full path):

```
claude mcp add --scope user ynab -- ~/.cargo/bin/ynab-mcp
```

Start a new session and ask for the `status` tool.

### Where things live

| What | macOS / Linux | Windows |
|---|---|---|
| Config | `~/.config/ynab-mcp/config.toml` (mode 600) | `%APPDATA%\ynab-mcp\config.toml` |
| Token | macOS Keychain item `ynab-mcp`; Linux secret-service via `secret-tool` | config file only |
| Journal | `~/.local/share/ynab-mcp/journal.jsonl` | `%LOCALAPPDATA%\ynab-mcp\journal.jsonl` |
| Error log | `~/.local/share/ynab-mcp/ynab-mcp.log` | `%LOCALAPPDATA%\ynab-mcp\ynab-mcp.log` |

`XDG_CONFIG_HOME` and `XDG_DATA_HOME` are honored on macOS and Linux.

### Config file

```toml
plan_id = "last-used"     # or a plan id from `status`
allow_writes = false      # true registers create_transactions, undo_batch, undo_last
# journal = "~/somewhere/journal.jsonl"
# access_token = "..."    # only if the secret store is unavailable; file must be mode 600
```

Environment variables override the file: `YNAB_ACCESS_TOKEN`, `YNAB_PLAN_ID`,
`YNAB_MCP_ALLOW_WRITES`, `YNAB_MCP_JOURNAL`. Token lookup order is env var, then
`access_token` in the config, then the secret store. Unknown keys in the config are an error.

### Linux notes

`secret-tool` comes from `libsecret` (`libsecret-tools` on Debian/Ubuntu, `libsecret` on Arch)
and needs a running secret service such as GNOME Keyring or KeePassXC. On a window-manager
setup without one, run `ynab-mcp setup --token-in-config` to keep the token in the config file
instead. The server refuses to start if that file is readable by other users.

### Windows notes

There is no secret-store integration on Windows; `ynab-mcp setup` stores the token in the
config file under `%APPDATA%`, which is private to your user account by default. WSL users
should use the Linux binary inside WSL.

## Undo and the write journal

Every write appends one batch record to an append-only JSONL journal
(`~/.local/share/ynab-mcp/journal.jsonl`, or `YNAB_MCP_JOURNAL`). Undo appends its own record
instead of editing history, so the file is a complete audit trail and any agent in any session
can undo what another one did. An exclusive OS file lock is held across the whole
read-check-delete-append cycle, so two server processes cannot double-undo or interleave.

YNAB has no server-side undo, so the journal is the only record. Undo therefore runs in two
phases:

1. `undo_batch` / `undo_last` without `confirm` fetches the current state of every row and
   returns a preview. Each row is `deletable`, or flagged: `missing` (row or its account is
   gone from YNAB), `reconciled`, or `changed_since_created` (amount, date, or account differs).
   Nothing is deleted or journaled.
2. With `confirm=true` the deletable rows are deleted. Flagged `reconciled` / `changed` rows
   are skipped unless `force=true`. `missing` rows are recorded as resolved. A batch stays
   open until every row is deleted or missing, so re-running is safe.

## Errors, logs, and filing an issue

No telemetry. Nothing leaves your machine unless you open a link yourself.

- Every warning and error is written to `~/.local/share/ynab-mcp/ynab-mcp.log`
  (`%LOCALAPPDATA%\ynab-mcp\ynab-mcp.log` on Windows), redacted before it hits disk: ids
  become `<id>`, anything token-shaped becomes `<secret>`. Payees, memos, amounts, and category
  names are never logged in the first place.
- Every error handed back to the agent is also logged, so the log has the full story even
  when the conversation moved on. Bad tool arguments log at warn; YNAB and journal failures at
  error.
- `ynab-mcp report` (or the `diagnostic_report` tool from inside a session) prints a redacted
  report: version, OS, whether the config and journal exist, token source, writes flag, and the
  last 40 log lines. It ends with a prefilled GitHub new-issue link. Read the report, then open
  the link and submit if you want to.

## Develop

```
cargo test
cargo clippy --all-targets
```

Smoke test the wire protocol without a real token:

```
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  | YNAB_ACCESS_TOKEN=x ./target/release/ynab-mcp
```

## Notes

- YNAB's API renamed budgets to **plans** (`/v1/plans/{plan_id}`); this client uses the new paths.
- Rate limit is 200 requests per hour per token. `status` shows the running count.
- Money is milliunits on the wire (`i64`, 1000 = 1.00) and converted exactly once in the output layer.
