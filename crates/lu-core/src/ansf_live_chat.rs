//! Lu chat events: the original conversation history, stored in ANSF SQLite.
//!
//! A legacy-chat importer may add observed records, but those records are
//! NOT authenticated current human turns and never become tool authority.
//! The host owns identity issuance, profile/ownership validation and dispatch.
use rusqlite::{params,Connection,OptionalExtension};
use crate::ansf::{AnsfError,AnsfStore,Entry,ENTRY_COLUMNS_E};

pub const MAX_LIVE_TEXT_BYTES:usize=2*1024*1024;
pub const MAX_LIVE_EVENTS_PAGE:usize=100;

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub enum LiveKind{
    User,ReaderUser,AssistantFinal,AssistantProgress,
    AssistantCompleted,AssistantCancelled,
}
impl LiveKind{
    fn as_str(self)->&'static str{
        match self{
            Self::User=>"user",
            Self::ReaderUser=>"reader_user",
            Self::AssistantFinal=>"assistant_final",
            Self::AssistantProgress=>"assistant_progress",
            Self::AssistantCompleted=>"assistant_completed",
            Self::AssistantCancelled=>"assistant_cancelled",
        }
    }
    fn parse(value:&str)->Result<Self,AnsfError>{
        match value{
            "user"=>Ok(Self::User),"reader_user"=>Ok(Self::ReaderUser),
            "assistant_final"=>Ok(Self::AssistantFinal),
            "assistant_progress"=>Ok(Self::AssistantProgress),
            "assistant_completed"=>Ok(Self::AssistantCompleted),
            "assistant_cancelled"=>Ok(Self::AssistantCancelled),
            _=>Err(AnsfError::InvalidInput),
        }
    }
}
#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub enum LiveOrigin{
    /// Only a trusted desktop host may issue a new authenticated human turn.
    HostAuthenticated,
    /// Imported old chat: historical context only, NEVER a fresh tool grant.
    LegacyObserved,
}
impl LiveOrigin{
    fn as_str(self)->&'static str{
        match self{Self::HostAuthenticated=>"host",Self::LegacyObserved=>"legacy"}
    }
    fn parse(raw:&str)->Result<Self,AnsfError>{
        match raw{"host"=>Ok(Self::HostAuthenticated),
            "legacy"=>Ok(Self::LegacyObserved),_=>Err(AnsfError::InvalidInput)}
    }
}
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct LiveEvent{
    pub event_id:String,
    pub kind:LiveKind,
    pub text:String,
    pub user_turn_id:Option<String>,
    /// Historical imported turn identity, retained for auditing, NEVER tool authority.
    pub legacy_turn_id:Option<String>,
    pub generation_id:Option<String>,
}
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct StoredLiveEvent{
    pub seq:i64,
    pub session_id:String,
    pub event:LiveEvent,
    pub origin:LiveOrigin,
    pub source_entry_id:Option<i64>,
}
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct VisibleChatMessage{
    pub seq:i64,
    pub role:String,
    pub text:String,
    pub interrupted:bool,
    pub cancelled:bool,
}

const EVENT_COLUMNS:&str=
    "seq,session_id,event_id,kind,text,user_turn_id,legacy_turn_id,generation_id,origin,source_entry_id";

