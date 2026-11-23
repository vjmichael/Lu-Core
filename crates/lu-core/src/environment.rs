//! Environment facts: what is installed on this machine, where, and which
//! version, so a fresh chat never claims a tool is missing when the user
//! already supplied it.
//!
//! - **Machine-wide.** Facts live in one database for the whole machine, not
//!   in a conversation, so every chat (and every model) sees them.
//! - **Discovered by Lu.** Probes find known tools on PATH and ask each for
//!   its version. This is deterministic host work: no model, no tokens.
//! - **Reported facts are unverified** until Lu checks them. A path the user
//!   gave, or one the model found, is recorded with its source (for example
//!   the exact chat entry) and checked before anything relies on it.
//! - **Facts expire.** A fact older than the caller's limit is re-checked
//!   before use: the file must still exist with the same size and time, and
//!   a tool's version is asked again.
//! - **Nothing is silently overwritten.** Every observation is a new row. A
//!   later observation that disagrees marks the fact contradicted, naming
//!   what it was before.
//! - **Facts are context, never authority.** Knowing a tool exists does not
//!   permit running it; the user's instruction does (see `origin`).
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

use rusqlite::{params, Connection, OptionalExtension};

use crate::memory::{EnvFact, ItemStatus};
use crate::process_tree::ProcessTree;

/// Tools Lu looks for, with the argument that prints a version.
/// Chosen for building, packaging, installers, Windows PE and QEMU setups.
pub const KNOWN_TOOLS: &[(&str, &str)] = &[
    ("git", "--version"),
    ("cargo", "--version"),
    ("rustc", "--version"),
    ("node", "--version"),
    ("npm", "--version"),
    ("python", "--version"),
    ("py", "--version"),
    ("pwsh", "-Version"),
    ("cmake", "--version"),
    ("clang", "--version"),
    ("gcc", "--version"),
    ("protoc", "--version"),
    ("gh", "--version"),
    ("makensis", "-VERSION"),
    ("dotnet", "--version"),
    ("winget", "--version"),
    ("choco", "--version"),
    ("scoop", "--version"),
    ("wsl", "--version"),
    ("qemu-system-x86_64", "--version"),
    ("qemu-img", "--version"),
    ("ffmpeg", "-version"),
    ("go", "version"),
    ("java", "-version"),
    ("dism", "/?"),
    ("oscdimg", "/?"),
];
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactStatus {
    /// Checked by Lu at `checked_at`.
    Verified,
    /// Reported (by the user or the model) or too old; check before use.
    Unverified,
    /// A later check disagreed with what was recorded before.
    Contradicted,
    /// Lu looked and it is not there.
    Missing,
}
impl FactStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Unverified => "unverified",
            Self::Contradicted => "contradicted",
            Self::Missing => "missing",
        }
    }
    fn parse(v: &str) -> Option<Self> {
        match v {
            "verified" => Some(Self::Verified),
            "unverified" => Some(Self::Unverified),
            "contradicted" => Some(Self::Contradicted),
            "missing" => Some(Self::Missing),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    /// Stable name, e.g. `tool:git` or `installer:nsis`.
    pub key: String,
    /// What it is, in a few words (a version line, a description).
    pub value: String,
    pub path: Option<PathBuf>,
    /// How Lu learned it: `probe`, a chat source ID, `op:<seq>`, ...
    pub source: String,
    pub status: FactStatus,
    /// UTC time of the last check, when it was checked.
    pub checked_at: Option<String>,
    /// Why the status changed, for contradicted and missing facts.
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvError {
    Database(String),
    Invalid(String),
    Locked,
}
impl From<rusqlite::Error> for EnvError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Database(e.to_string())
    }
}

/// Find an executable on PATH, the way a shell would.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.to_ascii_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &extensions {
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Run `program arg` in an owned process tree and return its first line
/// of output that mentions a version-like number, or the first line.
fn version_line(program: &Path, arg: &str) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.arg(arg).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let (mut child, tree) = ProcessTree::spawn(cmd).ok()?;
    let pipes = [
        child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
        child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
    ];
    let readers: Vec<_> = pipes
        .into_iter()
        .flatten()
        .map(|mut p| {
            thread::spawn(move || {
                let mut out = Vec::new();
                let _ = p.by_ref().take(64 * 1024).read_to_end(&mut out);
                out
            })
        })
        .collect();
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < PROBE_TIMEOUT => thread::sleep(Duration::from_millis(20)),
            _ => {
                tree.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    tree.kill();
    drop(tree);
    let text: String = readers
        .into_iter()
        .filter_map(|r| r.join().ok())
        .map(|b| decode_output(&b))
        .collect::<Vec<_>>()
        .join("\n");
    pick_version(&text, name_of(program))
}

