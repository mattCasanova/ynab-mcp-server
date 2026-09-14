//! Pure aggregation over transactions and months. No I/O, no opinions.
//!
//! The line this crate holds: the server counts, the model judges. Nothing in here scores,
//! recommends, or labels spending as good or bad. It turns transaction lists into the tables
//! a budget meeting needs, and the conversation does the rest.

use std::collections::{BTreeMap, HashMap};

use chrono::{Datelike, NaiveDate};
use serde::Serialize;

use crate::money::{Milliunits, to_decimal};
use crate::ynab::Transaction;

/// One category-level line of spending: a plain transaction, or one leg of a split.
#[derive(Debug, Clone)]
pub struct Posting {
    pub date: NaiveDate,
    pub amount: Milliunits,
    pub payee: Option<String>,
    pub category_id: Option<String>,
    pub category: Option<String>,
    pub is_transfer: bool,
}

/// Explode transactions into postings: splits become their legs, deleted rows are dropped.
/// Returns an error on a date YNAB should never produce.
pub fn postings(transactions: &[Transaction]) -> Result<Vec<Posting>, String> {
    let mut out = Vec::new();
    for t in transactions.iter().filter(|t| !t.deleted) {
        let date = crate::reconcile::parse_date(&t.date)
            .ok_or_else(|| format!("YNAB returned unparseable date {:?} on {}", t.date, t.id))?;
        if t.subtransactions.is_empty() {
            out.push(Posting {
                date,
                amount: t.amount,
                payee: t.payee_name.clone(),
                category_id: t.category_id.clone(),
                category: t.category_name.clone(),
                is_transfer: t.transfer_account_id.is_some(),
            });
        } else {
            for s in &t.subtransactions {
                out.push(Posting {
                    date,
                    amount: s.amount,
                    payee: s.payee_name.clone().or_else(|| t.payee_name.clone()),
                    category_id: s.category_id.clone(),
                    category: s.category_name.clone(),
                    is_transfer: s.transfer_account_id.is_some() || t.transfer_account_id.is_some(),
                });
            }
        }
    }
    Ok(out)
}

pub fn month_key(d: NaiveDate) -> String {
    format!("{:04}-{:02}", d.year(), d.month())
}

/// First day of the month `n` months before `today`'s month (n = 0 is this month).
pub fn month_start_back(today: NaiveDate, n: u32) -> NaiveDate {
    let total = today.year() * 12 + today.month0() as i32 - n as i32;
    let year = total.div_euclid(12);
    let month0 = total.rem_euclid(12) as u32;
    NaiveDate::from_ymd_opt(year, month0 + 1, 1).expect("valid month")
}

fn is_spending(p: &Posting) -> bool {
    !p.is_transfer
        && p.category
            .as_deref()
            .is_none_or(|c| c != "Inflow: Ready to Assign")
}

#[derive(Debug, Serialize)]
pub struct CategoryMonths {
    pub category_id: Option<String>,
    pub category: String,
    /// month -> net activity (outflows negative), as decimal strings
    pub months: BTreeMap<String, String>,
    pub total: String,
    pub average_per_month: String,
    /// Average of the most recent three months minus average of the earlier months, decimal.
    /// Negative means spending grew (activity is negative for outflows).
    pub recent_vs_earlier: String,
    pub transaction_count: usize,
}

/// Category x month table over the months present in `month_keys` (oldest first).
/// (category_id, category_name) -> (amount per month, transaction count)
type CategoryBuckets = HashMap<(Option<String>, String), (BTreeMap<String, Milliunits>, usize)>;