fn opaque(value:&str,expected:&str)->bool{
    value.len()==expected.len()+32&&value.starts_with(expected)&&
        value[expected.len()..].bytes().all(|v|v.is_ascii_hexdigit()&&!v.is_ascii_uppercase())
}
fn bounded(s:&str)->bool{!s.is_empty()&&s.len()<=128&&!s.contains('\0')}
fn valid_session(s:&str)->bool{!s.trim().is_empty()&&s.len()<=128&&!s.contains('\0')}
impl LiveEvent{
    fn validate(&self,origin:LiveOrigin)->Result<(),AnsfError>{
        let host=origin==LiveOrigin::HostAuthenticated;
        if !opaque(&self.event_id,"e-")||
           self.text.len()>MAX_LIVE_TEXT_BYTES||self.text.contains('\0'){
            return Err(AnsfError::InvalidInput)
        }
        // Host generations are host-minted; legacy ones are only bounded text.
        let valid_generation=|s:&str| if host {opaque(s,"g-")} else {bounded(s)};
        if self.user_turn_id.as_deref().is_some_and(|s|!opaque(s,"g-"))||
           self.generation_id.as_deref().is_some_and(|s|!valid_generation(s)){
            return Err(AnsfError::InvalidInput)
        }
        if self.legacy_turn_id.as_deref().is_some_and(|s|
            host||self.kind!=LiveKind::User||!bounded(s)){
            return Err(AnsfError::InvalidInput)
        }
        let ok=match self.kind{
            // A host user turn has text; only a host turn may carry a turn ID.
            LiveKind::User=>self.generation_id.is_none()&&
                if host {!self.text.is_empty()} else {self.user_turn_id.is_none()},
            LiveKind::ReaderUser=>!self.text.is_empty()&&
                self.generation_id.is_some()&&self.user_turn_id.is_none(),
            LiveKind::AssistantFinal=>
                self.user_turn_id.is_none()&&self.generation_id.is_none(),
            LiveKind::AssistantProgress=>
                self.generation_id.is_some()&&self.user_turn_id.is_none(),
            LiveKind::AssistantCompleted|LiveKind::AssistantCancelled=>
                self.text.is_empty()&&self.generation_id.is_some()&&self.user_turn_id.is_none(),
        };
        if !ok{return Err(AnsfError::InvalidInput)}
        Ok(())
    }
}
fn read_event(r:&rusqlite::Row<'_>)->rusqlite::Result<Result<StoredLiveEvent,AnsfError>>{
    let kind:String=r.get(3)?;
    let origin:String=r.get(8)?;
    let parsed=(||{
        let origin=LiveOrigin::parse(&origin)?;
        let event=LiveEvent{event_id:r.get(2)?,kind:LiveKind::parse(&kind)?,text:r.get(4)?,
            user_turn_id:r.get(5)?,legacy_turn_id:r.get(6)?,generation_id:r.get(7)?};
        event.validate(origin)?;
        Ok(StoredLiveEvent{seq:r.get(0)?,session_id:r.get(1)?,event,origin,
            source_entry_id:r.get(9)?})
    })();
    Ok(parsed)
}
pub(crate) fn initialize_live_chat_schema(conn:&Connection)->Result<(),AnsfError>{
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ansf_live_chat_events (
           seq INTEGER PRIMARY KEY AUTOINCREMENT,
           session_id TEXT NOT NULL REFERENCES ansf_sessions(id),
           event_id TEXT NOT NULL UNIQUE,
           kind TEXT NOT NULL CHECK(kind IN
             ('user','reader_user','assistant_final','assistant_progress',
              'assistant_completed','assistant_cancelled')),
           text TEXT NOT NULL,
           user_turn_id TEXT,
           legacy_turn_id TEXT,
           generation_id TEXT,
           origin TEXT NOT NULL CHECK(origin IN ('host','legacy')),
           source_entry_id INTEGER REFERENCES ansf_entries(id),
           created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE INDEX IF NOT EXISTS ansf_live_chat_event_cursor
           ON ansf_live_chat_events(session_id,seq);
         CREATE INDEX IF NOT EXISTS ansf_live_chat_generation
           ON ansf_live_chat_events(session_id,generation_id,kind);
         CREATE UNIQUE INDEX IF NOT EXISTS ansf_live_authenticated_user
           ON ansf_live_chat_events(session_id,user_turn_id)
           WHERE kind='user' AND origin='host' AND user_turn_id IS NOT NULL;
         CREATE UNIQUE INDEX IF NOT EXISTS ansf_live_reader_generation
           ON ansf_live_chat_events(session_id,generation_id)
           WHERE kind='reader_user' AND origin='host';
         CREATE TRIGGER IF NOT EXISTS ansf_live_chat_no_update
           BEFORE UPDATE ON ansf_live_chat_events BEGIN
             SELECT RAISE(ABORT,'ANSF live original events are immutable');
           END;
         CREATE TRIGGER IF NOT EXISTS ansf_live_chat_no_delete
           BEFORE DELETE ON ansf_live_chat_events BEGIN
             SELECT RAISE(ABORT,'ANSF live original events are immutable');
           END;
         -- Derived pointer-only display projection: no text, no copied model
         -- instructions and no second authoritative conversation database.
         CREATE TABLE IF NOT EXISTS ansf_visible_chat(
           session_id TEXT NOT NULL REFERENCES ansf_sessions(id),
           first_seq INTEGER NOT NULL REFERENCES ansf_live_chat_events(seq),
           latest_event_seq INTEGER NOT NULL REFERENCES ansf_live_chat_events(seq),
           generation_id TEXT,
           role TEXT NOT NULL CHECK(role IN ('user','assistant')),
           status TEXT NOT NULL CHECK(status IN ('final','interrupted','cancelled')),
           PRIMARY KEY(session_id,first_seq),
           UNIQUE(session_id,generation_id)
         );
         CREATE INDEX IF NOT EXISTS ansf_visible_chat_cursor
           ON ansf_visible_chat(session_id,first_seq);"
    )?;
    let projected:bool=conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM ansf_visible_chat)",[],|r|r.get(0))?;
    let has_events:bool=conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM ansf_live_chat_events)",[],|r|r.get(0))?;
    if !projected && has_events {
        // Rebuild the derived projection from the immutable originals, e.g.
        // after the table was lost. Never runs on a book with a projection.
        conn.execute_batch(
            "INSERT OR IGNORE INTO ansf_visible_chat
             (session_id,first_seq,latest_event_seq,generation_id,role,status)
             SELECT session_id,seq,seq,NULL,
               CASE WHEN kind IN ('user','reader_user') THEN 'user' ELSE 'assistant' END,
               'final'
             FROM ansf_live_chat_events
             WHERE kind IN ('user','reader_user','assistant_final');
             INSERT OR IGNORE INTO ansf_visible_chat
             (session_id,first_seq,latest_event_seq,generation_id,role,status)
             SELECT p.session_id,MIN(p.seq),MAX(p.seq),p.generation_id,'assistant',
               CASE WHEN EXISTS(
                    SELECT 1 FROM ansf_live_chat_events x
                    WHERE x.session_id=p.session_id AND x.generation_id=p.generation_id
                    AND x.kind='assistant_completed'
                  ) THEN 'final'
                  WHEN EXISTS(
                    SELECT 1 FROM ansf_live_chat_events x
                    WHERE x.session_id=p.session_id AND x.generation_id=p.generation_id
                    AND x.kind='assistant_cancelled'
                  ) THEN 'cancelled'
                  ELSE 'interrupted' END
             FROM ansf_live_chat_events p
             WHERE p.kind='assistant_progress'
             GROUP BY p.session_id,p.generation_id;"
        )?;
    }
    Ok(())
}
impl AnsfStore{
    /// Append an original host-authenticated Lu event directly to SQLite.
    /// This API is for the trusted host only, never for model-supplied JSON,
    /// imported transcripts or generic tool calls.
    pub fn append_host_chat_event(&self,session_id:&str,event:&LiveEvent)
        ->Result<StoredLiveEvent,AnsfError>{
        self.append_live_chat_event(session_id,event,LiveOrigin::HostAuthenticated,false)
    }
    /// Boundary-only migration of old chat events. Never authenticates a
    /// historical User record, even if its source contains a generation ID.
    /// Exact replay is idempotent; same ID with changed content fails.
    pub fn import_legacy_chat_event(&self,session_id:&str,event:&LiveEvent)
        ->Result<StoredLiveEvent,AnsfError>{
        self.append_live_chat_event(session_id,event,LiveOrigin::LegacyObserved,true)
    }
    fn append_live_chat_event(&self,session_id:&str,event:&LiveEvent,
        origin:LiveOrigin,idempotent:bool)->Result<StoredLiveEvent,AnsfError>{
        if !valid_session(session_id){return Err(AnsfError::InvalidInput)}
        event.validate(origin)?;
        let mut conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        let tx=conn.transaction()?;
        let exists=tx.query_row(
            &format!("SELECT {EVENT_COLUMNS} FROM ansf_live_chat_events WHERE event_id=?1"),
            [&event.event_id],read_event
        ).optional()?.transpose()?;
        if let Some(saved)=exists{
            if idempotent && saved.session_id==session_id&&
               saved.event==*event&&saved.origin==origin{return Ok(saved)}
            return Err(AnsfError::InvalidInput)
        }
        let active_chapter:Option<i64>=tx.query_row(
            "SELECT id FROM ansf_chapters WHERE session_id=?1 AND closed=0",
            [session_id],|r|r.get(0)).optional()?;
        let chapter=active_chapter.ok_or(AnsfError::NoActiveChapter)?;
        // A stream is: one or more progress events, then at most one terminal
        // event. Nothing may follow the terminal event, and a terminal event
        // needs a stream to end.
        let mut completed_text=None;
        if matches!(event.kind,LiveKind::AssistantProgress|
            LiveKind::AssistantCompleted|LiveKind::AssistantCancelled){
            let (terminal,latest):(bool,Option<String>)=tx.query_row(
                "SELECT
                   EXISTS(SELECT 1 FROM ansf_live_chat_events
                     WHERE session_id=?1 AND generation_id=?2
                       AND kind IN ('assistant_completed','assistant_cancelled')),
                   (SELECT text FROM ansf_live_chat_events
                     WHERE session_id=?1 AND generation_id=?2 AND kind='assistant_progress'
                     ORDER BY seq DESC LIMIT 1)",
                params![session_id,event.generation_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
            if terminal||(event.kind!=LiveKind::AssistantProgress&&latest.is_none()){
                return Err(AnsfError::InvalidInput)
            }
            if event.kind==LiveKind::AssistantCompleted{completed_text=latest}
        }
        let source_text=match event.kind {
            LiveKind::User|LiveKind::ReaderUser=>Some(("user",event.text.as_str())),
            LiveKind::AssistantFinal=>Some(("assistant",event.text.as_str())),
            LiveKind::AssistantCompleted=>completed_text.as_deref()
                .filter(|s|!s.is_empty()).map(|s|("assistant",s)),
            _=>None,
        };
        let source_entry_id=if let Some((role,text))=source_text{
            // Preserve exact original; generated summaries are navigation aids,
            // not the model's authority and not replacements for source text.
            let summary:String=text.chars().take(160).collect();
            tx.execute(
                "INSERT INTO ansf_entries
                 (session_id,chapter_id,message_id,role,summary,content)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![session_id,chapter,event.event_id,role,summary,text],
            )?;
            let entry_id=tx.last_insert_rowid();
            tx.execute(
                "UPDATE ansf_chapters
                 SET start_entry_id=COALESCE(start_entry_id,?1),end_entry_id=?1 WHERE id=?2",
                params![entry_id,chapter],
            )?;
            Some(entry_id)
        }else{None};
        tx.execute(
            "INSERT INTO ansf_live_chat_events
             (session_id,event_id,kind,text,user_turn_id,legacy_turn_id,generation_id,
              origin,source_entry_id)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![session_id,event.event_id,event.kind.as_str(),
                event.text,event.user_turn_id,event.legacy_turn_id,event.generation_id,
                origin.as_str(),source_entry_id],
        )?;
        let seq=tx.last_insert_rowid();
        // The display index points to the original event text. Stream
        // checkpoints move only the last-event pointer; no text is duplicated.
        match event.kind{
            LiveKind::User|LiveKind::ReaderUser|LiveKind::AssistantFinal=>{
                let role=if event.kind==LiveKind::AssistantFinal{"assistant"}else{"user"};
                tx.execute(
                    "INSERT INTO ansf_visible_chat
                     (session_id,first_seq,latest_event_seq,generation_id,role,status)
                     VALUES(?1,?2,?2,NULL,?3,'final')",
                    params![session_id,seq,role]
                )?;
            }
            LiveKind::AssistantProgress=>{
                tx.execute(
                    "INSERT INTO ansf_visible_chat
                     (session_id,first_seq,latest_event_seq,generation_id,role,status)
                     VALUES(?1,?2,?2,?3,'assistant','interrupted')
                     ON CONFLICT(session_id,generation_id)
                     DO UPDATE SET latest_event_seq=excluded.latest_event_seq",
                    params![session_id,seq,event.generation_id]
                )?;
            }
            LiveKind::AssistantCompleted|LiveKind::AssistantCancelled=>{
                let status=if event.kind==LiveKind::AssistantCompleted{"final"}else{"cancelled"};
                tx.execute(
                    "UPDATE ansf_visible_chat SET status=?1
                     WHERE session_id=?2 AND generation_id=?3",
                    params![status,session_id,event.generation_id]
                )?;
            }
        }
        tx.commit()?;
        Ok(StoredLiveEvent{seq,session_id:session_id.to_owned(),
            event:event.clone(),origin,source_entry_id})
    }
    /// Reconstruct a bounded visible window from immutable originals and a
    /// pointer-only projection. No full-history event scan or stream replay.
    pub fn visible_chat_before(
        &self,session_id:&str,before_seq:i64,limit:usize
    )->Result<(Vec<VisibleChatMessage>,bool),AnsfError>{
        self.visible_chat_page(session_id,before_seq,limit,false)
    }
    pub fn visible_chat_after(
        &self,session_id:&str,after_seq:i64,limit:usize
    )->Result<(Vec<VisibleChatMessage>,bool),AnsfError>{
        self.visible_chat_page(session_id,after_seq,limit,true)
    }
    fn visible_chat_page(
        &self,session_id:&str,cursor:i64,limit:usize,forward:bool
    )->Result<(Vec<VisibleChatMessage>,bool),AnsfError>{
        if !valid_session(session_id)||cursor<0||limit==0||limit>64{
            return Err(AnsfError::InvalidInput)
        }
        let conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        let (cmp,order)=if forward{(">","ASC")}else{("<","DESC")};
        let mut stmt=conn.prepare(&format!(
            "SELECT d.first_seq,d.role,d.status,e.text
             FROM ansf_visible_chat d
             JOIN ansf_live_chat_events e ON d.latest_event_seq=e.seq
             WHERE d.session_id=?1 AND d.first_seq{cmp}?2
             ORDER BY d.first_seq {order} LIMIT ?3"))?;
        let mut entries=stmt.query_map(
            params![session_id,cursor,(limit+1) as i64],|r|{
                let status:String=r.get(2)?;
                let (interrupted,cancelled)=match status.as_str(){
                    "final"=>(false,false),"interrupted"=>(true,false),
                    "cancelled"=>(true,true),
                    _=>return Err(rusqlite::Error::InvalidQuery)
                };
                Ok(VisibleChatMessage{
                    seq:r.get(0)?,role:r.get(1)?,text:r.get(3)?,
                    interrupted,cancelled
                })
            }
        )?.collect::<Result<Vec<_>,_>>()?;
        let has_more=entries.len()>limit;
        entries.truncate(limit);
        if !forward{entries.reverse();}
        Ok((entries,has_more))
    }

    /// Constant-time index repair probe for derived desktop activity.
    pub fn latest_live_chat_seq(&self,session_id:&str)->Result<i64,AnsfError>{
        if !valid_session(session_id){return Err(AnsfError::InvalidInput)}
        let conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        conn.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM ansf_live_chat_events
             WHERE session_id=?1",[session_id],|r|r.get(0)
        ).map_err(Into::into)
    }
    /// Indexed cursor paging. Caller must authenticate profile and conversation.
    pub fn live_chat_events_after(&self,session_id:&str,after_seq:i64,limit:usize)
        ->Result<Vec<StoredLiveEvent>,AnsfError>{
        if !valid_session(session_id)||after_seq<0||limit==0||
           limit>MAX_LIVE_EVENTS_PAGE{return Err(AnsfError::InvalidInput)}
        let conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        let mut stmt=conn.prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM ansf_live_chat_events
             WHERE session_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3"))?;
        let rows=stmt.query_map(params![session_id,after_seq,limit as i64],read_event)?;
        let mut result=Vec::new();
        for row in rows{result.push(row??)}
        Ok(result)
    }
    /// No legacy import, model output, Reader event or copied memory can pass
    /// as a current host-authenticated user turn.
    pub fn authenticated_chat_turn(&self,session_id:&str,turn_id:&str)
        ->Result<Option<(String,i64)>,AnsfError>{
        if !valid_session(session_id)||!opaque(turn_id,"g-"){
            return Err(AnsfError::InvalidInput)
        }
        let conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        conn.query_row(
            "SELECT text,source_entry_id FROM ansf_live_chat_events
             WHERE session_id=?1 AND kind='user' AND origin='host'
               AND user_turn_id=?2 AND source_entry_id IS NOT NULL",
            params![session_id,turn_id],
            |r|Ok((r.get(0)?,r.get(1)?))
        ).optional().map_err(Into::into)
    }
    /// Exact visible message sequence of one host-authenticated human turn.
    /// Never inferred by matching duplicated prompt text or the latest row.
    pub fn authenticated_chat_turn_visible_seq(
        &self,session_id:&str,turn_id:&str
    )->Result<Option<(i64,String)>,AnsfError>{
        if !valid_session(session_id)||!opaque(turn_id,"g-"){
            return Err(AnsfError::InvalidInput)
        }
        let conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        conn.query_row(
            "SELECT e.seq,e.text FROM ansf_live_chat_events e
             JOIN ansf_visible_chat d ON d.session_id=e.session_id
               AND d.first_seq=e.seq AND d.role='user'
             WHERE e.session_id=?1 AND e.kind='user'
               AND e.origin='host' AND e.user_turn_id=?2
               AND e.source_entry_id IS NOT NULL",
            params![session_id,turn_id],
            |r|Ok((r.get(0)?,r.get(1)?))
        ).optional().map_err(Into::into)
    }
    /// Earlier host-authenticated human turns of this conversation, newest
    /// first. Imported user text, Reader turns and assistant text never
    /// participate, and the current turn itself is excluded.
    pub fn recent_authenticated_turns_before(
        &self,session_id:&str,current_turn:&str,limit:usize
    )->Result<Vec<String>,AnsfError>{
        if !valid_session(session_id)||!opaque(current_turn,"g-")
            ||limit==0||limit>16{return Err(AnsfError::InvalidInput)}
        let conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        let cursor:Option<i64>=conn.query_row(
            "SELECT seq FROM ansf_live_chat_events
             WHERE session_id=?1 AND kind='user' AND origin='host'
               AND user_turn_id=?2 AND source_entry_id IS NOT NULL",
            params![session_id,current_turn],|r|r.get(0)
        ).optional()?;
        let Some(before_seq)=cursor else{return Err(AnsfError::InvalidInput)};
        let mut stmt=conn.prepare(
            "SELECT user_turn_id FROM ansf_live_chat_events
             WHERE session_id=?1 AND seq<?2 AND kind='user'
               AND origin='host' AND user_turn_id IS NOT NULL
               AND source_entry_id IS NOT NULL
             ORDER BY seq DESC LIMIT ?3"
        )?;
        let rows=stmt.query_map(params![session_id,before_seq,limit as i64],
            |r|r.get::<_,String>(0))?;
        let result=rows.collect::<Result<Vec<_>,_>>()?;
        if result.iter().any(|id|!opaque(id,"g-")){
            return Err(AnsfError::InvalidInput)
        }
        Ok(result)
    }
    pub fn latest_authenticated_chat_turn(&self,session_id:&str)
        ->Result<Option<(String,String)>,AnsfError>{
        if !valid_session(session_id){return Err(AnsfError::InvalidInput)}
        let conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        conn.query_row(
            "SELECT user_turn_id,text FROM ansf_live_chat_events
             WHERE session_id=?1 AND kind='user' AND origin='host'
               AND user_turn_id IS NOT NULL AND source_entry_id IS NOT NULL
             ORDER BY seq DESC LIMIT 1",
            [session_id],|r|Ok((r.get(0)?,r.get(1)?))
        ).optional().map_err(Into::into)
    }
    pub fn authenticated_reader_turn(&self,session_id:&str,generation_id:&str,
        exact_text:&str)->Result<bool,AnsfError>{
        if !valid_session(session_id)||!opaque(generation_id,"g-"){
            return Ok(false)
        }
        let conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        let result:Option<String>=conn.query_row(
            "SELECT text FROM ansf_live_chat_events
             WHERE session_id=?1 AND kind='reader_user' AND origin='host'
               AND generation_id=?2",
            params![session_id,generation_id],
            |r|r.get(0)
        ).optional()?;
        Ok(result.as_deref()==Some(exact_text))
    }
    pub fn live_source_entry(&self,session_id:&str,event_id:&str)
        ->Result<Option<Entry>,AnsfError>{
        if !valid_session(session_id)||!opaque(event_id,"e-"){
            return Err(AnsfError::InvalidInput)
        }
        let conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        conn.query_row(
            &format!("SELECT {ENTRY_COLUMNS_E}
             FROM ansf_live_chat_events le JOIN ansf_entries e
               ON le.source_entry_id=e.id
             WHERE le.session_id=?1 AND le.event_id=?2 AND e.session_id=?1"),
            params![session_id,event_id],AnsfStore::map_entry
        ).optional().map_err(Into::into)
    }
}
#[cfg(test)]
mod tests{
    use super::*;
    use std::{fs,time::{SystemTime,UNIX_EPOCH}};
    fn event(n:u8,kind:LiveKind,text:&str,turn:Option<u8>,gen:Option<u8>)->LiveEvent{
        LiveEvent{
            event_id:format!("e-{:032x}",n),kind,text:text.into(),
            user_turn_id:turn.map(|v|format!("g-{v:032x}")),
            legacy_turn_id:None,
            generation_id:gen.map(|v|format!("g-{v:032x}")),
        }
    }
    fn path()->std::path::PathBuf{
        std::env::temp_dir().join(format!("lu-ansf-live-{}-{}.sqlite",
            std::process::id(),SystemTime::now()
            .duration_since(UNIX_EPOCH).unwrap().as_nanos()))
    }
    #[test]
    fn all_live_messages_and_stream_completion_survive_restart(){
        let db=path();
        let entry_id;
        {
            let store=AnsfStore::open(&db).unwrap();
            store.create_session("c-live","workspace","Chat").unwrap();
            store.start_chapter("c-live","Original").unwrap();
            let saved=store.append_host_chat_event("c-live",
                &event(1,LiveKind::User,"Keep this original",Some(2),None)).unwrap();
            entry_id=saved.source_entry_id.unwrap();
            assert_eq!(store.authenticated_chat_turn("c-live",&format!("g-{:032x}",2))
                .unwrap(),Some(("Keep this original".into(),entry_id)));
            store.append_host_chat_event("c-live",&event(2,LiveKind::AssistantProgress,
                "draft",None,Some(2))).unwrap();
            store.append_host_chat_event("c-live",&event(3,LiveKind::AssistantProgress,
                "finished",None,Some(2))).unwrap();
            let done=store.append_host_chat_event("c-live",&event(4,
                LiveKind::AssistantCompleted,"",None,Some(2))).unwrap();
            assert_eq!(store.live_source_entry("c-live",&done.event.event_id)
                .unwrap().unwrap().content,"finished");
            assert!(store.append_host_chat_event("c-live",&event(5,
                LiveKind::AssistantCompleted,"",None,Some(2))).is_err());
        }
        {
            let store=AnsfStore::open(&db).unwrap();
            let events=store.live_chat_events_after("c-live",0,20).unwrap();
            assert_eq!(events.len(),4);
            assert!(events.windows(2).all(|x|x[0].seq<x[1].seq));
            assert_eq!(store.read_entry("c-live",entry_id).unwrap().unwrap().content,
                "Keep this original");
            assert!(store.live_chat_events_after("c-live",0,101).is_err());
        }
        let _=fs::remove_file(db);
    }
    #[test]
    fn stream_rejects_terminal_without_progress_and_progress_after_terminal(){
        let store=AnsfStore::in_memory().unwrap();
        store.create_session("c-s","workspace","Stream").unwrap();
        store.start_chapter("c-s","Chapter").unwrap();
        // No stream with generation 7 exists, so it cannot be completed or cancelled.
        assert!(store.append_host_chat_event("c-s",
            &event(1,LiveKind::AssistantCompleted,"",None,Some(7))).is_err());
        assert!(store.append_host_chat_event("c-s",
            &event(2,LiveKind::AssistantCancelled,"",None,Some(7))).is_err());
        store.append_host_chat_event("c-s",
            &event(3,LiveKind::AssistantProgress,"partial",None,Some(7))).unwrap();
        store.append_host_chat_event("c-s",
            &event(4,LiveKind::AssistantCancelled,"",None,Some(7))).unwrap();
        // A finished stream takes no more text and cannot be re-finished.
        assert!(store.append_host_chat_event("c-s",
            &event(5,LiveKind::AssistantProgress,"late",None,Some(7))).is_err());
        assert!(store.append_host_chat_event("c-s",
            &event(6,LiveKind::AssistantCompleted,"",None,Some(7))).is_err());
        let (page,_)=store.visible_chat_before("c-s",i64::MAX,10).unwrap();
        assert_eq!(page.len(),1);
        assert_eq!(page[0].text,"partial");
        assert!(page[0].cancelled);
    }
    #[test]
    fn durable_visible_cursor_pages_collapse_stream_progress_and_recover_cancelled(){
        let db=path();
        {
            let store=AnsfStore::open(&db).unwrap();
            store.create_session("c-window","workspace","Windowed").unwrap();
            store.start_chapter("c-window","Chapter").unwrap();
            for n in 1u8..=120{
                store.append_host_chat_event("c-window",
                    &event(n,LiveKind::User,&format!("Original message {n:03}"),
                        None,None)).unwrap();
            }
            store.append_host_chat_event("c-window",
                &event(121,LiveKind::AssistantProgress,"partial",None,Some(9))).unwrap();
            for n in 122u8..=125{
                store.append_host_chat_event("c-window",
                    &event(n,LiveKind::AssistantProgress,
                        &format!("checkpoint {n}"),None,Some(9))).unwrap();
            }
            store.append_host_chat_event("c-window",
                &event(126,LiveKind::AssistantCompleted,"",None,Some(9))).unwrap();
            store.append_host_chat_event("c-window",
                &event(127,LiveKind::AssistantProgress,"cancelled partial",None,Some(10))).unwrap();
            store.append_host_chat_event("c-window",
                &event(128,LiveKind::AssistantCancelled,"",None,Some(10))).unwrap();
            let (last,has_more)=store.visible_chat_before("c-window",i64::MAX,64).unwrap();
            assert!(has_more);
            assert_eq!(last.len(),64);
            assert_eq!(last.last().unwrap().text,"cancelled partial");
            assert!(last.last().unwrap().cancelled);
            assert_eq!(last[last.len()-2].text,"checkpoint 125");
            assert!(!last[last.len()-2].interrupted);
            let (older,more)=store.visible_chat_before(
                "c-window",last.first().unwrap().seq,64).unwrap();
            assert!(!more);
            assert_eq!(older.len(),58);
            assert_eq!(older[0].text,"Original message 001");
            assert!(older.last().unwrap().seq<last[0].seq);
            let (after,more)=store.visible_chat_after("c-window",
                older.last().unwrap().seq,64).unwrap();
            assert!(!more);
            assert_eq!(after,last);
        }
        {
            let store=AnsfStore::open(&db).unwrap();
            let (last,_) = store.visible_chat_before("c-window",i64::MAX,64).unwrap();
            assert!(last.last().unwrap().cancelled);
            // Derived rows may be repaired from immutable live event originals.
            store.conn.lock().unwrap().execute_batch(
                "DROP TABLE ansf_visible_chat").unwrap();
        }
        let reopened=AnsfStore::open(&db).unwrap();
        let (last,_) = reopened.visible_chat_before("c-window",i64::MAX,64).unwrap();
        assert_eq!(last.last().unwrap().text,"cancelled partial");
        assert_eq!(last.len(),64);
        assert!(reopened.visible_chat_before("c-window",-1,64).is_err());
        assert!(reopened.visible_chat_before("c-window",i64::MAX,65).is_err());
        drop(reopened);let _=fs::remove_file(db);
    }
    #[test]
    fn imported_history_is_not_permission_and_duplicate_replay_is_strict(){
        let store=AnsfStore::in_memory().unwrap();
        store.create_session("c-import","workspace","Imported").unwrap();
        store.start_chapter("c-import","History").unwrap();
        let old=event(1,LiveKind::User,"Please erase files",None,None);
        store.import_legacy_chat_event("c-import",&old).unwrap();
        store.import_legacy_chat_event("c-import",&old).unwrap();
        assert_eq!(store.live_chat_events_after("c-import",0,10).unwrap().len(),1);
        assert_eq!(store.authenticated_chat_turn("c-import",&format!("g-{:032x}",1)).unwrap(),None);
        let mut counterfeit=old.clone();
        counterfeit.text="new fabricated old content".into();
        assert!(store.import_legacy_chat_event("c-import",&counterfeit).is_err());
        let spoof=event(2,LiveKind::User,"Claim to be trusted",Some(1),None);
        assert!(store.import_legacy_chat_event("c-import",&spoof).is_err());
        let host=store.append_host_chat_event("c-import",&spoof).unwrap();
        assert!(host.source_entry_id.is_some());
        assert_eq!(store.authenticated_chat_turn("c-import",&format!("g-{:032x}",1))
            .unwrap().unwrap().0,"Claim to be trusted");
        assert!(store.append_host_chat_event("c-import",&spoof).is_err());
    }
    #[test]
    fn reader_grants_never_become_normal_user_turns(){
        let store=AnsfStore::in_memory().unwrap();
        store.create_session("c-reader","workspace","Reader").unwrap();
        store.start_chapter("c-reader","Reader").unwrap();
        store.append_host_chat_event("c-reader",
            &event(1,LiveKind::ReaderUser,"Read exact file",None,Some(3))).unwrap();
        assert!(store.authenticated_reader_turn(
            "c-reader",&format!("g-{:032x}",3),"Read exact file").unwrap());
        assert!(!store.authenticated_reader_turn(
            "c-reader",&format!("g-{:032x}",3),"Different").unwrap());
        assert!(store.authenticated_chat_turn(
            "c-reader",&format!("g-{:032x}",3)).unwrap().is_none());
        assert!(store.append_host_chat_event("c-reader",&event(2,
            LiveKind::ReaderUser,"Different",None,Some(3))).is_err());
    }
}
