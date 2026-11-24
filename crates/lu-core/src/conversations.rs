//! The conversation library behind the sidebar.
//!
//! Each conversation is its own ANSF book (`chats/<id>.sqlite`), immutable
//! inside. A small library database lists them with titles, folders and
//! last activity, and keeps a full-text index across all conversations for
//! search. Deleting a conversation moves its book to Lu's trash (it can be
//! restored) and removes it from the list and the index; nothing inside a
//! book is ever edited. Deleting a folder either moves its conversations
//! back to the top level or deletes them too, as the user chooses.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use rusqlite::{params, Connection, OptionalExtension};

use crate::ansf::AnsfStore;
use crate::chat::new_id;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatInfo {
    pub id: String,
    pub title: String,
    pub folder: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub chat: String,
    pub title: String,
    /// A short piece of the matching text, with `[` `]` around the match.
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashedChat {
    pub trash_id: String,
    pub title: String,
    pub deleted_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryError {
    NotFound,
    Invalid(String),
    Database(String),
    Io(String),
}
impl From<rusqlite::Error> for LibraryError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Database(e.to_string())
    }
}
impl From<std::io::Error> for LibraryError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}
impl std::fmt::Display for LibraryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for LibraryError {}

pub struct ConversationLibrary {
    dir: PathBuf,
    db: Mutex<Connection>,
}

