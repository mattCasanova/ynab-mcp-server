//! Data in and out: export transactions to a file, import bank CSV files, trigger YNAB's own
//! linked-account import. The server reads only the paths it is given and writes only the
//! export path it is given.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use rmcp::{
    ErrorData as McpError, RoleServer, handler::server::wrapper::Parameters, model::*, schemars,
    service::RequestContext, tool, tool_router,
};
use serde::Deserialize;
use serde_json::json;

use crate::csvio;
use crate::money::{Milliunits, to_decimal};
use crate::paths;
use crate::reconcile::{self, BankRow};
use crate::server::{NewTransactionArg, YnabServer, api_error, client_name, invalid, json_result};
use crate::ynab::TransactionFilter;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExportArgs {
    /// "csv" or "json".
    pub format: String,
    /// Where to write. `~/` is expanded. Parent directory must exist.
    pub path: String,
    /// Replace the file if it already exists. Default false.
    #[serde(default)]
    pub overwrite: bool,
    /// ISO date; defaults to one year ago (YNAB's default).
    pub since_date: Option<String>,
    /// ISO date.
    pub until_date: Option<String>,
    /// Restrict to one account.
    pub account_id: Option<String>,
    /// Restrict to one category (id from list_categories).
    pub category_id: Option<String>,
    /// Restrict to a category group by name (case-insensitive) or id. Splits are exported one
    /// row per leg so this filter is exact.
    pub category_group: Option<String>,
    /// Include transfers between accounts. Default false.
    #[serde(default)]
    pub include_transfers: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ColumnMapping {
    /// Header of the date column.
    pub date: String,
    /// Header of the description / payee column.
    pub description: String,
    /// Header of a single signed amount column. Use this OR debit + credit.
    pub amount: Option<String>,
    /// Header of the debit (money out) column, positive numbers.
    pub debit: Option<String>,
    /// Header of the credit (money in) column, positive numbers.
    pub credit: Option<String>,
    /// For a single amount column: true if the bank lists money OUT as positive. Default false.
    #[serde(default)]
    pub outflow_is_positive: bool,
    /// strftime pattern if the date is not YYYY-MM-DD, MM/DD/YYYY, or MM/DD/YY (e.g. "%d.%m.%Y").
    pub date_format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ImportCsvArgs {
    /// One or more CSV files from the same bank export format. `~/` is expanded.
    pub paths: Vec<String>,
    /// The YNAB account these files belong to (id from list_accounts).
    pub account_id: String,
    /// How the file's columns map onto date, description, and amount. Read the header first.
    pub mapping: ColumnMapping,
    /// Days of date slack when matching against YNAB. Default 3.
    pub window_days: Option<u32>,
    /// false (default) = reconcile and report only. true = also create the rows missing in YNAB
    /// (needs writes enabled). They land unapproved, journaled, and undoable.
    #[serde(default)]
    pub confirm: bool,
    /// Category for created rows. Default: uncategorized, so they show in the review queue.
    pub category_id: Option<String>,
}

/// (amount, category id, category name, memo, is transfer): one exported row.
type Leg<'a> = (
    Milliunits,
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
    bool,
);

fn resolve_path(p: &str) -> Result<PathBuf, McpError> {
    paths::expand_tilde(p).map_err(|e| invalid(format!("bad path {p:?}: {e:#}")))
}

fn parse_money(raw: &str) -> Result<Milliunits, String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| !matches!(c, '$' | '€' | '£' | ',' | ' ' | '\u{a0}'))
        .collect();
    if cleaned.is_empty() {
        return Ok(0);
    }
    let (negative, digits) =
        if let Some(inner) = cleaned.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
            (true, inner.to_string())
        } else if let Some(rest) = cleaned.strip_prefix('-') {
            (true, rest.to_string())
        } else {
            (
                false,
                cleaned.strip_prefix('+').unwrap_or(&cleaned).to_string(),
            )
        };
    let value: f64 = digits
        .parse()
        .map_err(|_| format!("not a number: {raw:?}"))?;
    let m = crate::money::from_decimal(value);
    Ok(if negative { -m } else { m })
}

fn parse_bank_date(raw: &str, format: Option<&str>) -> Result<NaiveDate, String> {
    match format {
        Some(f) => NaiveDate::parse_from_str(raw.trim(), f)
            .map_err(|e| format!("date {raw:?} does not match {f:?}: {e}")),
        None => reconcile::parse_date(raw)
            .ok_or_else(|| format!("unrecognized date {raw:?}; pass mapping.date_format")),
    }
}

#[derive(Debug)]
struct ParsedFile {
    path: PathBuf,
    rows: Vec<BankRow>,
}

