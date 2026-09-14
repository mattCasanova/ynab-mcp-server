use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::cache::MonthCache;
use crate::journal::{self, BatchRecord, BatchState, Journal, Op, Record, Skipped, UndoRecord};
use crate::money::{from_decimal, to_decimal};
use crate::reconcile::{self, BankRow, YnabRow};
use crate::ynab::{self, Client, SaveTransaction, TransactionFilter, TransactionKind, YnabError};

pub struct ServerMeta {
    pub token_source: String,
    pub config_path: std::path::PathBuf,
    pub log_path: Option<std::path::PathBuf>,
    pub cache_root: std::path::PathBuf,
    pub cache_ttl_days: i64,
}

#[derive(Clone)]
pub struct YnabServer {
    client: Arc<Client>,
    journal: Arc<Journal>,
    meta: Arc<ServerMeta>,
    /// Built on first use, once the concrete plan id is known (keys the cache per plan).
    month_cache: Arc<tokio::sync::OnceCell<MonthCache>>,
    allow_writes: bool,
    tool_router: ToolRouter<Self>,
}

impl YnabServer {
    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    pub(crate) fn writes_allowed(&self) -> bool {
        self.allow_writes
    }

    async fn month_cache(&self) -> Result<&MonthCache, McpError> {
        self.month_cache
            .get_or_try_init(|| async {
                let plan = self.client.resolve_plan_id().await.map_err(api_error)?;
                let dir = self.meta.cache_root.join(&plan).join("months");
                tracing::info!(dir = %dir.display(), ttl_days = self.meta.cache_ttl_days, "month cache ready");
                Ok(MonthCache::new(dir, self.meta.cache_ttl_days))
            })
            .await
    }

    /// Month detail through the cache. "current" and the current month are always live.
    pub(crate) async fn month_detail(
        &self,
        month: &str,
    ) -> Result<(ynab::MonthDetail, bool), McpError> {
        let today = chrono::Local::now().date_naive();
        let Some(date) = reconcile::parse_date(month) else {
            if month != "current" {
                return Err(invalid(format!(
                    "month must be YYYY-MM-01 or \"current\", got {month:?}"
                )));
            }
            return Ok((self.client.month(month).await.map_err(api_error)?, false));
        };
        let cache = self.month_cache().await?;
        let now = chrono::Utc::now();
        if let Some(detail) = cache.get(date, today, now) {
            return Ok((detail, true));
        }
        let detail = self.client.month(month).await.map_err(api_error)?;
        cache.put(date, today, now, &detail);
        Ok((detail, false))
    }

    pub fn new(client: Client, journal: Journal, allow_writes: bool, meta: ServerMeta) -> Self {
        let mut tool_router = Self::read_router()
            + Self::analytics_router()
            + Self::transfer_router()
            + Self::history_router()
            + Self::diag_router();
        if allow_writes {
            tool_router += Self::write_router() + Self::transfer_write_router();
        }
        Self {
            client: Arc::new(client),
            journal: Arc::new(journal),
            meta: Arc::new(meta),
            month_cache: Arc::new(tokio::sync::OnceCell::new()),
            allow_writes,
            tool_router,
        }
    }
}

pub(crate) fn client_name(ctx: &RequestContext<RoleServer>) -> Option<String> {
    let name = ctx
        .peer
        .peer_info()
        .map(|info| info.client_info.name.clone());
    if name.is_none() {
        tracing::warn!("no client info on this connection; journal entry will have client=null");
    }
    name
}

/// Every error handed back to the model is also logged, so the local log has the full story
/// even when the agent moves on.
fn journal_error(e: anyhow::Error) -> McpError {
    tracing::error!(error = format!("{e:#}"), "journal failure");
    McpError::internal_error(format!("journal: {e:#}"), None)
}

pub(crate) fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string(value)
        .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

pub(crate) fn api_error(e: YnabError) -> McpError {
    tracing::error!(error = %e, "YNAB call failed");
    match e {
        YnabError::Api { status: 429, .. } => {
            McpError::internal_error("YNAB rate limit hit (200 requests/hour)".to_string(), None)
        }
        other => McpError::internal_error(other.to_string(), None),
    }
}

