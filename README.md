# ynab-mcp

A small MCP server for [YNAB](https://www.ynab.com), written in Rust on the official
[`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) SDK. Read-only unless you opt in;
with writes on, a small gated set of write tools is registered, and every write is journaled,
two-phase (preview, then confirm), and undoable.

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
| `spending_summary` | category × month actuals for N months, with average and recent-vs-earlier change |
| `recurring_charges` | payees on a cadence: occurrences, first vs latest amount, drift, possibly lapsed |
| `category_history` | one category by payee: total, count, share, monthly series |
| `payee_summary` | payees ranked by outflow; frequency spend that hides in category totals |
| `budget_vs_actual` | one month: overspent, spent over target, assigned-but-unused, targets underfunded |
| `goal_analysis` | every target over N months: assigned vs spent per month, months over target, money moved in/out and from where |
| `money_movements` | category-to-category moves with names and per-category net |
| `pdf_to_csv` | bank statement PDF → Date, Description, Amount CSV, entirely on this machine; the PDF text never comes back |
| `export_transactions` | CSV or JSON to a path; filter by dates, account, category, or category group; splits one row per leg |
| `import_bank_csv` | one or more bank CSV files → parse by column mapping → dedupe across files → reconcile; `confirm` creates the missing rows (writes) |
| `trigger_bank_import` | **write, gated.** YNAB's Import button for linked accounts |
| `list_write_history` | every write batch any agent has made, with status open / partially_undone / undone |
| `diagnostic_report` | redacted local diagnostics + a prefilled GitHub issue link; sends nothing |
| `create_transactions` | **write, gated** by `YNAB_MCP_ALLOW_WRITES=1`; single-category or split (`subtransactions` that sum to `amount`); dedupe `import_id`, lands unapproved, journaled |
| `replace_transaction` | **write, gated.** Reshape one row to a new category or new split lines at the same account, date, payee, and total; creates the replacement, then deletes the original. Preview, then `confirm`; a reconciled original needs `force` |
| `assign_money` | **write, gated.** Per-month deltas on categories' assigned amounts (+ from Ready to Assign, − back to it). Preview shows before/after per category and per month; `confirm` applies; a month going negative needs `force`. Touched months leave the cache |
| `undo_batch`, `undo_last` | **write, gated.** Two-phase: preview first, then `confirm=true`. Flagged rows also need `force`. Works on every write kind |

Amounts are decimal strings in the plan currency; outflows are negative.

### The line

The server counts; the model judges. The analytics tools return tables and counts, never a
score, a label, or a recommendation. "Wasteful" is a judgment about your life, and it belongs in
the conversation with your context, not in a heuristic someone else wrote. What the server does
is make the budget meeting possible: twelve months summed correctly, subscriptions found by
cadence, the money you moved out of Groceries three of the last six months laid out as fact.
Then you, your partner, and the model talk about it.

## Install

**macOS or Linux, no Rust needed:**

```
curl -fsSL https://raw.githubusercontent.com/mattCasanova/ynab-mcp-server/master/install.sh | sh
```

That downloads the latest release binary for your machine, checks its SHA-256, and puts it in
`~/.local/bin/ynab-mcp`. Set `YNAB_MCP_INSTALL_DIR` to put it somewhere else.

**Windows:** download the `x86_64-pc-windows-msvc` zip from the
[releases page](https://github.com/mattCasanova/ynab-mcp-server/releases) and put `ynab-mcp.exe`
somewhere on your PATH.

**With Rust installed:**

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
| Month cache | `~/.local/share/ynab-mcp/cache/` | `%LOCALAPPDATA%\ynab-mcp\cache\` |

`XDG_CONFIG_HOME` and `XDG_DATA_HOME` are honored on macOS and Linux.

### Config file

```toml
plan_id = "last-used"     # or a plan id from `status`
allow_writes = false      # true registers the write tools (create, replace, assign, undo, import)
# journal = "~/somewhere/journal.jsonl"
# cache_ttl_days = 30       # closed-month cache; 0 disables
# access_token = "..."    # only if the secret store is unavailable; file must be mode 600
```

Environment variables override the file: `YNAB_ACCESS_TOKEN`, `YNAB_PLAN_ID`,
`YNAB_MCP_ALLOW_WRITES`, `YNAB_MCP_JOURNAL`, `YNAB_MCP_CACHE_TTL_DAYS`. Token lookup order is env var, then
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

Every write appends one record to an append-only JSONL journal
(`~/.local/share/ynab-mcp/journal.jsonl`, or `YNAB_MCP_JOURNAL`). Undo appends its own record
instead of editing history, so the file is a complete audit trail and any agent in any session
can undo what another one did. An exclusive OS file lock is held across the whole
read-check-write-append cycle, so two server processes cannot double-undo or interleave.

There are three write kinds, and undo reverses each with the matching inverse:

| Kind | Written by | Undo does |
|---|---|---|
| `batch` | `create_transactions`, `import_bank_csv` | deletes the rows it created |
| `replace` | `replace_transaction` | deletes the replacement, re-creates the original from its journaled snapshot (no `import_id`, unapproved, cleared state as it was) |
| `assign` | `assign_money` | sets each category's assigned amount back to what it was |

YNAB has no server-side undo, so the journal is the only record. Undo therefore runs in two
phases:

1. `undo_batch` / `undo_last` without `confirm` fetches the current state and returns a
   preview. A row is `deletable`, or flagged: `missing` (gone from YNAB), `reconciled`, or
   `changed_since_created` (amount, date, account, or split lines differ). An assignment is
   `revertable`, or flagged `changed_since_assigned` / `missing`. Nothing is changed or
   journaled.
2. With `confirm=true` the unflagged parts are reversed. Flagged parts are skipped unless
   `force=true`. `missing` rows are recorded as resolved. A write stays open until every part
   is reversed, so re-running is safe.

**Deleting a split through the API leaves stale month totals** (observed 2026-10-01): the row
and the account balance update, but each leg's category keeps counting the leg until some write
that references that category lands. A plain delete recomputes fine. So after any split
delete the server nudges every leg category by creating a zero-amount row in it and deleting
it again (re-writing the assigned amount unchanged does nothing). Every tool that deletes
reports what it nudged under `recompute_nudges`; a failed nudge is reported, never hidden.

`replace_transaction` creates before it deletes, so a failure can never leave money missing
from the account. If the delete fails after the create, both rows exist; the tool says so
loudly with both ids and the replace is still undoable. YNAB cannot change the category of an
existing split, which is why replace is create-then-delete rather than an update.

**Import ids stay reserved after a delete.** `create_transactions` and `import_bank_csv` give
every row an `import_id` of `YNAB:<milliunits>:<date>:<n>` so a re-run cannot double-enter. YNAB
keeps that id reserved even after the row is deleted, so once a row has been undone, creating
the same amount on the same day again is reported under `skipped_duplicate_import_ids` and
nothing is journaled. Enter that one by hand in YNAB, or change the amount or date.

## Files in and out

- **Statement PDFs stay on your machine.** `pdf_to_csv` (also `ynab-mcp pdf-to-csv` on the
  command line) reads the PDF locally, finds the transaction rows, scrubs any run of eight or
  more digits out of descriptions, and writes a Date, Description, Amount CSV. Only the row
  count, totals, section names with last four digits, and a three-row sample come back to the
  agent. Account numbers, addresses, and the rest of the statement are parsed and thrown away.
  Statements that cover several accounts need `account` (a name fragment or last four).
  Tested on Capital One 360 statements; the parser is generic (date-led rows, trailing amount
  and balance, Debit/Credit or +/- signs, wrapped descriptions), so other banks should mostly
  work and will say plainly when they do not.

- **Export** writes exactly the rows you ask for to the path you give, and refuses to overwrite
  unless told to. The category-group filter is the tax-ledger case: every leg in the business
  group for a date range, one row per leg, with the group and category on each row.
- **Import** takes one or more CSV files from the same bank format. You (or the agent) read the
  header and pass a column mapping: a date column, a description column, and either one signed
  amount column or debit/credit columns. Dates parse as ISO, MM/DD/YYYY, MM/DD/YY, or a
  strftime pattern you supply. Overlapping files are deduped and the duplicates reported. Then
  it reconciles against the account. With writes enabled and `confirm=true`, the rows missing
  in YNAB are created through the same journaled path as `create_transactions`, so they land
  unapproved and can be undone.
- The server reads only the paths it is given and writes only the export path it is given.

## Month cache

YNAB serves per-category month numbers one month per call, so a six-month `goal_analysis`
would be six live calls every time. Closed months rarely change, so month detail is cached on
disk under `~/.local/share/ynab-mcp/cache/<plan id>/months/`:

- the current month is never cached;
- the previous month is cached for one day (you are usually still reconciling it);
- older months are cached for `cache_ttl_days` (default 30; `0` disables).

`status` shows how many months are cached, `goal_analysis` reports live vs cached calls, and
`ynab-mcp cache clear` throws the cache away (it is refetched on demand). If you edit an old
month in YNAB and want the tools to see it now, clear the cache.

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
- Rate limit is 200 requests per hour per token. YNAB no longer sends a usage header, so
  `status` reports the count this process has made in the last hour; a second agent on the same
  token has its own count.
- Money is milliunits on the wire (`i64`, 1000 = 1.00) and converted exactly once in the output layer.
