//! Generic operational record: every command Lu runs, its result, and every
//! file it changes, written by the host into the conversation's ANSF book.
//!
//! Recording is generic. It does not depend on what the command means, so it
//! works for any command and needs no per-tool reader.
//!
//! **Access is not permission, and permission is not authority.** Each record
//! is tied to the host-authenticated user turn of *this* conversation that
//! started it. Records read from another (saved) conversation come back as
//! [`HistoricalOperation`], a separate type with no turn and no call identity,
//! so history can inform the model but can never prescribe an action in the
//! active chat.
//!
//! **Adaptive segments.** The record is grouped into segments of about
//! [`SEGMENT_SOFT_CAP_BYTES`]. A record is never split: one that crosses the
//! cap is stored whole in the current segment, which then closes, and the next
//! record opens a new segment.
use rusqlite::{params,Connection,OptionalExtension,Transaction};
use crate::ansf::{AnsfError,AnsfStore};

/// Soft size of one log segment. Never a limit on a single record.
pub const SEGMENT_SOFT_CAP_BYTES:i64=64*1024;
/// Largest single command output kept inline. Bigger output belongs in an
/// external file linked by an artifact reference (the command runner's job).
pub const MAX_OUTPUT_BYTES:usize=16*1024*1024;
const MAX_COMMAND_BYTES:usize=64*1024;
const MAX_PATH_BYTES:usize=4096;
const MAX_PAGE:usize=200;

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub enum OperationKind{
    /// The host started a command. Written before the process is spawned.
    CommandStarted,
    /// The command ended (or could not start); carries the full result.
    CommandFinished,
    /// A file changed during the command, with hashes and snapshot reference.
    FileChanged,
    /// The command was interrupted (crash, restart) and its effect is unknown.
    EffectUncertain,
}
impl OperationKind{
    fn as_str(self)->&'static str{
        match self{
            Self::CommandStarted=>"command_started",
            Self::CommandFinished=>"command_finished",
            Self::FileChanged=>"file_changed",
            Self::EffectUncertain=>"effect_uncertain",
        }
    }
    fn parse(v:&str)->Option<Self>{
        match v{
            "command_started"=>Some(Self::CommandStarted),
            "command_finished"=>Some(Self::CommandFinished),
            "file_changed"=>Some(Self::FileChanged),
            "effect_uncertain"=>Some(Self::EffectUncertain),
            _=>None,
        }
    }
}

#[derive(Debug,Clone,PartialEq,Eq)]
pub struct CommandStart{
    pub call_id:String,
    /// Host-authenticated user turn of this conversation that asked for it.
    pub turn_id:String,
    pub command:String,
    pub cwd:String,
}
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct CommandFinish{
    pub call_id:String,
    /// None when the process never started or was killed without a status.
    pub exit_code:Option<i32>,
    pub timed_out:bool,
    pub cancelled:bool,
    pub duration_ms:u64,
    /// Full output, stdout and stderr as the runner captured them.
    pub output:String,
}
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct FileChange{
    pub call_id:String,
    pub path:String,
    /// Lowercase hex SHA-256; None when the file did not exist.
    pub sha256_before:Option<String>,
    /// Lowercase hex SHA-256; None when the file was removed.
    pub sha256_after:Option<String>,
    /// Snapshot store reference to the pre-change copy, when one was taken.
    pub snapshot_ref:Option<String>,
}

/// Everything a record can carry. Unused fields are None for its kind.
#[derive(Debug,Clone,PartialEq,Eq,Default)]
pub struct OperationDetails{
    pub command:Option<String>,
    pub cwd:Option<String>,
    pub exit_code:Option<i32>,
    pub timed_out:bool,
    pub cancelled:bool,
    pub duration_ms:Option<u64>,
    pub output:Option<String>,
    pub path:Option<String>,
    pub sha256_before:Option<String>,
    pub sha256_after:Option<String>,
    pub snapshot_ref:Option<String>,
}

/// A record of the ACTIVE conversation, carrying the turn that authorized it.
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct OperationRecord{
    pub seq:i64,
    pub segment:i64,
    pub kind:OperationKind,
    pub call_id:String,
    pub turn_id:String,
    pub details:OperationDetails,
}