pub(crate) fn invalid(msg: impl Into<String>) -> McpError {
    let msg = msg.into();
    tracing::warn!(msg, "rejected tool arguments");
    McpError::invalid_params(msg, None)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListAccountsArgs {
    /// Include closed accounts. Default false.
    #[serde(default)]
    pub include_closed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListCategoriesArgs {
    /// Include hidden categories and groups. Default false.
    #[serde(default)]
    pub include_hidden: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetMonthArgs {
    /// Month as YYYY-MM-01, or "current".
    pub month: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListTransactionsArgs {
    /// Restrict to one account (id from list_accounts).
    pub account_id: Option<String>,
    /// Restrict to one category (id from list_categories). Ignored when account_id is set.
    pub category_id: Option<String>,
    /// ISO date; only transactions on or after. Defaults to one year ago.
    pub since_date: Option<String>,
    /// ISO date; only transactions on or before.
    pub until_date: Option<String>,
    /// "uncategorized" or "unapproved" to pull the review queue.
    pub kind: Option<String>,
    /// Max rows returned, newest first. Default 200.
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BankRowArg {
    /// Transaction date, YYYY-MM-DD or MM/DD/YYYY.
    pub date: String,
    /// Signed decimal amount in the plan currency. Outflows negative, inflows positive
    /// (YNAB convention). Normalize Debit/Credit columns before calling.
    pub amount: f64,
    /// The bank's description or payee text.
    pub description: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReconcileArgs {
    /// The YNAB account to reconcile (id from list_accounts).
    pub account_id: String,
    /// Rows from the bank statement or CSV export.
    pub rows: Vec<BankRowArg>,
    /// Days of date slack allowed between bank and YNAB. Default 3.
    pub window_days: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct NewTransactionArg {
    /// Account id from list_accounts.
    pub account_id: String,
    /// ISO date YYYY-MM-DD. Future dates are rejected by YNAB.
    pub date: String,
    /// Signed decimal amount. Outflows negative.
    pub amount: f64,
    /// Payee name; YNAB resolves or creates it.
    pub payee_name: Option<String>,
    /// Category id from list_categories. Leave unset to land uncategorized.
    pub category_id: Option<String>,
    pub memo: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateTransactionsArgs {
    pub transactions: Vec<NewTransactionArg>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListWriteHistoryArgs {
    /// Max batches, newest first. Default 20.
    pub limit: Option<usize>,
    /// Only batches that still have something to undo. Default false.
    #[serde(default)]
    pub open_only: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UndoBatchArgs {
    /// Batch id from list_write_history.
    pub batch_id: String,
    /// false (default) = preview only, nothing deleted. true = delete what the preview marked deletable.
    #[serde(default)]
    pub confirm: bool,
    /// With confirm: also delete rows flagged reconciled or changed_since_created. Default false.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UndoLastArgs {
    /// false (default) = preview only, nothing deleted. true = delete what the preview marked deletable.
    #[serde(default)]
    pub confirm: bool,
    /// With confirm: also delete rows flagged reconciled or changed_since_created. Default false.
    #[serde(default)]
    pub force: bool,
}

#[tool_router(router = read_router, vis = "pub")]
impl YnabServer {
    #[tool(
        description = "Plans on this token, the plan in use, write mode, and rate-limit usage (200/hour)."
    )]
    async fn status(&self) -> Result<CallToolResult, McpError> {
        let plans = self.client.plans().await.map_err(api_error)?;
        let cache = self.month_cache().await?;
        json_result(&json!({
            "plan_id": self.client.plan_id(),
            "writes_enabled": self.allow_writes,
            "rate_limit": self.client.rate_limit(),
            "month_cache": {
                "enabled": cache.enabled(),
                "ttl_days": self.meta.cache_ttl_days,
                "cached_months": cache.count(),
                "dir": cache.dir().display().to_string(),
            },
            "plans": plans,
        }))
    }

    #[tool(
        description = "Accounts with balances (decimal). Closed accounts hidden unless include_closed."
    )]
    async fn list_accounts(
        &self,
        Parameters(args): Parameters<ListAccountsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let accounts = self.client.accounts().await.map_err(api_error)?;
        let rows: Vec<_> = accounts
            .iter()
            .filter(|a| !a.deleted && (args.include_closed || !a.closed))
            .map(|a| {
                json!({
                    "id": a.id,
                    "name": a.name,
                    "type": a.kind,
                    "on_budget": a.on_budget,
                    "closed": a.closed,
                    "balance": to_decimal(a.balance),
                    "cleared_balance": to_decimal(a.cleared_balance),
                    "uncleared_balance": to_decimal(a.uncleared_balance),
                    "last_reconciled_at": a.last_reconciled_at,
                    "note": a.note,
                })
            })
            .collect();
        json_result(&rows)
    }

    #[tool(
        description = "Category groups and categories for the current month: assigned, activity, available, goal fields. Amounts decimal."
    )]
    async fn list_categories(
        &self,
        Parameters(args): Parameters<ListCategoriesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let groups = self.client.category_groups().await.map_err(api_error)?;
        let rows: Vec<_> = groups
            .iter()
            .filter(|g| !g.deleted && (args.include_hidden || !g.hidden))
            .map(|g| {
                json!({
                    "id": g.id,
                    "name": g.name,
                    "categories": g.categories.iter()
                        .filter(|c| !c.deleted && (args.include_hidden || !c.hidden))
                        .map(category_json)
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        json_result(&rows)
    }

    #[tool(
        description = "One plan month: income, assigned, activity, ready-to-assign, age of money, and every category's numbers for that month."
    )]
    async fn get_month(
        &self,
        Parameters(args): Parameters<GetMonthArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (month, from_cache) = self.month_detail(&args.month).await?;
        json_result(&json!({
            "month": month.month,
            "from_cache": from_cache,
            "note": month.note,
            "income": to_decimal(month.income),
            "assigned": to_decimal(month.budgeted),
            "activity": to_decimal(month.activity),
            "ready_to_assign": to_decimal(month.to_be_budgeted),
            "age_of_money_days": month.age_of_money,
            "categories": month.categories.iter()
                .filter(|c| !c.deleted && !c.hidden)
                .map(category_json)
                .collect::<Vec<_>>(),
        }))
    }

    #[tool(
        description = "Transactions, newest first, filtered by account, category, date range, or kind (uncategorized|unapproved). Amounts decimal; outflows negative."
    )]
    async fn list_transactions(
        &self,
        Parameters(args): Parameters<ListTransactionsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let kind = match args.kind.as_deref() {
            None => None,
            Some("uncategorized") => Some(TransactionKind::Uncategorized),
            Some("unapproved") => Some(TransactionKind::Unapproved),
            Some(other) => {
                return Err(invalid(format!(
                    "kind must be uncategorized or unapproved, got {other}"
                )));
            }
        };
        let filter = TransactionFilter {
            account_id: args.account_id,
            category_id: args.category_id,
            since_date: args.since_date,
            until_date: args.until_date,
            kind,
        };
        let mut txns = self.client.transactions(&filter).await.map_err(api_error)?;
        txns.retain(|t| !t.deleted);
        txns.sort_by(|a, b| b.date.cmp(&a.date));
        let limit = args.limit.unwrap_or(200);
        let total = txns.len();
        let rows: Vec<_> = txns.iter().take(limit).map(transaction_json).collect();
        json_result(&json!({ "total": total, "returned": rows.len(), "transactions": rows }))
    }

    #[tool(
        description = "Scheduled (upcoming, recurring) transactions with next date and frequency."
    )]
    async fn list_scheduled_transactions(&self) -> Result<CallToolResult, McpError> {
        let scheduled = self
            .client
            .scheduled_transactions()
            .await
            .map_err(api_error)?;
        let rows: Vec<_> = scheduled
            .iter()
            .filter(|s| !s.deleted)
            .map(|s| {
                json!({
                    "id": s.id,
                    "date_next": s.date_next,
                    "date_first": s.date_first,
                    "frequency": s.frequency,
                    "amount": to_decimal(s.amount),
                    "account": s.account_name,
                    "payee": s.payee_name,
                    "category": s.category_name,
                    "memo": s.memo,
                })
            })
            .collect();
        json_result(&rows)
    }

    #[tool(
        description = "Reconcile one account against bank rows. Deterministic: matches equal amounts within window_days, then reports bank rows missing in YNAB and YNAB rows (inside the bank date range) with no bank match."
    )]
    async fn reconcile_account(
        &self,
        Parameters(args): Parameters<ReconcileArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.rows.is_empty() {
            return Err(invalid("rows is empty"));
        }
        let window_days = i64::from(args.window_days.unwrap_or(3));
        let bank_rows = args
            .rows
            .iter()
            .map(|r| {
                reconcile::parse_date(&r.date)
                    .map(|date| BankRow {
                        date,
                        amount: from_decimal(r.amount),
                        description: r.description.clone(),
                    })
                    .ok_or_else(|| invalid(format!("unparseable date: {}", r.date)))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let summary = self
            .reconcile_rows(&args.account_id, &bank_rows, window_days)
            .await?;
        json_result(&summary)
    }
}

/// Reconcile outcome plus the bank rows that were missing, for callers that go on to create them.
pub(crate) struct ReconcileOutcome {
    pub summary: serde_json::Value,
    pub missing: Vec<BankRow>,
}

impl YnabServer {
    pub(crate) async fn reconcile_rows(
        &self,
        account_id: &str,
        bank_rows: &[BankRow],
        window_days: i64,
    ) -> Result<serde_json::Value, McpError> {
        Ok(self
            .reconcile_full(account_id, bank_rows, window_days)
            .await?
            .summary)
    }

    pub(crate) async fn reconcile_full(
        &self,
        account_id: &str,
        bank_rows: &[BankRow],
        window_days: i64,
    ) -> Result<ReconcileOutcome, McpError> {
        if bank_rows.is_empty() {
            return Err(invalid("no bank rows"));
        }
        let lo = bank_rows.iter().map(|r| r.date).min().expect("non-empty");
        let hi = bank_rows.iter().map(|r| r.date).max().expect("non-empty");
        let window = chrono::Duration::days(window_days);
        let filter = TransactionFilter {
            account_id: Some(account_id.to_string()),
            since_date: Some((lo - window).to_string()),
            until_date: Some((hi + window).to_string()),
            ..Default::default()
        };
        let txns = self.client.transactions(&filter).await.map_err(api_error)?;
        // A YNAB date that does not parse is an API-shape bug, not a row to skip quietly.
        let ynab_rows: Vec<YnabRow> = txns
            .iter()
            .filter(|t| !t.deleted)
            .map(|t| {
                let date = reconcile::parse_date(&t.date).ok_or_else(|| {
                    tracing::error!(id = %t.id, date = %t.date, "YNAB returned an unparseable date");
                    McpError::internal_error(format!("YNAB returned unparseable date {:?}", t.date), None)
                })?;
                Ok(YnabRow {
                    id: t.id.clone(),
                    date,
                    amount: t.amount,
                    payee: t.payee_name.clone(),
                    category: t.category_name.clone(),
                    cleared: t.cleared.clone(),
                    approved: t.approved,
                })
            })
            .collect::<Result<Vec<_>, McpError>>()?;

        let result = reconcile::reconcile(bank_rows, &ynab_rows, window_days);
        let summary = json!({
            "window_days": window_days,
            "bank_rows": bank_rows.len(),
            "ynab_rows_in_range": ynab_rows.len(),
            "matched": result.matched.len(),
            "missing_in_ynab": result.missing_in_ynab.iter().map(bank_row_json).collect::<Vec<_>>(),
            "in_ynab_not_at_bank": result.in_ynab_not_at_bank.iter().map(ynab_row_json).collect::<Vec<_>>(),
            "matched_with_date_drift": result.matched.iter()
                .filter(|m| m.day_offset != 0)
                .map(|m| json!({
                    "bank": bank_row_json(&m.bank),
                    "ynab": ynab_row_json(&m.ynab),
                    "day_offset": m.day_offset,
                }))
                .collect::<Vec<_>>(),
        });
        Ok(ReconcileOutcome {
            summary,
            missing: result.missing_in_ynab,
        })
    }
}

#[tool_router(router = history_router, vis = "pub")]
impl YnabServer {
    #[tool(
        description = "Every write batch this server (any agent, any session) has made, newest first, with status open | partially_undone | undone. Use the batch_id with undo_batch."
    )]
    async fn list_write_history(
        &self,
        Parameters(args): Parameters<ListWriteHistoryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut locked = self.journal.lock().map_err(journal_error)?;
        let mut batches = locked.batches().map_err(journal_error)?;
        drop(locked);
        batches.reverse();
        let rows: Vec<_> = batches
            .iter()
            .filter(|b| !args.open_only || !b.remaining.is_empty())
            .take(args.limit.unwrap_or(20))
            .map(batch_json)
            .collect();
        json_result(
            &json!({ "journal": self.journal.path().display().to_string(), "batches": rows }),
        )
    }
}

