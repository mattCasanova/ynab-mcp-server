//! Turn the text rows of a bank statement into date / description / amount rows.
//! Pure parsing over lines; the PDF reading lives in `pdf_text.rs`.
//!
//! Privacy: descriptions are scrubbed of any run of 8+ digits before they are returned,
//! so full account numbers cannot ride along on a payee string. Section headers, which carry
//! the account number, never leave this module except as a name and last four digits.

use chrono::NaiveDate;
use serde::Serialize;

use crate::money::Milliunits;

#[derive(Debug, Clone, Serialize)]
pub struct StatementRow {
    pub date: NaiveDate,
    pub description: String,
    pub amount: Milliunits,
    /// Running balance after this row, when the statement prints one.
    pub balance: Option<Milliunits>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Section {
    pub name: String,
    pub last4: Option<String>,
    pub rows: Vec<StatementRow>,
}

#[derive(Debug, Serialize)]
pub struct Parsed {
    pub period_start: Option<NaiveDate>,
    pub period_end: Option<NaiveDate>,
    pub sections: Vec<Section>,
    /// Lines that started with a date but could not be parsed into a row.
    pub unparsed_dated_lines: usize,
    /// Digit runs of 8+ that were scrubbed from descriptions.
    pub scrubbed_digit_runs: usize,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

fn month_number(word: &str) -> Option<u32> {
    let w = word.trim_end_matches('.').to_ascii_lowercase();
    MONTHS
        .iter()
        .position(|m| w.starts_with(m))
        .map(|i| i as u32 + 1)
}

/// "Aug 3", "Aug 31", "08/03/2026", "2026-08-03". Returns (month, day, year if present) and
/// how many leading words the date consumed.
fn leading_date(words: &[&str]) -> Option<(u32, u32, Option<i32>, usize)> {
    let first = *words.first()?;
    if let Some(d) = crate::reconcile::parse_date(first) {
        return Some((d.month(), d.day(), Some(d.year()), 1));
    }
    let month = month_number(first)?;
    let day: u32 = words.get(1)?.trim_end_matches(',').parse().ok()?;
    if !(1..=31).contains(&day) {
        return None;
    }
    if let Some(year) = words
        .get(2)
        .and_then(|w| w.parse::<i32>().ok())
        .filter(|y| (1990..=2100).contains(y))
    {
        return Some((month, day, Some(year), 3));
    }
    Some((month, day, None, 2))
}

use chrono::Datelike;

fn parse_money_token(tok: &str) -> Option<Milliunits> {
    let t = tok.trim_matches(|c: char| c == ',' || c == ';');
    let t = t.strip_prefix('$').unwrap_or(t);
    if t.is_empty() || !t.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    let (neg, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let body = body.strip_prefix('$').unwrap_or(body);
    if !body
        .chars()
        .all(|c| c.is_ascii_digit() || c == ',' || c == '.')
        || !body.contains('.')
    {
        return None;
    }
    let value: f64 = body.replace(',', "").parse().ok()?;
    let m = crate::money::from_decimal(value);
    Some(if neg { -m } else { m })
}

/// Statement period from a line like "Aug 1 - Aug 31, 2026" or "STATEMENT PERIOD Aug 1 - Aug 31, 2026".
fn find_period(lines: &[String]) -> Option<(NaiveDate, NaiveDate)> {
    for line in lines.iter().take(60) {
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(dash) = words
            .iter()
            .position(|w| *w == "-" || *w == "–" || *w == "to")
        else {
            continue;
        };
        if dash < 2 || words.len() < dash + 3 {
            continue;
        }
        let Some((m1, d1, y1, _)) = leading_date(&words[dash - 2..dash]) else {
            continue;
        };
        let Some((m2, d2, y2, _)) = leading_date(&words[dash + 1..]) else {
            continue;
        };
        let Some(end_year) = y2.or(y1) else { continue };
        let start_year = y1.unwrap_or(if m1 > m2 { end_year - 1 } else { end_year });
        let (Some(start), Some(end)) = (
            NaiveDate::from_ymd_opt(start_year, m1, d1),
            NaiveDate::from_ymd_opt(end_year, m2, d2),
        ) else {
            continue;
        };
        if start <= end {
            return Some((start, end));
        }
    }
    None
}

fn scrub(description: &str, scrubbed: &mut usize) -> String {
    let mut out = String::with_capacity(description.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String, scrubbed: &mut usize| {
        if run.len() >= 8 {
            out.push_str("<acct>");
            *scrubbed += 1;
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in description.chars() {
        if c.is_ascii_digit() {
            run.push(c);
        } else {
            flush(&mut run, &mut out, scrubbed);
            out.push(c);
        }
    }
    flush(&mut run, &mut out, scrubbed);
    out
}

/// A section header like "Personal Checking - 36083885861": name and last four.
fn section_header(line: &str) -> Option<(String, Option<String>)> {
    let (name, rest) = line.split_once(" - ")?;
    let digits: String = rest
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.len() < 8 || !rest.trim().starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let name = name.trim();
    if name.is_empty()
        || name.len() > 60
        || name
            .split_whitespace()
            .next()
            .is_some_and(|w| month_number(w).is_some())
    {
        return None;
    }
    Some((
        name.to_string(),
        Some(digits[digits.len() - 4..].to_string()),
    ))
}

pub fn parse(lines: &[String], year_hint: Option<i32>) -> Parsed {
    let period = find_period(lines);
    let (period_start, period_end) = match period {
        Some((s, e)) => (Some(s), Some(e)),
        None => (None, None),
    };
    let year_for = |month: u32| -> Option<i32> {
        if let (Some(s), Some(e)) = (period_start, period_end) {
            // A Dec–Jan statement: months before the start month belong to the end year.
            return Some(if s.year() != e.year() && month < s.month() {
                e.year()
            } else {
                s.year()
            });
        }
        year_hint
    };

    let mut sections: Vec<Section> = Vec::new();
    let mut unparsed = 0usize;
    let mut scrubbed = 0usize;
    let mut pending: Option<(NaiveDate, Vec<String>)> = None;
    // For layouts that vertically center the date/amount cells against a two-line description,
    // the description arrives as an orphan line before and/or after the dated line.
    let mut orphan_before: Option<String> = None;
    let mut expect_continuation = false;

    let finish_pending = |pending: &mut Option<(NaiveDate, Vec<String>)>,
                          sections: &mut Vec<Section>,
                          unparsed: &mut usize,
                          scrubbed: &mut usize| {
        if let Some((date, words)) = pending.take() {
            match row_from_words(date, &words, scrubbed) {
                Some(row) => {
                    if sections.is_empty() {
                        sections.push(Section {
                            name: "(statement)".to_string(),
                            last4: None,
                            rows: Vec::new(),
                        });
                    }
                    sections.last_mut().expect("non-empty").rows.push(row);
                }
                None => *unparsed += 1,
            }
        }
    };

    for line in lines {
        if let Some((name, last4)) = section_header(line) {
            finish_pending(&mut pending, &mut sections, &mut unparsed, &mut scrubbed);
            sections.push(Section {
                name,
                last4,
                rows: Vec::new(),
            });
            orphan_before = None;
            expect_continuation = false;
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        if is_noise_line(&words) {
            orphan_before = None;
            expect_continuation = false;
            continue;
        }
        if let Some((month, day, year, used)) = leading_date(&words) {
            // "Aug 1 - Aug 31, 2026" is the statement period, not a transaction.
            let is_period_line = words
                .get(used)
                .is_some_and(|w| *w == "-" || *w == "–" || *w == "to")
                && leading_date(&words[used + 1..]).is_some();
            if is_period_line {
                continue;
            }
            finish_pending(&mut pending, &mut sections, &mut unparsed, &mut scrubbed);
            let Some(year) = year.or_else(|| year_for(month)) else {
                unparsed += 1;
                continue;
            };
            let Some(date) = NaiveDate::from_ymd_opt(year, month, day) else {
                unparsed += 1;
                continue;
            };
            let rest: Vec<String> = words[used..].iter().map(|w| w.to_string()).collect();
            let lower = rest.join(" ").to_ascii_lowercase();
            if lower.starts_with("opening balance")
                || lower.starts_with("closing balance")
                || lower.starts_with("beginning balance")
                || lower.starts_with("ending balance")
            {
                continue;
            }
            let mut rest = rest;
            let complete = trailing_money(&rest).is_some();
            if complete && description_is_empty(&rest) {
                // Centered layout: the description was the line above.
                if let Some(above) = orphan_before.take() {
                    let mut joined: Vec<String> =
                        above.split_whitespace().map(str::to_string).collect();
                    joined.append(&mut rest);
                    rest = joined;
                }
                expect_continuation = true;
            } else {
                expect_continuation = false;
            }
            orphan_before = None;
            pending = Some((date, rest));
            if complete {
                finish_pending(&mut pending, &mut sections, &mut unparsed, &mut scrubbed);
            }
        } else if let Some((_, w)) = &mut pending {
            // Continuation of a wrapped description (money on the last line).
            w.extend(words.iter().map(|s| s.to_string()));
            if trailing_money(w).is_some() {
                finish_pending(&mut pending, &mut sections, &mut unparsed, &mut scrubbed);
            }
        } else if expect_continuation
            && trailing_money(&words.iter().map(|w| w.to_string()).collect::<Vec<_>>()).is_none()
        {
            // Centered layout: the rest of the description is the line below.
            if let Some(row) = sections.last_mut().and_then(|s| s.rows.last_mut()) {
                let extra = scrub(&words.join(" "), &mut scrubbed);
                row.description = format!("{} {}", row.description, extra).trim().to_string();
            }
            expect_continuation = false;
        } else {
            expect_continuation = false;
            orphan_before = Some(words.join(" "));
        }
    }
    finish_pending(&mut pending, &mut sections, &mut unparsed, &mut scrubbed);
    sections.retain(|s| !s.rows.is_empty());
    Parsed {
        period_start,
        period_end,
        sections,
        unparsed_dated_lines: unparsed,
        scrubbed_digit_runs: scrubbed,
    }
}

/// Table headers, page footers, and blank lines that must never be glued onto a row.
fn is_noise_line(words: &[&str]) -> bool {
    if words.is_empty() {
        return true;
    }
    let lower: Vec<String> = words.iter().map(|w| w.to_ascii_lowercase()).collect();
    let header_words = [
        "date",
        "description",
        "category",
        "amount",
        "balance",
        "transaction",
        "details",
    ];
    let header_hits = lower
        .iter()
        .filter(|w| header_words.contains(&w.as_str()))
        .count();
    if header_hits >= 2 {
        return true;
    }
    lower.first().is_some_and(|w| w == "page") && lower.iter().any(|w| w == "of")
}

/// True when, after the money and sign/category tokens are removed, nothing describes the row.
fn description_is_empty(words: &[String]) -> bool {
    let Some((_, _, money_at)) = trailing_money(words) else {
        return false;
    };
    words[..money_at].iter().all(|w| {
        w == "-" || w == "+" || w.eq_ignore_ascii_case("debit") || w.eq_ignore_ascii_case("credit")
    })
}

/// (amount, balance, index of first money token) from the tail of a row's words.
fn trailing_money(words: &[String]) -> Option<(Milliunits, Option<Milliunits>, usize)> {
    let mut money: Vec<(usize, Milliunits)> = Vec::new();
    for (i, w) in words.iter().enumerate().rev() {
        match parse_money_token(w) {
            Some(m) => money.push((i, m)),
            None if money.is_empty() => return None,
            None => break,
        }
        if money.len() == 2 {
            break;
        }
    }
    let first = money.last()?; // leftmost of the trailing money tokens
    match money.len() {
        1 => Some((first.1, None, first.0)),
        _ => Some((first.1, Some(money[0].1), first.0)),
    }
}

fn row_from_words(date: NaiveDate, words: &[String], scrubbed: &mut usize) -> Option<StatementRow> {
    let (mut amount, balance, money_at) = trailing_money(words)?;
    let mut desc_words: Vec<&str> = words[..money_at].iter().map(String::as_str).collect();
    // Sign: an explicit "-" or "+" token just before the amount, else a Debit/Credit word.
    let mut explicit_sign = false;
    if let Some(last) = desc_words.last() {
        if *last == "-" {
            amount = -amount.abs();
            explicit_sign = true;
            desc_words.pop();
        } else if *last == "+" {
            amount = amount.abs();
            explicit_sign = true;
            desc_words.pop();
        }
    }
    // The category word ("Debit"/"Credit") sits right before the sign/amount. Only that one is
    // a category; "CHASE CREDIT CRD" in the middle of a description is part of the name.
    let category = desc_words
        .last()
        .filter(|w| w.eq_ignore_ascii_case("debit") || w.eq_ignore_ascii_case("credit"))
        .map(|w| w.to_ascii_lowercase());
    if category.is_some() {
        desc_words.pop();
    }
    if !explicit_sign {
        let first = desc_words.first().map(|w| w.to_ascii_lowercase());
        match (category.as_deref(), first.as_deref()) {
            (Some("debit"), _) => amount = -amount.abs(),
            (Some("credit"), _) => amount = amount.abs(),
            (None, Some("withdrawal")) | (None, Some("payment")) => amount = -amount.abs(),
            (None, Some("deposit")) => amount = amount.abs(),
            _ => {} // keep the token's own sign
        }
    }
    let description = scrub(&desc_words.join(" "), scrubbed);
    if description.trim().is_empty() {
        return None;
    }
    Some(StatementRow {
        date,
        description,
        amount,
        balance,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(|l| l.to_string()).collect()
    }

    #[test]
    fn parses_capital_one_shape_with_sections_and_wrapped_rows() {
        let text = "\
Here's your August 2026 bank statement. STATEMENT PERIOD
Aug 1 - Aug 31, 2026
Personal Checking - 36083885861
DATE DESCRIPTION CATEGORY AMOUNT BALANCE
Aug 1 Opening Balance $8,889.13
Aug 3 Withdrawal from BILT PAYMENT BILTRENT Debit - $6,028.56 $2,860.57
Aug 5 Deposit from Personal Savings XXXXXXX6210 Credit + $4,000.00 $6,860.57
Aug 25 Preauthorized Withdrawal to JPMORGAN CHASE BANK, NA
checking account XXXXX8599 Debit - $500.00 $23,022.59
Aug 31 Monthly Interest Paid Credit + $0.76 $26,467.60
Aug 31 Closing Balance $26,467.60
Personal Savings - 36210146210
Aug 5 Withdrawal to Personal Checking XXXXXXX5861 Debit - $4,000.00 $46,639.75
";
        let p = parse(&lines(text), None);
        assert_eq!(p.period_start.unwrap().to_string(), "2026-08-01");
        assert_eq!(p.sections.len(), 2);
        let c = &p.sections[0];
        assert_eq!(c.name, "Personal Checking");
        assert_eq!(c.last4.as_deref(), Some("5861"));
        assert_eq!(
            c.rows.len(),
            4,
            "opening/closing skipped, wrapped row joined"
        );
        assert_eq!(c.rows[0].amount, -6_028_560);
        assert_eq!(c.rows[1].amount, 4_000_000);
        assert_eq!(
            c.rows[2].description,
            "Preauthorized Withdrawal to JPMORGAN CHASE BANK, NA checking account XXXXX8599"
        );
        assert_eq!(c.rows[2].amount, -500_000);
        assert_eq!(c.rows[2].balance, Some(23_022_590));
        assert_eq!(c.rows[3].date.to_string(), "2026-08-31");
        assert_eq!(p.sections[1].rows[0].amount, -4_000_000);
        assert_eq!(p.unparsed_dated_lines, 0);
    }

    #[test]
    fn centered_layout_reassembles_two_line_descriptions_and_keeps_credit_in_names() {
        let text = "\
Aug 1 - Aug 31, 2026
Personal Checking - 36083885861
DATE DESCRIPTION CATEGORY AMOUNT BALANCE
Aug 6 Withdrawal from CHASE CREDIT CRD EPAY Debit - $1,094.89 $5,765.68
Preauthorized Withdrawal to JPMORGAN CHASE BANK, NA
Aug 25 Debit - $500.00 $23,022.59
checking account XXXXX8599
Aug 28 Deposit from META PAYROLL Credit + $7,352.18 $10,174.64
Page 2 of 4
DATE DESCRIPTION CATEGORY AMOUNT BALANCE
Aug 31 Monthly Interest Paid Credit + $0.76 $26,467.60
";
        let p = parse(&lines(text), None);
        let rows = &p.sections[0].rows;
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].description, "Withdrawal from CHASE CREDIT CRD EPAY");
        assert_eq!(
            rows[1].description,
            "Preauthorized Withdrawal to JPMORGAN CHASE BANK, NA checking account XXXXX8599"
        );
        assert_eq!(rows[1].amount, -500_000);
        assert_eq!(
            rows[2].description, "Deposit from META PAYROLL",
            "footer/header must not glue on"
        );
        assert_eq!(rows[3].description, "Monthly Interest Paid");
        assert_eq!(p.unparsed_dated_lines, 0);
    }

    #[test]
    fn scrubs_long_digit_runs_from_descriptions() {
        let text =
            "Jan 1 - Jan 31, 2026\nJan 5 Transfer to account 123456789012 Debit - $10.00 $90.00\n";
        let p = parse(&lines(text), None);
        assert_eq!(
            p.sections[0].rows[0].description,
            "Transfer to account <acct>"
        );
        assert_eq!(p.scrubbed_digit_runs, 1);
    }

    #[test]
    fn year_rolls_over_across_december() {
        let text = "Dec 15 - Jan 14, 2026\nDec 20 Coffee Debit - $4.00 $96.00\nJan 3 Coffee Debit - $4.00 $92.00\n";
        let p = parse(&lines(text), None);
        assert_eq!(p.sections[0].rows[0].date.to_string(), "2025-12-20");
        assert_eq!(p.sections[0].rows[1].date.to_string(), "2026-01-03");
    }

    #[test]
    fn year_hint_used_when_no_period_found() {
        let p = parse(&lines("Mar 2 Something Credit $5.00\n"), Some(2024));
        assert_eq!(p.sections[0].rows[0].date.to_string(), "2024-03-02");
        assert_eq!(p.sections[0].rows[0].amount, 5_000);
        let none = parse(&lines("Mar 2 Something Credit $5.00\n"), None);
        assert!(none.sections.is_empty());
        assert_eq!(none.unparsed_dated_lines, 1);
    }

    #[test]
    fn sign_falls_back_to_words_then_token() {
        let text = "Jan 1 - Jan 31, 2026\nJan 2 Store Debit $5.00\nJan 3 Refund Credit $5.00\nJan 4 Plain -$7.00\nJan 5 Plain 7.00\n";
        let p = parse(&lines(text), None);
        let a: Vec<i64> = p.sections[0].rows.iter().map(|r| r.amount).collect();
        assert_eq!(a, vec![-5_000, 5_000, -7_000, 7_000]);
    }
}