fn read_bank_file(path: &Path, mapping: &ColumnMapping) -> Result<ParsedFile, McpError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| invalid(format!("cannot read {}: {e}", path.display())))?;
    let table = csvio::parse(&text).map_err(|e| invalid(format!("{}: {e:#}", path.display())))?;
    let Some((header, body)) = table.split_first() else {
        return Err(invalid(format!("{} is empty", path.display())));
    };
    let index = |name: &str| -> Result<usize, McpError> {
        header
            .iter()
            .position(|h| h.trim().eq_ignore_ascii_case(name.trim()))
            .ok_or_else(|| {
                invalid(format!(
                    "{}: no column {name:?}; header is {header:?}",
                    path.display()
                ))
            })
    };
    let date_i = index(&mapping.date)?;
    let desc_i = index(&mapping.description)?;
    let amount_cols = match (&mapping.amount, &mapping.debit, &mapping.credit) {
        (Some(a), None, None) => (Some(index(a)?), None, None),
        (None, Some(d), Some(c)) => (None, Some(index(d)?), Some(index(c)?)),
        (None, Some(d), None) => (None, Some(index(d)?), None),
        (None, None, Some(c)) => (None, None, Some(index(c)?)),
        _ => {
            return Err(invalid(
                "mapping needs either amount, or debit and/or credit, not both",
            ));
        }
    };
    let mut rows = Vec::with_capacity(body.len());
    for (n, r) in body.iter().enumerate() {
        let line = n + 2;
        if r.iter().all(|f| f.trim().is_empty()) {
            continue;
        }
        let get = |i: usize| r.get(i).map(String::as_str).unwrap_or("");
        let date = parse_bank_date(get(date_i), mapping.date_format.as_deref())
            .map_err(|e| invalid(format!("{} line {line}: {e}", path.display())))?;
        let amount = match amount_cols {
            (Some(a), _, _) => {
                let v = parse_money(get(a))
                    .map_err(|e| invalid(format!("{} line {line}: {e}", path.display())))?;
                if mapping.outflow_is_positive { -v } else { v }
            }
            (None, d, c) => {
                let debit = d
                    .map(|i| parse_money(get(i)))
                    .transpose()
                    .map_err(|e| invalid(format!("{} line {line}: {e}", path.display())))?
                    .unwrap_or(0);
                let credit = c
                    .map(|i| parse_money(get(i)))
                    .transpose()
                    .map_err(|e| invalid(format!("{} line {line}: {e}", path.display())))?
                    .unwrap_or(0);
                credit.abs() - debit.abs()
            }
        };
        rows.push(BankRow {
            date,
            amount,
            description: get(desc_i).trim().to_string(),
        });
    }
    Ok(ParsedFile {
        path: path.to_path_buf(),
        rows,
    })
}