#[tool_router(router = diag_router, vis = "pub")]
impl YnabServer {
    #[tool(
        description = "Redacted diagnostic report (version, OS, config state, recent local warnings/errors; never tokens, payees, or amounts) plus a prefilled GitHub issue link. Nothing is sent; show the link to the user."
    )]
    async fn diagnostic_report(&self) -> Result<CallToolResult, McpError> {
        let input = crate::diag::ReportInput {
            token_source: Some(self.meta.token_source.clone()),
            allow_writes: Some(self.allow_writes),
            config_error: None,
            config_path: self.meta.config_path.clone(),
            journal_path: self.journal.path().to_path_buf(),
        };
        let body = crate::diag::build_report(&input, self.meta.log_path.as_deref())
            .map_err(|e| McpError::internal_error(format!("report: {e:#}"), None))?;
        json_result(&json!({
            "report": body,
            "file_issue_url": crate::diag::issue_url("ynab-mcp: <short description>", &body),
            "log_path": self.meta.log_path.as_ref().map(|p| p.display().to_string()),
        }))
    }
}

#[tool_router(router = write_router, vis = "pub")]
impl YnabServer {
    #[tool(
        description = "WRITE. Undo one batch from list_write_history by deleting the transactions it created. Rows reconciled in YNAB since are skipped unless force. Safe to re-run; already-undone rows are never touched twice."
    )]
    async fn undo_batch(
        &self,
        Parameters(args): Parameters<UndoBatchArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.undo(
            Some(args.batch_id),
            args.confirm,
            args.force,
            client_name(&ctx),
        )
        .await
    }

    #[tool(
        description = "WRITE. Undo the most recent batch that still has something to undo. Same rules as undo_batch."
    )]
    async fn undo_last(
        &self,
        Parameters(args): Parameters<UndoLastArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.undo(None, args.confirm, args.force, client_name(&ctx))
            .await
    }

    #[tool(
        description = "WRITE. Create transactions in bulk. Each gets an import_id (YNAB:amount:date:n) so re-running never double-enters, and lands unapproved for review in YNAB."
    )]
    async fn create_transactions(
        &self,
        Parameters(args): Parameters<CreateTransactionsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let summary = self
            .create_rows(&args.transactions, "create_transactions", client_name(&ctx))
            .await?;
        json_result(&summary)
    }
}

