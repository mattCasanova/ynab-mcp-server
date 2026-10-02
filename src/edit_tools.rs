//! Two write tools beyond bulk create: reshape one transaction (`replace_transaction`) and
//! assign money to categories by month (`assign_money`). Both follow the server's write
//! rules: gated by `allow_writes`, two-phase (preview, then `confirm`; flagged cases need
//! `force`), journaled under the journal lock, and undoable. The undo halves live here too and
//! are dispatched from `server::undo`.
//!
//! Split support for `create_transactions` (`split_lines`) also lives here.

use std::collections::{HashMap, HashSet};

use chrono::Datelike;
use rmcp::{
    ErrorData as McpError, RoleServer, handler::server::wrapper::Parameters, model::*, schemars,
    service::RequestContext, tool, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::journal::{
    self, AssignOp, AssignRecord, AssignState, LockedJournal, Record, ReplaceRecord, ReplaceState,
    Skipped, SkippedAssign, SnapshotLine, TransactionSnapshot, UndoRecord,
};
use crate::money::{Milliunits, from_decimal, to_decimal};
use crate::reconcile;
use crate::server::{
    RowState, YnabServer, api_error, client_name, invalid, journal_error, json_result,
    transaction_json,
};
use crate::ynab::{SaveSubTransaction, SaveTransaction, Transaction, YnabError};

// ---------------------------------------------------------------------------------------------
// Splits
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SplitLineArg {
    /// Signed decimal amount for this line. All lines must sum exactly to the parent amount.
    pub amount: f64,
    /// Category id from list_categories. For income use the "Inflow: Ready to Assign"
    /// category id. Unset lands the line uncategorized.
    pub category_id: Option<String>,
    pub memo: Option<String>,
    /// Payee for this line; unset uses the parent's.
    pub payee_name: Option<String>,
}

/// Split lines as API legs, after the checks YNAB enforces plus ours: no parent category,
/// at least two lines, and an exact milliunit sum. The error names the difference.
pub(crate) fn split_lines(
    total: Milliunits,
    parent_category: Option<&str>,
    lines: &[SplitLineArg],
) -> Result<Vec<SaveSubTransaction>, String> {
    if parent_category.is_some() {
        return Err(
            "a split cannot also have a parent category_id; put the category on each line"
                .to_string(),
        );
    }
    if lines.len() < 2 {
        return Err(format!(
            "a split needs at least 2 lines, got {}",
            lines.len()
        ));
    }
    let legs: Vec<SaveSubTransaction> = lines
        .iter()
        .map(|l| SaveSubTransaction {
            amount: from_decimal(l.amount),
            category_id: l.category_id.clone(),
            memo: l.memo.clone(),
            payee_id: None,
            payee_name: l.payee_name.clone(),
        })
        .collect();
    let sum: Milliunits = legs.iter().map(|l| l.amount).sum();
    if sum != total {
        return Err(format!(
            "split lines sum to {} but the transaction amount is {}; difference {}",
            to_decimal(sum),
            to_decimal(total),
            to_decimal(sum - total)
        ));
    }
    Ok(legs)
}

// ---------------------------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------------------------

pub(crate) fn snapshot_of(t: &Transaction) -> TransactionSnapshot {
    TransactionSnapshot {
        transaction_id: t.id.clone(),
        account_id: t.account_id.clone(),
        date: t.date.clone(),
        amount: t.amount,
        payee_id: t.payee_id.clone(),
        payee_name: t.payee_name.clone(),
        category_id: t.category_id.clone(),
        memo: t.memo.clone(),
        cleared: t.cleared.clone(),
        approved: t.approved,
        flag_color: t.flag_color.clone(),
        import_id: t.import_id.clone(),
        subtransactions: t
            .subtransactions
            .iter()
            .filter(|s| !s.deleted)
            .map(|s| SnapshotLine {
                amount: s.amount,
                category_id: s.category_id.clone(),
                memo: s.memo.clone(),
                payee_id: s.payee_id.clone(),
                payee_name: s.payee_name.clone(),
            })
            .collect(),
    }
}

/// The snapshot as a create request: no import_id (YNAB drops a repeat as a duplicate),
/// `approved` false so the row lands in the review queue, everything else as it was. A split
/// parent carries no category of its own.
pub(crate) fn save_from_snapshot(s: &TransactionSnapshot) -> SaveTransaction {
    let split = !s.subtransactions.is_empty();
    SaveTransaction {
        account_id: s.account_id.clone(),
        date: s.date.clone(),
        amount: s.amount,
        payee_id: s.payee_id.clone(),
        payee_name: s.payee_name.clone(),
        category_id: if split { None } else { s.category_id.clone() },
        memo: s.memo.clone(),
        cleared: s.cleared.clone(),
        approved: false,
        flag_color: s.flag_color.clone(),
        import_id: None,
        subtransactions: if split {
            Some(
                s.subtransactions
                    .iter()
                    .map(|l| SaveSubTransaction {
                        amount: l.amount,
                        category_id: l.category_id.clone(),
                        memo: l.memo.clone(),
                        payee_id: l.payee_id.clone(),
                        payee_name: l.payee_name.clone(),
                    })
                    .collect(),
            )
        } else {
            None
        },
    }
}

/// What we asked YNAB to create, with the id it handed back. Used when the create response
/// carries ids but no row detail.
fn snapshot_from_save(id: &str, save: &SaveTransaction) -> TransactionSnapshot {
    TransactionSnapshot {
        transaction_id: id.to_string(),
        account_id: save.account_id.clone(),
        date: save.date.clone(),
        amount: save.amount,
        payee_id: save.payee_id.clone(),
        payee_name: save.payee_name.clone(),
        category_id: save.category_id.clone(),
        memo: save.memo.clone(),
        cleared: save.cleared.clone(),
        approved: save.approved,
        flag_color: save.flag_color.clone(),
        import_id: save.import_id.clone(),
        subtransactions: save
            .subtransactions
            .iter()
            .flatten()
            .map(|l| SnapshotLine {
                amount: l.amount,
                category_id: l.category_id.clone(),
                memo: l.memo.clone(),
                payee_id: l.payee_id.clone(),
                payee_name: l.payee_name.clone(),
            })
            .collect(),
    }
}

/// Shape check for undo: the row still has the amount, date, account, and category lines the
/// journal wrote. Memos and payees may be edited freely.
pub(crate) fn row_matches_snapshot(t: &Transaction, s: &TransactionSnapshot) -> bool {
    if t.amount != s.amount || t.date != s.date || t.account_id != s.account_id {
        return false;
    }
    let mut now: Vec<(Milliunits, Option<&str>)> = t
        .subtransactions
        .iter()
        .filter(|l| !l.deleted)
        .map(|l| (l.amount, l.category_id.as_deref()))
        .collect();
    let mut then: Vec<(Milliunits, Option<&str>)> = s
        .subtransactions
        .iter()
        .map(|l| (l.amount, l.category_id.as_deref()))
        .collect();
    now.sort_unstable();
    then.sort_unstable();
    if now != then {
        return false;
    }
    // A single-category row must keep its category; a split parent has none of its own.
    if then.is_empty() {
        t.category_id == s.category_id
    } else {
        true
    }
}

fn save_json(s: &SaveTransaction) -> serde_json::Value {
    json!({
        "account_id": s.account_id,
        "date": s.date,
        "amount": to_decimal(s.amount),
        "payee_id": s.payee_id,
        "payee": s.payee_name,
        "category_id": s.category_id,
        "memo": s.memo,
        "cleared": s.cleared,
        "approved": s.approved,
        "import_id": s.import_id,
        "splits": s.subtransactions.iter().flatten().map(|l| json!({
            "amount": to_decimal(l.amount),
            "category_id": l.category_id,
            "memo": l.memo,
            "payee": l.payee_name,
        })).collect::<Vec<_>>(),
    })
}

fn snapshot_json(s: &TransactionSnapshot) -> serde_json::Value {
    json!({
        "id": s.transaction_id,
        "account_id": s.account_id,
        "date": s.date,
        "amount": to_decimal(s.amount),
        "payee": s.payee_name,
        "category_id": s.category_id,
        "memo": s.memo,
        "cleared": s.cleared,
        "approved": s.approved,
        "import_id": s.import_id,
        "splits": s.subtransactions.iter().map(|l| json!({
            "amount": to_decimal(l.amount),
            "category_id": l.category_id,
            "memo": l.memo,
            "payee": l.payee_name,
        })).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------------------------
// replace_transaction
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReplaceTransactionArgs {
    /// Id of the transaction to reshape (from list_transactions).
    pub transaction_id: String,
    /// New single category id. Give exactly one of category_id / subtransactions.
    pub category_id: Option<String>,
    /// New split lines; they must sum exactly to the original amount.
    pub subtransactions: Option<Vec<SplitLineArg>>,
    /// New memo on the parent. Unset keeps the original's.
    pub memo: Option<String>,
    /// false (default) = preview only. true = create the replacement, then delete the original.
    #[serde(default)]
    pub confirm: bool,
    /// With confirm: also replace an original that is reconciled. Default false.
    #[serde(default)]
    pub force: bool,
}

pub(crate) enum NewShape {
    Single(String),
    Split(Vec<SplitLineArg>),
}

#[derive(Debug)]
pub(crate) struct Replacement {
    pub save: SaveTransaction,
    /// "reconciled" (needs force), "has_import_id" (the replacement gets none).
    pub flags: Vec<&'static str>,
}

/// The replacement row: same account, date, payee, cleared state, and total as the original,
/// by construction. Only the category shape and memo change.
pub(crate) fn plan_replacement(
    original: &Transaction,
    shape: &NewShape,
    memo: Option<&str>,
) -> Result<Replacement, String> {
    if original.deleted {
        return Err(format!("transaction {} is deleted in YNAB", original.id));
    }
    let is_transfer = original.transfer_account_id.is_some()
        || original
            .subtransactions
            .iter()
            .any(|s| !s.deleted && s.transfer_account_id.is_some());
    if is_transfer {
        return Err(
            "transfers are out of scope for replace_transaction: the other side would be left behind"
                .to_string(),
        );
    }
    let (category_id, subtransactions) = match shape {
        NewShape::Single(category) => (Some(category.clone()), None),
        NewShape::Split(lines) => (None, Some(split_lines(original.amount, None, lines)?)),
    };
    let mut flags = Vec::new();
    if original.cleared == "reconciled" {
        flags.push("reconciled");
    }
    if original.import_id.is_some() {
        flags.push("has_import_id");
    }
    Ok(Replacement {
        save: SaveTransaction {
            account_id: original.account_id.clone(),
            date: original.date.clone(),
            amount: original.amount,
            payee_id: original.payee_id.clone(),
            payee_name: original.payee_name.clone(),
            category_id,
            memo: memo.map(str::to_string).or_else(|| original.memo.clone()),
            cleared: original.cleared.clone(),
            approved: false,
            flag_color: original.flag_color.clone(),
            import_id: None,
            subtransactions,
        },
        flags,
    })
}

// ---------------------------------------------------------------------------------------------
// assign_money
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct AssignChangeArg {
    /// Month as YYYY-MM-01.
    pub month: String,
    /// Category id from list_categories.
    pub category_id: String,
    /// Decimal DELTA on the category's assigned amount: positive assigns from Ready to
    /// Assign, negative returns to it.
    pub amount: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AssignMoneyArgs {
    pub changes: Vec<AssignChangeArg>,
    /// false (default) = preview only. true = apply the changes.
    #[serde(default)]
    pub confirm: bool,
    /// With confirm: also apply when a month's Ready to Assign would go negative. Default false.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AssignChange {
    pub month: String,
    pub category_id: String,
    pub delta: Milliunits,
}

pub(crate) fn parse_changes(args: &[AssignChangeArg]) -> Result<Vec<AssignChange>, String> {
    if args.is_empty() {
        return Err("changes is empty".to_string());
    }
    let mut seen: HashSet<(String, String)> = HashSet::new();
    args.iter()
        .map(|c| {
            let date = reconcile::parse_date(&c.month)
                .ok_or_else(|| format!("month must be YYYY-MM-01, got {:?}", c.month))?;
            if date.day() != 1 {
                return Err(format!(
                    "month must be the first of the month (YYYY-MM-01), got {:?}",
                    c.month
                ));
            }
            let delta = from_decimal(c.amount);
            if delta == 0 {
                return Err(format!(
                    "amount for {}/{} is zero; nothing to assign",
                    c.month, c.category_id
                ));
            }
            let month = date.to_string();
            if !seen.insert((month.clone(), c.category_id.clone())) {
                return Err(format!(
                    "{month}/{} appears twice; combine the deltas into one change",
                    c.category_id
                ));
            }
            Ok(AssignChange {
                month,
                category_id: c.category_id.clone(),
                delta,
            })
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct MonthImpact {
    pub month: String,
    pub ready_before: Milliunits,
    pub ready_after: Milliunits,
}

#[derive(Debug)]
pub(crate) struct AssignPlan {
    pub ops: Vec<AssignOp>,
    pub months: Vec<MonthImpact>,
}

/// Current assigned amount and name per (month, category), and Ready to Assign per month.
pub(crate) type BudgetedBefore = HashMap<(String, String), (Milliunits, Option<String>)>;
pub(crate) type ReadyBefore = HashMap<String, Milliunits>;

/// Deltas applied to the CURRENT assigned amounts; Ready to Assign moves the other way.
pub(crate) fn plan_assign(
    changes: &[AssignChange],
    budgeted_before: &BudgetedBefore,
    ready_before: &ReadyBefore,
) -> Result<AssignPlan, String> {
    let ops = changes
        .iter()
        .map(|c| {
            let key = (c.month.clone(), c.category_id.clone());
            let (old, name) = budgeted_before
                .get(&key)
                .ok_or_else(|| format!("no current numbers for {}/{}", c.month, c.category_id))?;
            Ok(AssignOp {
                month: c.month.clone(),
                category_id: c.category_id.clone(),
                category_name: name.clone(),
                old_budgeted: *old,
                new_budgeted: old + c.delta,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut months: Vec<MonthImpact> = Vec::new();
    for c in changes {
        if !months.iter().any(|m| m.month == c.month) {
            let before = *ready_before
                .get(&c.month)
                .ok_or_else(|| format!("no Ready to Assign figure for {}", c.month))?;
            months.push(MonthImpact {
                month: c.month.clone(),
                ready_before: before,
                ready_after: before,
            });
        }
        let m = months
            .iter_mut()
            .find(|m| m.month == c.month)
            .expect("just inserted");
        m.ready_after -= c.delta;
    }
    Ok(AssignPlan { ops, months })
}

fn assign_op_json(op: &AssignOp) -> serde_json::Value {
    json!({
        "month": op.month,
        "category_id": op.category_id,
        "category": op.category_name,
        "assigned_before": to_decimal(op.old_budgeted),
        "assigned_after": to_decimal(op.new_budgeted),
        "delta": to_decimal(op.new_budgeted - op.old_budgeted),
    })
}

fn month_impact_json(m: &MonthImpact) -> serde_json::Value {
    json!({
        "month": m.month,
        "ready_to_assign_before": to_decimal(m.ready_before),
        "ready_to_assign_after": to_decimal(m.ready_after),
        "goes_negative": m.ready_after < 0,
    })
}

#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum AssignCheck {
    Revertable,
    Missing { reason: String },
    ChangedSinceAssigned { now: String },
}

// ---------------------------------------------------------------------------------------------
// Deleting splits: YNAB's rollup quirk
// ---------------------------------------------------------------------------------------------

/// Observed 2026-10-01 against the live API: deleting a split through the API removes the row
/// from the ledger and the account balance, but the month rollup of each leg's category keeps
/// counting the leg until some write that references that category lands. A plain (non-split)
/// delete recomputes correctly. So after deleting a split, every leg category is nudged with a
/// zero-amount row that is created and deleted again.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Nudge {
    pub category_id: String,
    pub month: String,
    /// Always "zero_row" today; kept so a future mechanism can be told apart in old output.
    pub how: &'static str,
    pub ok: bool,
    pub error: Option<String>,
}

/// `YYYY-MM-DD` to the `YYYY-MM-01` the month endpoints take.
pub(crate) fn month_of(date: &str) -> Option<String> {
    reconcile::parse_date(date).map(|d| format!("{:04}-{:02}-01", d.year(), d.month()))
}

/// Distinct categories of a split's live legs, in leg order. Empty for a non-split.
pub(crate) fn leg_categories(t: &Transaction) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for leg in t.subtransactions.iter().filter(|l| !l.deleted) {
        if let Some(c) = &leg.category_id
            && !out.contains(c)
        {
            out.push(c.clone());
        }
    }
    out
}

impl YnabServer {
    /// Delete a row; if it was a split, nudge each leg's category so YNAB recomputes its month.
    /// The delete's error is returned; nudge failures are logged and reported, never fatal.
    /// Does not touch the month cache: the calling tool invalidates once, after its last write.
    pub(crate) async fn delete_with_recompute(
        &self,
        t: &Transaction,
    ) -> Result<Vec<Nudge>, YnabError> {
        self.client().delete_transaction(&t.id).await?;
        let Some(month) = month_of(&t.date) else {
            tracing::error!(
                date = %t.date,
                "deleted a row whose date does not parse; its legs were not nudged"
            );
            return Ok(Vec::new());
        };
        let categories = leg_categories(t);
        if categories.is_empty() {
            return Ok(Vec::new());
        }
        let mut nudges = Vec::with_capacity(categories.len());
        for category_id in categories {
            nudges.push(
                self.nudge_category(&month, &category_id, &t.account_id, &t.date)
                    .await,
            );
        }
        Ok(nudges)
    }

    /// Create a zero-amount row in the category and delete it again. Any transaction write
    /// that references a category makes YNAB recompute that category's month, and a plain
    /// delete recomputes correctly, so the pair leaves the ledger as it was and the totals
    /// right. (Re-writing the assigned amount unchanged does nothing: tried 2026-10-01.)
    async fn nudge_category(
        &self,
        month: &str,
        category_id: &str,
        account_id: &str,
        date: &str,
    ) -> Nudge {
        let nudge = |error: Option<String>| Nudge {
            category_id: category_id.to_string(),
            month: month.to_string(),
            how: "zero_row",
            ok: error.is_none(),
            error,
        };
        let save = SaveTransaction {
            account_id: account_id.to_string(),
            date: date.to_string(),
            amount: 0,
            payee_id: None,
            payee_name: None,
            category_id: Some(category_id.to_string()),
            memo: Some("ynab-mcp recompute nudge, auto-deleted".to_string()),
            cleared: "uncleared".to_string(),
            approved: false,
            flag_color: None,
            import_id: None,
            subtransactions: None,
        };
        let created = match self.client().create_transactions(&[save]).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "zero-row nudge: create failed; month rollup may be stale");
                return nudge(Some(e.to_string()));
            }
        };
        let Some(id) = created.transaction_ids.first() else {
            tracing::error!("zero-row nudge: YNAB created nothing; month rollup may be stale");
            return nudge(Some("YNAB created no zero-amount row".to_string()));
        };
        match self.client().delete_transaction(id).await {
            Ok(()) => nudge(None),
            Err(e) => {
                tracing::error!(error = %e, "zero-row nudge: the zero-amount row could not be deleted");
                nudge(Some(format!(
                    "zero-amount row {id} was created but could not be deleted: {e}"
                )))
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------------------------

#[tool_router(router = edit_write_router, vis = "pub")]
impl YnabServer {
    #[tool(
        description = "WRITE. Reshape one transaction: a new single category or new split lines, keeping the account, date, payee, cleared state, and total (checked to the cent). Creates the replacement (unapproved, no import_id), then deletes the original. Two-phase: preview first, then confirm=true; a reconciled original also needs force. Transfers are refused."
    )]
    async fn replace_transaction(
        &self,
        Parameters(args): Parameters<ReplaceTransactionArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.require_writes()?;
        let shape = match (args.category_id, args.subtransactions) {
            (Some(category), None) => NewShape::Single(category),
            (None, Some(lines)) => NewShape::Split(lines),
            (None, None) => {
                return Err(invalid(
                    "give category_id (single category) or subtransactions (split)",
                ));
            }
            (Some(_), Some(_)) => {
                return Err(invalid("give category_id or subtransactions, not both"));
            }
        };
        // Lock first: a concurrent undo must not delete or re-create rows between our GET
        // and our POST/DELETE.
        let mut locked = self.journal().lock().map_err(journal_error)?;
        let original = self
            .client()
            .transaction(&args.transaction_id)
            .await
            .map_err(api_error)?;
        let replacement =
            plan_replacement(&original, &shape, args.memo.as_deref()).map_err(invalid)?;
        let needs_force = replacement.flags.contains(&"reconciled");

        if !args.confirm {
            let warning = if needs_force {
                "The original is reconciled. Ask the user, then call again with confirm=true and force=true."
            } else {
                "Totals match. Call again with confirm=true to create the replacement and delete the original."
            };
            return json_result(&json!({
                "preview_only": true,
                "transaction_id": original.id,
                "original": transaction_json(&original),
                "replacement": save_json(&replacement.save),
                "flags": replacement.flags,
                "warning": warning,
            }));
        }
        if needs_force && !args.force {
            return Err(invalid(format!(
                "transaction {} is reconciled; pass force=true to replace it anyway",
                original.id
            )));
        }

        // 1. Create the replacement. If this fails nothing has changed.
        let created = self
            .client()
            .create_transactions(std::slice::from_ref(&replacement.save))
            .await
            .map_err(api_error)?;
        let Some(new_id) = created.transaction_ids.first().cloned() else {
            tracing::error!(
                duplicates = ?created.duplicate_import_ids,
                "replace: YNAB created nothing; original untouched"
            );
            return Err(McpError::internal_error(
                format!(
                    "YNAB created no replacement row (duplicate_import_ids: {:?}); the original is untouched",
                    created.duplicate_import_ids
                ),
                None,
            ));
        };
        let replacement_snapshot = match created.transactions.iter().find(|t| t.id == new_id) {
            Some(t) => snapshot_of(t),
            None => {
                tracing::warn!(
                    "replace: create response had the id but no row detail; journaling what was sent"
                );
                snapshot_from_save(&new_id, &replacement.save)
            }
        };

        // 2. Delete the original. A failure here leaves both rows; say so loudly.
        let (delete_error, nudges) = match self.delete_with_recompute(&original).await {
            Ok(nudges) => (None, nudges),
            Err(e) => {
                tracing::error!(error = %e, "replace: replacement created but delete of the original failed");
                (Some(e), Vec::new())
            }
        };
        // The create landed either way, so the cache is stale either way.
        let touched: Vec<String> = month_of(&original.date).into_iter().collect();
        let dropped = self.invalidate_cache_from(&touched).await;
        let record = ReplaceRecord {
            batch_id: journal::new_batch_id(),
            at: journal::now(),
            plan_id: self.client().plan_id().to_string(),
            tool: "replace_transaction".to_string(),
            client: client_name(&ctx),
            original: snapshot_of(&original),
            replacement: replacement_snapshot,
            original_deleted: delete_error.is_none(),
        };
        if let Err(e) = locked.append(&Record::Replace(Box::new(record.clone()))) {
            tracing::error!(
                error = format!("{e:#}"),
                "replace: rows changed but could not journal"
            );
            return Err(McpError::internal_error(
                format!(
                    "created replacement {new_id} and {} the original {}, but could not write the journal ({e:#}); this replace is NOT undoable",
                    if delete_error.is_none() {
                        "deleted"
                    } else {
                        "FAILED to delete"
                    },
                    original.id
                ),
                None,
            ));
        }
        if let Some(e) = delete_error {
            return Err(McpError::internal_error(
                format!(
                    "created replacement {new_id} but could not delete the original {} ({e}). BOTH rows are in YNAB now: delete the original by hand, or undo batch {} to remove the replacement.",
                    original.id, record.batch_id
                ),
                None,
            ));
        }
        json_result(&json!({
            "batch_id": record.batch_id,
            "original_transaction_id": original.id,
            "replacement_transaction_id": new_id,
            "amount": to_decimal(original.amount),
            "replacement": snapshot_json(&record.replacement),
            "recompute_nudges": nudges,
            "months_dropped_from_cache": dropped,
            "undo_with": "undo_batch or undo_last",
        }))
    }

    #[tool(
        description = "WRITE. Assign money between Ready to Assign and categories, per month. Each amount is a DELTA on the category's assigned amount (positive assigns, negative returns to Ready to Assign). Two-phase: preview shows assigned before/after per change and Ready to Assign before/after per month; confirm=true applies; a month whose Ready to Assign would go negative also needs force. The earliest touched month and every later one are dropped from the month cache, since balances carry forward."
    )]
    async fn assign_money(
        &self,
        Parameters(args): Parameters<AssignMoneyArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.require_writes()?;
        let changes = parse_changes(&args.changes).map_err(invalid)?;
        let mut locked = self.journal().lock().map_err(journal_error)?;

        let mut budgeted_before: BudgetedBefore = HashMap::new();
        for c in &changes {
            let cat = self
                .client()
                .month_category(&c.month, &c.category_id)
                .await
                .map_err(api_error)?;
            budgeted_before.insert(
                (c.month.clone(), c.category_id.clone()),
                (cat.budgeted, Some(cat.name)),
            );
        }
        let mut ready_before: ReadyBefore = HashMap::new();
        for c in &changes {
            if ready_before.contains_key(&c.month) {
                continue;
            }
            // Live, not through the cache: this is a before-value for a write.
            let month = self.client().month(&c.month).await.map_err(api_error)?;
            ready_before.insert(c.month.clone(), month.to_be_budgeted);
        }
        let plan = plan_assign(&changes, &budgeted_before, &ready_before).map_err(invalid)?;
        let negative: Vec<&str> = plan
            .months
            .iter()
            .filter(|m| m.ready_after < 0)
            .map(|m| m.month.as_str())
            .collect();

        if !args.confirm {
            let warning = if negative.is_empty() {
                "Call again with confirm=true to apply.".to_string()
            } else {
                format!(
                    "Ready to Assign would go negative in {}. Ask the user; applying also needs force.",
                    negative.join(", ")
                )
            };
            return json_result(&json!({
                "preview_only": true,
                "changes": plan.ops.iter().map(assign_op_json).collect::<Vec<_>>(),
                "months": plan.months.iter().map(month_impact_json).collect::<Vec<_>>(),
                "note": "Ready to Assign carries forward, so every later month moves by the same net amount.",
                "flagged_months": negative,
                "warning": warning,
            }));
        }
        if !negative.is_empty() && !args.force {
            return Err(invalid(format!(
                "Ready to Assign would go negative in {}; pass force=true to apply anyway",
                negative.join(", ")
            )));
        }

        let mut done: Vec<AssignOp> = Vec::new();
        let mut failure: Option<(AssignOp, YnabError)> = None;
        for op in &plan.ops {
            match self
                .client()
                .update_month_category(&op.month, &op.category_id, op.new_budgeted)
                .await
            {
                Ok(cat) => {
                    if cat.budgeted != op.new_budgeted {
                        tracing::error!(
                            "assign: YNAB stored a different assigned amount than requested; journaling what it stored"
                        );
                    }
                    done.push(AssignOp {
                        new_budgeted: cat.budgeted,
                        ..op.clone()
                    });
                }
                Err(e) => {
                    tracing::error!(error = %e, "assign: PATCH failed part-way");
                    failure = Some((op.clone(), e));
                    break;
                }
            }
        }
        let touched: Vec<String> = done.iter().map(|op| op.month.clone()).collect();
        let invalidated = self.invalidate_cache_from(&touched).await;

        let mut batch_id = None;
        if !done.is_empty() {
            let record = AssignRecord {
                batch_id: journal::new_batch_id(),
                at: journal::now(),
                plan_id: self.client().plan_id().to_string(),
                tool: "assign_money".to_string(),
                client: client_name(&ctx),
                ops: done.clone(),
            };
            if let Err(e) = locked.append(&Record::Assign(record.clone())) {
                tracing::error!(
                    error = format!("{e:#}"),
                    "assign: applied but could not journal"
                );
                return Err(McpError::internal_error(
                    format!(
                        "applied {} assignment(s) but could not write the journal ({e:#}); NOT undoable: {}",
                        done.len(),
                        serde_json::to_string(&done).unwrap_or_default()
                    ),
                    None,
                ));
            }
            batch_id = Some(record.batch_id);
        }
        if let Some((op, e)) = failure {
            return Err(McpError::internal_error(
                format!(
                    "applied {} of {} changes, then {}/{} failed: {e}. {}",
                    done.len(),
                    plan.ops.len(),
                    op.month,
                    op.category_id,
                    match &batch_id {
                        Some(id) => format!(
                            "The applied ones are journaled as batch {id} and can be undone."
                        ),
                        None => "Nothing was journaled.".to_string(),
                    }
                ),
                None,
            ));
        }
        json_result(&json!({
            "batch_id": batch_id,
            "applied": done.len(),
            "changes": done.iter().map(assign_op_json).collect::<Vec<_>>(),
            "months_dropped_from_cache": invalidated,
            "undo_with": "undo_batch or undo_last",
        }))
    }
}

// ---------------------------------------------------------------------------------------------
// Undo halves, dispatched from server::undo
// ---------------------------------------------------------------------------------------------

impl YnabServer {
    async fn check_replacement(
        &self,
        snapshot: &TransactionSnapshot,
    ) -> Result<(RowState, Option<Transaction>), McpError> {
        match self.client().transaction(&snapshot.transaction_id).await {
            Err(YnabError::Api { status: 404, .. }) => Ok((
                RowState::Missing {
                    reason: "transaction not found in YNAB (deleted by hand?)".to_string(),
                },
                None,
            )),
            Err(e) => Err(api_error(e)),
            Ok(t) if t.deleted => Ok((
                RowState::Missing {
                    reason: "transaction deleted in YNAB".to_string(),
                },
                None,
            )),
            Ok(t) if t.cleared == "reconciled" => Ok((RowState::Reconciled, Some(t))),
            Ok(t) if !row_matches_snapshot(&t, snapshot) => Ok((
                RowState::ChangedSinceCreated {
                    now: transaction_json(&t),
                },
                Some(t),
            )),
            Ok(t) => Ok((RowState::Deletable, Some(t))),
        }
    }

    pub(crate) async fn undo_replace(
        &self,
        state: &ReplaceState,
        confirm: bool,
        force: bool,
        client: Option<String>,
        locked: &mut LockedJournal,
    ) -> Result<CallToolResult, McpError> {
        let record = &state.record;
        let replacement_id = record.replacement.transaction_id.clone();
        let (check, current) = if state.replacement_gone {
            (None, None)
        } else {
            let (check, current) = self.check_replacement(&record.replacement).await?;
            (Some(check), current)
        };
        let flagged = matches!(
            check,
            Some(RowState::Reconciled | RowState::ChangedSinceCreated { .. })
        );
        let will_restore = !state.original_restored;

        if !confirm {
            let warning = if flagged {
                "The replacement row cannot be vouched for by the journal any more (see check). Ask the user before confirming; it also needs force."
            } else {
                "Call again with confirm=true to delete the replacement and re-create the original."
            };
            return json_result(&json!({
                "batch_id": record.batch_id,
                "kind": "replace",
                "preview_only": true,
                "replacement": { "id": replacement_id, "check": check, "already_gone": state.replacement_gone },
                "would_recreate_original": will_restore,
                "original": snapshot_json(&record.original),
                "flagged": usize::from(flagged),
                "warning": warning,
            }));
        }

        let mut undo = UndoRecord::new(record.batch_id.clone(), client);
        let mut gone = state.replacement_gone;
        let mut nudges: Vec<Nudge> = Vec::new();
        let deletable = match &check {
            None | Some(RowState::Missing { .. }) => false,
            Some(RowState::Deletable) => true,
            Some(RowState::Reconciled | RowState::ChangedSinceCreated { .. }) => force,
        };
        match (&check, current) {
            (None, _) => {}
            (Some(RowState::Missing { reason }), _) => {
                tracing::warn!(
                    reason,
                    "undo replace: replacement already gone; recording as resolved"
                );
                undo.missing.push(replacement_id.clone());
                gone = true;
            }
            (Some(_), Some(row)) if deletable => match self.delete_with_recompute(&row).await {
                Ok(n) => {
                    undo.deleted.push(replacement_id.clone());
                    gone = true;
                    nudges = n;
                }
                Err(e) => undo.skipped.push(Skipped {
                    transaction_id: replacement_id.clone(),
                    reason: e.to_string(),
                }),
            },
            (Some(_), None) if deletable => {
                // check_replacement returns the row for every non-missing state; reaching
                // here is a programming error, so say so rather than deleting blind.
                tracing::error!("undo replace: deletable row without its detail; skipping");
                undo.skipped.push(Skipped {
                    transaction_id: replacement_id.clone(),
                    reason: "internal: row detail missing; re-run the undo".to_string(),
                });
            }
            (Some(_), _) => undo.skipped.push(Skipped {
                transaction_id: replacement_id.clone(),
                reason: "flagged in preview; pass force to delete anyway".to_string(),
            }),
        }

        let mut recreate_error: Option<String> = None;
        if gone && will_restore {
            let save = save_from_snapshot(&record.original);
            match self.client().create_transactions(&[save]).await {
                Ok(res) => match res.transaction_ids.first() {
                    Some(id) => undo.recreated_transaction_id = Some(id.clone()),
                    None => recreate_error = Some("YNAB created nothing".to_string()),
                },
                Err(e) => recreate_error = Some(e.to_string()),
            }
        }
        let wrote = !undo.deleted.is_empty() || undo.recreated_transaction_id.is_some();
        let touched: Vec<String> = if wrote {
            month_of(&record.original.date).into_iter().collect()
        } else {
            Vec::new()
        };
        let dropped = self.invalidate_cache_from(&touched).await;
        locked
            .append(&Record::Undo(undo.clone()))
            .map_err(journal_error)?;
        if let Some(err) = recreate_error {
            tracing::error!(error = %err, "undo replace: original could not be re-created");
            return Err(McpError::internal_error(
                format!(
                    "the replacement {} is gone but the original could not be re-created ({err}). The money is missing from the account until it is re-entered: run undo_batch {} again to retry, or enter it by hand from this snapshot: {}",
                    replacement_id,
                    record.batch_id,
                    serde_json::to_string(&record.original).unwrap_or_default()
                ),
                None,
            ));
        }
        let open = !(gone && (state.original_restored || undo.recreated_transaction_id.is_some()));
        json_result(&json!({
            "batch_id": undo.batch_id,
            "kind": "replace",
            "deleted": undo.deleted,
            "missing_already_gone": undo.missing,
            "skipped": undo.skipped,
            "recreated_transaction_id": undo.recreated_transaction_id,
            "recompute_nudges": nudges,
            "months_dropped_from_cache": dropped,
            "status": if open { "partially_undone" } else { "undone" },
        }))
    }

    pub(crate) async fn undo_assign(
        &self,
        state: &AssignState,
        confirm: bool,
        force: bool,
        client: Option<String>,
        locked: &mut LockedJournal,
    ) -> Result<CallToolResult, McpError> {
        let record = &state.record;
        let mut rows: Vec<(&AssignOp, AssignCheck)> = Vec::new();
        for op in &state.remaining {
            let check = match self
                .client()
                .month_category(&op.month, &op.category_id)
                .await
            {
                Ok(cat) if cat.budgeted == op.new_budgeted => AssignCheck::Revertable,
                Ok(cat) => AssignCheck::ChangedSinceAssigned {
                    now: to_decimal(cat.budgeted),
                },
                Err(YnabError::Api { status: 404, .. }) => AssignCheck::Missing {
                    reason: "category or month not found in YNAB".to_string(),
                },
                Err(e) => return Err(api_error(e)),
            };
            rows.push((op, check));
        }
        let flagged = rows
            .iter()
            .filter(|(_, c)| !matches!(c, AssignCheck::Revertable))
            .count();
        let preview: Vec<_> = rows
            .iter()
            .map(|(op, c)| json!({ "assignment": assign_op_json(op), "check": c }))
            .collect();

        if !confirm {
            let warning = if flagged > 0 {
                format!(
                    "{flagged} of {} assignments changed since they were made (see check). Ask the user before confirming; flagged ones also need force.",
                    rows.len()
                )
            } else {
                "All assignments still hold. Call again with confirm=true to set them back."
                    .to_string()
            };
            return json_result(&json!({
                "batch_id": record.batch_id,
                "kind": "assign",
                "preview_only": true,
                "would_revert": rows.len() - flagged,
                "flagged": flagged,
                "warning": warning,
                "rows": preview,
            }));
        }

        let mut undo = UndoRecord::new(record.batch_id.clone(), client);
        for (op, check) in &rows {
            let revert = match check {
                AssignCheck::Revertable => true,
                AssignCheck::ChangedSinceAssigned { .. } => force,
                AssignCheck::Missing { reason } => {
                    undo.skipped_assignments.push(SkippedAssign {
                        month: op.month.clone(),
                        category_id: op.category_id.clone(),
                        reason: reason.clone(),
                    });
                    continue;
                }
            };
            if !revert {
                undo.skipped_assignments.push(SkippedAssign {
                    month: op.month.clone(),
                    category_id: op.category_id.clone(),
                    reason: "flagged in preview; pass force to revert anyway".to_string(),
                });
                continue;
            }
            match self
                .client()
                .update_month_category(&op.month, &op.category_id, op.old_budgeted)
                .await
            {
                Ok(_) => undo.reverted.push(op.key()),
                Err(e) => undo.skipped_assignments.push(SkippedAssign {
                    month: op.month.clone(),
                    category_id: op.category_id.clone(),
                    reason: e.to_string(),
                }),
            }
        }
        let touched: Vec<String> = undo.reverted.iter().map(|k| k.month.clone()).collect();
        let invalidated = self.invalidate_cache_from(&touched).await;
        locked
            .append(&Record::Undo(undo.clone()))
            .map_err(journal_error)?;
        let remaining = rows.len() - undo.reverted.len();
        json_result(&json!({
            "batch_id": undo.batch_id,
            "kind": "assign",
            "reverted": undo.reverted,
            "skipped": undo.skipped_assignments,
            "remaining_in_batch": remaining,
            "months_dropped_from_cache": invalidated,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(amount: f64, category: &str) -> SplitLineArg {
        SplitLineArg {
            amount,
            category_id: Some(category.to_string()),
            memo: None,
            payee_name: None,
        }
    }

    fn original() -> Transaction {
        Transaction {
            id: "orig".to_string(),
            date: "2026-07-27".to_string(),
            amount: 1_553_000,
            memo: Some("payout".to_string()),
            cleared: "reconciled".to_string(),
            approved: true,
            flag_color: None,
            account_id: "acct".to_string(),
            account_name: Some("PR Buisness".to_string()),
            payee_id: Some("stripe-id".to_string()),
            payee_name: Some("STRIPE".to_string()),
            category_id: Some("split".to_string()),
            category_name: Some("Split".to_string()),
            transfer_account_id: None,
            import_id: Some("YNAB:1553000:2026-07-27:1".to_string()),
            deleted: false,
            subtransactions: vec![],
        }
    }

    #[test]
    fn split_lines_must_sum_exactly() {
        let legs = split_lines(
            1_553_000,
            None,
            &[
                line(800.0, "rta"),
                line(776.5, "owner"),
                line(-23.5, "fees"),
            ],
        )
        .unwrap();
        assert_eq!(legs.len(), 3);
        assert_eq!(legs.iter().map(|l| l.amount).sum::<Milliunits>(), 1_553_000);

        let err = split_lines(
            1_553_000,
            None,
            &[
                line(800.0, "rta"),
                line(776.51, "owner"),
                line(-23.5, "fees"),
            ],
        )
        .unwrap_err();
        assert!(err.contains("difference 0.01"), "{err}");
    }

    #[test]
    fn split_needs_two_lines_and_no_parent_category() {
        let err = split_lines(800_000, None, &[line(800.0, "rta")]).unwrap_err();
        assert!(err.contains("at least 2"), "{err}");
        let err = split_lines(
            800_000,
            Some("parent"),
            &[line(400.0, "a"), line(400.0, "b")],
        )
        .unwrap_err();
        assert!(err.contains("parent category_id"), "{err}");
    }

    #[test]
    fn replacement_keeps_account_date_payee_cleared_and_total() {
        let shape = NewShape::Split(vec![
            line(800.0, "rta"),
            line(776.5, "owner"),
            line(-23.5, "fees"),
        ]);
        let r = plan_replacement(&original(), &shape, None).unwrap();
        assert_eq!(r.save.account_id, "acct");
        assert_eq!(r.save.date, "2026-07-27");
        assert_eq!(r.save.payee_id.as_deref(), Some("stripe-id"));
        assert_eq!(r.save.payee_name.as_deref(), Some("STRIPE"));
        assert_eq!(r.save.amount, 1_553_000);
        assert_eq!(r.save.cleared, "reconciled");
        assert!(!r.save.approved);
        assert!(r.save.import_id.is_none(), "no import_id on a replacement");
        assert!(r.save.category_id.is_none());
        assert_eq!(r.save.memo.as_deref(), Some("payout"), "memo kept");
        assert_eq!(r.flags, vec!["reconciled", "has_import_id"]);
    }

    #[test]
    fn replacement_total_mismatch_is_refused() {
        let shape = NewShape::Split(vec![line(800.0, "rta"), line(-23.5, "fees")]);
        let err = plan_replacement(&original(), &shape, None).unwrap_err();
        assert!(err.contains("difference"), "{err}");
    }

    #[test]
    fn replacement_single_category_and_new_memo() {
        let mut t = original();
        t.cleared = "cleared".to_string();
        t.import_id = None;
        let r =
            plan_replacement(&t, &NewShape::Single("rta".into()), Some("Coaching pack")).unwrap();
        assert_eq!(r.save.category_id.as_deref(), Some("rta"));
        assert!(r.save.subtransactions.is_none());
        assert_eq!(r.save.memo.as_deref(), Some("Coaching pack"));
        assert!(r.flags.is_empty(), "not reconciled, no import id");
    }

    #[test]
    fn replacement_refuses_transfers_and_deleted_rows() {
        let mut t = original();
        t.transfer_account_id = Some("other".into());
        let err = plan_replacement(&t, &NewShape::Single("rta".into()), None).unwrap_err();
        assert!(err.contains("transfers"), "{err}");
        let mut t = original();
        t.deleted = true;
        let err = plan_replacement(&t, &NewShape::Single("rta".into()), None).unwrap_err();
        assert!(err.contains("deleted"), "{err}");
    }

    #[test]
    fn snapshot_round_trips_to_a_create_without_import_id_or_approval() {
        let mut t = original();
        t.subtransactions = vec![
            crate::ynab::SubTransaction {
                amount: 1_600_000,
                memo: None,
                payee_id: None,
                payee_name: None,
                category_id: Some("vci".into()),
                category_name: None,
                transfer_account_id: None,
                deleted: false,
            },
            crate::ynab::SubTransaction {
                amount: -47_000,
                memo: Some("fee".into()),
                payee_id: None,
                payee_name: None,
                category_id: Some("vci".into()),
                category_name: None,
                transfer_account_id: None,
                deleted: false,
            },
            crate::ynab::SubTransaction {
                amount: 1,
                memo: None,
                payee_id: None,
                payee_name: None,
                category_id: None,
                category_name: None,
                transfer_account_id: None,
                deleted: true,
            },
        ];
        let snap = snapshot_of(&t);
        assert_eq!(snap.subtransactions.len(), 2, "deleted legs dropped");
        let save = save_from_snapshot(&snap);
        assert!(save.import_id.is_none());
        assert!(!save.approved);
        assert_eq!(save.cleared, "reconciled");
        assert!(save.category_id.is_none(), "split parent has no category");
        assert_eq!(save.subtransactions.as_ref().unwrap().len(), 2);
        assert!(row_matches_snapshot(&t, &snap));
        let mut edited = t.clone();
        edited.subtransactions[1].amount = -46_000;
        assert!(!row_matches_snapshot(&edited, &snap));
    }

    #[test]
    fn month_of_and_leg_categories() {
        assert_eq!(month_of("2026-07-27").as_deref(), Some("2026-07-01"));
        assert!(month_of("July").is_none());
        let mut t = original();
        assert!(leg_categories(&t).is_empty(), "non-split has no legs");
        let leg = |cat: Option<&str>, deleted: bool| crate::ynab::SubTransaction {
            amount: 1,
            memo: None,
            payee_id: None,
            payee_name: None,
            category_id: cat.map(str::to_string),
            category_name: None,
            transfer_account_id: None,
            deleted,
        };
        t.subtransactions = vec![
            leg(Some("rta"), false),
            leg(Some("fees"), false),
            leg(Some("rta"), false),
            leg(Some("gone"), true),
            leg(None, false),
        ];
        assert_eq!(leg_categories(&t), vec!["rta", "fees"]);
    }

    fn change(month: &str, cat: &str, amount: f64) -> AssignChangeArg {
        AssignChangeArg {
            month: month.to_string(),
            category_id: cat.to_string(),
            amount,
        }
    }

    #[test]
    fn parse_changes_checks_month_shape_zero_and_duplicates() {
        assert!(parse_changes(&[]).unwrap_err().contains("empty"));
        let err = parse_changes(&[change("2026-06-15", "a", 1.0)]).unwrap_err();
        assert!(err.contains("first of the month"), "{err}");
        let err = parse_changes(&[change("June", "a", 1.0)]).unwrap_err();
        assert!(err.contains("YYYY-MM-01"), "{err}");
        let err = parse_changes(&[change("2026-06-01", "a", 0.0)]).unwrap_err();
        assert!(err.contains("zero"), "{err}");
        let err = parse_changes(&[
            change("2026-06-01", "a", 1.0),
            change("2026-06-01", "a", 2.0),
        ])
        .unwrap_err();
        assert!(err.contains("twice"), "{err}");
        let ok = parse_changes(&[change("2026-06-01", "a", -12.5)]).unwrap();
        assert_eq!(ok[0].delta, -12_500);
    }

    #[test]
    fn assign_plan_applies_deltas_to_current_and_moves_ready_to_assign_per_month() {
        let changes = parse_changes(&[
            change("2026-06-01", "tax", 84.0),
            change("2026-06-01", "bo", 12.0),
            change("2026-06-01", "ops", 704.0),
            change("2026-07-01", "fees", 23.5),
            change("2026-07-01", "ops", 776.5),
        ])
        .unwrap();
        let mut budgeted: BudgetedBefore = HashMap::new();
        budgeted.insert(
            ("2026-06-01".into(), "tax".into()),
            (252_000, Some("Sales Tax".into())),
        );
        budgeted.insert(("2026-06-01".into(), "bo".into()), (36_000, None));
        budgeted.insert(("2026-06-01".into(), "ops".into()), (0, None));
        budgeted.insert(("2026-07-01".into(), "fees".into()), (0, None));
        budgeted.insert(("2026-07-01".into(), "ops".into()), (-100_000, None));
        let mut ready: ReadyBefore = HashMap::new();
        ready.insert("2026-06-01".into(), 800_000);
        ready.insert("2026-07-01".into(), 800_000);
        let plan = plan_assign(&changes, &budgeted, &ready).unwrap();
        assert_eq!(plan.ops[0].old_budgeted, 252_000);
        assert_eq!(
            plan.ops[0].new_budgeted, 336_000,
            "delta on the current value"
        );
        assert_eq!(
            plan.ops[4].new_budgeted, 676_500,
            "delta on a negative current value"
        );
        assert_eq!(plan.months.len(), 2);
        assert_eq!(plan.months[0].ready_after, 0);
        assert_eq!(plan.months[1].ready_after, 0);
    }

    #[test]
    fn assign_plan_flags_negative_ready_to_assign() {
        let changes = parse_changes(&[change("2026-09-01", "fun", 50.0)]).unwrap();
        let mut budgeted: BudgetedBefore = HashMap::new();
        budgeted.insert(("2026-09-01".into(), "fun".into()), (10_000, None));
        let mut ready: ReadyBefore = HashMap::new();
        ready.insert("2026-09-01".into(), 20_000);
        let plan = plan_assign(&changes, &budgeted, &ready).unwrap();
        assert_eq!(plan.months[0].ready_after, -30_000);
        assert!(plan.months[0].ready_after < 0);
        let err = plan_assign(&changes, &HashMap::new(), &ready).unwrap_err();
        assert!(err.contains("no current numbers"), "{err}");
    }
}
