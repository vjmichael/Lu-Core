//! The loop guard: stops a model that loops, and drives restore-first
//! recovery when its work keeps failing.
//!
//! Three guards, all host-only (no model output can switch them off):
//! - [`StreamLoopGuard`] stops a generation that is repeating itself (token
//!   looping). Ported unchanged in behaviour from Lu-AI-GUI
//!   `generation_loop_guard.rs`.
//! - [`ToolLoopGuard`] stops a turn that keeps proposing the same action, or
//!   far too many actions (tool looping).
//! - [`Recovery`] is the repair cap. When a check on the model's work fails,
//!   the first move is to **restore** the snapshot, never to let the model
//!   patch a damaged file. Failed repairs of the same job are counted (and
//!   kept across restarts). At the cap, Lu restores the last good state,
//!   stops the model and asks the user, or offers a stronger model.
use std::{
    collections::HashMap,
    path::Path,
    sync::Mutex,
};

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::ansf_operations::{CommandFinish, CommandStart, FileChange};
use crate::recorder::{RecordError, Recorder};
use crate::snapshot::{SnapshotError, SnapshotStore};

// ---------------------------------------------------------------- stream

const WINDOW_BYTES: usize = 8192;
const MIN_CYCLE_BYTES: usize = 1024;
const MIN_REPETITIONS: usize = 6;
const MAX_CYCLE_PERIOD: usize = 512;
const CHECK_EVERY_BYTES: usize = 64;
const MAX_CONSECUTIVE_EMPTY_CHUNKS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamLoopVerdict {
    Continue,
    /// The last kilobyte or more is one short block repeated at least six
    /// times, or the stream sent 64 empty chunks in a row.
    RepeatedOutput,
}

/// Watches generated text as it streams. Conservative: prose, code with
/// repeating short labels and normal JSON are not stopped.
#[derive(Default)]
pub struct StreamLoopGuard {
    tail: Vec<u8>,
    since_check: usize,
    total_bytes: u64,
    empty_chunks: usize,
}
impl StreamLoopGuard {
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
    pub fn observe_chunk(&mut self, chunk: &str) -> StreamLoopVerdict {
        if chunk.is_empty() {
            self.empty_chunks = self.empty_chunks.saturating_add(1);
            return if self.empty_chunks >= MAX_CONSECUTIVE_EMPTY_CHUNKS {
                StreamLoopVerdict::RepeatedOutput
            } else {
                StreamLoopVerdict::Continue
            };
        }
        self.empty_chunks = 0;
        self.total_bytes = self.total_bytes.saturating_add(chunk.len() as u64);
        self.since_check = self.since_check.saturating_add(chunk.len());
        self.tail.extend_from_slice(chunk.as_bytes());
        if self.tail.len() > WINDOW_BYTES {
            let remove = self.tail.len() - WINDOW_BYTES;
            self.tail.drain(..remove);
        }
        if self.since_check < CHECK_EVERY_BYTES {
            return StreamLoopVerdict::Continue;
        }
        self.since_check = 0;
        if self.repeating_suffix() {
            StreamLoopVerdict::RepeatedOutput
        } else {
            StreamLoopVerdict::Continue
        }
    }
    fn repeating_suffix(&self) -> bool {
        let bytes = &self.tail;
        if bytes.len() < MIN_CYCLE_BYTES {
            return false;
        }
        (1..=MAX_CYCLE_PERIOD).any(|period| {
            let reps = MIN_REPETITIONS.max(MIN_CYCLE_BYTES.div_ceil(period));
            if reps * period > bytes.len() {
                return false;
            }
            let last = &bytes[bytes.len() - period..];
            (2..=reps).all(|r| {
                let start = bytes.len() - r * period;
                &bytes[start..start + period] == last
            })
        })
    }
}