impl YnabServer {
    /// The one write path: dedupe import ids, unapproved rows, journaled batch. Shared by
    /// create_transactions and the CSV import.
    pub(crate) async fn create_rows(
        &self,
        rows: &[NewTransactionArg],
        tool: &str,
        client: Option<String>,
    ) -> Result<serde_json::Value, McpError> {
        if !self.allow_writes {
            return Err(invalid(
                "writes are disabled on this server (YNAB_MCP_ALLOW_WRITES)",
            ));
        }
        if rows.is_empty() {
            return Err(invalid("transactions is empty"));
        }
        let mut occurrences: std::collections::HashMap<(i64, String), u32> = Default::default();
        let saves: Vec<SaveTransaction> = rows
            .iter()
            .map(|t| {
                reconcile::parse_date(&t.date)
                    .ok_or_else(|| invalid(format!("unparseable date: {}", t.date)))?;
                let amount = from_decimal(t.amount);
                let n = occurrences.entry((amount, t.date.clone())).or_insert(0);
                *n += 1;
                Ok(SaveTransaction {
                    account_id: t.account_id.clone(),
                    date: t.date.clone(),
                    amount,
                    payee_name: t.payee_name.clone(),
                    category_id: t.category_id.clone(),
                    memo: t.memo.clone(),
                    cleared: "uncleared",
                    approved: false,
                    import_id: format!("YNAB:{amount}:{}:{n}", t.date),
                })
            })
            .collect::<Result<Vec<_>, McpError>>()?;
        // Hold the lock across the API call so a concurrent undo cannot interleave.
        let mut locked = self.journal.lock().map_err(journal_error)?;
        let result = self
            .client
            .create_transactions(&saves)
            .await
            .map_err(api_error)?;
        let ops: Vec<Op> = result
            .transactions
            .iter()
            .filter(|t| result.transaction_ids.contains(&t.id))
            .map(|t| Op {
                transaction_id: t.id.clone(),
                account_id: t.account_id.clone(),
                date: t.date.clone(),
                amount: t.amount,
                payee_name: t.payee_name.clone(),
                import_id: t.import_id.clone(),
            })
            .collect();
        // If YNAB's response shape drifts, the ids exist but we could not build undo ops for
        // them. Journal what we have, then fail loudly with every id so nothing is orphaned.
        if ops.len() != result.transaction_ids.len() {
            let known: Vec<String> = ops.iter().map(|o| o.transaction_id.clone()).collect();
            let orphaned: Vec<String> = result
                .transaction_ids
                .iter()
                .filter(|id| !known.contains(id))
                .cloned()
                .collect();
            tracing::error!(
                ?orphaned,
                "created rows missing from YNAB response; undo cannot cover them"
            );
            let partial = BatchRecord {
                batch_id: journal::new_batch_id(),
                at: journal::now(),
                plan_id: self.client.plan_id().to_string(),
                tool: tool.to_string(),
                client: client.clone(),
                ops,
            };
            if let Err(e) = locked.append(&Record::Batch(partial)) {
                tracing::error!(
                    error = format!("{e:#}"),
                    "and the partial batch could not be journaled"
                );
            }
            return Err(McpError::internal_error(
                format!(
                    "YNAB created {} rows but returned details for only {}; these ids are NOT undoable and must be checked by hand: {:?}",
                    result.transaction_ids.len(),
                    known.len(),
                    orphaned
                ),
                None,
            ));
        }
        let batch = BatchRecord {
            batch_id: journal::new_batch_id(),
            at: journal::now(),
            plan_id: self.client.plan_id().to_string(),
            tool: tool.to_string(),
            client,
            ops,
        };
        if let Err(e) = locked.append(&Record::Batch(batch.clone())) {
            // The rows exist in YNAB but are not journaled: say so loudly with the ids.
            tracing::error!(error = %e, ids = ?result.transaction_ids, "created transactions but failed to journal them");
            return Err(McpError::internal_error(
                format!(
                    "created {} transactions but could not write the journal ({e:#}); ids not undoable: {:?}",
                    result.transaction_ids.len(),
                    result.transaction_ids
                ),
                None,
            ));
        }
        Ok(json!({
            "batch_id": batch.batch_id,
            "created": result.transaction_ids.len(),
            "transaction_ids": result.transaction_ids,
            "skipped_duplicate_import_ids": result.duplicate_import_ids,
            "undo_with": "undo_batch or undo_last",
        }))
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum RowState {
    Deletable,
    /// Not in YNAB any more; the journal is the only record left.
    Missing {
        reason: String,
    },
    Reconciled,
    ChangedSinceCreated {
        now: serde_json::Value,
    },
}

struct UndoPlan<'a> {
    batch: &'a BatchState,
    rows: Vec<(&'a Op, RowState)>,
}

impl YnabServer {
    async fn plan_undo<'a>(&self, state: &'a BatchState) -> Result<UndoPlan<'a>, McpError> {
        // One transactions call per account to learn the current state of each row. A missing
        // account marks every row in it as missing rather than failing the whole undo.
        let mut current: std::collections::HashMap<String, ynab::Transaction> = Default::default();
        let mut gone_accounts: Vec<String> = Vec::new();
        let mut accounts: Vec<&str> = state
            .remaining
            .iter()
            .map(|op| op.account_id.as_str())
            .collect();
        accounts.sort_unstable();
        accounts.dedup();
        let since = state
            .remaining
            .iter()
            .map(|op| op.date.as_str())
            .min()
            .map(str::to_string);
        for account in accounts {
            let filter = TransactionFilter {
                account_id: Some(account.to_string()),
                since_date: since.clone(),
                ..Default::default()
            };
            match self.client.transactions(&filter).await {
                Ok(txns) => {
                    for t in txns {
                        current.insert(t.id.clone(), t);
                    }
                }
                Err(YnabError::Api { status: 404, .. }) => {
                    tracing::warn!(
                        account,
                        "undo: account not found in YNAB; its rows will be flagged missing"
                    );
                    gone_accounts.push(account.to_string())
                }
                Err(e) => return Err(api_error(e)),
            }
        }

        let rows = state
            .remaining
            .iter()
            .map(|op| {
                let row_state = if gone_accounts.contains(&op.account_id) {
                    RowState::Missing {
                        reason: "account not found in YNAB (deleted?)".to_string(),
                    }
                } else {
                    match current.get(&op.transaction_id) {
                        None => RowState::Missing {
                            reason: "transaction not found in YNAB (deleted by hand?)".to_string(),
                        },
                        Some(t) if t.deleted => RowState::Missing {
                            reason: "transaction deleted in YNAB".to_string(),
                        },
                        Some(t) if t.cleared == "reconciled" => RowState::Reconciled,
                        Some(t)
                            if t.amount != op.amount
                                || t.date != op.date
                                || t.account_id != op.account_id =>
                        {
                            RowState::ChangedSinceCreated {
                                now: transaction_json(t),
                            }
                        }
                        Some(_) => RowState::Deletable,
                    }
                };
                (op, row_state)
            })
            .collect();
        Ok(UndoPlan { batch: state, rows })
    }

