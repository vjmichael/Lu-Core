//! Snapshot store and before/after manifest for the working scope.
//!
//! Before a command, Lu records the scope: every file's path, size and
//! SHA-256, plus a restorable copy where it can. After the command, a fresh
//! manifest is compared with it, every change is reported, and a changed file
//! that fails its check can be restored from the snapshot.
//!
//! Two kinds of snapshot:
//! - **Git**, when the scope is inside a Git repository. Tracked files are
//!   preserved by a commit object from `git stash create` (the working tree
//!   as it is, staged or not), pinned under `refs/lu/snapshots/` so Git's
//!   garbage collection keeps it. The working tree, index and branch are not
//!   touched. Untracked files that are not ignored are copied as blobs.
//!   Ignored files (build output and the like) are not covered.
//! - **Copy**, otherwise. Files are copied into a content-addressed blob
//!   store, so unchanged content is stored once across snapshots.
//!
//! Copying is bounded by [`SnapshotLimits`]. Past a limit the file is still
//! in the manifest (so a change is detected) but cannot be restored, and the
//! snapshot says how many such files there are so the user can be told.
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

/// Copy limits. Defaults approved by the owner on 2026-11-23.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotLimits {
    /// Most new bytes one snapshot may copy into the blob store.
    pub max_total_copy_bytes: u64,
    /// Largest single file that is copied.
    pub max_file_copy_bytes: u64,
}
impl Default for SnapshotLimits {
    fn default() -> Self {
        Self { max_total_copy_bytes: 1024 * 1024 * 1024, max_file_copy_bytes: 256 * 1024 * 1024 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotKind {
    Copy,
    Git,
}

/// Where the original content of one file can be recovered from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restorable {
    Blob,
    Git,
    /// Over a copy limit: a change is detected but cannot be undone.
    No,
}
impl Restorable {
    fn as_str(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Git => "git",
            Self::No => "none",
        }
    }
    fn parse(v: &str) -> Option<Self> {
        match v {
            "blob" => Some(Self::Blob),
            "git" => Some(Self::Git),
            "none" => Some(Self::No),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileState {
    pub size: u64,
    /// Lowercase hex SHA-256 of the content.
    pub sha256: String,
}

/// Files of a scope by relative path ('/'-separated).
pub type Manifest = BTreeMap<String, FileState>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub id: i64,
    pub scope: PathBuf,
    pub kind: SnapshotKind,
    pub git_commit: Option<String>,
    pub files: usize,
    pub copied_bytes: u64,
    /// Files in the manifest that cannot be restored (over a copy limit).
    pub not_restorable: usize,
    /// Entries skipped entirely: symbolic links and non-UTF-8 names.
    pub skipped: usize,
}
impl SnapshotInfo {
    /// The reference recorded with a file change, e.g. `snap:12`.
    pub fn reference(&self) -> String {
        format!("snap:{}", self.id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added { path: String, after: FileState },
    Removed { path: String, before: FileState },
    Modified { path: String, before: FileState, after: FileState },
}
impl Change {
    pub fn path(&self) -> &str {
        match self {
            Self::Added { path, .. } | Self::Removed { path, .. } | Self::Modified { path, .. } => path,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    Io(String),
    Database(String),
    Git(String),
    InvalidPath,
    UnknownSnapshot,
    /// The file is not in the snapshot, or it was over a copy limit.
    NotRestorable,
    /// The restored bytes did not match the recorded hash.
    IntegrityMismatch,
    Locked,
}
impl From<std::io::Error> for SnapshotError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}
impl From<rusqlite::Error> for SnapshotError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Database(e.to_string())
    }
}

/// Compare two manifests. Changes are sorted by path.
pub fn diff(before: &Manifest, after: &Manifest) -> Vec<Change> {
    let mut changes = Vec::new();
    for (path, old) in before {
        match after.get(path) {
            None => changes.push(Change::Removed { path: path.clone(), before: old.clone() }),
            Some(new) if new != old => changes.push(Change::Modified {
                path: path.clone(),
                before: old.clone(),
                after: new.clone(),
            }),
            Some(_) => {}
        }
    }
    for (path, new) in after {
        if !before.contains_key(path) {
            changes.push(Change::Added { path: path.clone(), after: new.clone() });
        }
    }
    changes.sort_by(|a, b| a.path().cmp(b.path()));
    changes
}

/// A Git repository that contains the scope.
struct GitScope {
    /// Path of the scope inside the repository, '/'-terminated or empty.
    prefix: String,
}

fn git(scope: &Path, args: &[&str]) -> Result<Vec<u8>, SnapshotError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(scope)
        .args(args)
        .output()
        .map_err(|e| SnapshotError::Git(e.to_string()))?;
    if !out.status.success() {
        return Err(SnapshotError::Git(String::from_utf8_lossy(&out.stderr).trim().to_owned()));
    }
    Ok(out.stdout)
}
fn git_line(scope: &Path, args: &[&str]) -> Result<String, SnapshotError> {
    Ok(String::from_utf8_lossy(&git(scope, args)?).trim().to_owned())
}
/// Some(..) when the scope is inside a repository that has a commit.
fn git_scope(scope: &Path) -> Option<GitScope> {
    if git_line(scope, &["rev-parse", "--is-inside-work-tree"]).ok()? != "true" {
        return None;
    }
    git_line(scope, &["rev-parse", "--verify", "HEAD"]).ok()?;
    let prefix = git_line(scope, &["rev-parse", "--show-prefix"]).ok()?;
    Some(GitScope { prefix })
}
/// NUL-separated path list from git, relative to the scope.
fn git_paths(scope: &Path, args: &[&str]) -> Result<Vec<String>, SnapshotError> {
    Ok(git(scope, args)?
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .filter_map(|p| std::str::from_utf8(p).ok().map(str::to_owned))
        .collect())
}

/// A relative path that stays inside the scope.
fn safe_relative(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('/')
        && !rel.contains('\\')
        && !rel.contains(':')
        && !rel.contains(char::from(0))
        && rel.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
}

/// Every regular file of a directory tree except `.git` and `exclude`.
fn walk(scope: &Path, exclude: Option<&Path>, out: &mut Vec<String>, skipped: &mut usize)
    -> Result<(), SnapshotError> {
    let mut stack = vec![PathBuf::new()];
    while let Some(rel_dir) = stack.pop() {
        for entry in fs::read_dir(scope.join(&rel_dir))? {
            let entry = entry?;
            let rel = rel_dir.join(entry.file_name());
            let full = scope.join(&rel);
            if exclude.is_some_and(|x| full.starts_with(x)) {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                *skipped += 1;
            } else if kind.is_dir() {
                if entry.file_name() != ".git" {
                    stack.push(rel);
                }
            } else if kind.is_file() {
                let parts: Option<Vec<&str>> = rel.iter().map(|c| c.to_str()).collect();
                match parts {
                    Some(parts) => out.push(parts.join("/")),
                    None => *skipped += 1,
                }
            }
        }
    }
    Ok(())
}

/// Stream a file through SHA-256, copying it to `copy_to` at the same time.
fn hash_file(path: &Path, mut copy_to: Option<&mut File>) -> Result<FileState, SnapshotError> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        if let Some(out) = copy_to.as_deref_mut() {
            out.write_all(&buf[..n])?;
        }
        size += n as u64;
    }
    Ok(FileState { size, sha256: format!("{:x}", hash.finalize()) })
}

/// `fs::canonicalize` on Windows returns a verbatim `\\?\` path, which Git
/// and some tools reject. Drop that prefix for ordinary drive paths.
fn canonical(path: &Path) -> Result<PathBuf, SnapshotError> {
    let full = fs::canonicalize(path)?;
    let text = full.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => Ok(PathBuf::from(rest)),
        _ => Ok(full),
    }
}

