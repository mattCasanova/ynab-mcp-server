# ynab-mcp

An MCP server for [YNAB](https://www.ynab.com), written in Rust on the official
[`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) SDK.

- **Read-only unless you opt in.** Write tools are only registered when you turn them on.
- **Every write is previewed, journaled, and undoable.** Preview first, then confirm. Undo
  works across sessions and agents.
- **No telemetry.** Nothing leaves your machine except calls to the YNAB API.
- **The server counts; the model judges.** Analytics tools return tables and counts, never a
  score or a recommendation.

## Install

macOS or Linux, no Rust needed:

```
curl -fsSL https://raw.githubusercontent.com/mattCasanova/ynab-mcp-server/master/install.sh | sh
```

That downloads the latest release for your machine, checks its SHA-256, and puts it in
`~/.local/bin/ynab-mcp`. Set `YNAB_MCP_INSTALL_DIR` to put it somewhere else.

With Rust installed:

```
cargo install ynab-mcp-server
```

The crate is `ynab-mcp-server`; the binary is `ynab-mcp`.

Windows: download the `x86_64-pc-windows-msvc` zip from the
[releases page](https://github.com/mattCasanova/ynab-mcp-server/releases) and put
`ynab-mcp.exe` on your PATH.

## Setup

```
ynab-mcp setup
```

It asks for your YNAB token, checks it, lets you pick a plan, stores the token in the OS
secret store, writes the config, and prints the registration line for Claude Code:

```
claude mcp add --scope user ynab -- /full/path/to/ynab-mcp
```

Start a new session and ask for the `status` tool.

On Linux the secret store is `secret-tool` from `libsecret`, which needs a running secret
service such as GNOME Keyring or KeePassXC. Without one, run
`ynab-mcp setup --token-in-config` to keep the token in the config file instead. Windows has
no secret-store integration and always uses the config file.

## Tools

Amounts are decimal strings in the plan currency; outflows are negative.

| Tool | What it does |
|---|---|
| `status` | plans on the token, plan in use, write mode, rate-limit usage |
| `list_accounts` | accounts and balances |
| `list_categories` | groups, categories, current month numbers and goals |
| `get_month` | one month's numbers |
| `list_transactions` | by account, category, date range, or uncategorized / unapproved |
| `list_scheduled_transactions` | upcoming recurring transactions |
| `reconcile_account` | bank rows in; matched, missing in YNAB, in YNAB but not at the bank |
| `spending_summary` | category by month actuals, with average and recent-vs-earlier change |
| `recurring_charges` | payees on a cadence, with amount drift and lapsed charges |
| `category_history` | one category by payee: totals, share, monthly series |
| `payee_summary` | payees ranked by outflow |
| `budget_vs_actual` | one month: overspent, over target, assigned but unused, underfunded |
| `goal_analysis` | every target over N months: assigned vs spent, money moved in and out |
| `money_movements` | category-to-category moves with per-category net |
| `pdf_to_csv` | bank statement PDF to a Date, Description, Amount CSV, parsed locally |
| `export_transactions` | CSV or JSON to a path, filtered by date, account, category, or group |
| `import_bank_csv` | bank CSV files in; parse, dedupe, reconcile, and (with writes) create the missing rows |
| `list_write_history` | every write batch, with status open / partially undone / undone |
| `diagnostic_report` | redacted local diagnostics and a prefilled GitHub issue link |

Write tools, registered only when writes are on:

| Tool | What it does |
|---|---|
| `create_transactions` | single-category or split rows; deduped by import id, land unapproved |
| `replace_transaction` | recategorize or re-split one row at the same account, date, payee, and total |
| `assign_money` | per-month changes to categories' assigned amounts |
| `trigger_bank_import` | YNAB's Import button for linked accounts; bank rows, so not journaled |
| `undo_batch`, `undo_last` | reverse any journaled write above |

Create, replace, assign, and undo run in two phases: call without `confirm` for a preview,
then with `confirm=true` to apply. Rows that changed since they were written, or are reconciled, are
skipped on undo unless you pass `force=true`.

## Privacy

- Statement PDFs are parsed on your machine. Account numbers and anything else that looks
  like a long digit run are scrubbed from descriptions, and only row counts, totals, and a
  three-row sample go back to the agent.
- The server reads only the paths it is given and writes only the export path it is given.
- Warnings and errors go to a local log, redacted before they hit disk. Payees, memos,
  amounts, and category names are never logged.
- `ynab-mcp report` (or the `diagnostic_report` tool) prints a redacted report and a
  prefilled GitHub issue link. Nothing is sent unless you open the link.

## Config

```toml
plan_id = "last-used"     # or a plan id from `status`
allow_writes = false      # true registers the write tools
# journal = "~/somewhere/journal.jsonl"
# cache_ttl_days = 30     # closed-month cache; 0 disables
# access_token = "..."    # only if the secret store is unavailable; file must be mode 600
```

Environment variables override the file: `YNAB_ACCESS_TOKEN`, `YNAB_PLAN_ID`,
`YNAB_MCP_ALLOW_WRITES`, `YNAB_MCP_JOURNAL`, `YNAB_MCP_CACHE_TTL_DAYS`.

| What | macOS / Linux | Windows |
|---|---|---|
| Config | `~/.config/ynab-mcp/config.toml` | `%APPDATA%\ynab-mcp\config.toml` |
| Token | Keychain or secret-service item `ynab-mcp` | config file |
| Journal | `~/.local/share/ynab-mcp/journal.jsonl` | `%LOCALAPPDATA%\ynab-mcp\journal.jsonl` |
| Log | `~/.local/share/ynab-mcp/ynab-mcp.log` | `%LOCALAPPDATA%\ynab-mcp\ynab-mcp.log` |
| Month cache | `~/.local/share/ynab-mcp/cache/` | `%LOCALAPPDATA%\ynab-mcp\cache\` |

`XDG_CONFIG_HOME` and `XDG_DATA_HOME` are honored on macOS and Linux.

Closed months are cached on disk so multi-month analytics do not burn the 200-request-per-hour
limit. The current month is never cached, and every write drops the months it touched.
`ynab-mcp cache clear` throws the cache away if you edited an old month in the YNAB app.

## How undo works

Every write appends a record to an append-only JSONL journal, and undo appends its own
record rather than editing history, so the file is a complete audit trail. YNAB has no
server-side undo, so the journal is the only record. Undo deletes created rows, restores a
replaced row from its journaled snapshot, and sets assigned amounts back to what they were.

Two things to know:

- `replace_transaction` creates the new row before deleting the old one, so a failure can
  never leave money missing. If the delete fails, both rows exist and the tool says so.
- YNAB keeps an import id reserved after the row is deleted. Once a row is undone, creating
  the same amount on the same day again is reported as a skipped duplicate. Enter that one
  by hand or change the amount or date.

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
