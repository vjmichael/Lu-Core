//! The command runner, the first basic tool: run any command the user's
//! instruction calls for, as an owned process tree, with a timeout, a cancel,
//! bounded output to the model, and background tasks that report back.
//!
//! For every command, in order:
//! 1. The origin gate decides (piece 4). A held or refused command never runs.
//! 2. The working folder is snapshotted (piece 2), unless the request opts out.
//! 3. The recorder journals it and writes `command_started` (piece 1).
//! 4. It runs in an owned process tree. Output is streamed to a file and kept
//!    in memory up to the record limit.
//! 5. When the main process ends, the rest of the tree is ended too, so
//!    nothing is left running. A timeout or a cancel ends the whole tree.
//! 6. The folder is compared with the snapshot and every change recorded.
//! 7. The full result is recorded. The model gets a trimmed copy with a note
//!    saying what was cut and where the full output is.
//!
//! Background tasks follow the same steps on their own thread. The host loop
//! calls [`CommandRunner::collect_finished`], which records each finished
//! task and returns a report for the model, so the model never polls.
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use rusqlite::{params, Connection};

use crate::ansf_operations::{CommandFinish, CommandStart, MAX_OUTPUT_BYTES};
use crate::origin::{Decision, EffectKind, OriginError, OriginGate, Proposal, Source};
use crate::process_tree::{kill_leftover_group, start_marker, ProcessTree};
use crate::recorder::{RecordError, Recorder};
use crate::snapshot::{diff, Change, SnapshotError, SnapshotInfo, SnapshotLimits, SnapshotStore};

/// Output given back to the model: the first and last parts of it.
pub const MODEL_OUTPUT_HEAD: usize = 8 * 1024;
pub const MODEL_OUTPUT_TAIL: usize = 24 * 1024;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    pub call_id: String,
    pub turn_id: String,
    /// One command line, run by the platform shell (PowerShell on Windows,
    /// `sh` elsewhere).
    pub command: String,
    pub cwd: PathBuf,
    pub timeout: Duration,
    /// Snapshot the working folder before and compare after. On by default;
    /// off for commands known not to write (saves hashing a large folder).
    pub snapshot: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub call_id: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub duration_ms: u64,
    /// What the model sees: whole if short, else head and tail with a note.
    pub output_for_model: String,
    pub output_bytes: u64,
    /// The full output on disk, when it was too large for the record.
    pub output_file: Option<PathBuf>,
    pub changes: Vec<Change>,
    pub snapshot: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// Held by the origin gate; the user decides.
    Held { held_id: i64, source: Source, notify_user: bool },
    /// A send traced to untrusted content; never runs.
    Refused { source: Source },
    Origin(OriginError),
    Record(RecordError),
    Snapshot(SnapshotError),
    Io(String),
    UnknownTask,
}
impl From<OriginError> for RunError {
    fn from(e: OriginError) -> Self {
        Self::Origin(e)
    }
}
impl From<RecordError> for RunError {
    fn from(e: RecordError) -> Self {
        Self::Record(e)
    }
}
impl From<SnapshotError> for RunError {
    fn from(e: SnapshotError) -> Self {
        Self::Snapshot(e)
    }
}
impl From<std::io::Error> for RunError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}
impl From<rusqlite::Error> for RunError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// Cancels a running command (its whole tree) from another thread.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);
impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    /// The flag itself, for code that checks it directly.
    pub fn flag(&self) -> &AtomicBool {
        &self.0
    }
}

/// Output collected from both pipes, in arrival order.
struct Sink {
    file: File,
    path: PathBuf,
    kept: Vec<u8>,
    total: u64,
    tail: Vec<u8>,
}
impl Sink {
    fn push(&mut self, bytes: &[u8]) {
        let _ = self.file.write_all(bytes);
        self.total += bytes.len() as u64;
        let room = MAX_OUTPUT_BYTES.saturating_sub(self.kept.len());
        self.kept.extend_from_slice(&bytes[..bytes.len().min(room)]);
        self.tail.extend_from_slice(bytes);
        if self.tail.len() > 2 * MODEL_OUTPUT_TAIL {
            self.tail.drain(..self.tail.len() - MODEL_OUTPUT_TAIL);
        }
    }
}

/// What a finished process left behind.
struct Outcome {
    exit_code: Option<i32>,
    timed_out: bool,
    cancelled: bool,
    duration: Duration,
    sink: Sink,
}

fn shell(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut c = Command::new("powershell.exe");
        c.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", command]);
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        c
    }
}