pub fn spending_summary(postings: &[Posting], month_keys: &[String]) -> Vec<CategoryMonths> {
    let mut by_cat: CategoryBuckets = HashMap::new();
    for p in postings.iter().filter(|p| is_spending(p)) {
        let key = (
            p.category_id.clone(),
            p.category
                .clone()
                .unwrap_or_else(|| "Uncategorized".to_string()),
        );
        let entry = by_cat.entry(key).or_default();
        *entry.0.entry(month_key(p.date)).or_default() += p.amount;
        entry.1 += 1;
    }
    let n_months = month_keys.len().max(1) as i64;
    let mut rows: Vec<CategoryMonths> = by_cat
        .into_iter()
        .map(|((category_id, category), (months, count))| {
            let total: Milliunits = months.values().sum();
            let series: Vec<Milliunits> = month_keys
                .iter()
                .map(|k| months.get(k).copied().unwrap_or(0))
                .collect();
            let split = series.len().saturating_sub(3);
            let recent = avg(&series[split..]);
            let earlier = avg(&series[..split]);
            CategoryMonths {
                category_id,
                category,
                months: month_keys
                    .iter()
                    .map(|k| (k.clone(), to_decimal(months.get(k).copied().unwrap_or(0))))
                    .collect(),
                total: to_decimal(total),
                average_per_month: to_decimal(total / n_months),
                recent_vs_earlier: to_decimal(recent - earlier),
                transaction_count: count,
            }
        })
        .collect();
    rows.sort_by_key(|r| parse_decimal_for_sort(&r.total));
    rows
}

fn avg(xs: &[Milliunits]) -> Milliunits {
    if xs.is_empty() {
        0
    } else {
        xs.iter().sum::<Milliunits>() / xs.len() as Milliunits
    }
}

fn parse_decimal_for_sort(s: &str) -> i64 {
    // "-12.34" -> -1234; only used for ordering so precision loss is irrelevant
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    cleaned.parse().unwrap_or(0)
}

#[derive(Debug, Serialize, PartialEq)]
pub enum Cadence {
    Weekly,
    Biweekly,
    Monthly,
    Quarterly,
    Yearly,
    Irregular,
}

#[derive(Debug, Serialize)]
pub struct RecurringCharge {
    pub payee: String,
    pub cadence: Cadence,
    pub occurrences: usize,
    pub first_date: String,
    pub last_date: String,
    pub latest_amount: String,
    pub first_amount: String,
    pub average_amount: String,
    /// latest minus first, decimal; negative means the charge got bigger (outflows are negative)
    pub amount_drift: String,
    pub median_gap_days: i64,
    /// true when the last charge is older than 1.5x the cadence: it may have stopped
    pub possibly_lapsed: bool,
    pub categories: Vec<String>,
}

/// Payees charged at a regular interval. Requires 3+ outflows (2 for yearly).
pub fn recurring_charges(postings: &[Posting], today: NaiveDate) -> Vec<RecurringCharge> {
    let mut by_payee: HashMap<String, Vec<&Posting>> = HashMap::new();
    for p in postings.iter().filter(|p| is_spending(p) && p.amount < 0) {
        if let Some(payee) = &p.payee {
            by_payee.entry(payee.clone()).or_default().push(p);
        }
    }
    let mut out = Vec::new();
    for (payee, mut ps) in by_payee {
        ps.sort_by_key(|p| p.date);
        let gaps: Vec<i64> = ps
            .windows(2)
            .map(|w| (w[1].date - w[0].date).num_days())
            .collect();
        let Some(median_gap) = median(&gaps) else {
            continue;
        };
        let cadence = classify_gap(median_gap);
        let enough = match cadence {
            Cadence::Yearly => ps.len() >= 2,
            Cadence::Irregular => false,
            _ => ps.len() >= 3,
        };
        if !enough {
            continue;
        }
        let amounts: Vec<Milliunits> = ps.iter().map(|p| p.amount).collect();
        let last = ps.last().expect("non-empty");
        let first = ps.first().expect("non-empty");
        let mut categories: Vec<String> = ps.iter().filter_map(|p| p.category.clone()).collect();
        categories.sort();
        categories.dedup();
        out.push(RecurringCharge {
            payee,
            occurrences: ps.len(),
            first_date: first.date.to_string(),
            last_date: last.date.to_string(),
            latest_amount: to_decimal(last.amount),
            first_amount: to_decimal(first.amount),
            average_amount: to_decimal(avg(&amounts)),
            amount_drift: to_decimal(last.amount - first.amount),
            median_gap_days: median_gap,
            possibly_lapsed: (today - last.date).num_days() > median_gap * 3 / 2,
            cadence,
            categories,
        });
    }
    out.sort_by(|a, b| {
        a.average_amount
            .cmp(&b.average_amount)
            .then(a.payee.cmp(&b.payee))
    });
    out
}

