//! Append-only write journal so any agent (or a human) can undo what another agent did.
//!
//! One JSON record per line. Writes are never rewritten; an undo appends its own record and
//! the current state is derived by replay. An exclusive OS file lock guards every read-modify
//! cycle so concurrent server processes cannot double-undo or interleave writes.
//!
//! Three write kinds share the one undo record shape: `batch` (rows created; undo deletes
//! them), `replace` (one row swapped for another; undo deletes the replacement and re-creates
//! the original from its snapshot), and `assign` (category assigned amounts changed; undo sets
//! them back). Fields that only one kind uses are optional on the wire, so the records written
//! before those kinds existed still parse.

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

/// One leg of a snapshotted split.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotLine {
    pub amount: Milliunits,
    pub category_id: Option<String>,
    pub memo: Option<String>,
    pub payee_id: Option<String>,
    pub payee_name: Option<String>,
}

/// Everything needed to re-create a transaction as it was. Kept independent of the API
/// structs so a change in what YNAB returns cannot break old journal lines.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransactionSnapshot {
    pub transaction_id: String,
    pub account_id: String,
    pub date: String,
    pub amount: Milliunits,
    pub payee_id: Option<String>,
    pub payee_name: Option<String>,
    pub category_id: Option<String>,
    pub memo: Option<String>,
    pub cleared: String,
    pub approved: bool,
    pub flag_color: Option<String>,
    pub import_id: Option<String>,
    #[serde(default)]
    pub subtransactions: Vec<SnapshotLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplaceRecord {
    pub batch_id: String,
    pub at: String,
    pub plan_id: String,
    pub tool: String,
    pub client: Option<String>,
    /// The row as it was before, so undo can put it back.
    pub original: TransactionSnapshot,
    /// The row that took its place, as YNAB returned it.
    pub replacement: TransactionSnapshot,
    /// False when the delete of the original failed after the replacement was created: both
    /// rows exist in YNAB and the user was told which one to delete by hand.
    pub original_deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssignKey {
    pub month: String,
    pub category_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignOp {
    pub month: String,
    pub category_id: String,
    pub category_name: Option<String>,
    pub old_budgeted: Milliunits,
    pub new_budgeted: Milliunits,
}

impl AssignOp {
    pub fn key(&self) -> AssignKey {
        AssignKey {
            month: self.month.clone(),
            category_id: self.category_id.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignRecord {
    pub batch_id: String,
    pub at: String,
    pub plan_id: String,
    pub tool: String,
    pub client: Option<String>,
    pub ops: Vec<AssignOp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skipped {
    pub transaction_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedAssign {
    pub month: String,
    pub category_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoRecord {
    pub batch_id: String,
    pub at: String,
    pub client: Option<String>,
    /// Deleted from YNAB by this undo (batch rows, or a replace's replacement row).
    pub deleted: Vec<String>,
    /// Already gone from YNAB (someone deleted them by hand); treated as resolved.
    pub missing: Vec<String>,
    /// Still in YNAB; the write stays open for these.
    pub skipped: Vec<Skipped>,
    /// Replace undo: the id of the re-created original.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recreated_transaction_id: Option<String>,
    /// Assign undo: the assignments set back to their old amount.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reverted: Vec<AssignKey>,
    /// Assign undo: assignments left as they are, with the reason.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_assignments: Vec<SkippedAssign>,
}

impl UndoRecord {
    pub fn new(batch_id: String, client: Option<String>) -> Self {
        Self {
            batch_id,
            at: now(),
            client,
            deleted: Vec::new(),
            missing: Vec::new(),
            skipped: Vec::new(),
            recreated_transaction_id: None,
            reverted: Vec::new(),
            skipped_assignments: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Batch(BatchRecord),
    Replace(Box<ReplaceRecord>),
    Assign(AssignRecord),
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

#[derive(Debug, Clone, Serialize)]
pub struct ReplaceState {
    pub record: ReplaceRecord,
    pub undos: Vec<UndoRecord>,
    /// The replacement row has been deleted, or was found already gone.
    pub replacement_gone: bool,
    /// The original is back in YNAB (re-created by an undo, or never deleted).
    pub original_restored: bool,
}

impl ReplaceState {
    pub fn is_open(&self) -> bool {
        !(self.replacement_gone && self.original_restored)
    }

    pub fn status(&self) -> &'static str {
        if !self.is_open() {
            "undone"
        } else if self.undos.is_empty() {
            "open"
        } else {
            "partially_undone"
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AssignState {
    pub record: AssignRecord,
    pub undos: Vec<UndoRecord>,
    /// Assignments not yet set back.
    pub remaining: Vec<AssignOp>,
}

impl AssignState {
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

/// Any write with its undo history folded in.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WriteState {
    Batch(BatchState),
    Replace(Box<ReplaceState>),
    Assign(AssignState),
}

impl WriteState {
    pub fn batch_id(&self) -> &str {
        match self {
            Self::Batch(s) => &s.batch.batch_id,
            Self::Replace(s) => &s.record.batch_id,
            Self::Assign(s) => &s.record.batch_id,
        }
    }

    pub fn plan_id(&self) -> &str {
        match self {
            Self::Batch(s) => &s.batch.plan_id,
            Self::Replace(s) => &s.record.plan_id,
            Self::Assign(s) => &s.record.plan_id,
        }
    }

    pub fn at(&self) -> &str {
        match self {
            Self::Batch(s) => &s.batch.at,
            Self::Replace(s) => &s.record.at,
            Self::Assign(s) => &s.record.at,
        }
    }

    pub fn tool(&self) -> &str {
        match self {
            Self::Batch(s) => &s.batch.tool,
            Self::Replace(s) => &s.record.tool,
            Self::Assign(s) => &s.record.tool,
        }
    }

    pub fn client(&self) -> Option<&str> {
        match self {
            Self::Batch(s) => s.batch.client.as_deref(),
            Self::Replace(s) => s.record.client.as_deref(),
            Self::Assign(s) => s.record.client.as_deref(),
        }
    }

    pub fn undos(&self) -> &[UndoRecord] {
        match self {
            Self::Batch(s) => &s.undos,
            Self::Replace(s) => &s.undos,
            Self::Assign(s) => &s.undos,
        }
    }

    pub fn status(&self) -> &'static str {
        match self {
            Self::Batch(s) => s.status(),
            Self::Replace(s) => s.status(),
            Self::Assign(s) => s.status(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.status() != "undone"
    }

    fn push_undo(&mut self, undo: UndoRecord) {
        match self {
            Self::Batch(s) => {
                let resolved: HashSet<&String> =
                    undo.deleted.iter().chain(undo.missing.iter()).collect();
                s.remaining
                    .retain(|op| !resolved.contains(&op.transaction_id));
                s.undos.push(undo);
            }
            Self::Replace(s) => {
                let replacement = &s.record.replacement.transaction_id;
                if undo.deleted.contains(replacement) || undo.missing.contains(replacement) {
                    s.replacement_gone = true;
                }
                if undo.recreated_transaction_id.is_some() {
                    s.original_restored = true;
                }
                s.undos.push(undo);
            }
            Self::Assign(s) => {
                s.remaining.retain(|op| !undo.reverted.contains(&op.key()));
                s.undos.push(undo);
            }
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

    /// Writes in journal order (oldest first) with undo history applied.
    pub fn writes(&mut self) -> Result<Vec<WriteState>> {
        Ok(fold(self.records()?))
    }
}

pub fn fold(records: Vec<Record>) -> Vec<WriteState> {
    let mut writes: Vec<WriteState> = Vec::new();
    for record in records {
        match record {
            Record::Batch(batch) => {
                let remaining = batch.ops.clone();
                writes.push(WriteState::Batch(BatchState {
                    batch,
                    undos: Vec::new(),
                    remaining,
                }));
            }
            Record::Replace(record) => {
                let original_restored = !record.original_deleted;
                writes.push(WriteState::Replace(Box::new(ReplaceState {
                    record: *record,
                    undos: Vec::new(),
                    replacement_gone: false,
                    original_restored,
                })));
            }
            Record::Assign(record) => {
                let remaining = record.ops.clone();
                writes.push(WriteState::Assign(AssignState {
                    record,
                    undos: Vec::new(),
                    remaining,
                }));
            }
            Record::Undo(undo) => {
                let Some(idx) = writes.iter().position(|w| w.batch_id() == undo.batch_id) else {
                    tracing::warn!(batch_id = %undo.batch_id, "undo record for unknown batch; ignoring");
                    continue;
                };
                let recreated = match (&writes[idx], &undo.recreated_transaction_id) {
                    (WriteState::Replace(r), Some(new_id)) => {
                        Some((r.record.original.transaction_id.clone(), new_id.clone()))
                    }
                    _ => None,
                };
                writes[idx].push_undo(undo);
                if let Some((old_id, new_id)) = recreated {
                    repoint_batches(&mut writes, &old_id, &new_id);
                }
            }
        }
    }
    writes
}

/// An undone replace re-creates its original under a new id. If that original had been
/// created by a journaled batch, the batch's op now points at a row that no longer exists, so
/// re-point it at the re-created row: same account, date, and amount, just a new id.
fn repoint_batches(writes: &mut [WriteState], old_id: &str, new_id: &str) {
    for w in writes.iter_mut() {
        let WriteState::Batch(b) = w else { continue };
        for op in b
            .remaining
            .iter_mut()
            .chain(b.batch.ops.iter_mut())
            .filter(|op| op.transaction_id == old_id)
        {
            op.transaction_id = new_id.to_string();
        }
    }
}

/// Pick the write an undo targets: by id, or the most recent one that is still open. The
/// plan-id guard keeps one plan's journal entries from being undone against another plan.
pub fn select_for_undo<'a>(
    writes: &'a [WriteState],
    batch_id: Option<&str>,
    plan_id: &str,
) -> std::result::Result<&'a WriteState, String> {
    let state = match batch_id {
        Some(id) => writes
            .iter()
            .find(|w| w.batch_id() == id)
            .ok_or_else(|| format!("no batch {id} in the journal"))?,
        None => writes
            .iter()
            .rev()
            .find(|w| w.is_open())
            .ok_or_else(|| "nothing left to undo".to_string())?,
    };
    if !state.is_open() {
        return Err(format!(
            "batch {} is already fully undone",
            state.batch_id()
        ));
    }
    if state.plan_id() != plan_id {
        return Err(format!(
            "batch {} belongs to plan {}, this server is on plan {plan_id}",
            state.batch_id(),
            state.plan_id()
        ));
    }
    Ok(state)
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
        let mut u = UndoRecord::new(id.to_string(), None);
        u.deleted = deleted.iter().map(|s| s.to_string()).collect();
        u.missing = missing.iter().map(|s| s.to_string()).collect();
        u.skipped = skipped
            .iter()
            .map(|s| Skipped {
                transaction_id: s.to_string(),
                reason: "reconciled".to_string(),
            })
            .collect();
        Record::Undo(u)
    }

    fn snapshot(id: &str, amount: Milliunits) -> TransactionSnapshot {
        TransactionSnapshot {
            transaction_id: id.to_string(),
            account_id: "acct".to_string(),
            date: "2026-06-11".to_string(),
            amount,
            payee_id: Some("p1".to_string()),
            payee_name: Some("Cash".to_string()),
            category_id: Some("vci".to_string()),
            memo: None,
            cleared: "reconciled".to_string(),
            approved: true,
            flag_color: None,
            import_id: Some("YNAB:800000:2026-06-11:1".to_string()),
            subtransactions: vec![],
        }
    }

    fn replace(id: &str, original_deleted: bool) -> Record {
        Record::Replace(Box::new(ReplaceRecord {
            batch_id: id.to_string(),
            at: now(),
            plan_id: "plan".to_string(),
            tool: "replace_transaction".to_string(),
            client: Some("test".to_string()),
            original: snapshot("old", 800_000),
            replacement: TransactionSnapshot {
                subtransactions: vec![
                    SnapshotLine {
                        amount: 823_500,
                        category_id: Some("rta".to_string()),
                        memo: None,
                        payee_id: None,
                        payee_name: None,
                    },
                    SnapshotLine {
                        amount: -23_500,
                        category_id: Some("fees".to_string()),
                        memo: None,
                        payee_id: None,
                        payee_name: None,
                    },
                ],
                category_id: None,
                ..snapshot("new", 800_000)
            },
            original_deleted,
        }))
    }

    fn assign_op(month: &str, cat: &str, old: Milliunits, new: Milliunits) -> AssignOp {
        AssignOp {
            month: month.to_string(),
            category_id: cat.to_string(),
            category_name: None,
            old_budgeted: old,
            new_budgeted: new,
        }
    }

    fn assign(id: &str, ops: Vec<AssignOp>) -> Record {
        Record::Assign(AssignRecord {
            batch_id: id.to_string(),
            at: now(),
            plan_id: "plan".to_string(),
            tool: "assign_money".to_string(),
            client: None,
            ops,
        })
    }

    fn as_batch(w: &WriteState) -> &BatchState {
        match w {
            WriteState::Batch(b) => b,
            other => panic!("expected a batch, got {other:?}"),
        }
    }

    #[test]
    fn replay_tracks_remaining_across_partial_undos() {
        let states = fold(vec![
            batch("b1", &["t1", "t2", "t3"]),
            undo("b1", &["t1"], &["t2"], &["t3"]),
        ]);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].status(), "partially_undone");
        assert_eq!(as_batch(&states[0]).remaining.len(), 1);
        assert_eq!(as_batch(&states[0]).remaining[0].transaction_id, "t3");

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
        let states = locked.writes().unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(as_batch(&states[0]).remaining.len(), 1);
        assert_eq!(as_batch(&states[0]).remaining[0].transaction_id, "t2");
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

    /// Verbatim shape of the lines written by 0.1.1 (ids replaced). These must keep parsing
    /// after every journal change, or the real journal becomes unreadable.
    #[test]
    fn journal_lines_from_0_1_1_still_parse() {
        let old_batch = r#"{"kind":"batch","batch_id":"b1789485262555p15546","at":"2026-09-15T15:14:22Z","plan_id":"636a7bed","tool":"create_transactions","client":"claude-code","ops":[{"transaction_id":"1d67477b","account_id":"acct","date":"2026-09-15","amount":-96960,"payee_name":"Comic Books","import_id":"YNAB:-96960:2026-09-15:1"}]}"#;
        let old_undo = r#"{"kind":"undo","batch_id":"b1789485262555p15546","at":"2026-09-15T15:17:20Z","client":"claude-code","deleted":["1d67477b"],"missing":[],"skipped":[]}"#;
        let batch: Record = serde_json::from_str(old_batch).unwrap();
        let undo: Record = serde_json::from_str(old_undo).unwrap();
        let Record::Undo(u) = &undo else {
            panic!("not an undo")
        };
        assert!(u.recreated_transaction_id.is_none());
        assert!(u.reverted.is_empty());
        assert!(u.skipped_assignments.is_empty());
        let states = fold(vec![batch, undo]);
        assert_eq!(states[0].status(), "undone");
        assert_eq!(states[0].tool(), "create_transactions");
    }

    #[test]
    fn undo_record_without_new_fields_serializes_like_before() {
        let u = UndoRecord::new("b1".into(), None);
        let text = serde_json::to_string(&Record::Undo(u)).unwrap();
        assert!(!text.contains("recreated_transaction_id"));
        assert!(!text.contains("reverted"));
        assert!(!text.contains("skipped_assignments"));
    }

    #[test]
    fn replace_and_assign_records_round_trip() {
        let journal = temp_journal("kinds");
        {
            let mut locked = journal.lock().unwrap();
            locked.append(&replace("r1", true)).unwrap();
            locked
                .append(&assign(
                    "a1",
                    vec![assign_op("2026-06-01", "tax", 252_000, 336_000)],
                ))
                .unwrap();
        }
        let mut locked = journal.lock().unwrap();
        let records = locked.records().unwrap();
        assert!(
            matches!(&records[0], Record::Replace(r) if r.replacement.subtransactions.len() == 2)
        );
        assert!(matches!(&records[1], Record::Assign(a) if a.ops[0].new_budgeted == 336_000));
        let states = locked.writes().unwrap();
        assert_eq!(states[0].status(), "open");
        assert_eq!(states[1].status(), "open");
        assert_eq!(states[0].tool(), "replace_transaction");
        assert_eq!(states[1].tool(), "assign_money");
    }

    #[test]
    fn replace_state_needs_replacement_gone_and_original_back() {
        let mut half = UndoRecord::new("r1".into(), None);
        half.deleted.push("new".into());
        let states = fold(vec![replace("r1", true), Record::Undo(half.clone())]);
        assert_eq!(
            states[0].status(),
            "partially_undone",
            "replacement deleted but original not re-created yet"
        );

        let mut full = half.clone();
        full.recreated_transaction_id = Some("old-again".into());
        let states = fold(vec![replace("r1", true), Record::Undo(full)]);
        assert_eq!(states[0].status(), "undone");

        // A second undo that only re-creates finishes the job.
        let mut recreate_only = UndoRecord::new("r1".into(), None);
        recreate_only.recreated_transaction_id = Some("old-again".into());
        let states = fold(vec![
            replace("r1", true),
            Record::Undo(half),
            Record::Undo(recreate_only),
        ]);
        assert_eq!(states[0].status(), "undone");
    }

    #[test]
    fn undoing_a_replace_repoints_the_batch_that_created_the_original() {
        let mut u = UndoRecord::new("r1".into(), None);
        u.deleted.push("new".into());
        u.recreated_transaction_id = Some("old-again".into());
        let states = fold(vec![
            batch("b1", &["old", "other"]),
            replace("r1", true),
            Record::Undo(u),
        ]);
        assert_eq!(states[1].status(), "undone");
        let b = as_batch(&states[0]);
        assert_eq!(b.status(), "open");
        assert_eq!(b.remaining[0].transaction_id, "old-again");
        assert_eq!(b.remaining[1].transaction_id, "other");
        assert_eq!(b.batch.ops[0].transaction_id, "old-again");
    }

    #[test]
    fn replace_whose_delete_failed_is_undone_by_deleting_the_replacement_alone() {
        let mut u = UndoRecord::new("r1".into(), None);
        u.missing.push("new".into());
        let states = fold(vec![replace("r1", false), Record::Undo(u)]);
        assert_eq!(states[0].status(), "undone");
    }

    #[test]
    fn assign_state_tracks_reverted_keys() {
        let ops = vec![
            assign_op("2026-06-01", "tax", 252_000, 336_000),
            assign_op("2026-06-01", "bo", 36_000, 48_000),
            assign_op("2026-07-01", "fees", 0, 23_500),
        ];
        let mut u = UndoRecord::new("a1".into(), None);
        u.reverted.push(AssignKey {
            month: "2026-06-01".into(),
            category_id: "tax".into(),
        });
        u.skipped_assignments.push(SkippedAssign {
            month: "2026-07-01".into(),
            category_id: "fees".into(),
            reason: "changed since".into(),
        });
        let states = fold(vec![assign("a1", ops), Record::Undo(u)]);
        let WriteState::Assign(a) = &states[0] else {
            panic!("not an assign")
        };
        assert_eq!(a.status(), "partially_undone");
        assert_eq!(a.remaining.len(), 2);
        assert!(
            a.remaining
                .iter()
                .all(|op| !(op.month == "2026-06-01" && op.category_id == "tax"))
        );
    }

    #[test]
    fn select_for_undo_prefers_latest_open_of_any_kind_and_guards_plan() {
        let mut done = UndoRecord::new("a1".into(), None);
        done.reverted.push(AssignKey {
            month: "2026-06-01".into(),
            category_id: "tax".into(),
        });
        let states = fold(vec![
            batch("b1", &["t1"]),
            replace("r1", true),
            assign("a1", vec![assign_op("2026-06-01", "tax", 252_000, 336_000)]),
            Record::Undo(done),
        ]);
        assert_eq!(
            select_for_undo(&states, None, "plan").unwrap().batch_id(),
            "r1",
            "the assign is undone, so the replace is the latest open write"
        );
        assert_eq!(
            select_for_undo(&states, Some("b1"), "plan")
                .unwrap()
                .batch_id(),
            "b1"
        );
        let err = select_for_undo(&states, Some("a1"), "plan").unwrap_err();
        assert!(err.contains("already fully undone"));
        let err = select_for_undo(&states, Some("r1"), "other-plan").unwrap_err();
        assert!(err.contains("belongs to plan plan"));
        let err = select_for_undo(&states, Some("nope"), "plan").unwrap_err();
        assert!(err.contains("no batch nope"));
    }
}
