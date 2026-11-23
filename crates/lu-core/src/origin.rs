//! Origin tracking: every action the model proposes carries where it came
//! from, and the host decides from that, never from the model's say-so.
//!
//! - **The user's instruction** (a host-authenticated turn of the active
//!   chat), or a step it plainly implies: the action runs, with no prompt.
//! - **Untrusted external content** (a web page, a downloaded or cloned
//!   file, a plug-in's output): it has no authority. An action traced to it
//!   is held, and the user is told once which content asked for it. A
//!   network send traced to it is refused outright.
//!
//! How an action is traced, since Lu cannot read the model's mind:
//! 1. **Marked files.** A file that came from outside carries a mark. An
//!    action that names a marked file to run is external. A file the model
//!    writes with text copied from untrusted content gets the mark too, so
//!    origin travels with the content.
//! 2. **Copied text.** Untrusted content seen in this conversation is kept as
//!    overlapping 12-character pieces (normalized). If an action's text
//!    copies a run of 24 or more characters from it that is not also in the
//!    user's own instruction, the action was built from that content.
//!
//! Deliberate limits, to keep normal work free of prompts: local files,
//! command output and recalled memory are recorded but do NOT trigger holds
//! by text overlap; only untrusted sources do. A copied run shorter than 24
//! characters is not traced, so short injected commands can get through. Prompt injection cannot be fully
//! prevented; this limits it, and the journal shows what happened.
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Mutex,
};

use rusqlite::{params, Connection, OptionalExtension};

use crate::ansf::{AnsfError, AnsfStore};

/// Length of one traced piece of text, in characters after normalizing.
pub const PIECE_CHARS: usize = 12;
/// Shortest copied run that counts as tracing an action to a source. Shorter
/// runs are everyday text (`cargo build --release` is 21) that any docs
/// page contains, and would hold normal work.
pub const MIN_MATCH_CHARS: usize = 24;
/// Untrusted sources remembered per conversation for tracing.
const SOURCES_PER_SESSION: usize = 32;
/// Most text kept from one source.
const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    WebPage,
    /// A downloaded, cloned or otherwise outside file.
    ExternalFile,
    Plugin,
}
impl SourceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::WebPage => "web_page",
            Self::ExternalFile => "external_file",
            Self::Plugin => "plugin",
        }
    }
    fn parse(v: &str) -> Option<Self> {
        match v {
            "web_page" => Some(Self::WebPage),
            "external_file" => Some(Self::ExternalFile),
            "plugin" => Some(Self::Plugin),
            _ => None,
        }
    }
}

/// Which untrusted content an action was traced to, e.g. a page URL.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Source {
    pub kind: SourceKind,
    pub reference: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectKind {
    Read,
    Write,
    Delete,
    Execute,
    Install,
    NetworkRead,
    NetworkSend,
}
impl EffectKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Delete => "delete",
            Self::Execute => "execute",
            Self::Install => "install",
            Self::NetworkRead => "network_read",
            Self::NetworkSend => "network_send",
        }
    }
}

/// One action the model proposes, as the host sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub session_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub effect: EffectKind,
    /// The full action text: a command line, or a path (plus a short
    /// description) for a file operation.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Traced to the user's instruction: run it.
    Run,
    /// Traced to untrusted content: not run. `notify` is true the first time
    /// this source causes a hold in this conversation, so the user is told
    /// once, not once per action.
    Hold { held_id: i64, source: Source, notify: bool },
    /// A network send traced to untrusted content: never run.
    Refuse { source: Source },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldAction {
    pub id: i64,
    pub session_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub effect: EffectKind,
    pub text: String,
    pub source: Source,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginError {
    Database(String),
    Ansf(AnsfError),
    /// The turn is not a host-authenticated turn of this conversation.
    NotAuthorized,
    Invalid(String),
    Locked,
}
impl From<rusqlite::Error> for OriginError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Database(e.to_string())
    }
}
impl From<AnsfError> for OriginError {
    fn from(e: AnsfError) -> Self {
        Self::Ansf(e)
    }
}