fn median(xs: &[i64]) -> Option<i64> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_unstable();
    Some(v[v.len() / 2])
}

fn classify_gap(days: i64) -> Cadence {
    match days {
        5..=9 => Cadence::Weekly,
        12..=16 => Cadence::Biweekly,
        26..=35 => Cadence::Monthly,
        80..=100 => Cadence::Quarterly,
        350..=380 => Cadence::Yearly,
        _ => Cadence::Irregular,
    }
}

#[derive(Debug, Serialize)]
pub struct PayeeLine {
    pub payee: String,
    pub total: String,
    pub count: usize,
    pub average: String,
    /// share of the total outflow in this set, percent with one decimal
    pub share_percent: String,
    pub months: BTreeMap<String, String>,
    pub categories: Vec<String>,
}

/// Payees ranked by total outflow. Inflows and transfers are excluded.
/// payee -> (total, count, amount per month, categories seen)
type PayeeBuckets = HashMap<String, (Milliunits, usize, BTreeMap<String, Milliunits>, Vec<String>)>;

pub fn payee_breakdown(postings: &[Posting], limit: usize) -> Vec<PayeeLine> {
    let mut by_payee: PayeeBuckets = HashMap::new();
    let mut grand: Milliunits = 0;
    for p in postings.iter().filter(|p| is_spending(p) && p.amount < 0) {
        let payee = p.payee.clone().unwrap_or_else(|| "(no payee)".to_string());
        let e = by_payee.entry(payee).or_default();
        e.0 += p.amount;
        e.1 += 1;
        *e.2.entry(month_key(p.date)).or_default() += p.amount;
        if let Some(c) = &p.category
            && !e.3.contains(c)
        {
            e.3.push(c.clone());
        }
        grand += p.amount;
    }
    let mut rows: Vec<(Milliunits, PayeeLine)> = by_payee
        .into_iter()
        .map(|(payee, (total, count, months, categories))| {
            let share = if grand == 0 {
                0.0
            } else {
                total as f64 / grand as f64 * 100.0
            };
            (
                total,
                PayeeLine {
                    payee,
                    total: to_decimal(total),
                    count,
                    average: to_decimal(total / count as Milliunits),
                    share_percent: format!("{share:.1}"),
                    months: months
                        .into_iter()
                        .map(|(k, v)| (k, to_decimal(v)))
                        .collect(),
                    categories,
                },
            )
        })
        .collect();
    rows.sort_by_key(|(total, _)| *total);
    rows.into_iter().take(limit).map(|(_, line)| line).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ynab::SubTransaction;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn txn(id: &str, date: &str, amount: Milliunits, payee: &str, category: &str) -> Transaction {
        Transaction {
            id: id.to_string(),
            date: date.to_string(),
            amount,
            memo: None,
            cleared: "cleared".to_string(),
            approved: true,
            flag_color: None,
            account_id: "a".to_string(),
            account_name: None,
            payee_id: None,
            payee_name: Some(payee.to_string()),
            category_id: Some(format!("cat-{category}")),
            category_name: Some(category.to_string()),
            transfer_account_id: None,
            import_id: None,
            deleted: false,
            subtransactions: vec![],
        }
    }

    #[test]
    fn splits_become_their_legs_and_transfers_are_flagged() {
        let mut split = txn("s", "2026-09-01", -30000, "Costco", "Split");
        split.subtransactions = vec![
            SubTransaction {
                amount: -20000,
                memo: None,
                payee_name: None,
                category_id: Some("g".into()),
                category_name: Some("Groceries".into()),
                transfer_account_id: None,
            },
            SubTransaction {
                amount: -10000,
                memo: None,
                payee_name: None,
                category_id: Some("h".into()),
                category_name: Some("Household".into()),
                transfer_account_id: None,
            },
        ];
        let mut transfer = txn("t", "2026-09-02", -50000, "Transfer : Savings", "x");
        transfer.transfer_account_id = Some("savings".into());
        transfer.category_name = None;
        let ps = postings(&[split, transfer]).unwrap();
        assert_eq!(ps.len(), 3);
        assert_eq!(ps[0].category.as_deref(), Some("Groceries"));
        assert_eq!(ps[0].payee.as_deref(), Some("Costco"));
        assert!(ps[2].is_transfer);
    }

    #[test]
    fn month_arithmetic_crosses_years() {
        assert_eq!(month_start_back(d("2026-02-15"), 3), d("2025-11-01"));
        assert_eq!(month_start_back(d("2026-09-14"), 0), d("2026-09-01"));
    }

    #[test]
    fn summary_buckets_by_category_and_month_and_skips_transfers_and_inflow() {
        let mut transfer = txn("t", "2026-08-05", -50000, "Transfer", "x");
        transfer.transfer_account_id = Some("s".into());
        let rows = spending_summary(
            &postings(&[
                txn("1", "2026-08-03", -10000, "A", "Groceries"),
                txn("2", "2026-08-20", -5000, "B", "Groceries"),
                txn("3", "2026-09-02", -7000, "A", "Groceries"),
                txn("4", "2026-09-02", 500000, "Meta", "Inflow: Ready to Assign"),
                transfer,
            ])
            .unwrap(),
            &["2026-08".to_string(), "2026-09".to_string()],
        );
        assert_eq!(rows.len(), 1);
        let g = &rows[0];
        assert_eq!(g.category, "Groceries");
        assert_eq!(g.months["2026-08"], "-15.00");
        assert_eq!(g.months["2026-09"], "-7.00");
        assert_eq!(g.total, "-22.00");
        assert_eq!(g.average_per_month, "-11.00");
        assert_eq!(g.transaction_count, 3);
    }

    #[test]
    fn detects_monthly_subscription_with_price_creep_and_lapse() {
        let ps = postings(&[
            txn("1", "2026-03-05", -9990, "Netflix", "Fun"),
            txn("2", "2026-04-05", -9990, "Netflix", "Fun"),
            txn("3", "2026-05-06", -12990, "Netflix", "Fun"),
            txn("4", "2026-06-05", -12990, "Netflix", "Fun"),
            txn("5", "2026-06-05", -4000, "Coffee", "Fun"),
            txn("6", "2026-06-07", -4000, "Coffee", "Fun"),
        ])
        .unwrap();
        let rows = recurring_charges(&ps, d("2026-09-14"));
        assert_eq!(
            rows.len(),
            1,
            "coffee has irregular gaps and must not appear"
        );
        let n = &rows[0];
        assert_eq!(n.payee, "Netflix");
        assert_eq!(n.cadence, Cadence::Monthly);
        assert_eq!(n.amount_drift, "-3.00");
        assert!(
            n.possibly_lapsed,
            "last charge in June, checked in September"
        );
    }

    #[test]
    fn yearly_needs_only_two_occurrences() {
        let ps = postings(&[
            txn("1", "2025-09-01", -99000, "Domain", "Blog"),
            txn("2", "2026-09-01", -119000, "Domain", "Blog"),
        ])
        .unwrap();
        let rows = recurring_charges(&ps, d("2026-09-14"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cadence, Cadence::Yearly);
    }

    #[test]
    fn payee_breakdown_ranks_and_shares() {
        let ps = postings(&[
            txn("1", "2026-09-01", -30000, "Big", "Blog"),
            txn("2", "2026-09-02", -10000, "Small", "Blog"),
            txn("3", "2026-09-03", 5000, "Refund", "Blog"),
        ])
        .unwrap();
        let rows = payee_breakdown(&ps, 10);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].payee, "Big");
        assert_eq!(rows[0].share_percent, "75.0");
        assert_eq!(rows[1].count, 1);
    }
}
