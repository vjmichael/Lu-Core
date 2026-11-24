//! The user's model list and each model's own settings, plus app settings.

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelSettings {
    /// Context length in tokens.
    pub context: u32,
    /// Layers on the GPU; `None` = all (GPU backends) or none (CPU).
    pub gpu_layers: Option<u32>,
    /// `auto`, `rocm`, `vulkan` or `cpu`.
    pub backend: String,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    /// Replaces Lu's default instructions when set.
    pub system_prompt: Option<String>,
}
impl Default for ModelSettings {
    fn default() -> Self {
        Self { context: 16384, gpu_layers: None, backend: "auto".into(), temperature: None, top_p: None, system_prompt: None }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ModelEntry {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub file_name: String,
    pub architecture: Option<String>,
    pub trained_context: Option<u64>,
    pub bytes: u64,
    /// `imported` or `downloaded`.
    pub source: String,
    pub repo: Option<String>,
    pub projector: Option<String>,
    pub exists: bool,
    pub settings: ModelSettings,
}

pub struct Catalog {
    db: Mutex<Connection>,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Self, String> {
        let db = Connection::open(path).map_err(|e| e.to_string())?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS models (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                architecture TEXT, trained_context INTEGER, bytes INTEGER NOT NULL,
                source TEXT NOT NULL CHECK(source IN ('imported','downloaded')),
                repo TEXT, projector TEXT,
                settings TEXT NOT NULL,
                added_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS app_settings (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);",
        )
        .map_err(|e| e.to_string())?;
        Ok(Self { db: Mutex::new(db) })
    }
    fn db(&self) -> Result<std::sync::MutexGuard<'_, Connection>, String> {
        self.db.lock().map_err(|_| "lock".to_owned())
    }

    /// Add a model (or return the existing entry for the same file).
    #[allow(clippy::too_many_arguments)]
    pub fn add(&self, path: &Path, name: &str, architecture: Option<&str>, trained_context: Option<u64>, bytes: u64, source: &str, repo: Option<&str>, projector: Option<&Path>) -> Result<i64, String> {
        let db = self.db()?;
        let p = path.to_string_lossy();
        if let Some(id) = db.query_row("SELECT id FROM models WHERE path = ?1", params![p], |r| r.get::<_, i64>(0)).optional().map_err(|e| e.to_string())? {
            return Ok(id);
        }
        let settings = serde_json::to_string(&ModelSettings::default()).map_err(|e| e.to_string())?;
        db.execute(
            "INSERT INTO models(path, name, architecture, trained_context, bytes, source, repo, projector, settings) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![p, name, architecture, trained_context.map(|c| c as i64), bytes as i64, source, repo, projector.map(|x| x.to_string_lossy().into_owned()), settings],
        )
        .map_err(|e| e.to_string())?;
        Ok(db.last_insert_rowid())
    }

    pub fn list(&self) -> Result<Vec<ModelEntry>, String> {
        let db = self.db()?;
        let mut q = db
            .prepare("SELECT id, name, path, architecture, trained_context, bytes, source, repo, projector, settings FROM models ORDER BY name COLLATE NOCASE")
            .map_err(|e| e.to_string())?;
        let rows = q
            .query_map([], |r| {
                let path: String = r.get(2)?;
                let settings: String = r.get(9)?;
                Ok(ModelEntry {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    file_name: path.rsplit(['/', '\\']).next().unwrap_or("").to_owned(),
                    exists: Path::new(&path).is_file(),
                    path,
                    architecture: r.get(3)?,
                    trained_context: r.get::<_, Option<i64>>(4)?.map(|c| c as u64),
                    bytes: r.get::<_, i64>(5)? as u64,
                    source: r.get(6)?,
                    repo: r.get(7)?,
                    projector: r.get(8)?,
                    settings: serde_json::from_str(&settings).unwrap_or_default(),
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
    }

    pub fn get(&self, id: i64) -> Result<ModelEntry, String> {
        self.list()?.into_iter().find(|m| m.id == id).ok_or_else(|| "no such model".to_owned())
    }

    pub fn set_settings(&self, id: i64, s: &ModelSettings) -> Result<(), String> {
        if !(512..=1_048_576).contains(&s.context) {
            return Err("context must be between 512 and 1,048,576 tokens".into());
        }
        if !matches!(s.backend.as_str(), "auto" | "rocm" | "vulkan" | "cpu") {
            return Err("backend must be auto, rocm, vulkan or cpu".into());
        }
        if s.temperature.is_some_and(|t| !(0.0..=2.0).contains(&t)) || s.top_p.is_some_and(|p| !(0.0..=1.0).contains(&p)) {
            return Err("temperature 0-2, top-p 0-1".into());
        }
        let json = serde_json::to_string(s).map_err(|e| e.to_string())?;
        let n = self.db()?.execute("UPDATE models SET settings = ?2 WHERE id = ?1", params![id, json]).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("no such model".into());
        }
        Ok(())
    }

    pub fn rename(&self, id: i64, name: &str) -> Result<(), String> {
        let n = name.trim();
        if n.is_empty() {
            return Err("a name is needed".into());
        }
        self.db()?.execute("UPDATE models SET name = ?2 WHERE id = ?1", params![id, n]).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Take a model off the list. The file itself is left alone.
    pub fn remove(&self, id: i64) -> Result<(), String> {
        self.db()?.execute("DELETE FROM models WHERE id = ?1", params![id]).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn setting(&self, key: &str) -> Option<String> {
        self.db().ok()?.query_row("SELECT value FROM app_settings WHERE key = ?1", params![key], |r| r.get(0)).optional().ok().flatten()
    }
    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), String> {
        self.db()?
            .execute("INSERT INTO app_settings(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![key, value])
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// GGUF files under a folder (LM Studio's model folder, for example),
/// without projectors, at most `limit`.
pub fn find_gguf(dir: &Path, limit: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() && depth < 6 {
                stack.push((p, depth + 1));
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("gguf")) && !p.file_name().is_some_and(|n| n.to_string_lossy().to_ascii_lowercase().contains("mmproj")) {
                out.push(p);
                if out.len() >= limit {
                    return out;
                }
            }
        }
    }
    out.sort();
    out
}