/// A record read from a saved conversation. It deliberately has no turn and
/// no call identity: it is history the model may read, never an instruction
/// and never a handle to repeat or continue an action.
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct HistoricalOperation{
    pub source_session_id:String,
    pub seq:i64,
    pub kind:OperationKind,
    pub details:OperationDetails,
}

#[derive(Debug,Clone,PartialEq,Eq)]
pub struct Segment{
    pub segment:i64,
    pub first_seq:i64,
    pub last_seq:i64,
    pub bytes:i64,
    pub closed:bool,
}

fn bounded(s:&str,max:usize)->bool{!s.is_empty()&&s.len()<=max&&!s.contains('\0')}
fn opaque(value:&str,prefix:&str)->bool{
    value.len()==prefix.len()+32&&value.starts_with(prefix)&&
        value[prefix.len()..].bytes().all(|b|b.is_ascii_hexdigit()&&!b.is_ascii_uppercase())
}
fn valid_sha(s:&str)->bool{
    s.len()==64&&s.bytes().all(|b|b.is_ascii_hexdigit()&&!b.is_ascii_uppercase())
}
fn valid_session(s:&str)->bool{!s.trim().is_empty()&&s.len()<=128&&!s.contains('\0')}

pub(crate) fn initialize_operations_schema(conn:&Connection)->Result<(),AnsfError>{
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ansf_operation_segments(
           session_id TEXT NOT NULL REFERENCES ansf_sessions(id),
           segment INTEGER NOT NULL CHECK(segment>0),
           first_seq INTEGER NOT NULL,
           last_seq INTEGER NOT NULL,
           bytes INTEGER NOT NULL CHECK(bytes>=0),
           closed INTEGER NOT NULL DEFAULT 0 CHECK(closed IN (0,1)),
           PRIMARY KEY(session_id,segment)
         );
         CREATE UNIQUE INDEX IF NOT EXISTS ansf_one_open_segment
           ON ansf_operation_segments(session_id) WHERE closed=0;
         CREATE TABLE IF NOT EXISTS ansf_operations(
           seq INTEGER PRIMARY KEY AUTOINCREMENT,
           session_id TEXT NOT NULL REFERENCES ansf_sessions(id),
           segment INTEGER NOT NULL,
           kind TEXT NOT NULL CHECK(kind IN
             ('command_started','command_finished','file_changed','effect_uncertain')),
           call_id TEXT NOT NULL,
           turn_id TEXT NOT NULL,
           command TEXT,
           cwd TEXT,
           exit_code INTEGER,
           timed_out INTEGER NOT NULL DEFAULT 0 CHECK(timed_out IN (0,1)),
           cancelled INTEGER NOT NULL DEFAULT 0 CHECK(cancelled IN (0,1)),
           duration_ms INTEGER CHECK(duration_ms IS NULL OR duration_ms>=0),
           output TEXT,
           path TEXT,
           sha256_before TEXT,
           sha256_after TEXT,
           snapshot_ref TEXT,
           bytes INTEGER NOT NULL CHECK(bytes>=0),
           created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
           FOREIGN KEY(session_id,segment) REFERENCES ansf_operation_segments(session_id,segment)
         );
         CREATE INDEX IF NOT EXISTS ansf_operations_cursor
           ON ansf_operations(session_id,seq);
         CREATE INDEX IF NOT EXISTS ansf_operations_call
           ON ansf_operations(session_id,call_id,kind);
         CREATE UNIQUE INDEX IF NOT EXISTS ansf_operations_one_start
           ON ansf_operations(call_id) WHERE kind='command_started';
         CREATE UNIQUE INDEX IF NOT EXISTS ansf_operations_one_end
           ON ansf_operations(call_id)
           WHERE kind IN ('command_finished','effect_uncertain');
         CREATE TRIGGER IF NOT EXISTS ansf_operations_no_update
           BEFORE UPDATE ON ansf_operations BEGIN
             SELECT RAISE(ABORT,'ANSF operation records are immutable');
           END;
         CREATE TRIGGER IF NOT EXISTS ansf_operations_no_delete
           BEFORE DELETE ON ansf_operations BEGIN
             SELECT RAISE(ABORT,'ANSF operation records are immutable');
           END;"
    )?;
    Ok(())
}

