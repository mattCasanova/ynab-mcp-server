//! On-disk cache for month detail. YNAB only serves per-category month numbers one month per
//! call, and closed months rarely change, so goal analysis over six months should not cost six
//! live calls every time.
//!
//! Rules: the current month is never cached. The previous month is cached for one day, since
//! it is usually still being reconciled. Older months are cached for `ttl_days` (default 30).
//! A corrupt or unreadable cache file is logged and refetched, never silently trusted.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::ynab::MonthDetail;

const PREVIOUS_MONTH_TTL_DAYS: i64 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    fetched_at: DateTime<Utc>,
    month: MonthDetail,
}

pub struct MonthCache {
    dir: PathBuf,
    ttl_days: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Age {
    Current,
    Previous,
    Older,
}

fn age_of(month: NaiveDate, today: NaiveDate) -> Age {
    let m = month.year() * 12 + month.month0() as i32;
    let t = today.year() * 12 + today.month0() as i32;
    match t - m {
        i32::MIN..=0 => Age::Current,
        1 => Age::Previous,
        _ => Age::Older,
    }
}

impl MonthCache {
    /// `ttl_days` of 0 disables the cache entirely.
    pub fn new(dir: PathBuf, ttl_days: i64) -> Self {
        Self { dir, ttl_days }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn enabled(&self) -> bool {
        self.ttl_days > 0
    }

    fn path_for(&self, month: NaiveDate) -> PathBuf {
        self.dir.join(format!("{month}.json"))
    }

    fn ttl_for(&self, month: NaiveDate, today: NaiveDate) -> Option<i64> {
        if !self.enabled() {
            return None;
        }
        match age_of(month, today) {
            Age::Current => None,
            Age::Previous => Some(PREVIOUS_MONTH_TTL_DAYS.min(self.ttl_days)),
            Age::Older => Some(self.ttl_days),
        }
    }

    /// A cached month that is still within its TTL, or None (with a log line saying why).
    pub fn get(
        &self,
        month: NaiveDate,
        today: NaiveDate,
        now: DateTime<Utc>,
    ) -> Option<MonthDetail> {
        let ttl = self.ttl_for(month, today)?;
        let path = self.path_for(month);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "month cache unreadable; refetching");
                return None;
            }
        };
        let entry: Entry = match serde_json::from_str(&text) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "month cache corrupt; refetching");
                return None;
            }
        };
        let age_days = (now - entry.fetched_at).num_days();
        if age_days >= ttl {
            tracing::debug!(%month, age_days, ttl, "month cache expired");
            return None;
        }
        Some(entry.month)
    }

    /// Store a month if its age allows caching. Failures are logged, never fatal.
    pub fn put(
        &self,
        month: NaiveDate,
        today: NaiveDate,
        now: DateTime<Utc>,
        detail: &MonthDetail,
    ) {
        if self.ttl_for(month, today).is_none() {
            return;
        }
        if let Err(e) = self.write(month, now, detail) {
            tracing::warn!(%month, error = format!("{e:#}"), "could not write month cache");
        }
    }

    fn write(&self, month: NaiveDate, now: DateTime<Utc>, detail: &MonthDetail) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("create {}", self.dir.display()))?;
        let entry = serde_json::json!({ "fetched_at": now, "month": detail });
        let path = self.path_for(month);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&entry)?)
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename into {}", path.display()))?;
        Ok(())
    }

    pub fn count(&self) -> usize {
        std::fs::read_dir(&self.dir)
            .map(|d| {
                d.filter_map(Result::ok)
                    .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                    .count()
            })
            .unwrap_or(0)
    }

    pub fn clear(&self) -> Result<usize> {
        let mut removed = 0;
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e).with_context(|| format!("read {}", self.dir.display())),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_some_and(|x| x == "json") {
                std::fs::remove_file(&path)
                    .with_context(|| format!("remove {}", path.display()))?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn detail(month: &str) -> MonthDetail {
        MonthDetail {
            month: month.to_string(),
            note: None,
            income: 0,
            budgeted: 0,
            activity: 0,
            to_be_budgeted: 0,
            age_of_money: None,
            categories: vec![],
        }
    }

    fn cache(name: &str, ttl: i64) -> MonthCache {
        let dir =
            std::env::temp_dir().join(format!("ynab-mcp-cache-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        MonthCache::new(dir, ttl)
    }

    #[test]
    fn classifies_month_age_across_year_boundary() {
        assert_eq!(age_of(d("2026-01-01"), d("2026-01-15")), Age::Current);
        assert_eq!(age_of(d("2025-12-01"), d("2026-01-15")), Age::Previous);
        assert_eq!(age_of(d("2025-06-01"), d("2026-01-15")), Age::Older);
        assert_eq!(
            age_of(d("2026-03-01"), d("2026-01-15")),
            Age::Current,
            "future months are never cached"
        );
    }

    #[test]
    fn current_month_is_never_cached() {
        let c = cache("current", 30);
        let today = d("2026-09-14");
        let now = Utc::now();
        c.put(d("2026-09-01"), today, now, &detail("2026-09-01"));
        assert_eq!(c.count(), 0);
        assert!(c.get(d("2026-09-01"), today, now).is_none());
    }

    #[test]
    fn older_month_round_trips_and_expires() {
        let c = cache("older", 30);
        let today = d("2026-09-14");
        let now = Utc::now();
        c.put(d("2026-06-01"), today, now, &detail("2026-06-01"));
        assert_eq!(
            c.get(d("2026-06-01"), today, now).unwrap().month,
            "2026-06-01"
        );
        let later = now + chrono::Duration::days(31);
        assert!(c.get(d("2026-06-01"), today, later).is_none());
    }

    #[test]
    fn previous_month_expires_after_a_day() {
        let c = cache("prev", 30);
        let today = d("2026-09-14");
        let now = Utc::now();
        c.put(d("2026-08-01"), today, now, &detail("2026-08-01"));
        assert!(
            c.get(d("2026-08-01"), today, now + chrono::Duration::hours(23))
                .is_some()
        );
        assert!(
            c.get(d("2026-08-01"), today, now + chrono::Duration::hours(25))
                .is_none()
        );
    }

    #[test]
    fn ttl_zero_disables_and_corrupt_file_is_refetched() {
        let off = cache("off", 0);
        off.put(
            d("2026-06-01"),
            d("2026-09-14"),
            Utc::now(),
            &detail("2026-06-01"),
        );
        assert_eq!(off.count(), 0);

        let c = cache("corrupt", 30);
        std::fs::create_dir_all(c.dir()).unwrap();
        std::fs::write(c.dir().join("2026-06-01.json"), "{nope").unwrap();
        assert!(
            c.get(d("2026-06-01"), d("2026-09-14"), Utc::now())
                .is_none()
        );
        assert_eq!(c.clear().unwrap(), 1);
        assert_eq!(c.count(), 0);
    }
}
