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
| `create_transactions` | **write, gated** by `YNAB_MCP_ALLOW_WRITES=1`; dedupe `import_id`, lands unapproved, journaled |
| `undo_batch`, `undo_last` | **write, gated.** Two-phase: preview first, then `confirm=true`. Flagged rows also need `force` |

Amounts are decimal strings in the plan currency; outflows are negative.

## Setup

1. Create a Personal Access Token: YNAB → Account Settings → Developer Settings.
2. Put it in the macOS Keychain (paste it into Dashlane too):

   ```
   security add-generic-password -a "$USER" -s ynab-mcp -w
   ```

3. Build:

   ```
   cargo build --release
   ```

4. Register with Claude Code (user scope, so every project sees it):

   ```
   claude mcp add --scope user ynab -- ~/src/ynab-mcp/scripts/ynab-mcp.sh
   ```

The launcher script reads the token from the Keychain at start, so nothing secret lands in
`~/.claude.json`.

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

## Environment

| Var | Default | Meaning |
|---|---|---|
| `YNAB_ACCESS_TOKEN` | required | personal access token |
| `YNAB_PLAN_ID` | `last-used` | plan (budget) id |
| `YNAB_MCP_ALLOW_WRITES` | unset | `1` registers `create_transactions`, `undo_batch`, `undo_last` |
| `YNAB_MCP_JOURNAL` | `~/.local/share/ynab-mcp/journal.jsonl` | write journal path |
| `RUST_LOG` | `info` | log filter; logs go to stderr |

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
