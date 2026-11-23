//! Goal and clarification events: validation and projection into the
//! current goal state. Storage lives in `ansf_goal_records`. This is task
//! *context*, never execution authority.
use crate::ansf::AnsfError;


const MAX_EVENTS:usize=1024;
const MAX_QUESTIONS:usize=64;

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum GoalEventKind { SetGoal, AskQuestion, RespondQuestion }

#[derive(Clone,Debug,PartialEq,Eq)]
pub struct GoalEvent {
    pub event_id:String,
    pub goal_id:String,
    pub question_id:Option<String>,
    pub kind:GoalEventKind,
    pub text:String,
    pub choice:Option<String>,
    pub comment:Option<String>,
}
#[derive(Clone,Debug,PartialEq,Eq)]
pub struct Clarification {
    pub question_id:String,
    pub question:String,
    pub status:String,
    pub choice:Option<String>,
    pub comment:Option<String>,
    pub opened_event_id:String,
    pub last_event_id:String,
}
#[derive(Clone,Debug,Default,PartialEq,Eq)]
pub struct GoalSnapshot {
    pub goal_id:Option<String>,
    pub goal:String,
    pub revision_event_id:Option<String>,
    pub questions:Vec<Clarification>,
}
fn opaque_id(id:&str,prefix:&str)->bool {
    id.len()==prefix.len()+32 && id.starts_with(prefix) &&
    id[prefix.len()..].bytes().all(|c|c.is_ascii_hexdigit()&&!c.is_ascii_uppercase())
}
fn text_ok(value:&str,max:usize)->bool{
    !value.trim().is_empty()&&value.len()<=max&&!value.contains('\0')
}
impl GoalEvent {
    pub fn validate(&self)->Result<(),AnsfError>{
        if !opaque_id(&self.event_id,"m-")||
           !opaque_id(&self.goal_id,"goal-"){
            return Err(AnsfError::InvalidInput)
        }
        if self.comment.as_ref().is_some_and(|c|c.len()>1024||c.contains('\0')){
            return Err(AnsfError::InvalidInput)
        }
        match self.kind {
            GoalEventKind::SetGoal=>{
                if self.question_id.is_some()||!text_ok(&self.text,4096)||
                   self.choice.is_some()||self.comment.is_some(){
                    return Err(AnsfError::InvalidInput)
                }
            },
            GoalEventKind::AskQuestion=>{
                if !self.question_id.as_deref().is_some_and(|q|opaque_id(q,"q-"))||
                   !text_ok(&self.text,1024)||self.choice.is_some()||
                   self.comment.is_some(){return Err(AnsfError::InvalidInput)}
            },
            GoalEventKind::RespondQuestion=>{
                if !self.question_id.as_deref().is_some_and(|q|opaque_id(q,"q-"))||
                   !self.text.is_empty()||
                   !matches!(self.choice.as_deref(),Some("yes"|"no"|"not_now")){
                    return Err(AnsfError::InvalidInput)
                }
            },
        }
        Ok(())
    }
}
/// Project only host-journaled events. Answered questions remain immutable;
/// Not now is a deferral, not implied consent and may later be answered.
/// A revised goal supersedes pending/deferred questions to avoid stale prompts.
pub fn project_goal_events(events:&[GoalEvent])->Result<GoalSnapshot,AnsfError>{
    if events.len()>MAX_EVENTS{return Err(AnsfError::InvalidInput);}
    let mut seen=std::collections::BTreeSet::new();
    let mut context=GoalSnapshot::default();
    for event in events{
        event.validate()?;
        if !seen.insert(event.event_id.as_str()){return Err(AnsfError::InvalidInput);}
        match event.kind{
            GoalEventKind::SetGoal=>{
                if context.goal_id.as_deref()!=Some(event.goal_id.as_str()) {
                    context.questions.clear();
                }else{
                    for q in &mut context.questions{
                        if q.status=="pending"||q.status=="deferred"{
                            q.status="superseded".into();
                        }
                    }
                }
                context.goal_id=Some(event.goal_id.clone());
                context.goal=event.text.clone();
                context.revision_event_id=Some(event.event_id.clone());
            },
            GoalEventKind::AskQuestion=>{
                if context.goal_id.as_deref()!=Some(event.goal_id.as_str())||
                   context.questions.len()>=MAX_QUESTIONS||
                   context.questions.iter().any(|q|Some(q.question_id.as_str())==
                       event.question_id.as_deref()){
                    return Err(AnsfError::InvalidInput)
                }
                context.questions.push(Clarification{
                    question_id:event.question_id.clone().unwrap_or_default(),
                    question:event.text.clone(),status:"pending".into(),
                    choice:None,comment:None,
                    opened_event_id:event.event_id.clone(),
                    last_event_id:event.event_id.clone(),
                });
            },
            GoalEventKind::RespondQuestion=>{
                if context.goal_id.as_deref()!=Some(event.goal_id.as_str()){
                    return Err(AnsfError::InvalidInput)
                }
                let q=context.questions.iter_mut().find(|q|
                    Some(q.question_id.as_str())==event.question_id.as_deref())
                    .ok_or(AnsfError::InvalidInput)?;
                if !matches!(q.status.as_str(),"pending"|"deferred"){
                    return Err(AnsfError::InvalidInput)
                }
                q.status=if event.choice.as_deref()==Some("not_now"){
                    "deferred".into()
                }else{"answered".into()};
                q.choice=event.choice.clone();
                q.comment=event.comment.clone();
                q.last_event_id=event.event_id.clone();
            }
        }
    }
    Ok(context)
}
#[cfg(test)]
mod tests{
    use super::*;
    fn event(n:u8,k:GoalEventKind,q:Option<String>,text:&str,choice:Option<&str>)->GoalEvent{
        GoalEvent{event_id:format!("m-{:032x}",n),goal_id:format!("goal-{:032x}",1),
            question_id:q,kind:k,text:text.into(),choice:choice.map(str::to_owned),comment:None}
    }
    #[test]
    fn goal_revision_question_deferral_and_answer(){
        let q=format!("q-{:032x}",2);
        let records=[
            event(1,GoalEventKind::SetGoal,None,"Create a screenplay",None),
            event(2,GoalEventKind::AskQuestion,Some(q.clone()),"Documentary?",None),
            event(3,GoalEventKind::RespondQuestion,Some(q.clone()),"",Some("not_now")),
            event(4,GoalEventKind::RespondQuestion,Some(q),"",Some("yes")),
        ];
        let view=project_goal_events(&records).unwrap();
        assert_eq!(view.questions[0].status,"answered");
        assert_eq!(view.questions[0].choice.as_deref(),Some("yes"));
        let mut illegal=records.to_vec();
        illegal.push(event(5,GoalEventKind::RespondQuestion,
            illegal[1].question_id.clone(),"",Some("no")));
        assert!(project_goal_events(&illegal).is_err());
        let revised=event(6,GoalEventKind::SetGoal,None,"Create a feature film",None);
        illegal=records[..3].to_vec(); illegal.push(revised);
        assert_eq!(project_goal_events(&illegal).unwrap().questions[0].status,"superseded");
    }
}
