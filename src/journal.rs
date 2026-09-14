//! Append-only write journal so any agent (or a human) can undo what another agent did.
//!
//! One JSON record per line. Batches are never rewritten; an undo appends its own record and
//! the current state is derived by replay. An exclusive OS file lock guards every read-modify
//! cycle so concurrent server processes cannot double-undo or interleave writes.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::money::Milliunits;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Op {
    pub transaction_id: String,
    pub account_id: String,
    pub date: String,
    pub amount: Milliunits,
    pub payee_name: Option<String>,
    pub import_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRecord {
    pub batch_id: String,
    pub at: String,
    pub plan_id: String,
    pub tool: String,
    pub client: Option<String>,
    pub ops: Vec<Op>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skipped {
    pub transaction_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoRecord {
    pub batch_id: String,
    pub at: String,
    pub client: Option<String>,
    /// Deleted from YNAB by this undo.
    pub deleted: Vec<String>,
    /// Already gone from YNAB (someone deleted them by hand); treated as resolved.
    pub missing: Vec<String>,
    /// Still in YNAB; the batch stays open for these.
    pub skipped: Vec<Skipped>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Batch(BatchRecord),
    Undo(UndoRecord),
}

/// A batch with its undo history folded in.
#[derive(Debug, Clone, Serialize)]
pub struct BatchState {
    pub batch: BatchRecord,
    pub undos: Vec<UndoRecord>,
    /// Ops not yet deleted or found missing.
    pub remaining: Vec<Op>,
}

impl BatchState {
    pub fn status(&self) -> &'static str {
        if self.remaining.is_empty() {
            "undone"
        } else if self.undos.is_empty() {
            "open"
        } else {
            "partially_undone"
        }
    }
}

pub struct Journal {
    path: PathBuf,
}

/// The journal file, opened and exclusively locked. Dropping it releases the lock.
pub struct LockedJournal {
    file: File,
}

impl Journal {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn default_path() -> Result<PathBuf> {
        if let Ok(p) = std::env::var("YNAB_MCP_JOURNAL") {
            return Ok(PathBuf::from(p));
        }
        let home = std::env::var("HOME").context("HOME is not set")?;
        Ok(Path::new(&home).join(".local/share/ynab-mcp/journal.jsonl"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Blocks until the exclusive lock is held. Callers keep the guard across any YNAB calls
    /// that must not race another process's undo.
    pub fn lock(&self) -> Result<LockedJournal> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("open journal {}", self.path.display()))?;
        file.lock().context("lock journal")?;
        Ok(LockedJournal { file })
    }
}

impl LockedJournal {
    pub fn append(&mut self, record: &Record) -> Result<()> {
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.flush()?;
        Ok(())
    }

    pub fn records(&mut self) -> Result<Vec<Record>> {
        self.file.seek(SeekFrom::Start(0))?;
        let reader = BufReader::new(&self.file);
        let mut records = Vec::new();
        for (n, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let record = serde_json::from_str(&line)
                .with_context(|| format!("journal line {} is not a valid record", n + 1))?;
            records.push(record);
        }
        Ok(records)
    }

    /// Batches in journal order (oldest first) with undo history applied.
    pub fn batches(&mut self) -> Result<Vec<BatchState>> {
        Ok(fold(self.records()?))
    }
}

pub fn fold(records: Vec<Record>) -> Vec<BatchState> {
    let mut batches: Vec<BatchState> = Vec::new();
    for record in records {
        match record {
            Record::Batch(batch) => {
                let remaining = batch.ops.clone();
                batches.push(BatchState {
                    batch,
                    undos: Vec::new(),
                    remaining,
                });
            }
            Record::Undo(undo) => {
                let Some(state) = batches
                    .iter_mut()
                    .find(|b| b.batch.batch_id == undo.batch_id)
                else {
                    tracing::warn!(batch_id = %undo.batch_id, "undo record for unknown batch; ignoring");
                    continue;
                };
                let resolved: HashSet<&String> =
                    undo.deleted.iter().chain(undo.missing.iter()).collect();
                state
                    .remaining
                    .retain(|op| !resolved.contains(&op.transaction_id));
                state.undos.push(undo);
            }
        }
    }
    batches
}

pub fn new_batch_id() -> String {
    let millis = chrono::Utc::now().timestamp_millis();
    format!("b{millis}p{}", std::process::id())
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_journal(name: &str) -> Journal {
        let dir = std::env::temp_dir().join(format!("ynab-mcp-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Journal::new(dir.join("journal.jsonl"))
    }

    fn op(id: &str) -> Op {
        Op {
            transaction_id: id.to_string(),
            account_id: "acct".to_string(),
            date: "2026-09-01".to_string(),
            amount: -1000,
            payee_name: None,
            import_id: None,
        }
    }

    fn batch(id: &str, ops: &[&str]) -> Record {
        Record::Batch(BatchRecord {
            batch_id: id.to_string(),
            at: now(),
            plan_id: "plan".to_string(),
            tool: "create_transactions".to_string(),
            client: Some("test".to_string()),
            ops: ops.iter().map(|o| op(o)).collect(),
        })
    }

    fn undo(id: &str, deleted: &[&str], missing: &[&str], skipped: &[&str]) -> Record {
        Record::Undo(UndoRecord {
            batch_id: id.to_string(),
            at: now(),
            client: None,
            deleted: deleted.iter().map(|s| s.to_string()).collect(),
            missing: missing.iter().map(|s| s.to_string()).collect(),
            skipped: skipped
                .iter()
                .map(|s| Skipped {
                    transaction_id: s.to_string(),
                    reason: "reconciled".to_string(),
                })
                .collect(),
        })
    }

    #[test]
    fn replay_tracks_remaining_across_partial_undos() {
        let states = fold(vec![
            batch("b1", &["t1", "t2", "t3"]),
            undo("b1", &["t1"], &["t2"], &["t3"]),
        ]);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].status(), "partially_undone");
        assert_eq!(states[0].remaining.len(), 1);
        assert_eq!(states[0].remaining[0].transaction_id, "t3");

        let states = fold(vec![
            batch("b1", &["t1", "t2", "t3"]),
            undo("b1", &["t1"], &["t2"], &["t3"]),
            undo("b1", &["t3"], &[], &[]),
        ]);
        assert_eq!(states[0].status(), "undone");
    }

    #[test]
    fn open_batch_and_unknown_undo() {
        let states = fold(vec![batch("b1", &["t1"]), undo("ghost", &["x"], &[], &[])]);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].status(), "open");
    }

    #[test]
    fn append_then_read_round_trips_through_the_file() {
        let journal = temp_journal("roundtrip");
        {
            let mut locked = journal.lock().unwrap();
            locked.append(&batch("b1", &["t1", "t2"])).unwrap();
            locked.append(&undo("b1", &["t1"], &[], &[])).unwrap();
        }
        let mut locked = journal.lock().unwrap();
        let states = locked.batches().unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].remaining.len(), 1);
        assert_eq!(states[0].remaining[0].transaction_id, "t2");
    }

    #[test]
    fn lock_excludes_a_second_holder_until_dropped() {
        let journal = temp_journal("lock");
        let path = journal.path().to_path_buf();
        let first = journal.lock().unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let second = Journal::new(path).lock().unwrap();
            tx.send(std::time::Instant::now()).unwrap();
            drop(second);
        });
        std::thread::sleep(std::time::Duration::from_millis(150));
        let released = std::time::Instant::now();
        drop(first);
        let acquired = rx.recv().unwrap();
        handle.join().unwrap();
        assert!(
            acquired >= released,
            "second lock was granted while the first was held"
        );
    }

    #[test]
    fn corrupt_line_is_an_error_not_a_silent_skip() {
        let journal = temp_journal("corrupt");
        let mut locked = journal.lock().unwrap();
        locked.append(&batch("b1", &["t1"])).unwrap();
        locked.file.write_all(b"{not json}\n").unwrap();
        assert!(locked.records().is_err());
    }
}
