//! M2 native relational ANSF goal records, no JSON event serialization.
//!
//! Old ansf_goals::mirror_goal_event remains a legacy migration adapter;
//! current user decisions are persisted directly as typed SQLite columns.
//! Goal/clarification memory is data, not an execution capability.
use crate::ansf::{AnsfError,AnsfStore};
use crate::ansf_goals::{GoalEvent,GoalEventKind,GoalSnapshot,project_goal_events};
use rusqlite::{params,Connection,OptionalExtension};

pub(crate) fn initialize_goal_record_schema(conn:&Connection)->Result<(),AnsfError>{
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ansf_goal_records (
           seq INTEGER PRIMARY KEY AUTOINCREMENT,
           session_id TEXT NOT NULL REFERENCES ansf_sessions(id),
           event_id TEXT NOT NULL UNIQUE,
           goal_id TEXT NOT NULL,
           question_id TEXT,
           kind TEXT NOT NULL CHECK(kind IN ('set_goal','ask_question','respond_question')),
           text TEXT NOT NULL,
           choice TEXT CHECK(choice IS NULL OR choice IN ('yes','no','not_now')),
           comment TEXT,
           source_origin TEXT NOT NULL CHECK(source_origin IN ('host','legacy')),
           created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );
         CREATE INDEX IF NOT EXISTS ansf_goal_records_session_seq
            ON ansf_goal_records(session_id,seq);
         CREATE TRIGGER IF NOT EXISTS ansf_goal_records_immutable_update
            BEFORE UPDATE ON ansf_goal_records
            BEGIN SELECT RAISE(ABORT,'ANSF goal records are immutable'); END;
         CREATE TRIGGER IF NOT EXISTS ansf_goal_records_immutable_delete
            BEFORE DELETE ON ansf_goal_records
            BEGIN SELECT RAISE(ABORT,'ANSF goal records are immutable'); END;"
    )?;
    Ok(())
}
fn kind_value(k:GoalEventKind)->&'static str{
    match k{GoalEventKind::SetGoal=>"set_goal",
        GoalEventKind::AskQuestion=>"ask_question",
        GoalEventKind::RespondQuestion=>"respond_question"}
}
fn parse_kind(value:&str)->Result<GoalEventKind,AnsfError>{
    match value{"set_goal"=>Ok(GoalEventKind::SetGoal),
        "ask_question"=>Ok(GoalEventKind::AskQuestion),
        "respond_question"=>Ok(GoalEventKind::RespondQuestion),
        _=>Err(AnsfError::InvalidInput)}
}
/// Records of the current goal only: everything after the last record that
/// belongs to a different goal. Setting a new goal clears all questions, so
/// earlier goals never affect the projection. Loading only this run keeps a
/// long conversation from hitting the projection's event cap.
fn load_current_goal(conn:&Connection,session_id:&str)->Result<Vec<GoalEvent>,AnsfError>{
    let events=load_rows(conn,session_id,
        "AND seq>COALESCE((SELECT MAX(seq) FROM ansf_goal_records
           WHERE session_id=?1 AND goal_id<>(SELECT goal_id FROM ansf_goal_records
             WHERE session_id=?1 ORDER BY seq DESC LIMIT 1)),0)")?;
    project_goal_events(&events)?;
    Ok(events)
}
fn load_rows(conn:&Connection,session_id:&str,filter:&str)->Result<Vec<GoalEvent>,AnsfError>{
    let mut stmt=conn.prepare(&format!(
        "SELECT event_id,goal_id,question_id,kind,text,choice,comment
         FROM ansf_goal_records WHERE session_id=?1 {filter} ORDER BY seq"
    ))?;
    let rows=stmt.query_map([session_id],|r|{
        Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,
            r.get::<_,Option<String>>(2)?,r.get::<_,String>(3)?,
            r.get::<_,String>(4)?,r.get::<_,Option<String>>(5)?,
            r.get::<_,Option<String>>(6)?))
    })?;
    let mut result=Vec::new();
    for row in rows{
        let (event_id,goal_id,question_id,kind,text,choice,comment)=row?;
        let event=GoalEvent{event_id,goal_id,question_id,
            kind:parse_kind(&kind)?,text,choice,comment};
        event.validate()?;
        result.push(event);
    }
    Ok(result)
}
fn valid_session(session_id:&str)->bool{
    !session_id.is_empty()&&session_id.len()<=128&&!session_id.contains('\0')
}
impl AnsfStore{
    /// Every goal record of the conversation, oldest first.
    pub fn native_goal_records(&self,session_id:&str)->Result<Vec<GoalEvent>,AnsfError>{
        if !valid_session(session_id){return Err(AnsfError::InvalidInput)}
        let conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        load_rows(&conn,session_id,"")
    }
    pub fn native_goal_snapshot(&self,session_id:&str)->Result<GoalSnapshot,AnsfError>{
        if !valid_session(session_id){return Err(AnsfError::InvalidInput)}
        let conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        project_goal_events(&load_current_goal(&conn,session_id)?)
    }
    /// Appends a verified UI-host decision. Old imported goal answers must
    /// instead use import_observed_goal_record; they are historical data only.
    pub fn append_native_goal_record(&self,session_id:&str,event:&GoalEvent)
        ->Result<GoalSnapshot,AnsfError>{
        self.append_record(session_id,event,"host",false)
    }
    pub fn import_observed_goal_record(&self,session_id:&str,event:&GoalEvent)
        ->Result<GoalSnapshot,AnsfError>{
        self.append_record(session_id,event,"legacy",true)
    }
    fn append_record(&self,session_id:&str,event:&GoalEvent,origin:&str,idempotent:bool)
        ->Result<GoalSnapshot,AnsfError>{
        event.validate()?;
        if !valid_session(session_id){return Err(AnsfError::InvalidInput)}
        let mut conn=self.conn.lock().map_err(|_|AnsfError::Locked)?;
        let tx=conn.transaction()?;
        let existing:Option<(String,GoalEvent)>=tx.query_row(
            "SELECT source_origin,goal_id,question_id,kind,text,choice,comment
             FROM ansf_goal_records WHERE event_id=?1",
            [&event.event_id],|r|{
                Ok((r.get::<_,String>(0)?,
                    r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,
                    r.get::<_,String>(3)?,r.get::<_,String>(4)?,
                    r.get::<_,Option<String>>(5)?,r.get::<_,Option<String>>(6)?))
            }
        ).optional()?.map(|(saved_origin,goal_id,question_id,kind,text,choice,comment)|{
            Ok::<_,AnsfError>((saved_origin,GoalEvent{
                event_id:event.event_id.clone(),goal_id,question_id,
                kind:parse_kind(&kind)?,text,choice,comment,
            }))
        }).transpose()?;
        if let Some((saved_origin,old))=existing{
            if idempotent&&saved_origin==origin&&old==*event {
                return project_goal_events(&load_current_goal(&tx,session_id)?)
            }
            return Err(AnsfError::InvalidInput)
        }
        // A new goal starts from nothing; otherwise continue the current run.
        let current=load_current_goal(&tx,session_id)?;
        let continues=current.last().is_some_and(|e|e.goal_id==event.goal_id);
        let mut prior=if continues{current}else{Vec::new()};
        prior.push(event.clone());
        let state=project_goal_events(&prior)?;
        tx.execute(
            "INSERT INTO ansf_goal_records(session_id,event_id,goal_id,question_id,
                kind,text,choice,comment,source_origin)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![session_id,event.event_id,event.goal_id,event.question_id,
                kind_value(event.kind),event.text,event.choice,event.comment,origin],
        )?;
        tx.commit()?;
        Ok(state)
    }
}
#[cfg(test)]
mod tests{
    use super::*;
    fn event(n:u8,k:GoalEventKind,q:Option<String>,text:&str,choice:Option<&str>)->GoalEvent{
        GoalEvent{event_id:format!("m-{n:032x}"),goal_id:format!("goal-{:032x}",1),
            question_id:q,kind:k,text:text.into(),
            choice:choice.map(str::to_owned),comment:None}
    }
    #[test]
    fn long_conversation_with_many_goals_never_locks_out(){
        let store=AnsfStore::in_memory().unwrap();
        store.create_session("c-long","local","Long").unwrap();
        // 1,200 records across 600 goals; the old code refused everything
        // after 1,024 records in a conversation.
        for g in 0..600u32{
            let goal=format!("goal-{g:032x}");
            let q=format!("q-{g:032x}");
            let set=GoalEvent{event_id:format!("m-{:032x}",g*2),goal_id:goal.clone(),
                question_id:None,kind:GoalEventKind::SetGoal,text:format!("Goal {g}"),
                choice:None,comment:None};
            let ask=GoalEvent{event_id:format!("m-{:032x}",g*2+1),goal_id:goal,
                question_id:Some(q),kind:GoalEventKind::AskQuestion,text:"Ready?".into(),
                choice:None,comment:None};
            store.append_native_goal_record("c-long",&set).unwrap();
            store.append_native_goal_record("c-long",&ask).unwrap();
        }
        let state=store.native_goal_snapshot("c-long").unwrap();
        assert_eq!(state.goal,"Goal 599");
        assert_eq!(state.questions.len(),1);
        assert_eq!(store.native_goal_records("c-long").unwrap().len(),1200);
    }
    #[test]
    fn relational_goal_records_no_json_and_explicit_deferral(){
        let store=AnsfStore::in_memory().unwrap();
        store.create_session("c-1","local","Test").unwrap();
        let q=format!("q-{:032x}",8);
        store.append_native_goal_record("c-1",
            &event(1,GoalEventKind::SetGoal,None,"Build Lu",None)).unwrap();
        store.append_native_goal_record("c-1",
            &event(2,GoalEventKind::AskQuestion,Some(q.clone()),"Use tests?",None)).unwrap();
        let mut answer=event(3,GoalEventKind::RespondQuestion,
            Some(q.clone()),"",Some("not_now"));
        assert_eq!(store.append_native_goal_record("c-1",&answer)
            .unwrap().questions[0].status,"deferred");
        assert!(store.append_native_goal_record("c-1",&answer).is_err());
        answer=event(4,GoalEventKind::RespondQuestion,Some(q),"",Some("yes"));
        let state=store.append_native_goal_record("c-1",&answer).unwrap();
        assert_eq!(state.questions[0].status,"answered");
        assert!(store.native_goal_records("c-2").unwrap().is_empty());
        let old=event(5,GoalEventKind::SetGoal,None,"Legacy goal",None);
        assert!(store.import_observed_goal_record("c-1",&old).is_ok());
        assert!(store.import_observed_goal_record("c-1",&old).is_ok());
        let mut altered=old.clone();altered.text="Changed legacy".into();
        assert!(store.import_observed_goal_record("c-1",&altered).is_err());
    }
}