    async fn undo(
        &self,
        batch_id: Option<String>,
        confirm: bool,
        force: bool,
        client: Option<String>,
    ) -> Result<CallToolResult, McpError> {
        let mut locked = self.journal.lock().map_err(journal_error)?;
        let batches = locked.batches().map_err(journal_error)?;
        let state = match &batch_id {
            Some(id) => batches
                .iter()
                .find(|b| &b.batch.batch_id == id)
                .ok_or_else(|| invalid(format!("no batch {id} in the journal")))?,
            None => batches
                .iter()
                .rev()
                .find(|b| !b.remaining.is_empty())
                .ok_or_else(|| invalid("nothing left to undo"))?,
        };
        if state.remaining.is_empty() {
            return Err(invalid(format!(
                "batch {} is already fully undone",
                state.batch.batch_id
            )));
        }
        if state.batch.plan_id != self.client.plan_id() {
            return Err(invalid(format!(
                "batch {} belongs to plan {}, this server is on plan {}",
                state.batch.batch_id,
                state.batch.plan_id,
                self.client.plan_id()
            )));
        }

        let plan = self.plan_undo(state).await?;
        let flagged = plan
            .rows
            .iter()
            .filter(|(_, s)| !matches!(s, RowState::Deletable))
            .count();
        let preview: Vec<_> = plan
            .rows
            .iter()
            .map(
                |(op, s)| json!({ "transaction": op, "amount": to_decimal(op.amount), "check": s }),
            )
            .collect();

        if !confirm {
            let warning = if flagged > 0 {
                format!(
                    "{flagged} of {} rows cannot be vouched for by the journal any more (see check). \
                     Ask the user before confirming; flagged rows also need force.",
                    plan.rows.len()
                )
            } else {
                "All rows match the journal. Call again with confirm=true to delete them."
                    .to_string()
            };
            return json_result(&json!({
                "batch_id": plan.batch.batch.batch_id,
                "preview_only": true,
                "would_delete": plan.rows.iter().filter(|(_, s)| matches!(s, RowState::Deletable)).count(),
                "flagged": flagged,
                "warning": warning,
                "rows": preview,
            }));
        }

        let mut undo = UndoRecord {
            batch_id: plan.batch.batch.batch_id.clone(),
            at: journal::now(),
            client,
            deleted: Vec::new(),
            missing: Vec::new(),
            skipped: Vec::new(),
        };
        for (op, row_state) in &plan.rows {
            let id = op.transaction_id.clone();
            let delete = match row_state {
                RowState::Deletable => true,
                RowState::Reconciled | RowState::ChangedSinceCreated { .. } => force,
                RowState::Missing { reason } => {
                    undo.missing.push(id.clone());
                    tracing::warn!(id, reason, "undo: row already gone; recording as resolved");
                    continue;
                }
            };
            if !delete {
                undo.skipped.push(Skipped {
                    transaction_id: id,
                    reason: "flagged in preview; pass force to delete anyway".to_string(),
                });
                continue;
            }
            match self.client.delete_transaction(&id).await {
                Ok(()) => undo.deleted.push(id),
                Err(e) => undo.skipped.push(Skipped {
                    transaction_id: id,
                    reason: e.to_string(),
                }),
            }
        }
        locked
            .append(&Record::Undo(undo.clone()))
            .map_err(journal_error)?;
        let remaining = plan.rows.len() - undo.deleted.len() - undo.missing.len();
        json_result(&json!({
            "batch_id": undo.batch_id,
            "deleted": undo.deleted,
            "missing_already_gone": undo.missing,
            "skipped": undo.skipped,
            "remaining_in_batch": remaining,
        }))
    }
}