const RECORD_COLUMNS:&str="seq,segment,kind,call_id,turn_id,command,cwd,exit_code,\
    timed_out,cancelled,duration_ms,output,path,sha256_before,sha256_after,snapshot_ref";

fn read_record(r:&rusqlite::Row<'_>)->rusqlite::Result<OperationRecord>{
    let kind:String=r.get(2)?;
    let duration:Option<i64>=r.get(10)?;
    Ok(OperationRecord{
        seq:r.get(0)?,segment:r.get(1)?,
        kind:OperationKind::parse(&kind).ok_or(rusqlite::Error::InvalidQuery)?,
        call_id:r.get(3)?,turn_id:r.get(4)?,
        details:OperationDetails{
            command:r.get(5)?,cwd:r.get(6)?,exit_code:r.get(7)?,
            timed_out:r.get::<_,i64>(8)?==1,cancelled:r.get::<_,i64>(9)?==1,
            duration_ms:duration.map(|d|d as u64),output:r.get(11)?,path:r.get(12)?,
            sha256_before:r.get(13)?,sha256_after:r.get(14)?,snapshot_ref:r.get(15)?,
        },
    })
}

/// The turn a call belongs to, from its start record in this conversation.
fn started_turn(tx:&Transaction<'_>,session_id:&str,call_id:&str)
    ->Result<Option<String>,AnsfError>{
    tx.query_row(
        "SELECT turn_id FROM ansf_operations
         WHERE session_id=?1 AND call_id=?2 AND kind='command_started'",
        params![session_id,call_id],|r|r.get(0)
    ).optional().map_err(Into::into)
}

/// Append one record to the open segment, opening one if needed. The record
/// is stored whole; the segment closes once it reaches the soft cap.
fn append(tx:&Transaction<'_>,session_id:&str,kind:OperationKind,call_id:&str,
    turn_id:&str,d:&OperationDetails)->Result<i64,AnsfError>{
    let bytes=[&d.command,&d.cwd,&d.output,&d.path,&d.sha256_before,
        &d.sha256_after,&d.snapshot_ref].iter()
        .map(|v|v.as_deref().map_or(0,str::len)).sum::<usize>()
        +call_id.len()+turn_id.len();
    let bytes=bytes as i64;
    let open:Option<(i64,i64)>=tx.query_row(
        "SELECT segment,bytes FROM ansf_operation_segments
         WHERE session_id=?1 AND closed=0",[session_id],|r|Ok((r.get(0)?,r.get(1)?))
    ).optional()?;
    let segment=match open{
        Some((segment,_))=>segment,
        None=>{
            let next:i64=tx.query_row(
                "SELECT COALESCE(MAX(segment),0)+1 FROM ansf_operation_segments
                 WHERE session_id=?1",[session_id],|r|r.get(0))?;
            tx.execute(
                "INSERT INTO ansf_operation_segments(session_id,segment,first_seq,last_seq,bytes)
                 VALUES(?1,?2,0,0,0)",params![session_id,next])?;
            next
        }
    };
    tx.execute(
        "INSERT INTO ansf_operations(session_id,segment,kind,call_id,turn_id,command,cwd,
           exit_code,timed_out,cancelled,duration_ms,output,path,sha256_before,
           sha256_after,snapshot_ref,bytes)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
        params![session_id,segment,kind.as_str(),call_id,turn_id,d.command,d.cwd,
            d.exit_code,d.timed_out as i64,d.cancelled as i64,
            d.duration_ms.map(|v|v as i64),d.output,d.path,d.sha256_before,
            d.sha256_after,d.snapshot_ref,bytes])?;
    let seq=tx.last_insert_rowid();
    tx.execute(
        "UPDATE ansf_operation_segments
         SET first_seq=CASE WHEN first_seq=0 THEN ?3 ELSE first_seq END,
             last_seq=?3, bytes=bytes+?4,
             closed=CASE WHEN bytes+?4>=?5 THEN 1 ELSE 0 END
         WHERE session_id=?1 AND segment=?2",
        params![session_id,segment,seq,bytes,SEGMENT_SOFT_CAP_BYTES])?;
    Ok(seq)
}

impl AnsfStore{
    /// Record a command before it is spawned. The turn must be a
    /// host-authenticated user turn of THIS conversation: an imported turn,
    /// a Reader turn, a turn from another conversation or recalled text
    /// cannot start an action.
    pub fn record_command_started(&self,session_id:&str,start:&CommandStart)
        ->Result<i64,AnsfError>{
        if !valid_session(session_id)||!bounded(&start.call_id,128)||
           !opaque(&start.turn_id,"g-")||!bounded(&start.command,MAX_COMMAND_BYTES)||
           !bounded(&start.cwd,MAX_PATH_BYTES){
            return Err(AnsfError::InvalidInput)
        }
        let mut conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        let tx=conn.transaction()?;
        let authenticated:bool=tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM ansf_live_chat_events
             WHERE session_id=?1 AND kind='user' AND origin='host'
               AND user_turn_id=?2 AND source_entry_id IS NOT NULL)",
            params![session_id,start.turn_id],|r|r.get(0))?;
        if !authenticated{return Err(AnsfError::NotAuthorized)}
        let seq=append(&tx,session_id,OperationKind::CommandStarted,&start.call_id,
            &start.turn_id,&OperationDetails{command:Some(start.command.clone()),
                cwd:Some(start.cwd.clone()),..Default::default()})?;
        tx.commit()?;
        Ok(seq)
    }

    /// Record a command's full result. Exactly one end record per command.
    pub fn record_command_finished(&self,session_id:&str,finish:&CommandFinish)
        ->Result<i64,AnsfError>{
        if !valid_session(session_id)||!bounded(&finish.call_id,128)||
           finish.output.len()>MAX_OUTPUT_BYTES||finish.output.contains('\0')||
           finish.duration_ms>i64::MAX as u64{
            return Err(AnsfError::InvalidInput)
        }
        let mut conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        let tx=conn.transaction()?;
        let turn=started_turn(&tx,session_id,&finish.call_id)?
            .ok_or(AnsfError::InvalidInput)?;
        let seq=append(&tx,session_id,OperationKind::CommandFinished,&finish.call_id,&turn,
            &OperationDetails{exit_code:finish.exit_code,timed_out:finish.timed_out,
                cancelled:finish.cancelled,duration_ms:Some(finish.duration_ms),
                output:Some(finish.output.clone()),..Default::default()})
            .map_err(already_ended)?;
        tx.commit()?;
        Ok(seq)
    }

    /// Record one changed file of a started command.
    pub fn record_file_change(&self,session_id:&str,change:&FileChange)
        ->Result<i64,AnsfError>{
        if !valid_session(session_id)||!bounded(&change.call_id,128)||
           !bounded(&change.path,MAX_PATH_BYTES)||
           change.sha256_before.as_deref().is_some_and(|s|!valid_sha(s))||
           change.sha256_after.as_deref().is_some_and(|s|!valid_sha(s))||
           (change.sha256_before.is_none()&&change.sha256_after.is_none())||
           change.snapshot_ref.as_deref().is_some_and(|s|!bounded(s,512)){
            return Err(AnsfError::InvalidInput)
        }
        let mut conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        let tx=conn.transaction()?;
        let turn=started_turn(&tx,session_id,&change.call_id)?
            .ok_or(AnsfError::InvalidInput)?;
        let seq=append(&tx,session_id,OperationKind::FileChanged,&change.call_id,&turn,
            &OperationDetails{path:Some(change.path.clone()),
                sha256_before:change.sha256_before.clone(),
                sha256_after:change.sha256_after.clone(),
                snapshot_ref:change.snapshot_ref.clone(),..Default::default()})?;
        tx.commit()?;
        Ok(seq)
    }

    /// Mark a started command as interrupted with an unknown effect. Does
    /// nothing if the command already has an end record, so it is safe to
    /// repeat after every restart.
    pub fn record_effect_uncertain(&self,session_id:&str,call_id:&str)
        ->Result<Option<i64>,AnsfError>{
        if !valid_session(session_id)||!bounded(call_id,128){
            return Err(AnsfError::InvalidInput)
        }
        let mut conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        let tx=conn.transaction()?;
        let Some(turn)=started_turn(&tx,session_id,call_id)? else{return Ok(None)};
        let ended:bool=tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM ansf_operations WHERE session_id=?1 AND call_id=?2
             AND kind IN ('command_finished','effect_uncertain'))",
            params![session_id,call_id],|r|r.get(0))?;
        if ended{return Ok(None)}
        let seq=append(&tx,session_id,OperationKind::EffectUncertain,call_id,&turn,
            &OperationDetails::default())?;
        tx.commit()?;
        Ok(Some(seq))
    }

    /// Records of the ACTIVE conversation after a cursor, oldest first.
    pub fn active_operations(&self,active_session_id:&str,after_seq:i64,limit:usize)
        ->Result<Vec<OperationRecord>,AnsfError>{
        if !valid_session(active_session_id)||after_seq<0||limit==0||limit>MAX_PAGE{
            return Err(AnsfError::InvalidInput)
        }
        let conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        let mut stmt=conn.prepare(&format!(
            "SELECT {RECORD_COLUMNS} FROM ansf_operations
             WHERE session_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3"))?;
        let rows=stmt.query_map(params![active_session_id,after_seq,limit as i64],read_record)?;
        rows.collect::<Result<Vec<_>,_>>().map_err(Into::into)
    }

    /// The newest records of the active conversation, oldest first.
    pub fn recent_operations(&self,active_session_id:&str,limit:usize)
        ->Result<Vec<OperationRecord>,AnsfError>{
        if !valid_session(active_session_id)||limit==0||limit>MAX_PAGE{
            return Err(AnsfError::InvalidInput)
        }
        let conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        let mut stmt=conn.prepare(&format!(
            "SELECT {RECORD_COLUMNS} FROM ansf_operations
             WHERE session_id=?1 ORDER BY seq DESC LIMIT ?2"))?;
        let mut rows=stmt.query_map(params![active_session_id,limit as i64],read_record)?
            .collect::<Result<Vec<_>,_>>()?;
        rows.reverse();
        Ok(rows)
    }

    /// Records of a SAVED conversation, as history only. The caller must
    /// already have decided the reader may see that conversation; this
    /// returns no turn or call identity, so nothing here can authorize,
    /// continue or repeat an action in the active chat.
    pub fn historical_operations(&self,source_session_id:&str,after_seq:i64,limit:usize)
        ->Result<Vec<HistoricalOperation>,AnsfError>{
        Ok(self.active_operations(source_session_id,after_seq,limit)?.into_iter()
            .map(|r|HistoricalOperation{source_session_id:source_session_id.to_owned(),
                seq:r.seq,kind:r.kind,details:r.details}).collect())
    }

    /// Call IDs of commands that started but have no end record yet.
    pub fn unfinished_operations(&self,session_id:&str)->Result<Vec<String>,AnsfError>{
        if !valid_session(session_id){return Err(AnsfError::InvalidInput)}
        let conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        let mut stmt=conn.prepare(
            "SELECT s.call_id FROM ansf_operations s
             WHERE s.session_id=?1 AND s.kind='command_started'
               AND NOT EXISTS(SELECT 1 FROM ansf_operations e
                 WHERE e.session_id=s.session_id AND e.call_id=s.call_id
                   AND e.kind IN ('command_finished','effect_uncertain'))
             ORDER BY s.seq")?;
        let rows=stmt.query_map([session_id],|r|r.get::<_,String>(0))?;
        rows.collect::<Result<Vec<_>,_>>().map_err(Into::into)
    }

    pub fn operation_segments(&self,session_id:&str)->Result<Vec<Segment>,AnsfError>{
        if !valid_session(session_id){return Err(AnsfError::InvalidInput)}
        let conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        let mut stmt=conn.prepare(
            "SELECT segment,first_seq,last_seq,bytes,closed FROM ansf_operation_segments
             WHERE session_id=?1 ORDER BY segment")?;
        let rows=stmt.query_map([session_id],|r|Ok(Segment{segment:r.get(0)?,
            first_seq:r.get(1)?,last_seq:r.get(2)?,bytes:r.get(3)?,
            closed:r.get::<_,i64>(4)?==1}))?;
        rows.collect::<Result<Vec<_>,_>>().map_err(Into::into)
    }
}

/// A second end record hits the unique index; report it as bad input.
fn already_ended(e:AnsfError)->AnsfError{
    match e{AnsfError::Database(m) if m.contains("UNIQUE")=>AnsfError::InvalidInput,other=>other}
}