// ---------------------------------------------------------------- tools

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolLoopLimits {
    /// The same action (same kind and text) this many times in one turn.
    pub same_action: u32,
    /// Any actions this many times in one turn.
    pub per_turn: u32,
}
impl Default for ToolLoopLimits {
    fn default() -> Self {
        Self { same_action: 3, per_turn: 200 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolLoopVerdict {
    Continue,
    /// The model proposed this exact action `count` times this turn.
    SameAction { count: u32 },
    /// The turn hit the per-turn action limit.
    TooManyActions { count: u32 },
}

/// Counts the actions of one turn. Make a new one per user turn.
#[derive(Default)]
pub struct ToolLoopGuard {
    limits: ToolLoopLimits,
    seen: HashMap<String, u32>,
    total: u32,
}
impl ToolLoopGuard {
    pub fn new(limits: ToolLoopLimits) -> Self {
        Self { limits, ..Default::default() }
    }
    /// Call before running each proposed action; stop the turn on anything
    /// but `Continue`.
    pub fn observe(&mut self, kind: &str, text: &str) -> ToolLoopVerdict {
        self.total += 1;
        let count = self.seen.entry(format!("{kind}\u{1f}{text}")).or_insert(0);
        *count += 1;
        if *count >= self.limits.same_action {
            ToolLoopVerdict::SameAction { count: *count }
        } else if self.total >= self.limits.per_turn {
            ToolLoopVerdict::TooManyActions { count: self.total }
        } else {
            ToolLoopVerdict::Continue
        }
    }
}

// ---------------------------------------------------------------- repairs

/// Failed repairs of one job allowed before Lu stops the model.
/// A DEFAULT PENDING THE OWNER'S APPROVAL.
pub const DEFAULT_REPAIR_CAP: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// The files were restored; the model may try again.
    Retry { restored: Vec<String>, failures: u32, remaining: u32 },
    /// The cap was reached: the files were restored to the last good state
    /// and the model must stop. Ask the user, or offer a stronger model,
    /// which then gets the case file of everything tried.
    Stop { restored: Vec<String>, failures: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryError {
    Database(String),
    Snapshot(SnapshotError),
    Record(RecordError),
    Locked,
}
impl From<rusqlite::Error> for RecoveryError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Database(e.to_string())
    }
}
impl From<SnapshotError> for RecoveryError {
    fn from(e: SnapshotError) -> Self {
        Self::Snapshot(e)
    }
}
impl From<RecordError> for RecoveryError {
    fn from(e: RecordError) -> Self {
        Self::Record(e)
    }
}

/// A failed check on the model's work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedCheck<'s> {
    /// Unique call ID for the restore operation itself.
    pub call_id: &'s str,
    /// The user turn the job serves.
    pub turn_id: &'s str,
    /// What the repeated attempts are for, e.g. the file or task being fixed.
    pub job: &'s str,
    /// The snapshot taken before the failed attempt.
    pub snapshot_id: i64,
    /// Paths (relative to the snapshot's folder) to put back.
    pub paths: &'s [String],
    /// What failed, in a sentence, for the record and the case file.
    pub what_failed: &'s str,
}

pub struct Recovery<'a> {
    recorder: &'a Recorder<'a>,
    snapshots: &'a SnapshotStore,
    cap: u32,
    conn: Mutex<Connection>,
}

fn file_sha(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|b| format!("{:x}", Sha256::digest(&b)))
}

impl<'a> Recovery<'a> {
    pub fn new(recorder: &'a Recorder<'a>, snapshots: &'a SnapshotStore, cap: u32, db: &Path)
        -> Result<Self, RecoveryError> {
        let conn = Connection::open(db)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS repair_jobs(
               session_id TEXT NOT NULL,
               job TEXT NOT NULL,
               failures INTEGER NOT NULL,
               stopped INTEGER NOT NULL DEFAULT 0 CHECK(stopped IN (0,1)),
               last_failure TEXT NOT NULL,
               updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
               PRIMARY KEY(session_id,job)
             );",
        )?;
        Ok(Self { recorder, snapshots, cap: cap.max(1), conn: Mutex::new(conn) })
    }

    /// Failed repairs counted so far for a job.
    pub fn failures(&self, job: &str) -> Result<u32, RecoveryError> {
        let conn = self.conn.lock().map_err(|_| RecoveryError::Locked)?;
        Ok(conn
            .query_row(
                "SELECT failures FROM repair_jobs WHERE session_id=?1 AND job=?2",
                params![self.recorder.session_id(), job],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0) as u32)
    }