fn valid_chat_id(id: &str) -> bool {
    id.len() == 34 && id.starts_with("c-") && id[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// An FTS5 query that matches every word, each taken literally.
fn fts_query(text: &str) -> Option<String> {
    let words: Vec<String> = text.split_whitespace().take(12).map(|w| format!("\"{}\"", w.replace('"', "\"\""))).collect();
    (!words.is_empty()).then(|| words.join(" "))
}

/// A title from the first message: its first line, at most 60 characters.
pub fn title_from(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("New conversation");
    let mut t: String = line.chars().take(60).collect();
    if line.chars().count() > 60 {
        t.push('…');
    }
    t
}

impl ConversationLibrary {
    pub fn open(dir: &Path) -> Result<Self, LibraryError> {
        fs::create_dir_all(dir.join("chats"))?;
        fs::create_dir_all(dir.join("trash"))?;
        let db = Connection::open(dir.join("library.sqlite"))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS folders (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS chats (
                id TEXT PRIMARY KEY NOT NULL,
                title TEXT NOT NULL,
                titled_by_user INTEGER NOT NULL DEFAULT 0,
                folder_id INTEGER REFERENCES folders(id) ON DELETE SET NULL,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
            );
            CREATE TABLE IF NOT EXISTS trash (
                trash_id TEXT PRIMARY KEY NOT NULL,
                chat_id TEXT NOT NULL,
                title TEXT NOT NULL,
                deleted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS chat_text USING fts5(chat_id UNINDEXED, role UNINDEXED, text);",
        )?;
        Ok(Self { dir: dir.to_path_buf(), db: Mutex::new(db) })
    }
    fn db(&self) -> Result<std::sync::MutexGuard<'_, Connection>, LibraryError> {
        self.db.lock().map_err(|_| LibraryError::Database("lock".into()))
    }
    fn book_path(&self, id: &str) -> PathBuf {
        self.dir.join("chats").join(format!("{id}.sqlite"))
    }

    /// A new, empty conversation; returns its ID (also its ANSF session).
    pub fn create(&self, folder: Option<i64>) -> Result<String, LibraryError> {
        let id = new_id('c');
        let book = AnsfStore::open(&self.book_path(&id)).map_err(|e| LibraryError::Database(format!("{e:?}")))?;
        book.create_session(&id, "lu", "New conversation").map_err(|e| LibraryError::Database(format!("{e:?}")))?;
        book.start_chapter(&id, "Start").map_err(|e| LibraryError::Database(format!("{e:?}")))?;
        self.db()?.execute("INSERT INTO chats(id, title, folder_id) VALUES (?1, 'New conversation', ?2)", params![id, folder])?;
        Ok(id)
    }

    /// The conversation's ANSF book.
    pub fn book(&self, id: &str) -> Result<AnsfStore, LibraryError> {
        if !valid_chat_id(id) || !self.book_path(id).is_file() {
            return Err(LibraryError::NotFound);
        }
        AnsfStore::open(&self.book_path(id)).map_err(|e| LibraryError::Database(format!("{e:?}")))
    }

    /// Conversations, most recent first, and folders by name.
    pub fn list(&self) -> Result<(Vec<Folder>, Vec<ChatInfo>), LibraryError> {
        let db = self.db()?;
        let mut q = db.prepare("SELECT id, name FROM folders ORDER BY name COLLATE NOCASE")?;
        let folders = q.query_map([], |r| Ok(Folder { id: r.get(0)?, name: r.get(1)? }))?.collect::<Result<_, _>>()?;
        let mut q = db.prepare("SELECT id, title, folder_id, created_at, updated_at FROM chats ORDER BY updated_at DESC")?;
        let chats = q
            .query_map([], |r| Ok(ChatInfo { id: r.get(0)?, title: r.get(1)?, folder: r.get(2)?, created_at: r.get(3)?, updated_at: r.get(4)? }))?
            .collect::<Result<_, _>>()?;
        Ok((folders, chats))
    }

    pub fn get(&self, id: &str) -> Result<ChatInfo, LibraryError> {
        self.db()?
            .query_row("SELECT id, title, folder_id, created_at, updated_at FROM chats WHERE id = ?1", params![id], |r| {
                Ok(ChatInfo { id: r.get(0)?, title: r.get(1)?, folder: r.get(2)?, created_at: r.get(3)?, updated_at: r.get(4)? })
            })
            .optional()?
            .ok_or(LibraryError::NotFound)
    }

    /// Record a message for the list and the search index. The first user
    /// message names the conversation unless the user renamed it.
    pub fn note_message(&self, id: &str, role: &str, text: &str) -> Result<(), LibraryError> {
        let db = self.db()?;
        let n = db.execute("UPDATE chats SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?1", params![id])?;
        if n == 0 {
            return Err(LibraryError::NotFound);
        }
        db.execute("INSERT INTO chat_text(chat_id, role, text) VALUES (?1, ?2, ?3)", params![id, role, text])?;
        if role == "user" {
            db.execute(
                "UPDATE chats SET title = ?2 WHERE id = ?1 AND titled_by_user = 0 AND title = 'New conversation'",
                params![id, title_from(text)],
            )?;
        }
        Ok(())
    }

    pub fn rename(&self, id: &str, title: &str) -> Result<(), LibraryError> {
        let t = title.trim();
        if t.is_empty() || t.chars().count() > 200 {
            return Err(LibraryError::Invalid("a title of 1 to 200 characters".into()));
        }
        let n = self.db()?.execute("UPDATE chats SET title = ?2, titled_by_user = 1 WHERE id = ?1", params![id, t])?;
        if n == 0 {
            return Err(LibraryError::NotFound);
        }
        Ok(())
    }

    pub fn create_folder(&self, name: &str) -> Result<i64, LibraryError> {
        let n = name.trim();
        if n.is_empty() || n.chars().count() > 100 {
            return Err(LibraryError::Invalid("a folder name of 1 to 100 characters".into()));
        }
        let db = self.db()?;
        db.execute("INSERT INTO folders(name) VALUES (?1)", params![n])?;
        Ok(db.last_insert_rowid())
    }
    pub fn rename_folder(&self, id: i64, name: &str) -> Result<(), LibraryError> {
        let n = name.trim();
        if n.is_empty() || n.chars().count() > 100 {
            return Err(LibraryError::Invalid("a folder name of 1 to 100 characters".into()));
        }
        if self.db()?.execute("UPDATE folders SET name = ?2 WHERE id = ?1", params![id, n])? == 0 {
            return Err(LibraryError::NotFound);
        }
        Ok(())
    }

    /// Move a conversation into a folder, or to the top level with `None`.
    pub fn move_to(&self, chat: &str, folder: Option<i64>) -> Result<(), LibraryError> {
        let db = self.db()?;
        if let Some(f) = folder {
            let exists: Option<i64> = db.query_row("SELECT 1 FROM folders WHERE id = ?1", params![f], |r| r.get(0)).optional()?;
            if exists.is_none() {
                return Err(LibraryError::NotFound);
            }
        }
        if db.execute("UPDATE chats SET folder_id = ?2 WHERE id = ?1", params![chat, folder])? == 0 {
            return Err(LibraryError::NotFound);
        }
        Ok(())
    }

    /// Delete a folder. Its conversations go back to the top level, or, with
    /// `with_chats`, to the trash as well.
    pub fn delete_folder(&self, id: i64, with_chats: bool) -> Result<(), LibraryError> {
        let chats: Vec<String> = {
            let db = self.db()?;
            let mut q = db.prepare("SELECT id FROM chats WHERE folder_id = ?1")?;
            let rows = q.query_map(params![id], |r| r.get(0))?.collect::<Result<_, _>>()?;
            rows
        };
        if with_chats {
            for c in &chats {
                self.delete(c)?;
            }
        }
        if self.db()?.execute("DELETE FROM folders WHERE id = ?1", params![id])? == 0 {
            return Err(LibraryError::NotFound);
        }
        Ok(())
    }

    /// Move a conversation's book to the trash and take it out of the list
    /// and the search index. It can be restored.
    pub fn delete(&self, id: &str) -> Result<String, LibraryError> {
        let info = self.get(id)?;
        let trash_id = new_id('t');
        let dest = self.dir.join("trash").join(&trash_id);
        fs::create_dir_all(&dest)?;
        for suffix in ["", "-wal", "-shm"] {
            let from = self.dir.join("chats").join(format!("{id}.sqlite{suffix}"));
            if from.exists() {
                fs::rename(&from, dest.join(format!("{id}.sqlite{suffix}")))?;
            }
        }
        let mut db = self.db()?;
        let tx = db.transaction()?;
        tx.execute("INSERT INTO trash(trash_id, chat_id, title) VALUES (?1, ?2, ?3)", params![trash_id, id, info.title])?;
        tx.execute("DELETE FROM chat_text WHERE chat_id = ?1", params![id])?;
        tx.execute("DELETE FROM chats WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(trash_id)
    }

    pub fn trashed(&self) -> Result<Vec<TrashedChat>, LibraryError> {
        let db = self.db()?;
        let mut q = db.prepare("SELECT trash_id, title, deleted_at FROM trash ORDER BY deleted_at DESC")?;
        let rows = q.query_map([], |r| Ok(TrashedChat { trash_id: r.get(0)?, title: r.get(1)?, deleted_at: r.get(2)? }))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Bring a deleted conversation back (to the top level), re-indexed.
    pub fn restore(&self, trash_id: &str) -> Result<String, LibraryError> {
        let (chat, title): (String, String) = self
            .db()?
            .query_row("SELECT chat_id, title FROM trash WHERE trash_id = ?1", params![trash_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?
            .ok_or(LibraryError::NotFound)?;
        let from = self.dir.join("trash").join(trash_id);
        for suffix in ["", "-wal", "-shm"] {
            let f = from.join(format!("{chat}.sqlite{suffix}"));
            if f.exists() {
                fs::rename(&f, self.dir.join("chats").join(format!("{chat}.sqlite{suffix}")))?;
            }
        }
        let _ = fs::remove_dir(&from);
        let messages = self.messages(&chat).unwrap_or_default();
        let mut db = self.db()?;
        let tx = db.transaction()?;
        tx.execute("INSERT INTO chats(id, title, titled_by_user) VALUES (?1, ?2, 1)", params![chat, title])?;
        for (role, text) in messages {
            tx.execute("INSERT INTO chat_text(chat_id, role, text) VALUES (?1, ?2, ?3)", params![chat, role, text])?;
        }
        tx.execute("DELETE FROM trash WHERE trash_id = ?1", params![trash_id])?;
        tx.commit()?;
        Ok(chat)
    }

    /// Permanently delete a trashed conversation.
    pub fn empty_trash_item(&self, trash_id: &str) -> Result<(), LibraryError> {
        if !trash_id.starts_with("t-") || trash_id.contains(['/', '\\', '.']) {
            return Err(LibraryError::Invalid("not a trash item".into()));
        }
        let n = self.db()?.execute("DELETE FROM trash WHERE trash_id = ?1", params![trash_id])?;
        if n == 0 {
            return Err(LibraryError::NotFound);
        }
        let dir = self.dir.join("trash").join(trash_id);
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    /// Search every conversation's text and titles.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, LibraryError> {
        let Some(q) = fts_query(query) else { return Ok(Vec::new()) };
        let db = self.db()?;
        let mut stmt = db.prepare(
            "SELECT t.chat_id, c.title, snippet(chat_text, 2, '[', ']', '…', 12)
             FROM chat_text t JOIN chats c ON c.id = t.chat_id
             WHERE chat_text MATCH ?1 ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![q, (limit * 4).clamp(1, 400) as i64], |r| Ok(SearchHit { chat: r.get(0)?, title: r.get(1)?, snippet: r.get(2)? }))?;
        let mut out: Vec<SearchHit> = Vec::new();
        for hit in rows {
            let hit = hit?;
            if !out.iter().any(|h| h.chat == hit.chat) {
                out.push(hit);
            }
            if out.len() >= limit {
                break;
            }
        }
        // Titles that match but whose text does not.
        let lower = query.to_lowercase();
        let mut t = db.prepare("SELECT id, title FROM chats ORDER BY updated_at DESC")?;
        for row in t.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (id, title) = row?;
            if out.len() >= limit {
                break;
            }
            if title.to_lowercase().contains(&lower) && !out.iter().any(|h| h.chat == id) {
                out.push(SearchHit { chat: id, title: title.clone(), snippet: title });
            }
        }
        Ok(out)
    }

    /// The conversation's messages for display: (role, text), oldest first.
    pub fn messages(&self, id: &str) -> Result<Vec<(String, String)>, LibraryError> {
        let book = self.book(id)?;
        let mut out = Vec::new();
        let mut after = 0;
        loop {
            let page = book.page_after(id, after, 100).map_err(|e| LibraryError::Database(format!("{e:?}")))?;
            if page.is_empty() {
                break;
            }
            after = page.last().map(|e| e.id).unwrap_or(after);
            out.extend(page.into_iter().filter(|e| e.role == "user" || e.role == "assistant").map(|e| (e.role, e.content)));
        }
        Ok(out)
    }
}