fn unique_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos())
    )
}

fn matches(bytes: &[u8], state: &FileState) -> bool {
    bytes.len() as u64 == state.size && format!("{:x}", Sha256::digest(bytes)) == state.sha256
}

/// Write bytes to `target` through a temporary file in the same folder and
/// an atomic rename, so the target is never half-written.
fn atomic_write(target: &Path, bytes: &[u8]) -> Result<(), SnapshotError> {
    let dir = target.parent().ok_or(SnapshotError::InvalidPath)?;
    fs::create_dir_all(dir)?;
    let temp = dir.join(format!(".lu-restore-{}", unique_suffix()));
    let result = (|| {
        let mut f = File::create(&temp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&temp, target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    Ok(result?)
}

pub struct SnapshotStore {
    root: PathBuf,
    conn: Mutex<Connection>,
}

impl SnapshotStore {
    /// Open or create a store in its own folder (Lu's data folder, never the
    /// user's project).
    pub fn open(root: &Path) -> Result<Self, SnapshotError> {
        fs::create_dir_all(root.join("blobs"))?;
        let root = canonical(root)?;
        let conn = Connection::open(root.join("snapshots.sqlite"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS snapshots(
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               scope TEXT NOT NULL,
               kind TEXT NOT NULL CHECK(kind IN ('copy','git')),
               git_commit TEXT,
               git_prefix TEXT,
               copied_bytes INTEGER NOT NULL,
               skipped INTEGER NOT NULL,
               complete INTEGER NOT NULL DEFAULT 0 CHECK(complete IN (0,1)),
               created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE TABLE IF NOT EXISTS snapshot_files(
               snapshot_id INTEGER NOT NULL REFERENCES snapshots(id),
               path TEXT NOT NULL,
               size INTEGER NOT NULL CHECK(size>=0),
               sha256 TEXT NOT NULL,
               restorable TEXT NOT NULL CHECK(restorable IN ('blob','git','none')),
               PRIMARY KEY(snapshot_id,path)
             );
             CREATE TABLE IF NOT EXISTS trash(
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               original_path TEXT NOT NULL,
               stored_name TEXT NOT NULL UNIQUE,
               size INTEGER NOT NULL,
               sha256 TEXT NOT NULL,
               trashed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
               restored INTEGER NOT NULL DEFAULT 0 CHECK(restored IN (0,1))
             );",
        )?;
        Ok(Self { root, conn: Mutex::new(conn) })
    }

    fn blob_path(&self, sha: &str) -> PathBuf {
        self.root.join("blobs").join(&sha[..2]).join(sha)
    }

    /// Record the scope before a command, keeping restorable copies.
    pub fn take(&self, scope: &Path, limits: SnapshotLimits) -> Result<SnapshotInfo, SnapshotError> {
        let scope = canonical(scope)?;
        let git_scope = git_scope(&scope);
        let mut skipped = 0;
        // (path, restorable from git?)
        let mut files: Vec<(String, bool)> = Vec::new();
        let mut git_commit = None;
        if git_scope.is_some() {
            let stash = git_line(&scope, &["stash", "create"])?;
            let commit = if stash.is_empty() {
                git_line(&scope, &["rev-parse", "HEAD"])?
            } else {
                stash
            };
            let tracked = git_paths(&scope, &["ls-files", "-z", "--cached"])?;
            let untracked =
                git_paths(&scope, &["ls-files", "-z", "--others", "--exclude-standard"])?;
            files.extend(tracked.into_iter().map(|p| (p, true)));
            files.extend(untracked.into_iter().map(|p| (p, false)));
            // A tracked file deleted in the working tree is not there to hash.
            files.retain(|(p, _)| scope.join(p).is_file());
            git_commit = Some(commit);
        } else {
            let mut paths = Vec::new();
            walk(&scope, Some(&self.root), &mut paths, &mut skipped)?;
            files.extend(paths.into_iter().map(|p| (p, false)));
        }
        files.sort();
        files.dedup_by(|a, b| a.0 == b.0);
        self.record(scope, files, git_scope.map(|g| g.prefix), git_commit, skipped, limits)
    }

    /// Snapshot one file before Lu writes, moves or deletes it. Always a copy
    /// (no Git), so it is cheap and independent of the folder around it.
    pub fn take_file(&self, path: &Path, limits: SnapshotLimits) -> Result<SnapshotInfo, SnapshotError> {
        let path = canonical(path)?;
        if !path.is_file() {
            return Err(SnapshotError::InvalidPath);
        }
        let scope = path.parent().ok_or(SnapshotError::InvalidPath)?.to_path_buf();
        let name = path.file_name().and_then(|n| n.to_str()).ok_or(SnapshotError::InvalidPath)?;
        self.record(scope, vec![(name.to_owned(), false)], None, None, 0, limits)
    }

    fn record(
        &self,
        scope: PathBuf,
        files: Vec<(String, bool)>,
        git_prefix: Option<String>,
        git_commit: Option<String>,
        mut skipped: usize,
        limits: SnapshotLimits,
    ) -> Result<SnapshotInfo, SnapshotError> {
        let mut conn = self.conn.lock().map_err(|_| SnapshotError::Locked)?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO snapshots(scope,kind,git_commit,git_prefix,copied_bytes,skipped)
             VALUES(?1,?2,?3,?4,0,?5)",
            params![
                scope.to_string_lossy(),
                if git_commit.is_some() { "git" } else { "copy" },
                git_commit,
                git_prefix,
                skipped as i64
            ],
        )?;
        let id = tx.last_insert_rowid();
        let mut copied = 0u64;
        let mut not_restorable = 0usize;
        for (rel, in_git) in &files {
            if !safe_relative(rel) {
                skipped += 1;
                continue;
            }
            let full = scope.join(rel);
            let size = fs::metadata(&full)?.len();
            let (state, restorable) = if *in_git {
                (hash_file(&full, None)?, Restorable::Git)
            } else if size <= limits.max_file_copy_bytes
                && copied + size <= limits.max_total_copy_bytes
            {
                let temp = self.root.join("blobs").join(format!(".incoming-{}", unique_suffix()));
                let mut out = File::create(&temp)?;
                let state = hash_file(&full, Some(&mut out));
                out.sync_all()?;
                drop(out);
                let state = match state {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = fs::remove_file(&temp);
                        return Err(e);
                    }
                };
                let blob = self.blob_path(&state.sha256);
                if blob.exists() {
                    fs::remove_file(&temp)?;
                } else {
                    fs::create_dir_all(blob.parent().ok_or(SnapshotError::InvalidPath)?)?;
                    fs::rename(&temp, &blob)?;
                    copied += state.size;
                }
                (state, Restorable::Blob)
            } else {
                not_restorable += 1;
                (hash_file(&full, None)?, Restorable::No)
            };
            tx.execute(
                "INSERT INTO snapshot_files(snapshot_id,path,size,sha256,restorable)
                 VALUES(?1,?2,?3,?4,?5)",
                params![id, rel, state.size as i64, state.sha256, restorable.as_str()],
            )?;
        }
        if let Some(commit) = &git_commit {
            // Pin the commit so `git gc` cannot drop the snapshot.
            git(&scope, &["update-ref", &format!("refs/lu/snapshots/{id}"), commit])?;
        }
        tx.execute(
            "UPDATE snapshots SET copied_bytes=?2,skipped=?3,complete=1 WHERE id=?1",
            params![id, copied as i64, skipped as i64],
        )?;
        tx.commit()?;
        Ok(SnapshotInfo {
            id,
            scope,
            kind: if git_commit.is_some() { SnapshotKind::Git } else { SnapshotKind::Copy },
            git_commit,
            files: files.len(),
            copied_bytes: copied,
            not_restorable,
            skipped,
        })
    }

    /// The scope's manifest now, with no copying. In a Git repository it
    /// covers the same files a snapshot would (tracked plus untracked files
    /// that are not ignored).
    pub fn scan(&self, scope: &Path) -> Result<Manifest, SnapshotError> {
        let scope = canonical(scope)?;
        let mut paths = Vec::new();
        if git_scope(&scope).is_some() {
            paths.extend(git_paths(&scope, &["ls-files", "-z", "--cached"])?);
            paths.extend(git_paths(&scope, &["ls-files", "-z", "--others", "--exclude-standard"])?);
        } else {
            let mut skipped = 0;
            walk(&scope, Some(&self.root), &mut paths, &mut skipped)?;
        }
        let mut manifest = Manifest::new();
        for rel in paths {
            let full = scope.join(&rel);
            if safe_relative(&rel) && full.is_file() {
                manifest.insert(rel, hash_file(&full, None)?);
            }
        }
        Ok(manifest)
    }

    fn info_row(&self, id: i64) -> Result<(PathBuf, Option<String>, Option<String>), SnapshotError> {
        let conn = self.conn.lock().map_err(|_| SnapshotError::Locked)?;
        conn.query_row(
            "SELECT scope,git_commit,git_prefix FROM snapshots WHERE id=?1 AND complete=1",
            [id],
            |r| Ok((PathBuf::from(r.get::<_, String>(0)?), r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .ok_or(SnapshotError::UnknownSnapshot)
    }

    /// The manifest recorded by a snapshot.
    pub fn manifest(&self, id: i64) -> Result<Manifest, SnapshotError> {
        self.info_row(id)?;
        let conn = self.conn.lock().map_err(|_| SnapshotError::Locked)?;
        let mut stmt =
            conn.prepare("SELECT path,size,sha256 FROM snapshot_files WHERE snapshot_id=?1")?;
        let rows = stmt.query_map([id], |r| {
            Ok((r.get::<_, String>(0)?, FileState { size: r.get::<_, i64>(1)? as u64, sha256: r.get(2)? }))
        })?;
        Ok(rows.collect::<Result<Manifest, _>>()?)
    }

    pub fn restorable(&self, id: i64, path: &str) -> Result<Restorable, SnapshotError> {
        self.info_row(id)?;
        let conn = self.conn.lock().map_err(|_| SnapshotError::Locked)?;
        let value: Option<String> = conn
            .query_row(
                "SELECT restorable FROM snapshot_files WHERE snapshot_id=?1 AND path=?2",
                params![id, path],
                |r| r.get(0),
            )
            .optional()?;
        match value {
            None => Ok(Restorable::No),
            Some(v) => Restorable::parse(&v).ok_or(SnapshotError::Database("bad restorable".into())),
        }
    }

    /// Put one file back exactly as the snapshot recorded it. The content is
    /// checked against the recorded hash before it replaces anything, and the
    /// replacement is atomic.
    pub fn restore(&self, id: i64, path: &str) -> Result<(), SnapshotError> {
        if !safe_relative(path) {
            return Err(SnapshotError::InvalidPath);
        }
        let (scope, commit, prefix) = self.info_row(id)?;
        let (state, restorable) = {
            let conn = self.conn.lock().map_err(|_| SnapshotError::Locked)?;
            conn.query_row(
                "SELECT size,sha256,restorable FROM snapshot_files WHERE snapshot_id=?1 AND path=?2",
                params![id, path],
                |r| Ok((FileState { size: r.get::<_, i64>(0)? as u64, sha256: r.get(1)? }, r.get::<_, String>(2)?)),
            )
            .optional()?
            .ok_or(SnapshotError::NotRestorable)?
        };
        let bytes = match Restorable::parse(&restorable) {
            Some(Restorable::Blob) => fs::read(self.blob_path(&state.sha256))?,
            Some(Restorable::Git) => {
                let commit = commit.ok_or(SnapshotError::NotRestorable)?;
                let object = format!("{commit}:{}{path}", prefix.unwrap_or_default());
                // The file on disk may be the stored blob as is, or converted
                // by Git's filters (core.autocrlf, .gitattributes). Take the
                // form whose hash matches what was recorded before the command.
                let raw = git(&scope, &["cat-file", "blob", &object])?;
                if matches(&raw, &state) {
                    raw
                } else {
                    git(&scope, &["cat-file", "--filters", &object])?
                }
            }
            _ => return Err(SnapshotError::NotRestorable),
        };
        if !matches(&bytes, &state) {
            return Err(SnapshotError::IntegrityMismatch);
        }
        atomic_write(&scope.join(path), &bytes)
    }
}

/// One file in Lu's recoverable trash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashEntry {
    pub id: i64,
    pub original_path: PathBuf,
    pub size: u64,
    pub sha256: String,
}

impl SnapshotStore {
    /// Move a file into Lu's trash instead of deleting it. The file is moved
    /// (renamed) when the trash is on the same volume, otherwise copied,
    /// checked against its hash, and only then removed.
    pub fn trash(&self, path: &Path) -> Result<TrashEntry, SnapshotError> {
        let path = canonical(path)?;
        if !path.is_file() {
            return Err(SnapshotError::InvalidPath);
        }
        let state = hash_file(&path, None)?;
        let dir = self.root.join("trash");
        fs::create_dir_all(&dir)?;
        let stored_name = format!("{}-{}", unique_suffix(), state.sha256);
        let stored = dir.join(&stored_name);
        if fs::rename(&path, &stored).is_err() {
            fs::copy(&path, &stored)?;
            if hash_file(&stored, None)? != state {
                let _ = fs::remove_file(&stored);
                return Err(SnapshotError::IntegrityMismatch);
            }
            fs::remove_file(&path)?;
        }
        let conn = self.conn.lock().map_err(|_| SnapshotError::Locked)?;
        conn.execute(
            "INSERT INTO trash(original_path,stored_name,size,sha256) VALUES(?1,?2,?3,?4)",
            params![path.to_string_lossy(), stored_name, state.size as i64, state.sha256],
        )?;
        Ok(TrashEntry { id: conn.last_insert_rowid(), original_path: path, size: state.size, sha256: state.sha256 })
    }

    /// Put a trashed file back where it was. Refused when something now
    /// exists at that path, so a restore never overwrites newer work.
    pub fn restore_trashed(&self, id: i64) -> Result<PathBuf, SnapshotError> {
        let conn = self.conn.lock().map_err(|_| SnapshotError::Locked)?;
        let (original, stored_name, size, sha256): (String, String, i64, String) = conn
            .query_row(
                "SELECT original_path,stored_name,size,sha256 FROM trash WHERE id=?1 AND restored=0",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
            .ok_or(SnapshotError::NotRestorable)?;
        let original = PathBuf::from(original);
        if original.exists() {
            return Err(SnapshotError::NotRestorable);
        }
        let bytes = fs::read(self.root.join("trash").join(&stored_name))?;
        if !matches(&bytes, &FileState { size: size as u64, sha256 }) {
            return Err(SnapshotError::IntegrityMismatch);
        }
        atomic_write(&original, &bytes)?;
        fs::remove_file(self.root.join("trash").join(&stored_name))?;
        conn.execute("UPDATE trash SET restored=1 WHERE id=?1", [id])?;
        Ok(original)
    }
}