/// Text of possibly-cut UTF-8, with any broken character replaced.
fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Start the command in an owned tree and wait for it on this thread.
fn spawn_and_wait(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    cancel: &CancelToken,
    output_path: PathBuf,
    on_spawn: impl FnOnce(u32),
) -> Result<Outcome, RunError> {
    let mut cmd = shell(command);
    cmd.current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(dir) = output_path.parent() {
        fs::create_dir_all(dir)?;
    }
    let sink = Arc::new(Mutex::new(Sink {
        file: File::create(&output_path)?,
        path: output_path,
        kept: Vec::new(),
        total: 0,
        tail: Vec::new(),
    }));
    let started = Instant::now();
    let (mut child, tree) = ProcessTree::spawn(cmd)?;
    on_spawn(child.id());
    let readers: Vec<JoinHandle<()>> = [
        child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
        child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|mut pipe| {
        let sink = Arc::clone(&sink);
        thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if let Ok(mut s) = sink.lock() {
                    s.push(&buf[..n]);
                }
            }
        })
    })
    .collect();
    let (mut timed_out, mut cancelled) = (false, false);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if started.elapsed() >= timeout {
            timed_out = true;
        } else if cancel.is_cancelled() {
            cancelled = true;
        }
        if timed_out || cancelled {
            tree.kill();
            break child.wait().ok();
        }
        thread::sleep(Duration::from_millis(20));
    };
    // The main process ended: end anything it left running, which also
    // closes the pipes so the readers finish.
    tree.kill();
    drop(tree);
    for reader in readers {
        let _ = reader.join();
    }
    let duration = started.elapsed();
    let sink = Arc::try_unwrap(sink)
        .map_err(|_| RunError::Io("output still in use".into()))?
        .into_inner()
        .map_err(|_| RunError::Io("output lock poisoned".into()))?;
    let exit_code = if timed_out || cancelled { None } else { status.and_then(|s| s.code()) };
    Ok(Outcome { exit_code, timed_out, cancelled, duration, sink })
}

struct Background {
    request: RunRequest,
    snapshot: Option<SnapshotInfo>,
    cancel: CancelToken,
    handle: JoinHandle<Result<Outcome, RunError>>,
}

/// A finished background task, ready to hand to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskReport {
    pub call_id: String,
    pub command: String,
    pub result: Result<RunResult, String>,
}

pub struct CommandRunner<'a> {
    recorder: &'a Recorder<'a>,
    snapshots: &'a SnapshotStore,
    gate: &'a OriginGate,
    limits: SnapshotLimits,
    data_dir: PathBuf,
    processes: Mutex<Connection>,
    background: Mutex<HashMap<String, Background>>,
}