fn batch_json(b: &BatchState) -> serde_json::Value {
    let total: i64 = b.batch.ops.iter().map(|op| op.amount).sum();
    json!({
        "batch_id": b.batch.batch_id,
        "at": b.batch.at,
        "tool": b.batch.tool,
        "client": b.batch.client,
        "status": b.status(),
        "ops": b.batch.ops.len(),
        "remaining": b.remaining.len(),
        "total_amount": to_decimal(total),
        "undos": b.undos.len(),
        "transactions": b.batch.ops.iter().map(|op| json!({
            "id": op.transaction_id,
            "date": op.date,
            "amount": to_decimal(op.amount),
            "payee": op.payee_name,
            "account_id": op.account_id,
            "undone": !b.remaining.iter().any(|r| r.transaction_id == op.transaction_id),
        })).collect::<Vec<_>>(),
    })
}

fn category_json(c: &ynab::Category) -> serde_json::Value {
    json!({
        "id": c.id,
        "name": c.name,
        "group": c.category_group_name,
        "group_id": c.category_group_id,
        "assigned": to_decimal(c.budgeted),
        "activity": to_decimal(c.activity),
        "available": to_decimal(c.balance),
        "goal_type": c.goal_type,
        "goal_target": c.goal_target.map(to_decimal),
        "goal_target_date": c.goal_target_date,
        "goal_percentage_complete": c.goal_percentage_complete,
        "goal_under_funded": c.goal_under_funded.map(to_decimal),
        "goal_overall_left": c.goal_overall_left.map(to_decimal),
        "note": c.note,
    })
}