#[tool_router(router = transfer_router, vis = "pub")]
impl YnabServer {
    #[tool(
        description = "Write transactions to a CSV or JSON file: date, account, payee, category group, category, memo, amount, cleared, approved, ids. Splits are one row per leg. Filter by dates, account, category, or category group (e.g. a business group for a tax year). Writes only to the given path; refuses to overwrite unless asked."
    )]
    async fn export_transactions(
        &self,
        Parameters(args): Parameters<ExportArgs>,
    ) -> Result<CallToolResult, McpError> {
        let format = args.format.to_ascii_lowercase();
        if format != "csv" && format != "json" {
            return Err(invalid(format!(
                "format must be csv or json, got {:?}",
                args.format
            )));
        }
        let path = resolve_path(&args.path)?;
        if path.exists() && !args.overwrite {
            return Err(invalid(format!(
                "{} exists; pass overwrite=true to replace it",
                path.display()
            )));
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.is_dir()
        {
            return Err(invalid(format!(
                "directory {} does not exist",
                parent.display()
            )));
        }

        // Category ids in scope, and group names for every category.
        let groups = self.client().category_groups().await.map_err(api_error)?;
        let mut group_of: HashMap<String, String> = HashMap::new();
        for g in &groups {
            for c in &g.categories {
                group_of.insert(c.id.clone(), g.name.clone());
            }
        }
        let scope: Option<BTreeSet<String>> = match (&args.category_id, &args.category_group) {
            (Some(id), _) => Some(BTreeSet::from([id.clone()])),
            (None, Some(g)) => {
                let matched: Vec<_> = groups
                    .iter()
                    .filter(|x| x.id == *g || x.name.eq_ignore_ascii_case(g))
                    .collect();
                match matched.as_slice() {
                    [one] => Some(one.categories.iter().map(|c| c.id.clone()).collect()),
                    [] => {
                        return Err(invalid(format!(
                            "no category group {g:?}; see list_categories"
                        )));
                    }
                    _ => {
                        return Err(invalid(format!(
                            "more than one group matches {g:?}; pass the id"
                        )));
                    }
                }
            }
            (None, None) => None,
        };

        let filter = TransactionFilter {
            account_id: args.account_id.clone(),
            since_date: args.since_date.clone(),
            until_date: args.until_date.clone(),
            ..Default::default()
        };
        let txns = self
            .client()
            .transactions(&filter)
            .await
            .map_err(api_error)?;
        let mut rows: Vec<serde_json::Value> = Vec::new();
        let mut total: Milliunits = 0;
        for t in txns.iter().filter(|t| !t.deleted) {
            let legs: Vec<Leg<'_>> = if t.subtransactions.is_empty() {
                vec![(
                    t.amount,
                    t.category_id.as_deref(),
                    t.category_name.as_deref(),
                    t.memo.as_deref(),
                    t.transfer_account_id.is_some(),
                )]
            } else {
                t.subtransactions
                    .iter()
                    .map(|s| {
                        (
                            s.amount,
                            s.category_id.as_deref(),
                            s.category_name.as_deref(),
                            s.memo.as_deref().or(t.memo.as_deref()),
                            s.transfer_account_id.is_some() || t.transfer_account_id.is_some(),
                        )
                    })
                    .collect()
            };
            for (amount, cat_id, cat_name, memo, is_transfer) in legs {
                if is_transfer && !args.include_transfers {
                    continue;
                }
                if let Some(scope) = &scope
                    && !cat_id.is_some_and(|c| scope.contains(c))
                {
                    continue;
                }
                total += amount;
                rows.push(json!({
                    "date": t.date,
                    "account": t.account_name,
                    "payee": t.payee_name,
                    "category_group": cat_id.and_then(|c| group_of.get(c)),
                    "category": cat_name,
                    "memo": memo,
                    "amount": to_decimal(amount),
                    "cleared": t.cleared,
                    "approved": t.approved,
                    "transfer": is_transfer,
                    "transaction_id": t.id,
                    "import_id": t.import_id,
                }));
            }
        }
        rows.sort_by(|a, b| a["date"].as_str().cmp(&b["date"].as_str()));

        let header = [
            "date",
            "account",
            "payee",
            "category_group",
            "category",
            "memo",
            "amount",
            "cleared",
            "approved",
            "transfer",
            "transaction_id",
            "import_id",
        ];
        let body = if format == "csv" {
            let cells: Vec<Vec<String>> = rows
                .iter()
                .map(|r| {
                    header
                        .iter()
                        .map(|h| match &r[*h] {
                            serde_json::Value::Null => String::new(),
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .collect()
                })
                .collect();
            csvio::write_rows(&header, &cells)
        } else {
            serde_json::to_string_pretty(&rows)
                .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?
        };
        std::fs::write(&path, body).map_err(|e| {
            tracing::error!(path = %path.display(), error = %e, "export write failed");
            McpError::internal_error(format!("write {}: {e}", path.display()), None)
        })?;
        json_result(&json!({
            "path": path.display().to_string(),
            "format": format,
            "rows": rows.len(),
            "total_amount": to_decimal(total),
            "filters": { "since_date": args.since_date, "until_date": args.until_date, "account_id": args.account_id,
                         "category_id": args.category_id, "category_group": args.category_group, "include_transfers": args.include_transfers },
        }))
    }

    #[tool(
        description = "Import one or more bank CSV files (same bank format) for one account: parse with the given column mapping, dedupe overlapping files, reconcile against YNAB, and report matched / missing / in-YNAB-only. With confirm=true (writes enabled) the missing rows are created: unapproved, journaled, undoable. Read the file header first and pass the mapping; nothing is guessed."
    )]
    async fn import_bank_csv(
        &self,
        Parameters(args): Parameters<ImportCsvArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if args.paths.is_empty() {
            return Err(invalid("paths is empty"));
        }
        if args.confirm && !self.writes_allowed() {
            return Err(invalid(
                "confirm=true needs writes enabled (YNAB_MCP_ALLOW_WRITES=1); run without confirm to reconcile only",
            ));
        }
        let mut files = Vec::new();
        for p in &args.paths {
            files.push(read_bank_file(&resolve_path(p)?, &args.mapping)?);
        }
        let per_file: Vec<_> = files
            .iter()
            .map(|f| json!({ "path": f.path.display().to_string(), "rows": f.rows.len() }))
            .collect();

        // Overlapping monthly downloads repeat rows at the edges; keep the first occurrence.
        let mut seen: BTreeSet<(NaiveDate, Milliunits, String)> = BTreeSet::new();
        let mut rows: Vec<BankRow> = Vec::new();
        let mut duplicates: Vec<serde_json::Value> = Vec::new();
        for f in &files {
            for r in &f.rows {
                let key = (r.date, r.amount, r.description.to_ascii_lowercase());
                if seen.insert(key) {
                    rows.push(r.clone());
                } else {
                    duplicates.push(json!({ "file": f.path.display().to_string(), "date": r.date.to_string(), "amount": to_decimal(r.amount), "description": r.description }));
                }
            }
        }
        if rows.is_empty() {
            return Err(invalid("no data rows after parsing"));
        }
        let window_days = i64::from(args.window_days.unwrap_or(3));
        let outcome = self
            .reconcile_full(&args.account_id, &rows, window_days)
            .await?;

        let mut result = json!({
            "files": per_file,
            "rows_after_dedupe": rows.len(),
            "duplicates_across_files": duplicates,
            "reconcile": outcome.summary,
        });
        if !args.confirm {
            result["next"] = json!(if outcome.missing.is_empty() {
                "nothing missing in YNAB"
            } else if self.writes_allowed() {
                "review missing_in_ynab, then call again with confirm=true to create them"
            } else {
                "writes are disabled; enter the missing rows in YNAB by hand or enable writes"
            });
            return json_result(&result);
        }
        if outcome.missing.is_empty() {
            result["created"] = json!(0);
            return json_result(&result);
        }
        let new_rows: Vec<NewTransactionArg> = outcome
            .missing
            .iter()
            .map(|r| NewTransactionArg {
                account_id: args.account_id.clone(),
                date: r.date.to_string(),
                amount: r.amount as f64 / 1000.0,
                payee_name: Some(r.description.clone()),
                category_id: args.category_id.clone(),
                memo: None,
            })
            .collect();
        let created = self
            .create_rows(&new_rows, "import_bank_csv", client_name(&ctx))
            .await?;
        result["created"] = created;
        json_result(&result)
    }
}

