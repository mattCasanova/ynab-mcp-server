//! The analytics tools: read-only, and they only ever return counts and tables.
//! See `analytics.rs` for the line they hold.

use std::collections::{BTreeMap, HashMap};

use chrono::NaiveDate;
use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::*, schemars, tool,
    tool_router,
};
use serde::Deserialize;
use serde_json::json;

use crate::analytics::{self, Posting};
use crate::money::to_decimal;
use crate::server::{YnabServer, api_error, invalid, json_result};
use crate::ynab::{Category, TransactionFilter};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MonthsArg {
    /// How many months back, counting the current month. Default 6.
    pub months: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecurringArgs {
    /// How many months back to scan. Default 12.
    pub months: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CategoryHistoryArgs {
    /// Category id from list_categories. Either this or category_name.
    pub category_id: Option<String>,
    /// Category name, case-insensitive. Must match exactly one category.
    pub category_name: Option<String>,
    /// How many months back. Default 6.
    pub months: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PayeeSummaryArgs {
    /// How many months back. Default 3.
    pub months: Option<u32>,
    /// Max payees returned. Default 25.
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MonthArg {
    /// Month as YYYY-MM-01, or "current". Default current.
    pub month: Option<String>,
}

fn today() -> NaiveDate {
    chrono::Local::now().date_naive()
}

fn month_keys(today: NaiveDate, months: u32) -> Vec<String> {
    (0..months)
        .rev()
        .map(|n| analytics::month_key(analytics::month_start_back(today, n)))
        .collect()
}

fn ynab_month_ids(today: NaiveDate, months: u32) -> Vec<String> {
    (0..months)
        .rev()
        .map(|n| analytics::month_start_back(today, n).to_string())
        .collect()
}

fn clamp_months(months: Option<u32>, default: u32) -> Result<u32, McpError> {
    match months.unwrap_or(default) {
        0 => Err(invalid("months must be at least 1")),
        n if n > 36 => Err(invalid("months must be 36 or fewer")),
        n => Ok(n),
    }
}

impl YnabServer {
    async fn postings_since(&self, filter: TransactionFilter) -> Result<Vec<Posting>, McpError> {
        let txns = self
            .client()
            .transactions(&filter)
            .await
            .map_err(api_error)?;
        analytics::postings(&txns).map_err(|e| {
            tracing::error!(error = %e, "bad transaction shape from YNAB");
            McpError::internal_error(e, None)
        })
    }

    async fn category_names(&self) -> Result<HashMap<String, String>, McpError> {
        let groups = self.client().category_groups().await.map_err(api_error)?;
        Ok(groups
            .iter()
            .flat_map(|g| {
                g.categories
                    .iter()
                    .map(move |c| (c.id.clone(), format!("{}: {}", g.name, c.name)))
            })
            .collect())
    }
}

#[tool_router(router = analytics_router, vis = "pub")]
impl YnabServer {
    #[tool(
        description = "Category x month table of actual spending for the last N months (transfers and income excluded, splits counted per leg): per category the monthly amounts, total, average, and recent-vs-earlier change. Outflows negative. The base for any 'where does the money go' question."
    )]
    async fn spending_summary(
        &self,
        Parameters(args): Parameters<MonthsArg>,
    ) -> Result<CallToolResult, McpError> {
        let months = clamp_months(args.months, 6)?;
        let today = today();
        let since = analytics::month_start_back(today, months - 1);
        let postings = self
            .postings_since(TransactionFilter {
                since_date: Some(since.to_string()),
                ..Default::default()
            })
            .await?;
        let keys = month_keys(today, months);
        let rows = analytics::spending_summary(&postings, &keys);
        let grand: i64 = rows.iter().map(|r| analytics_total(&r.total)).sum();
        json_result(&json!({
            "months": keys,
            "categories": rows,
            "total_all_categories": to_decimal(grand),
        }))
    }

    #[tool(
        description = "Payees charged on a regular cadence (weekly, biweekly, monthly, quarterly, yearly) over the last N months: occurrences, first and latest amount, drift, and whether the charge looks lapsed. Subscriptions and price creep fall out of this; whether they are worth it is the user's call."
    )]
    async fn recurring_charges(
        &self,
        Parameters(args): Parameters<RecurringArgs>,
    ) -> Result<CallToolResult, McpError> {
        let months = clamp_months(args.months, 12)?;
        let today = today();
        let since = analytics::month_start_back(today, months - 1);
        let postings = self
            .postings_since(TransactionFilter {
                since_date: Some(since.to_string()),
                ..Default::default()
            })
            .await?;
        let rows = analytics::recurring_charges(&postings, today);
        let monthly_equivalent: i64 = rows
            .iter()
            .map(|r| {
                let per = analytics_total(&r.average_amount);
                match r.cadence {
                    analytics::Cadence::Weekly => per * 52 / 12,
                    analytics::Cadence::Biweekly => per * 26 / 12,
                    analytics::Cadence::Monthly => per,
                    analytics::Cadence::Quarterly => per / 3,
                    analytics::Cadence::Yearly => per / 12,
                    analytics::Cadence::Irregular => 0,
                }
            })
            .sum();
        json_result(&json!({
            "since": since.to_string(),
            "recurring": rows,
            "monthly_equivalent_total": to_decimal(monthly_equivalent),
        }))
    }

    #[tool(
        description = "One category in depth for the last N months: spend by payee with count, average, share, and monthly series. Use this when the user wants to find money in a specific category."
    )]
    async fn category_history(
        &self,
        Parameters(args): Parameters<CategoryHistoryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let months = clamp_months(args.months, 6)?;
        let groups = self.client().category_groups().await.map_err(api_error)?;
        let all: Vec<&Category> = groups
            .iter()
            .filter(|g| !g.deleted)
            .flat_map(|g| g.categories.iter())
            .filter(|c| !c.deleted)
            .collect();
        let category = match (&args.category_id, &args.category_name) {
            (Some(id), _) => all
                .iter()
                .find(|c| &c.id == id)
                .copied()
                .ok_or_else(|| invalid(format!("no category with id {id}")))?,
            (None, Some(name)) => {
                let matches: Vec<&Category> = all
                    .iter()
                    .filter(|c| c.name.eq_ignore_ascii_case(name))
                    .copied()
                    .collect();
                match matches.len() {
                    1 => matches[0],
                    0 => {
                        return Err(invalid(format!(
                            "no category named {name:?}; use list_categories"
                        )));
                    }
                    n => {
                        let ids: Vec<&str> = matches.iter().map(|c| c.id.as_str()).collect();
                        return Err(invalid(format!(
                            "{n} categories named {name:?}; pass category_id from {ids:?}"
                        )));
                    }
                }
            }
            (None, None) => return Err(invalid("pass category_id or category_name")),
        };
        let today = today();
        let since = analytics::month_start_back(today, months - 1);
        let postings = self
            .postings_since(TransactionFilter {
                category_id: Some(category.id.clone()),
                since_date: Some(since.to_string()),
                ..Default::default()
            })
            .await?;
        let keys = month_keys(today, months);
        let mut monthly: BTreeMap<String, i64> = keys.iter().map(|k| (k.clone(), 0)).collect();
        for p in postings.iter().filter(|p| !p.is_transfer) {
            *monthly.entry(analytics::month_key(p.date)).or_default() += p.amount;
        }
        let total: i64 = monthly.values().sum();
        json_result(&json!({
            "category": { "id": category.id, "name": category.name, "group": category.category_group_name,
                          "goal_type": category.goal_type, "goal_target": category.goal_target.map(to_decimal),
                          "assigned_this_month": to_decimal(category.budgeted), "available_now": to_decimal(category.balance) },
            "months": monthly.iter().map(|(k, v)| (k.clone(), to_decimal(*v))).collect::<BTreeMap<_, _>>(),
            "total": to_decimal(total),
            "average_per_month": to_decimal(total / months as i64),
            "payees": analytics::payee_breakdown(&postings, 50),
        }))
    }

    #[tool(
        description = "Payees ranked by total outflow over the last N months with count, average, share, and categories. Surfaces frequency spending that hides inside category totals."
    )]
    async fn payee_summary(
        &self,
        Parameters(args): Parameters<PayeeSummaryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let months = clamp_months(args.months, 3)?;
        let today = today();
        let since = analytics::month_start_back(today, months - 1);
        let postings = self
            .postings_since(TransactionFilter {
                since_date: Some(since.to_string()),
                ..Default::default()
            })
            .await?;
        let rows = analytics::payee_breakdown(&postings, args.limit.unwrap_or(25));
        json_result(&json!({ "since": since.to_string(), "payees": rows }))
    }

    #[tool(
        description = "One month's assigned vs actual per category: overspent (available below zero), assigned-but-unused, targets underfunded, and totals. The month-end audit in one call."
    )]
    async fn budget_vs_actual(
        &self,
        Parameters(args): Parameters<MonthArg>,
    ) -> Result<CallToolResult, McpError> {
        let month = args.month.unwrap_or_else(|| "current".to_string());
        let (detail, from_cache) = self.month_detail(&month).await?;
        let live: Vec<&Category> = detail
            .categories
            .iter()
            .filter(|c| !c.deleted && !c.hidden)
            .collect();
        let line = |c: &Category| {
            json!({
                "id": c.id, "name": c.name, "group": c.category_group_name,
                "assigned": to_decimal(c.budgeted), "activity": to_decimal(c.activity), "available": to_decimal(c.balance),
                "goal_type": c.goal_type, "goal_target": c.goal_target.map(to_decimal),
                "goal_under_funded": c.goal_under_funded.map(to_decimal),
            })
        };
        let overspent: Vec<_> = live
            .iter()
            .filter(|c| c.balance < 0)
            .map(|c| line(c))
            .collect();
        let unused: Vec<_> = live
            .iter()
            .filter(|c| c.budgeted > 0 && c.activity == 0)
            .map(|c| line(c))
            .collect();
        let underfunded: Vec<_> = live
            .iter()
            .filter(|c| c.goal_under_funded.is_some_and(|u| u > 0))
            .map(|c| line(c))
            .collect();
        let over_target: Vec<_> = live
            .iter()
            .filter(|c| {
                matches!(c.goal_type.as_deref(), Some("MF") | Some("NEED"))
                    && c.goal_target.is_some_and(|t| -c.activity > t)
            })
            .map(|c| line(c))
            .collect();
        json_result(&json!({
            "month": detail.month,
            "from_cache": from_cache,
            "income": to_decimal(detail.income),
            "assigned": to_decimal(detail.budgeted),
            "activity": to_decimal(detail.activity),
            "ready_to_assign": to_decimal(detail.to_be_budgeted),
            "age_of_money_days": detail.age_of_money,
            "overspent": overspent,
            "spent_over_target": over_target,
            "assigned_but_unused": unused,
            "targets_underfunded": underfunded,
        }))
    }

    #[tool(
        description = "Every category with a target, over the last N months: target vs what was actually assigned and spent each month, how many months spend exceeded the target, and the money moved into or out of it (with counterpart categories). The facts for 'why am I not hitting my goals'; the conclusion is the user's."
    )]
    async fn goal_analysis(
        &self,
        Parameters(args): Parameters<MonthsArg>,
    ) -> Result<CallToolResult, McpError> {
        let months = clamp_months(args.months, 6)?;
        let today = today();
        let ids = ynab_month_ids(today, months);
        let names = self.category_names().await?;

        // One month call per month: per-category assigned/activity/goal for that month.
        let mut per_cat: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
        let mut meta: HashMap<String, (String, Option<String>, Option<i64>)> = HashMap::new();
        let mut live_calls = 0;
        let mut cached_months = 0;
        for m in &ids {
            let (detail, from_cache) = self.month_detail(m).await?;
            if from_cache {
                cached_months += 1;
            } else {
                live_calls += 1;
            }
            for c in detail
                .categories
                .iter()
                .filter(|c| !c.deleted && !c.hidden && c.goal_type.is_some())
            {
                per_cat.entry(c.id.clone()).or_default().push(json!({
                    "month": analytics::month_key(crate::reconcile::parse_date(&detail.month).unwrap_or(today)),
                    "assigned": to_decimal(c.budgeted),
                    "activity": to_decimal(c.activity),
                    "available_end": to_decimal(c.balance),
                    "target": c.goal_target.map(to_decimal),
                    "spent_over_target": c.goal_target.is_some_and(|t| -c.activity > t),
                    "under_funded": c.goal_under_funded.map(to_decimal),
                }));
                meta.insert(
                    c.id.clone(),
                    (c.name.clone(), c.goal_type.clone(), c.goal_target),
                );
            }
        }

        let first_month = ids.first().cloned().unwrap_or_default();
        let movements = self.client().money_movements().await.map_err(api_error)?;
        let mut moved: HashMap<String, (i64, i64, HashMap<String, i64>)> = HashMap::new();
        for mv in movements.iter().filter(|mv| mv.month >= first_month) {
            if let Some(to) = &mv.to_category_id {
                let e = moved.entry(to.clone()).or_default();
                e.0 += mv.amount;
                if let Some(from) = &mv.from_category_id {
                    *e.2.entry(from.clone()).or_default() += mv.amount;
                }
            }
            if let Some(from) = &mv.from_category_id {
                let e = moved.entry(from.clone()).or_default();
                e.1 += mv.amount;
                if let Some(to) = &mv.to_category_id {
                    *e.2.entry(to.clone()).or_default() -= mv.amount;
                }
            }
        }

        let goals: Vec<_> = per_cat
            .iter()
            .map(|(id, months_json)| {
                let (name, goal_type, target) = meta.get(id).cloned().expect("meta recorded with per_cat");
                let over = months_json.iter().filter(|m| m["spent_over_target"] == true).count();
                let (moved_in, moved_out, counterparts) = moved.get(id).cloned().unwrap_or_default();
                let mut cp: Vec<_> = counterparts
                    .into_iter()
                    .map(|(cid, net)| json!({ "category": names.get(&cid).cloned().unwrap_or(cid), "net_from_them": to_decimal(net) }))
                    .collect();
                cp.sort_by_key(|v| v["net_from_them"].as_str().map(str::to_string));
                json!({
                    "category_id": id,
                    "category": names.get(id).cloned().unwrap_or(name),
                    "goal_type": goal_type,
                    "target_now": target.map(to_decimal),
                    "months_spent_over_target": over,
                    "months_examined": months_json.len(),
                    "moved_in_total": to_decimal(moved_in),
                    "moved_out_total": to_decimal(moved_out),
                    "counterpart_categories": cp,
                    "months": months_json,
                })
            })
            .collect();
        json_result(&json!({
            "months": month_keys(today, months),
            "month_calls": { "live": live_calls, "from_cache": cached_months },
            "goal_types": { "MF": "monthly funding", "NEED": "needed for spending by date/period", "TB": "target balance", "TBD": "target balance by date", "DEBT": "debt payoff" },
            "goals": goals,
        }))
    }

    #[tool(
        description = "Money moved between categories in the last N months, newest first, with category names and per-category net. The audit trail of every 'cover it from somewhere else'."
    )]
    async fn money_movements(
        &self,
        Parameters(args): Parameters<MonthsArg>,
    ) -> Result<CallToolResult, McpError> {
        let months = clamp_months(args.months, 3)?;
        let today = today();
        let first_month = analytics::month_start_back(today, months - 1).to_string();
        let names = self.category_names().await?;
        let name_of = |id: &Option<String>| {
            id.as_ref()
                .map(|i| names.get(i).cloned().unwrap_or_else(|| i.clone()))
        };
        let mut movements = self.client().money_movements().await.map_err(api_error)?;
        movements.retain(|m| m.month >= first_month);
        movements.sort_by(|a, b| b.moved_at.cmp(&a.moved_at));
        let mut net: HashMap<String, i64> = HashMap::new();
        for m in &movements {
            if let Some(to) = name_of(&m.to_category_id) {
                *net.entry(to).or_default() += m.amount;
            }
            if let Some(from) = name_of(&m.from_category_id) {
                *net.entry(from).or_default() -= m.amount;
            }
        }
        let mut net_rows: Vec<_> = net
            .into_iter()
            .map(|(c, v)| json!({ "category": c, "net": to_decimal(v) }))
            .collect();
        net_rows.sort_by_key(|v| v["category"].as_str().map(str::to_string));
        json_result(&json!({
            "since_month": first_month,
            "count": movements.len(),
            "movements": movements.iter().map(|m| json!({
                "month": m.month, "moved_at": m.moved_at, "amount": to_decimal(m.amount),
                "from": name_of(&m.from_category_id), "to": name_of(&m.to_category_id), "note": m.note,
            })).collect::<Vec<_>>(),
            "net_by_category": net_rows,
        }))
    }
}

/// Parse a decimal string produced by `to_decimal` back to milliunits for totals.
fn analytics_total(s: &str) -> i64 {
    let negative = s.starts_with('-');
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    let cents: i64 = digits.parse().unwrap_or_else(|_| {
        tracing::error!(value = s, "decimal string did not parse; treating as zero");
        0
    });
    if negative { -cents * 10 } else { cents * 10 }
}
