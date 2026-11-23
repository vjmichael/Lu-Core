//! The host's recorder for one conversation: every command, result and file
//! change goes into both the effect journal (what state each effect is in)
//! and the conversation's ANSF book (what happened, in full).
//!
//! Order of writes, and why:
//! 1. Check that the turn is a host-authenticated turn of this conversation.
//! 2. Journal PREPARED then EXECUTING, before anything runs.
//! 3. ANSF `command_started`. If this fails, the journal is marked FAILED,
//!    which is true: nothing has run yet.
//! 4. The caller runs the command.
//! 5. ANSF `command_finished` with the full output, then the journal's final
//!    state. A crash between the two leaves the journal EXECUTING, which
//!    becomes UNCERTAIN on restart. That errs on the safe side: the output
//!    is kept, and the effect is still treated as unknown.
//!
//! The model never writes here. Recording is not permission: a recorded
//! command from a saved conversation is only history (see `ansf_operations`).
use sha2::{Digest,Sha256};

use crate::ansf::{AnsfError,AnsfStore,Entry};
use crate::ansf_goals::GoalSnapshot;
use crate::ansf_operations::{CommandFinish,CommandStart,FileChange,OperationRecord};
use crate::contracts::{ProtoPayload,ToolInvocation};
use crate::journal::{EffectJournal,JournalError,PrepareOutcome};

/// Tool ID under which every recorded command is journaled.
pub const COMMAND_TOOL_ID:&str="lu.command";

#[derive(Debug,Clone,PartialEq,Eq)]
pub enum RecordError{
    Ansf(AnsfError),
    Journal(JournalError),
    /// The call ID was already used; a call is never started twice.
    DuplicateCall,
}
impl From<AnsfError> for RecordError{fn from(e:AnsfError)->Self{Self::Ansf(e)}}
impl From<JournalError> for RecordError{fn from(e:JournalError)->Self{Self::Journal(e)}}

/// What a new turn needs to continue where the conversation left off,
/// rebuilt from the record rather than from any summary.
#[derive(Debug,Clone,PartialEq)]
pub struct Continuity{
    /// The current goal, its constraints and open questions.
    pub goal:GoalSnapshot,
    /// The latest user and assistant messages (current revisions only).
    pub recent_messages:Vec<Entry>,
    /// The latest commands, results and file changes.
    pub recent_operations:Vec<OperationRecord>,
    /// Commands whose effect is unknown. Never retried blindly.
    pub uncertain_calls:Vec<String>,
}

pub struct Recorder<'a>{
    book:&'a AnsfStore,
    journal:&'a EffectJournal,
    session_id:String,
}

impl<'a> Recorder<'a>{
    pub fn new(book:&'a AnsfStore,journal:&'a EffectJournal,session_id:impl Into<String>)->Self{
        Self{book,journal,session_id:session_id.into()}
    }

    /// Record a command before it runs. Returns only when both stores have
    /// it, so the caller may then spawn the process.
    pub fn begin_command(&self,start:&CommandStart)->Result<(),RecordError>{
        if self.book.authenticated_chat_turn(&self.session_id,&start.turn_id)?.is_none(){
            return Err(RecordError::Ansf(AnsfError::NotAuthorized))
        }
        let invocation=ToolInvocation{
            call_id:start.call_id.clone(),
            tool_id:COMMAND_TOOL_ID.into(),
            workspace_id:Some(self.session_id.clone()),
            input:ProtoPayload{
                type_url:"lu.command.v1".into(),
                bytes:length_prefixed(&[&start.turn_id,&start.cwd,&start.command]),
            },
        };
        if self.journal.prepare(&invocation)?!=PrepareOutcome::New{
            return Err(RecordError::DuplicateCall)
        }
        self.journal.mark_executing(&start.call_id)?;
        if let Err(e)=self.book.record_command_started(&self.session_id,start){
            self.journal.mark_failed(&start.call_id)?;
            return Err(e.into())
        }
        Ok(())
    }

    /// Record one file the running command changed.
    pub fn file_changed(&self,change:&FileChange)->Result<(),RecordError>{
        self.book.record_file_change(&self.session_id,change)?;
        Ok(())
    }

    /// Record the full result and settle the effect's state:
    /// - no exit status and neither timed out nor cancelled: it never
    ///   started, so FAILED (no effect);
    /// - timed out or cancelled: it may have done part of its work, so
    ///   UNCERTAIN;
    /// - otherwise it ran to the end: APPLIED, whatever its exit code.
    pub fn finish_command(&self,finish:&CommandFinish)->Result<(),RecordError>{
        self.book.record_command_finished(&self.session_id,finish)?;
        let id=&finish.call_id;
        if finish.timed_out||finish.cancelled{
            self.journal.mark_uncertain(id)?;
        }else if let Some(code)=finish.exit_code{
            let receipt=format!("exit={code};output_sha256={:x}",
                Sha256::digest(finish.output.as_bytes()));
            self.journal.mark_applied(id,receipt.as_bytes())?;
        }else{
            self.journal.mark_failed(id)?;
        }
        Ok(())
    }

    /// Run once at startup, only after the previous Lu process has stopped.
    /// Every effect that was still running becomes UNCERTAIN in the journal,
    /// and each uncertain effect of this conversation gets an
    /// `effect_uncertain` record in ANSF (once). Returns those call IDs.
    pub fn recover_after_restart(&self)->Result<Vec<String>,RecordError>{
        self.journal.reconcile_interrupted()?;
        let uncertain=self.journal.uncertain_effects(&self.session_id)?;
        for call_id in &uncertain{
            self.book.record_effect_uncertain(&self.session_id,call_id)?;
        }
        Ok(uncertain)
    }

    /// Rebuild the working context for a new turn from the record.
    pub fn continuity(&self,messages:usize,operations:usize)->Result<Continuity,RecordError>{
        Ok(Continuity{
            goal:self.book.native_goal_snapshot(&self.session_id)?,
            recent_messages:self.book.recent_context_entries(&self.session_id,messages)?,
            recent_operations:self.book.recent_operations(&self.session_id,operations)?,
            uncertain_calls:self.journal.uncertain_effects(&self.session_id)?,
        })
    }
}

fn length_prefixed(parts:&[&str])->Vec<u8>{
    let mut out=Vec::new();
    for part in parts{
        out.extend((part.len() as u64).to_le_bytes());
        out.extend(part.as_bytes());
    }
    out
}