fn transaction_json(t: &ynab::Transaction) -> serde_json::Value {
    json!({
        "id": t.id,
        "date": t.date,
        "amount": to_decimal(t.amount),
        "account": t.account_name,
        "account_id": t.account_id,
        "payee": t.payee_name,
        "payee_id": t.payee_id,
        "category": t.category_name,
        "category_id": t.category_id,
        "memo": t.memo,
        "import_id": t.import_id,
        "cleared": t.cleared,
        "approved": t.approved,
        "flag": t.flag_color,
        "transfer_account_id": t.transfer_account_id,
        "splits": t.subtransactions.iter().map(|s| json!({
            "amount": to_decimal(s.amount),
            "payee": s.payee_name,
            "category": s.category_name,
            "memo": s.memo,
        })).collect::<Vec<_>>(),
    })
}

fn bank_row_json(r: &BankRow) -> serde_json::Value {
    json!({ "date": r.date.to_string(), "amount": to_decimal(r.amount), "description": r.description })
}

fn ynab_row_json(r: &YnabRow) -> serde_json::Value {
    json!({
        "id": r.id,
        "date": r.date.to_string(),
        "amount": to_decimal(r.amount),
        "payee": r.payee,
        "category": r.category,
        "cleared": r.cleared,
        "approved": r.approved,
    })
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for YnabServer {
    fn get_info(&self) -> ServerInfo {
        let mode = if self.allow_writes {
            "Writes are ENABLED: create_transactions lands rows unapproved and journals every batch; undo_batch / undo_last reverse them."
        } else {
            "Read-only: no tool can change the plan."
        };
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(format!(
                "YNAB plan access. Amounts are decimal strings in the plan currency; outflows are negative. \
                 Call list_accounts first to get account ids. Use reconcile_account for bank-vs-YNAB diffs \
                 instead of comparing rows yourself. {mode}"
            ))
    }
}
