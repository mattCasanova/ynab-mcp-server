//! Deterministic bank-statement-vs-YNAB matcher. Pure functions, no I/O.
//!
//! Greedy by bank row in date order: each bank row takes the unmatched YNAB
//! transaction with the same amount whose date is closest within the window.

use chrono::NaiveDate;
use serde::Serialize;

use crate::money::Milliunits;

#[derive(Debug, Clone, Serialize)]
pub struct BankRow {
    pub date: NaiveDate,
    pub amount: Milliunits,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct YnabRow {
    pub id: String,
    pub date: NaiveDate,
    pub amount: Milliunits,
    pub payee: Option<String>,
    pub category: Option<String>,
    pub cleared: String,
    pub approved: bool,
}

#[derive(Debug, Serialize)]
pub struct Match {
    pub bank: BankRow,
    pub ynab: YnabRow,
    /// YNAB date minus bank date, in days.
    pub day_offset: i64,
}

#[derive(Debug, Serialize)]
pub struct Reconciliation {
    pub matched: Vec<Match>,
    pub missing_in_ynab: Vec<BankRow>,
    pub in_ynab_not_at_bank: Vec<YnabRow>,
}

pub fn reconcile(bank: &[BankRow], ynab: &[YnabRow], window_days: i64) -> Reconciliation {
    let mut bank_sorted: Vec<BankRow> = bank.to_vec();
    bank_sorted.sort_by_key(|r| r.date);

    let mut used = vec![false; ynab.len()];
    let mut matched = Vec::new();
    let mut missing_in_ynab = Vec::new();

    for row in bank_sorted {
        let best = ynab
            .iter()
            .enumerate()
            .filter(|(i, y)| !used[*i] && y.amount == row.amount)
            .map(|(i, y)| (i, (y.date - row.date).num_days()))
            .filter(|(_, offset)| offset.abs() <= window_days)
            .min_by_key(|(i, offset)| (offset.abs(), ynab[*i].date, *i));
        match best {
            Some((i, day_offset)) => {
                used[i] = true;
                matched.push(Match {
                    bank: row,
                    ynab: ynab[i].clone(),
                    day_offset,
                });
            }
            None => missing_in_ynab.push(row),
        }
    }

    let range = bank
        .iter()
        .map(|r| r.date)
        .fold(None, |acc: Option<(NaiveDate, NaiveDate)>, d| {
            Some(match acc {
                None => (d, d),
                Some((lo, hi)) => (lo.min(d), hi.max(d)),
            })
        });

    let in_ynab_not_at_bank = ynab
        .iter()
        .enumerate()
        .filter(|(i, _)| !used[*i])
        .map(|(_, y)| y.clone())
        .filter(|y| range.is_some_and(|(lo, hi)| y.date >= lo && y.date <= hi))
        .collect();

    Reconciliation {
        matched,
        missing_in_ynab,
        in_ynab_not_at_bank,
    }
}

/// Accepts ISO `YYYY-MM-DD` and US `MM/DD/YYYY` (the two formats bank CSVs use).
pub fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d);
    }
    // chrono's %Y accepts a two-digit year as year 26 AD, so pick the format by digit count.
    let year_digits = s.rsplit('/').next()?.len();
    let format = if year_digits == 2 {
        "%m/%d/%y"
    } else {
        "%m/%d/%Y"
    };
    NaiveDate::parse_from_str(s, format).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        parse_date(s).unwrap()
    }

    fn bank(date: &str, amount: Milliunits, desc: &str) -> BankRow {
        BankRow {
            date: d(date),
            amount,
            description: desc.to_string(),
        }
    }

    fn ynab(id: &str, date: &str, amount: Milliunits) -> YnabRow {
        YnabRow {
            id: id.to_string(),
            date: d(date),
            amount,
            payee: Some(format!("payee-{id}")),
            category: None,
            cleared: "uncleared".to_string(),
            approved: true,
        }
    }

    #[test]
    fn parses_both_date_formats() {
        assert_eq!(d("2026-09-01"), d("09/01/2026"));
        assert_eq!(d("09/01/26"), d("2026-09-01"));
        assert!(parse_date("Sept 1").is_none());
    }

    #[test]
    fn matches_same_amount_within_window() {
        let b = [bank("2026-09-01", -12340, "COFFEE")];
        let y = [ynab("a", "2026-09-03", -12340)];
        let r = reconcile(&b, &y, 3);
        assert_eq!(r.matched.len(), 1);
        assert_eq!(r.matched[0].day_offset, 2);
        assert!(r.missing_in_ynab.is_empty());
        assert!(r.in_ynab_not_at_bank.is_empty());
    }

    #[test]
    fn outside_window_is_missing() {
        let b = [bank("2026-09-01", -12340, "COFFEE")];
        let y = [ynab("a", "2026-09-09", -12340)];
        let r = reconcile(&b, &y, 3);
        assert!(r.matched.is_empty());
        assert_eq!(r.missing_in_ynab.len(), 1);
        // The YNAB row is outside the bank range, so it is not reported either.
        assert!(r.in_ynab_not_at_bank.is_empty());
    }

    #[test]
    fn prefers_closest_date_and_never_reuses_a_row() {
        let b = [
            bank("2026-09-01", -5000, "A"),
            bank("2026-09-05", -5000, "B"),
        ];
        let y = [
            ynab("far", "2026-09-04", -5000),
            ynab("near", "2026-09-01", -5000),
        ];
        let r = reconcile(&b, &y, 3);
        assert_eq!(r.matched.len(), 2);
        assert_eq!(r.matched[0].ynab.id, "near");
        assert_eq!(r.matched[1].ynab.id, "far");
    }

    #[test]
    fn duplicate_bank_rows_need_two_ynab_rows() {
        let b = [
            bank("2026-09-01", -5000, "A"),
            bank("2026-09-01", -5000, "A"),
        ];
        let y = [ynab("only", "2026-09-01", -5000)];
        let r = reconcile(&b, &y, 3);
        assert_eq!(r.matched.len(), 1);
        assert_eq!(r.missing_in_ynab.len(), 1);
    }

    #[test]
    fn reports_ynab_rows_inside_range_with_no_bank_match() {
        let b = [
            bank("2026-09-01", -5000, "A"),
            bank("2026-09-10", -7000, "B"),
        ];
        let y = [
            ynab("a", "2026-09-01", -5000),
            ynab("b", "2026-09-10", -7000),
            ynab("typo", "2026-09-05", -70000),
            ynab("old", "2026-08-01", -1000),
        ];
        let r = reconcile(&b, &y, 3);
        assert_eq!(r.matched.len(), 2);
        assert_eq!(r.in_ynab_not_at_bank.len(), 1);
        assert_eq!(r.in_ynab_not_at_bank[0].id, "typo");
    }
}