impl<'a> CommandRunner<'a> {
    /// `data_dir` is in Lu's own data folder; output files and the table of
    /// started processes live there.
    pub fn new(
        recorder: &'a Recorder<'a>,
        snapshots: &'a SnapshotStore,
        gate: &'a OriginGate,
        limits: SnapshotLimits,
        data_dir: &Path,
    ) -> Result<Self, RunError> {
        fs::create_dir_all(data_dir.join("output"))?;
        let conn = Connection::open(data_dir.join("processes.sqlite"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS processes(
               call_id TEXT PRIMARY KEY NOT NULL,
               session_id TEXT NOT NULL,
               pid INTEGER NOT NULL,
               start_marker INTEGER,
               background INTEGER NOT NULL CHECK(background IN (0,1)),
               state TEXT NOT NULL CHECK(state IN ('running','finished','lost')),
               started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );",
        )?;
        Ok(Self {
            recorder,
            snapshots,
            gate,
            limits,
            data_dir: data_dir.to_path_buf(),
            processes: Mutex::new(conn),
            background: Mutex::new(HashMap::new()),
        })
    }

    /// Steps 1-3: gate, snapshot, record. Returns the snapshot taken.
    fn prepare(&self, request: &RunRequest) -> Result<Option<SnapshotInfo>, RunError> {
        let proposal = Proposal {
            session_id: self.recorder.session_id().to_owned(),
            turn_id: request.turn_id.clone(),
            call_id: request.call_id.clone(),
            effect: EffectKind::Execute,
            text: request.command.clone(),
        };
        match self.gate.decide(self.recorder.book(), &proposal)? {
            Decision::Run => {}
            Decision::Hold { held_id, source, notify } => {
                return Err(RunError::Held { held_id, source, notify_user: notify })
            }
            Decision::Refuse { source } => return Err(RunError::Refused { source }),
        }
        let snapshot = if request.snapshot { Some(self.snapshots.take(&request.cwd, self.limits)?) } else { None };
        self.recorder.begin_command(&CommandStart {
            call_id: request.call_id.clone(),
            turn_id: request.turn_id.clone(),
            command: request.command.clone(),
            cwd: request.cwd.to_string_lossy().into_owned(),
        })?;
        Ok(snapshot)
    }

    fn output_path(&self, call_id: &str) -> PathBuf {
        let safe: String = call_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
        self.data_dir.join("output").join(format!("{safe}.log"))
    }

    fn note_spawn(&self, call_id: &str, pid: u32, background: bool) {
        if let Ok(conn) = self.processes.lock() {
            let _ = conn.execute(
                "INSERT OR REPLACE INTO processes(call_id,session_id,pid,start_marker,background,state)
                 VALUES(?1,?2,?3,?4,?5,'running')",
                params![
                    call_id,
                    self.recorder.session_id(),
                    pid as i64,
                    start_marker(pid).map(|m| m as i64),
                    background as i64
                ],
            );
        }
    }
    fn note_finished(&self, call_id: &str) {
        if let Ok(conn) = self.processes.lock() {
            let _ = conn.execute("UPDATE processes SET state='finished' WHERE call_id=?1", [call_id]);
        }
    }

    /// Steps 6-7: compare, record, trim for the model.
    fn finish(&self, request: &RunRequest, snapshot: Option<SnapshotInfo>, outcome: Result<Outcome, RunError>)
        -> Result<RunResult, RunError> {
        self.note_finished(&request.call_id);
        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                // It never started (spawn failed): FAILED, no effect.
                self.recorder.finish_command(&CommandFinish {
                    call_id: request.call_id.clone(),
                    exit_code: None,
                    timed_out: false,
                    cancelled: false,
                    duration_ms: 0,
                    output: format!("could not start: {e:?}"),
                })?;
                return Err(e);
            }
        };
        let sink = outcome.sink;
        let mut changes = Vec::new();
        if let Some(snap) = &snapshot {
            changes = diff(&self.snapshots.manifest(snap.id)?, &self.snapshots.scan(&request.cwd)?);
            self.recorder.record_changes(&request.call_id, snap, &changes)?;
        }
        let cut = sink.total > MAX_OUTPUT_BYTES as u64;
        let mut recorded = lossy(&sink.kept);
        if cut {
            recorded.push_str(&format!(
                "\n[Lu: output was {} bytes; the record keeps the first {}; full output: {}]",
                sink.total,
                MAX_OUTPUT_BYTES,
                sink.path.display()
            ));
        }
        self.recorder.finish_command(&CommandFinish {
            call_id: request.call_id.clone(),
            exit_code: outcome.exit_code,
            timed_out: outcome.timed_out,
            cancelled: outcome.cancelled,
            duration_ms: outcome.duration.as_millis() as u64,
            output: recorded,
        })?;
        let output_file = if cut {
            Some(sink.path.clone())
        } else {
            let _ = fs::remove_file(&sink.path);
            None
        };
        let for_model = if sink.total as usize <= MODEL_OUTPUT_HEAD + MODEL_OUTPUT_TAIL {
            lossy(&sink.kept)
        } else {
            let head = &sink.kept[..MODEL_OUTPUT_HEAD.min(sink.kept.len())];
            let tail = &sink.tail[sink.tail.len().saturating_sub(MODEL_OUTPUT_TAIL)..];
            let shown = (head.len() + tail.len()) as u64;
            format!(
                "{}\n[Lu: {} bytes of output cut from the middle; the full output is in the record{}]\n{}",
                lossy(head),
                sink.total - shown,
                if cut { format!(" and at {}", sink.path.display()) } else { String::new() },
                lossy(tail)
            )
        };
        Ok(RunResult {
            call_id: request.call_id.clone(),
            exit_code: outcome.exit_code,
            timed_out: outcome.timed_out,
            cancelled: outcome.cancelled,
            duration_ms: outcome.duration.as_millis() as u64,
            output_for_model: for_model,
            output_bytes: sink.total,
            output_file,
            changes,
            snapshot: snapshot.as_ref().map(SnapshotInfo::reference),
        })
    }

    /// Run a command and wait for it.
    pub fn run(&self, request: &RunRequest, cancel: &CancelToken) -> Result<RunResult, RunError> {
        let snapshot = self.prepare(request)?;
        let outcome = spawn_and_wait(
            &request.command,
            &request.cwd,
            request.timeout,
            cancel,
            self.output_path(&request.call_id),
            |pid| self.note_spawn(&request.call_id, pid, false),
        );
        self.finish(request, snapshot, outcome)
    }

    /// Start a command in the background and return at once. Its result is
    /// delivered by [`collect_finished`](Self::collect_finished).
    pub fn start_background(&self, request: RunRequest) -> Result<String, RunError> {
        let snapshot = self.prepare(&request)?;
        let cancel = CancelToken::default();
        let (pid_tx, pid_rx) = std::sync::mpsc::channel();
        let handle = {
            let (command, cwd, timeout, token, out) = (
                request.command.clone(),
                request.cwd.clone(),
                request.timeout,
                cancel.clone(),
                self.output_path(&request.call_id),
            );
            thread::spawn(move || {
                spawn_and_wait(&command, &cwd, timeout, &token, out, |pid| {
                    let _ = pid_tx.send(pid);
                })
            })
        };
        if let Ok(pid) = pid_rx.recv_timeout(Duration::from_secs(30)) {
            self.note_spawn(&request.call_id, pid, true);
        }
        let id = request.call_id.clone();
        self.background
            .lock()
            .map_err(|_| RunError::Io("lock poisoned".into()))?
            .insert(id.clone(), Background { request, snapshot, cancel, handle });
        Ok(id)
    }

    /// Call IDs of background tasks still running.
    pub fn running_tasks(&self) -> Vec<String> {
        self.background.lock().map(|m| m.keys().cloned().collect()).unwrap_or_default()
    }

    /// Stop a background task (ends its whole tree). Its result still
    /// arrives through `collect_finished`, marked cancelled.
    pub fn stop_task(&self, call_id: &str) -> Result<(), RunError> {
        let tasks = self.background.lock().map_err(|_| RunError::Io("lock poisoned".into()))?;
        tasks.get(call_id).ok_or(RunError::UnknownTask)?.cancel.cancel();
        Ok(())
    }

    /// Record every background task that has finished and return reports
    /// for the model. Called by the host loop, never by the model.
    pub fn collect_finished(&self) -> Vec<TaskReport> {
        let done: Vec<Background> = {
            let Ok(mut tasks) = self.background.lock() else { return Vec::new() };
            let ids: Vec<String> = tasks.iter().filter(|(_, t)| t.handle.is_finished()).map(|(k, _)| k.clone()).collect();
            ids.into_iter().filter_map(|id| tasks.remove(&id)).collect()
        };
        done.into_iter()
            .map(|task| {
                let outcome = task.handle.join().unwrap_or_else(|_| Err(RunError::Io("task thread panicked".into())));
                let result = self.finish(&task.request, task.snapshot, outcome).map_err(|e| format!("{e:?}"));
                TaskReport { call_id: task.request.call_id.clone(), command: task.request.command.clone(), result }
            })
            .collect()
    }

    /// Wait until no background task is running, collecting results.
    pub fn wait_all(&self, limit: Duration) -> Vec<TaskReport> {
        let until = Instant::now() + limit;
        let mut reports = Vec::new();
        while Instant::now() < until {
            reports.extend(self.collect_finished());
            if self.running_tasks().is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        reports
    }

    /// Startup, after the previous Lu process has stopped: end any process
    /// tree it left running (on Windows the Job Object already did), mark
    /// those commands lost, and let the recorder mark their effects
    /// UNCERTAIN. Returns the uncertain call IDs.
    pub fn reconcile_after_restart(&self) -> Result<Vec<String>, RunError> {
        let leftovers: Vec<(String, u32, Option<u64>)> = {
            let conn = self.processes.lock().map_err(|_| RunError::Io("lock poisoned".into()))?;
            let mut stmt = conn.prepare(
                "SELECT call_id,pid,start_marker FROM processes WHERE session_id=?1 AND state='running'",
            )?;
            let rows = stmt.query_map([self.recorder.session_id()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32, r.get::<_, Option<i64>>(2)?.map(|m| m as u64)))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for (call_id, pid, marker) in &leftovers {
            kill_leftover_group(*pid, *marker);
            let conn = self.processes.lock().map_err(|_| RunError::Io("lock poisoned".into()))?;
            conn.execute("UPDATE processes SET state='lost' WHERE call_id=?1", [call_id])?;
        }
        Ok(self.recorder.recover_after_restart()?)
    }
}