fn name_of(program: &Path) -> String {
    program.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// Tool output as text. Some Windows tools (wsl) print UTF-16.
fn decode_output(bytes: &[u8]) -> String {
    let utf16 = bytes.starts_with(&[0xFF, 0xFE])
        || (bytes.len() >= 4 && bytes.iter().skip(1).step_by(2).filter(|b| **b == 0).count() * 2 >= bytes.len() / 2);
    if utf16 {
        let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units).trim_start_matches('\u{feff}').to_owned()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// The version line: one naming the tool and a version number, else any
/// line with a version number, else the first line. A tool that only
/// printed an error is reported as found with its version unavailable.
fn pick_version(text: &str, name: String) -> Option<String> {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let has_version = |l: &&&str| {
        l.as_bytes().windows(3).any(|w| w[0].is_ascii_digit() && w[1] == b'.' && w[2].is_ascii_digit())
    };
    let chosen = lines
        .iter()
        .find(|l| l.to_lowercase().starts_with(&format!("{name} version")))
        .or_else(|| lines.iter().filter(has_version).find(|l| l.to_lowercase().contains(&name)))
        .or_else(|| lines.iter().find(has_version))
        .or(lines.first())?;
    if !has_version(&chosen) && chosen.to_lowercase().starts_with("error") {
        return Some(format!("{name} found; version unavailable ({})", chosen.chars().take(80).collect::<String>()));
    }
    Some(chosen.chars().take(200).collect())
}

/// Size and modification time, as a cheap identity for an existing file.
fn identity(path: &Path) -> Option<(i64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    Some((meta.len() as i64, modified))
}

pub struct EnvironmentStore {
    conn: Mutex<Connection>,
}

const FACT_COLUMNS: &str = "key,value,path,source,status,checked_at,note";

fn read_fact(r: &rusqlite::Row<'_>) -> rusqlite::Result<Fact> {
    let status: String = r.get(4)?;
    Ok(Fact {
        key: r.get(0)?,
        value: r.get(1)?,
        path: r.get::<_, Option<String>>(2)?.map(PathBuf::from),
        source: r.get(3)?,
        status: FactStatus::parse(&status).ok_or(rusqlite::Error::InvalidQuery)?,
        checked_at: r.get(5)?,
        note: r.get(6)?,
    })
}

impl EnvironmentStore {
    pub fn open(path: &Path) -> Result<Self, EnvError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS env_observations(
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               key TEXT NOT NULL,
               value TEXT NOT NULL,
               path TEXT,
               size INTEGER,
               modified INTEGER,
               source TEXT NOT NULL,
               status TEXT NOT NULL CHECK(status IN ('verified','unverified','contradicted','missing')),
               checked_at TEXT,
               note TEXT,
               observed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE INDEX IF NOT EXISTS env_by_key ON env_observations(key,id);
             CREATE TRIGGER IF NOT EXISTS env_no_update BEFORE UPDATE ON env_observations
               BEGIN SELECT RAISE(ABORT,'environment observations are immutable'); END;
             CREATE TRIGGER IF NOT EXISTS env_no_delete BEFORE DELETE ON env_observations
               BEGIN SELECT RAISE(ABORT,'environment observations are immutable'); END;",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    #[allow(clippy::too_many_arguments)]
    fn observe(
        &self,
        key: &str,
        value: &str,
        path: Option<&Path>,
        source: &str,
        status: FactStatus,
        checked: bool,
        note: Option<String>,
    ) -> Result<Fact, EnvError> {
        if key.is_empty() || key.len() > 200 || source.is_empty() {
            return Err(EnvError::Invalid("bad key or source".into()));
        }
        let id = path.and_then(identity);
        let conn = self.conn.lock().map_err(|_| EnvError::Locked)?;
        conn.execute(
            "INSERT INTO env_observations(key,value,path,size,modified,source,status,checked_at,note)
             VALUES(?1,?2,?3,?4,?5,?6,?7,CASE WHEN ?8 THEN strftime('%Y-%m-%dT%H:%M:%fZ','now') END,?9)",
            params![
                key,
                value,
                path.map(|p| p.to_string_lossy().into_owned()),
                id.map(|i| i.0),
                id.map(|i| i.1),
                source,
                status.as_str(),
                checked,
                note
            ],
        )?;
        let rowid = conn.last_insert_rowid();
        Ok(conn.query_row(&format!("SELECT {FACT_COLUMNS} FROM env_observations WHERE id=?1"), [rowid], read_fact)?)
    }

    /// The current state of one fact (its latest observation).
    pub fn get(&self, key: &str) -> Result<Option<Fact>, EnvError> {
        let conn = self.conn.lock().map_err(|_| EnvError::Locked)?;
        Ok(conn
            .query_row(
                &format!("SELECT {FACT_COLUMNS} FROM env_observations WHERE key=?1 ORDER BY id DESC LIMIT 1"),
                [key],
                read_fact,
            )
            .optional()?)
    }

    /// Every fact's current state, by key.
    pub fn all(&self) -> Result<Vec<Fact>, EnvError> {
        let conn = self.conn.lock().map_err(|_| EnvError::Locked)?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {FACT_COLUMNS} FROM env_observations o
             WHERE id=(SELECT MAX(id) FROM env_observations WHERE key=o.key) ORDER BY key"
        ))?;
        let rows = stmt.query_map([], read_fact)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Every observation of one fact, oldest first.
    pub fn history(&self, key: &str) -> Result<Vec<Fact>, EnvError> {
        let conn = self.conn.lock().map_err(|_| EnvError::Locked)?;
        let mut stmt = conn.prepare(&format!("SELECT {FACT_COLUMNS} FROM env_observations WHERE key=?1 ORDER BY id"))?;
        let rows = stmt.query_map([key], read_fact)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Record a checked result, marking a disagreement with the previous
    /// observation as a contradiction.
    fn checked(&self, key: &str, value: &str, path: Option<&Path>, source: &str, found: bool) -> Result<Fact, EnvError> {
        let previous = self.get(key)?;
        let (status, note) = match (&previous, found) {
            (_, false) => match &previous {
                Some(p) if p.status != FactStatus::Missing => (
                    FactStatus::Contradicted,
                    Some(format!("was {} at {}; no longer found", p.value, p.path.as_deref().map_or("?".into(), |x| x.display().to_string()))),
                ),
                _ => (FactStatus::Missing, None),
            },
            (Some(p), true) if p.status != FactStatus::Missing && (p.value != value || p.path.as_deref() != path) => (
                FactStatus::Verified,
                Some(format!("changed: was {} at {}", p.value, p.path.as_deref().map_or("?".into(), |x| x.display().to_string()))),
            ),
            _ => (FactStatus::Verified, None),
        };
        self.observe(key, value, path, source, status, true, note)
    }

    /// Look for one tool on PATH and record what Lu finds.
    pub fn probe_tool(&self, name: &str, version_arg: &str) -> Result<Fact, EnvError> {
        let key = format!("tool:{name}");
        match find_on_path(name) {
            Some(path) => {
                let value = version_line(&path, version_arg).unwrap_or_else(|| format!("{name} (version unknown)"));
                self.checked(&key, &value, Some(&path), "probe", true)
            }
            None => self.checked(&key, &format!("{name} not on PATH"), None, "probe", false),
        }
    }

    /// Probe every known tool. Runs on the CPU; costs no model tokens.
    pub fn probe_known_tools(&self) -> Result<Vec<Fact>, EnvError> {
        KNOWN_TOOLS.iter().map(|(name, arg)| self.probe_tool(name, arg)).collect()
    }

    /// A fact the user stated or the model found, e.g. "the installer is at
    /// C:/tools/nsis/makensis.exe". `source` names where it was said (a chat
    /// source ID). It stays unverified until [`verify`](Self::verify).
    pub fn record_reported(&self, key: &str, value: &str, path: Option<&Path>, source: &str) -> Result<Fact, EnvError> {
        self.observe(key, value, path, source, FactStatus::Unverified, false, None)
    }

    /// Check a fact against the machine now. A path must still exist with the
    /// same size and modification time; a tool is asked its version again.
    pub fn verify(&self, key: &str) -> Result<Fact, EnvError> {
        let current = self.get(key)?.ok_or(EnvError::Invalid(format!("no fact {key}")))?;
        if let Some(name) = key.strip_prefix("tool:") {
            let arg = KNOWN_TOOLS.iter().find(|(n, _)| *n == name).map_or("--version", |(_, a)| *a);
            return self.probe_tool(name, arg);
        }
        let Some(path) = current.path.clone() else {
            // A fact with no path cannot be checked by Lu; it stays as reported.
            return Ok(current);
        };
        let recorded = {
            let conn = self.conn.lock().map_err(|_| EnvError::Locked)?;
            conn.query_row(
                "SELECT size,modified FROM env_observations WHERE key=?1 ORDER BY id DESC LIMIT 1",
                [key],
                |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?)),
            )?
        };
        match identity(&path) {
            None => self.checked(key, &current.value, Some(&path), &current.source, false),
            Some((size, modified)) => {
                let same = recorded == (Some(size), Some(modified));
                let value = if same || current.status == FactStatus::Unverified {
                    current.value.clone()
                } else {
                    format!("{} (file changed since last check)", current.value)
                };
                self.checked(key, &value, Some(&path), &current.source, true)
            }
        }
    }

    /// Facts ready for use: anything not checked within `max_age` is checked
    /// again first. Missing tools are left out unless `include_missing`.
    pub fn facts_for_use(&self, max_age: Duration, include_missing: bool) -> Result<Vec<Fact>, EnvError> {
        let stale: Vec<String> = {
            let conn = self.conn.lock().map_err(|_| EnvError::Locked)?;
            let mut stmt = conn.prepare(
                "SELECT key FROM env_observations o
                 WHERE id=(SELECT MAX(id) FROM env_observations WHERE key=o.key)
                   AND (checked_at IS NULL
                        OR (julianday('now')-julianday(checked_at))*86400.0 >= ?1)",
            )?;
            let rows = stmt.query_map([max_age.as_secs_f64()], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for key in stale {
            self.verify(&key)?;
        }
        Ok(self.all()?.into_iter().filter(|f| include_missing || f.status != FactStatus::Missing).collect())
    }

    /// Facts for the memory block, checked first when older than `max_age`.
    pub fn memory_facts(&self, max_age: Duration) -> Result<Vec<EnvFact>, EnvError> {
        Ok(self
            .facts_for_use(max_age, false)?
            .into_iter()
            .map(|f| {
                let status = match f.status {
                    FactStatus::Verified => ItemStatus::Verified,
                    FactStatus::Contradicted => ItemStatus::Contradicted,
                    FactStatus::Unverified | FactStatus::Missing => ItemStatus::Unverified,
                };
                let place = f.path.as_deref().map(|p| format!(" at {}", p.display())).unwrap_or_default();
                let when = f.checked_at.as_deref().map(|t| format!(" (checked {t})")).unwrap_or_default();
                let note = f.note.as_deref().map(|n| format!("; {n}")).unwrap_or_default();
                EnvFact { source: format!("env:{}", f.key), status, text: format!("{}{place}{when}{note}", f.value) }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn version_lines_from_real_tool_output() {
        // A git build of ffmpeg has no dotted version on its first line.
        let ffmpeg = "ffmpeg version 2026-06-26-git-d66e84695b Copyright (c) 2000-2026\nbuilt with gcc 16.1.0 (Rev2)";
        assert_eq!(pick_version(ffmpeg, "ffmpeg".into()).unwrap(), "ffmpeg version 2026-06-26-git-d66e84695b Copyright (c) 2000-2026");
        let wsl: Vec<u8> = "WSL version: 2.0.5.0\r\nKernel version: 5.15\r\n".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(pick_version(&decode_output(&wsl), "wsl".into()).unwrap(), "WSL version: 2.0.5.0");
        assert_eq!(pick_version("\nError: 740\n\nElevated permissions are required", "dism".into()).unwrap(), "dism found; version unavailable (Error: 740)");
        assert_eq!(pick_version("v24.18.0", "node".into()).unwrap(), "v24.18.0");
    }
}