    /// Whether the model was stopped on this job. It stays stopped until the
    /// user's next instruction calls [`reset`](Self::reset).
    pub fn is_stopped(&self, job: &str) -> Result<bool, RecoveryError> {
        let conn = self.conn.lock().map_err(|_| RecoveryError::Locked)?;
        Ok(conn
            .query_row(
                "SELECT stopped FROM repair_jobs WHERE session_id=?1 AND job=?2",
                params![self.recorder.session_id(), job],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            == Some(1))
    }

    /// A check on the model's work failed: restore first, count, decide.
    pub fn check_failed(&self, check: &FailedCheck<'_>) -> Result<RecoveryOutcome, RecoveryError> {
        // Count first, so a crash during the restore still counts the failure.
        let failures = {
            let conn = self.conn.lock().map_err(|_| RecoveryError::Locked)?;
            conn.execute(
                "INSERT INTO repair_jobs(session_id,job,failures,last_failure) VALUES(?1,?2,1,?3)
                 ON CONFLICT(session_id,job) DO UPDATE SET failures=failures+1,
                   last_failure=excluded.last_failure,updated_at=excluded.updated_at",
                params![self.recorder.session_id(), check.job, check.what_failed],
            )?;
            conn.query_row(
                "SELECT failures FROM repair_jobs WHERE session_id=?1 AND job=?2",
                params![self.recorder.session_id(), check.job],
                |r| r.get::<_, i64>(0),
            )? as u32
        };
        let stop = failures >= self.cap;
        let restored = self.restore(check, failures, stop)?;
        if stop {
            let conn = self.conn.lock().map_err(|_| RecoveryError::Locked)?;
            conn.execute(
                "UPDATE repair_jobs SET stopped=1 WHERE session_id=?1 AND job=?2",
                params![self.recorder.session_id(), check.job],
            )?;
            Ok(RecoveryOutcome::Stop { restored, failures })
        } else {
            Ok(RecoveryOutcome::Retry { restored, failures, remaining: self.cap - failures })
        }
    }

    /// Put each path back from the snapshot, recorded as one operation.
    fn restore(&self, check: &FailedCheck<'_>, failures: u32, stop: bool) -> Result<Vec<String>, RecoveryError> {
        let scope = self.snapshots.scope(check.snapshot_id)?;
        self.recorder.begin_command(&CommandStart {
            call_id: check.call_id.into(),
            turn_id: check.turn_id.into(),
            command: format!("lu.recover.restore job={} snapshot=snap:{}", check.job, check.snapshot_id),
            cwd: scope.to_string_lossy().into_owned(),
        })?;
        let mut restored = Vec::new();
        let mut problems = Vec::new();
        for path in check.paths {
            let full = scope.join(path);
            let before = file_sha(&full);
            match self.snapshots.restore(check.snapshot_id, path) {
                Ok(()) => {
                    self.recorder.file_changed(&FileChange {
                        call_id: check.call_id.into(),
                        path: full.to_string_lossy().into_owned(),
                        sha256_before: before,
                        sha256_after: file_sha(&full),
                        snapshot_ref: Some(format!("snap:{}", check.snapshot_id)),
                    })?;
                    restored.push(path.clone());
                }
                Err(e) => problems.push(format!("{path}: {e:?}")),
            }
        }
        let summary = format!(
            "check failed: {}\nfailed repairs of this job: {failures} of {}\nrestored: {}\n{}{}",
            check.what_failed,
            self.cap,
            if restored.is_empty() { "nothing".to_owned() } else { restored.join(", ") },
            if problems.is_empty() { String::new() } else { format!("could not restore: {}\n", problems.join("; ")) },
            if stop { "model stopped: ask the user or offer a stronger model" } else { "the model may try again" }
        );
        self.recorder.finish_command(&CommandFinish {
            call_id: check.call_id.into(),
            exit_code: Some(if problems.is_empty() { 0 } else { 1 }),
            timed_out: false,
            cancelled: false,
            duration_ms: 0,
            output: summary,
        })?;
        Ok(restored)
    }

    /// The job's check passed, or the user gave a new instruction for it:
    /// start counting again.
    pub fn reset(&self, job: &str) -> Result<(), RecoveryError> {
        let conn = self.conn.lock().map_err(|_| RecoveryError::Locked)?;
        conn.execute(
            "DELETE FROM repair_jobs WHERE session_id=?1 AND job=?2",
            params![self.recorder.session_id(), job],
        )?;
        Ok(())
    }
}