/// Lowercase, '/' for '\', whitespace collapsed to one space.
fn normalize(text: &str) -> Vec<char> {
    let mut out = Vec::with_capacity(text.len());
    let mut space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !space && !out.is_empty() {
                out.push(' ');
            }
            space = true;
            continue;
        }
        space = false;
        let c = if c == '\\' { '/' } else { c };
        out.extend(c.to_lowercase());
    }
    out
}
fn pieces(text: &str) -> HashSet<String> {
    let chars = normalize(text);
    if chars.len() < PIECE_CHARS {
        return HashSet::new();
    }
    chars.windows(PIECE_CHARS).map(|w| w.iter().collect()).collect()
}
fn path_key(path: &Path) -> String {
    normalize(&path.to_string_lossy()).into_iter().collect()
}

struct Traced {
    source: Source,
    pieces: HashSet<String>,
}

/// The origin gate, with its own small database of marks and held actions.
pub struct OriginGate {
    conn: Mutex<Connection>,
    /// Pieces of untrusted content per conversation, newest last.
    cache: Mutex<HashMap<String, Vec<Traced>>>,
}

impl OriginGate {
    pub fn open(path: &Path) -> Result<Self, OriginError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS external_files(
               path_key TEXT PRIMARY KEY NOT NULL,
               path TEXT NOT NULL,
               source_kind TEXT NOT NULL,
               source_ref TEXT NOT NULL,
               marked_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE TABLE IF NOT EXISTS external_content(
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               session_id TEXT NOT NULL,
               source_kind TEXT NOT NULL,
               source_ref TEXT NOT NULL,
               text TEXT NOT NULL,
               added_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE INDEX IF NOT EXISTS external_content_session
               ON external_content(session_id,id);
             CREATE TABLE IF NOT EXISTS held_actions(
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               session_id TEXT NOT NULL,
               turn_id TEXT NOT NULL,
               call_id TEXT NOT NULL,
               effect TEXT NOT NULL,
               text TEXT NOT NULL,
               source_kind TEXT NOT NULL,
               source_ref TEXT NOT NULL,
               status TEXT NOT NULL CHECK(status IN ('held','refused','approved','declined')),
               decided_turn TEXT,
               created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE INDEX IF NOT EXISTS held_by_session ON held_actions(session_id,id);",
        )?;
        Ok(Self { conn: Mutex::new(conn), cache: Mutex::new(HashMap::new()) })
    }

    /// Record untrusted content the model has seen in this conversation (a
    /// fetched page, a downloaded file's text, a plug-in's reply).
    pub fn register_content(&self, session_id: &str, source: &Source, text: &str) -> Result<(), OriginError> {
        let mut cut = text.len().min(MAX_SOURCE_BYTES);
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        let text = &text[..cut];
        {
            let conn = self.conn.lock().map_err(|_| OriginError::Locked)?;
            conn.execute(
                "INSERT INTO external_content(session_id,source_kind,source_ref,text) VALUES(?1,?2,?3,?4)",
                params![session_id, source.kind.as_str(), source.reference, text],
            )?;
        }
        let mut cache = self.cache.lock().map_err(|_| OriginError::Locked)?;
        if let Some(list) = cache.get_mut(session_id) {
            list.push(Traced { source: source.clone(), pieces: pieces(text) });
            if list.len() > SOURCES_PER_SESSION {
                list.remove(0);
            }
        }
        Ok(())
    }

    /// Mark a file as coming from outside (downloaded, cloned, extracted).
    pub fn mark_external_file(&self, path: &Path, source: &Source) -> Result<(), OriginError> {
        let conn = self.conn.lock().map_err(|_| OriginError::Locked)?;
        conn.execute(
            "INSERT INTO external_files(path_key,path,source_kind,source_ref) VALUES(?1,?2,?3,?4)
             ON CONFLICT(path_key) DO UPDATE SET source_kind=excluded.source_kind,
               source_ref=excluded.source_ref,marked_at=excluded.marked_at",
            params![path_key(path), path.to_string_lossy(), source.kind.as_str(), source.reference],
        )?;
        Ok(())
    }

    /// The source a file came from, if it is marked as external.
    pub fn external_file(&self, path: &Path) -> Result<Option<Source>, OriginError> {
        let conn = self.conn.lock().map_err(|_| OriginError::Locked)?;
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT source_kind,source_ref FROM external_files WHERE path_key=?1",
                [path_key(path)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.and_then(|(k, r)| SourceKind::parse(&k).map(|kind| Source { kind, reference: r })))
    }

    /// When a file is written, give it the mark of any untrusted content its
    /// text was copied from. Returns that source.
    pub fn propagate_to_written_file(
        &self,
        session_id: &str,
        user_instruction: &str,
        path: &Path,
        content: &str,
    ) -> Result<Option<Source>, OriginError> {
        let source = self.trace_text(session_id, user_instruction, content)?;
        if let Some(source) = &source {
            self.mark_external_file(path, source)?;
        }
        Ok(source)
    }

    fn load_session(&self, session_id: &str) -> Result<(), OriginError> {
        let mut cache = self.cache.lock().map_err(|_| OriginError::Locked)?;
        if cache.contains_key(session_id) {
            return Ok(());
        }
        let conn = self.conn.lock().map_err(|_| OriginError::Locked)?;
        let mut stmt = conn.prepare(
            "SELECT source_kind,source_ref,text FROM external_content
             WHERE session_id=?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session_id, SOURCES_PER_SESSION as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        let mut list = Vec::new();
        for row in rows {
            let (kind, reference, text) = row?;
            if let Some(kind) = SourceKind::parse(&kind) {
                list.push(Traced { source: Source { kind, reference }, pieces: pieces(&text) });
            }
        }
        list.reverse();
        cache.insert(session_id.to_owned(), list);
        Ok(())
    }

    /// The untrusted source that `text` copies a run of at least
    /// [`MIN_MATCH_CHARS`] characters from, not counting text that is also in
    /// the user's own instruction. Newest source first.
    fn trace_text(&self, session_id: &str, user_instruction: &str, text: &str) -> Result<Option<Source>, OriginError> {
        self.load_session(session_id)?;
        let own = pieces(user_instruction);
        let chars = normalize(text);
        if chars.len() < MIN_MATCH_CHARS {
            return Ok(None);
        }
        let windows: Vec<String> = chars.windows(PIECE_CHARS).map(|w| w.iter().collect()).collect();
        let cache = self.cache.lock().map_err(|_| OriginError::Locked)?;
        let Some(list) = cache.get(session_id) else { return Ok(None) };
        for traced in list.iter().rev() {
            let mut run = 0usize;
            for w in &windows {
                if !own.contains(w) && traced.pieces.contains(w) {
                    run += 1;
                    if run + PIECE_CHARS > MIN_MATCH_CHARS {
                        return Ok(Some(traced.source.clone()));
                    }
                } else {
                    run = 0;
                }
            }
        }
        Ok(None)
    }

    /// The marked file named in an action's text, if any.
    fn trace_marked_file(&self, text: &str) -> Result<Option<Source>, OriginError> {
        let haystack: String = normalize(text).into_iter().collect();
        let conn = self.conn.lock().map_err(|_| OriginError::Locked)?;
        let mut stmt = conn.prepare("SELECT path_key,source_kind,source_ref FROM external_files")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        for row in rows {
            let (key, kind, reference) = row?;
            if haystack.contains(&key) {
                if let Some(kind) = SourceKind::parse(&kind) {
                    return Ok(Some(Source { kind, reference }));
                }
            }
        }
        Ok(None)
    }

    /// Decide whether a proposed action runs. The turn must be a
    /// host-authenticated user turn of the active conversation.
    pub fn decide(&self, book: &AnsfStore, proposal: &Proposal) -> Result<Decision, OriginError> {
        let (instruction, _) = book
            .authenticated_chat_turn(&proposal.session_id, &proposal.turn_id)?
            .ok_or(OriginError::NotAuthorized)?;
        if matches!(proposal.effect, EffectKind::Read | EffectKind::NetworkRead) {
            return Ok(Decision::Run);
        }
        // A later user turn may have approved exactly this held action.
        if self.approved(proposal)? {
            return Ok(Decision::Run);
        }
        let traced = match self.trace_text(&proposal.session_id, &instruction, &proposal.text)? {
            Some(source) => Some(source),
            None if matches!(proposal.effect, EffectKind::Execute | EffectKind::Install) => {
                self.trace_marked_file(&proposal.text)?
            }
            None => None,
        };
        let Some(source) = traced else { return Ok(Decision::Run) };
        let refuse = proposal.effect == EffectKind::NetworkSend;
        let conn = self.conn.lock().map_err(|_| OriginError::Locked)?;
        let seen_before: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM held_actions WHERE session_id=?1
               AND source_kind=?2 AND source_ref=?3)",
            params![proposal.session_id, source.kind.as_str(), source.reference],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO held_actions(session_id,turn_id,call_id,effect,text,source_kind,source_ref,status)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                proposal.session_id,
                proposal.turn_id,
                proposal.call_id,
                proposal.effect.as_str(),
                proposal.text,
                source.kind.as_str(),
                source.reference,
                if refuse { "refused" } else { "held" }
            ],
        )?;
        let held_id = conn.last_insert_rowid();
        Ok(if refuse { Decision::Refuse { source } } else { Decision::Hold { held_id, source, notify: !seen_before } })
    }

    fn approved(&self, p: &Proposal) -> Result<bool, OriginError> {
        let conn = self.conn.lock().map_err(|_| OriginError::Locked)?;
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM held_actions WHERE session_id=?1 AND status='approved'
               AND decided_turn=?2 AND effect=?3 AND text=?4)",
            params![p.session_id, p.turn_id, p.effect.as_str(), p.text],
            |r| r.get(0),
        )?)
    }

    /// The user's answer to a held action. Only a NEW host-authenticated
    /// turn of the same conversation can decide, never the turn that was
    /// held, recalled text or the model. An approved action may then be
    /// proposed again under the approving turn, and it runs. A refused
    /// network send cannot be approved.
    pub fn decide_held(&self, book: &AnsfStore, held_id: i64, deciding_turn: &str, approve: bool)
        -> Result<HeldAction, OriginError> {
        let held = self.held(held_id)?.ok_or(OriginError::Invalid("no such held action".into()))?;
        if held.status != "held" {
            return Err(OriginError::Invalid(format!("the action is {}", held.status)));
        }
        // The deciding turn must be a real user turn that came after the hold.
        let held_entry = book.authenticated_chat_turn(&held.session_id, &held.turn_id)?.map(|(_, e)| e);
        let deciding_entry = book.authenticated_chat_turn(&held.session_id, deciding_turn)?.map(|(_, e)| e);
        match (held_entry, deciding_entry) {
            (Some(h), Some(d)) if d > h => {}
            _ => return Err(OriginError::NotAuthorized),
        }
        let conn = self.conn.lock().map_err(|_| OriginError::Locked)?;
        conn.execute(
            "UPDATE held_actions SET status=?2,decided_turn=?3 WHERE id=?1 AND status='held'",
            params![held_id, if approve { "approved" } else { "declined" }, deciding_turn],
        )?;
        drop(conn);
        self.held(held_id)?.ok_or(OriginError::Invalid("lost held action".into()))
    }

    pub fn held(&self, id: i64) -> Result<Option<HeldAction>, OriginError> {
        let conn = self.conn.lock().map_err(|_| OriginError::Locked)?;
        conn.query_row(
            "SELECT id,session_id,turn_id,call_id,effect,text,source_kind,source_ref,status
             FROM held_actions WHERE id=?1",
            [id],
            read_held,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Held and refused actions of a conversation, oldest first.
    pub fn held_actions(&self, session_id: &str) -> Result<Vec<HeldAction>, OriginError> {
        let conn = self.conn.lock().map_err(|_| OriginError::Locked)?;
        let mut stmt = conn.prepare(
            "SELECT id,session_id,turn_id,call_id,effect,text,source_kind,source_ref,status
             FROM held_actions WHERE session_id=?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([session_id], read_held)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

fn read_held(r: &rusqlite::Row<'_>) -> rusqlite::Result<HeldAction> {
    let effect: String = r.get(4)?;
    let kind: String = r.get(6)?;
    let effect = [
        EffectKind::Read,
        EffectKind::Write,
        EffectKind::Delete,
        EffectKind::Execute,
        EffectKind::Install,
        EffectKind::NetworkRead,
        EffectKind::NetworkSend,
    ]
    .into_iter()
    .find(|e| e.as_str() == effect)
    .ok_or(rusqlite::Error::InvalidQuery)?;
    Ok(HeldAction {
        id: r.get(0)?,
        session_id: r.get(1)?,
        turn_id: r.get(2)?,
        call_id: r.get(3)?,
        effect,
        text: r.get(5)?,
        source: Source { kind: SourceKind::parse(&kind).ok_or(rusqlite::Error::InvalidQuery)?, reference: r.get(7)? },
        status: r.get(8)?,
    })
}

/// The path text of a file operation, for a [`Proposal`].
pub fn path_text(what: &str, path: &Path) -> String {
    format!("{what} {}", PathBuf::from(path).display())
}