#[tool_router(router = transfer_write_router, vis = "pub")]
impl YnabServer {
    #[tool(
        description = "WRITE. Press YNAB's Import button for every linked account: pulls whatever the bank connection has waiting. Returns the imported transaction ids. These are bank rows, not rows this server invented, so they are not journaled for undo."
    )]
    async fn trigger_bank_import(&self) -> Result<CallToolResult, McpError> {
        let ids = self
            .client()
            .import_linked_transactions()
            .await
            .map_err(api_error)?;
        tracing::info!(count = ids.len(), "linked-account import triggered");
        json_result(&json!({ "imported": ids.len(), "transaction_ids": ids }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_money_in_bank_styles() {
        assert_eq!(parse_money("-4.50").unwrap(), -4500);
        assert_eq!(parse_money("$1,234.56").unwrap(), 1234560);
        assert_eq!(parse_money("(12.00)").unwrap(), -12000);
        assert_eq!(parse_money("").unwrap(), 0);
        assert!(parse_money("abc").is_err());
    }

    fn write_temp(name: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ynab-mcp-csv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn reads_debit_credit_files_with_sign_convention() {
        let p = write_temp(
            "dc.csv",
            "Posting Date,Description,Debit,Credit\n09/01/2026,COFFEE,4.50,\n09/02/2026,PAYROLL,,1000.00\n\n",
        );
        let mapping = ColumnMapping {
            date: "posting date".into(),
            description: "Description".into(),
            amount: None,
            debit: Some("Debit".into()),
            credit: Some("Credit".into()),
            outflow_is_positive: false,
            date_format: None,
        };
        let f = read_bank_file(&p, &mapping).unwrap();
        assert_eq!(f.rows.len(), 2);
        assert_eq!(f.rows[0].amount, -4500);
        assert_eq!(f.rows[1].amount, 1_000_000);
    }

    #[test]
    fn single_amount_column_with_positive_outflows_and_custom_date() {
        let p = write_temp("amt.csv", "Datum,Text,Belopp\n01.09.2026,KAFFE,45.00\n");
        let mapping = ColumnMapping {
            date: "Datum".into(),
            description: "Text".into(),
            amount: Some("Belopp".into()),
            debit: None,
            credit: None,
            outflow_is_positive: true,
            date_format: Some("%d.%m.%Y".into()),
        };
        let f = read_bank_file(&p, &mapping).unwrap();
        assert_eq!(f.rows[0].amount, -45000);
        assert_eq!(f.rows[0].date.to_string(), "2026-09-01");
    }

    #[test]
    fn missing_column_names_the_header() {
        let p = write_temp("bad.csv", "Date,Memo,Amount\n2026-09-01,x,1\n");
        let mapping = ColumnMapping {
            date: "Date".into(),
            description: "Description".into(),
            amount: Some("Amount".into()),
            debit: None,
            credit: None,
            outflow_is_positive: false,
            date_format: None,
        };
        let err = read_bank_file(&p, &mapping).unwrap_err();
        assert!(err.message.contains("Description"));
        assert!(err.message.contains("Memo"));
    }
}
