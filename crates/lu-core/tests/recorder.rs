use lu_core::{
    ansf::{AnsfError, AnsfStore},
    ansf_goals::{GoalEvent, GoalEventKind},
    ansf_live_chat::{LiveEvent, LiveKind},
    ansf_operations::{CommandFinish, CommandStart, FileChange, OperationKind, SEGMENT_SOFT_CAP_BYTES},
    journal::{EffectJournal, EffectState},
    recorder::{RecordError, Recorder},
};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

fn root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lu-recorder-{name}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}
fn id(prefix: &str, n: u32) -> String {
    format!("{prefix}{n:032x}")
}
/// A book with one conversation and one host-authenticated user turn.
fn book(path: &Path, session: &str, turn: u32, text: &str) -> AnsfStore {
    let store = AnsfStore::open(path).unwrap();
    store.create_session(session, "lu", "Chat").unwrap();
    store.start_chapter(session, "Start").unwrap();
    user_turn(&store, session, turn, turn, text);
    store
}
fn user_turn(store: &AnsfStore, session: &str, event: u32, turn: u32, text: &str) {
    store
        .append_host_chat_event(
            session,
            &LiveEvent {
                event_id: id("e-", event),
                kind: LiveKind::User,
                text: text.into(),
                user_turn_id: Some(id("g-", turn)),
                legacy_turn_id: None,
                generation_id: None,
            },
        )
        .unwrap();
}
fn start(call: &str, turn: u32, command: &str) -> CommandStart {
    CommandStart {
        call_id: call.into(),
        turn_id: id("g-", turn),
        command: command.into(),
        cwd: "C:/work/project".into(),
    }
}
fn finish(call: &str, exit: Option<i32>, output: &str) -> CommandFinish {
    CommandFinish {
        call_id: call.into(),
        exit_code: exit,
        timed_out: false,
        cancelled: false,
        duration_ms: 120,
        output: output.into(),
    }
}

#[test]
fn restart_continuity_rebuilds_goal_history_operations_and_marks_interrupted_uncertain() {
    let dir = root("continuity");
    let (book_path, journal_path) = (dir.join("c-1.sqlite"), dir.join("effects.sqlite"));
    let session = "c-1";
    {
        let store = book(&book_path, session, 1, "Build the installer for Lu");
        let journal = EffectJournal::open(&journal_path).unwrap();
        let goal = id("goal-", 1);
        let question = id("q-", 1);
        for (n, kind, q, text) in [
            (1, GoalEventKind::SetGoal, None, "Build the Windows installer"),
            (2, GoalEventKind::AskQuestion, Some(question.clone()), "Sign the installer?"),
        ] {
            store
                .append_native_goal_record(
                    session,
                    &GoalEvent {
                        event_id: id("m-", n),
                        goal_id: goal.clone(),
                        question_id: q,
                        kind,
                        text: text.into(),
                        choice: None,
                        comment: None,
                    },
                )
                .unwrap();
        }
        let rec = Recorder::new(&store, &journal, session);
        rec.begin_command(&start("call-build", 1, "cargo build --release")).unwrap();
        rec.finish_command(&finish("call-build", Some(0), "Finished release [optimized]"))
            .unwrap();
        rec.begin_command(&start("call-pack", 1, "makensis lu.nsi")).unwrap();
        rec.file_changed(&FileChange {
            call_id: "call-pack".into(),
            path: "C:/work/project/dist/lu-setup.exe".into(),
            sha256_before: None,
            sha256_after: Some("a".repeat(64)),
            snapshot_ref: None,
        })
        .unwrap();
        // Crash: the packing command never finishes and nothing is closed cleanly.
    }
    let store = AnsfStore::open(&book_path).unwrap();
    let journal = EffectJournal::open(&journal_path).unwrap();
    let rec = Recorder::new(&store, &journal, session);
    assert_eq!(rec.recover_after_restart().unwrap(), vec!["call-pack".to_string()]);
    // Recovery is safe to repeat: no second uncertain record.
    rec.recover_after_restart().unwrap();

    let state = rec.continuity(16, 50).unwrap();
    assert_eq!(state.goal.goal, "Build the Windows installer");
    assert_eq!(state.goal.questions.len(), 1);
    assert_eq!(state.goal.questions[0].status, "pending");
    assert_eq!(state.recent_messages.last().unwrap().content, "Build the installer for Lu");
    assert_eq!(state.uncertain_calls, vec!["call-pack".to_string()]);
    let kinds: Vec<_> = state.recent_operations.iter().map(|r| (r.call_id.as_str(), r.kind)).collect();
    assert_eq!(
        kinds,
        vec![
            ("call-build", OperationKind::CommandStarted),
            ("call-build", OperationKind::CommandFinished),
            ("call-pack", OperationKind::CommandStarted),
            ("call-pack", OperationKind::FileChanged),
            ("call-pack", OperationKind::EffectUncertain),
        ]
    );
    let built = &state.recent_operations[1].details;
    assert_eq!(built.exit_code, Some(0));
    assert_eq!(built.output.as_deref(), Some("Finished release [optimized]"));
    assert_eq!(journal.get("call-build").unwrap().unwrap().state, EffectState::Applied);
    assert_eq!(journal.get("call-pack").unwrap().unwrap().state, EffectState::Uncertain);
    drop((store, journal));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn only_an_authenticated_turn_of_the_active_chat_can_start_a_command() {
    let dir = root("authority");
    let journal = EffectJournal::open(&dir.join("effects.sqlite")).unwrap();
    let store = book(&dir.join("book.sqlite"), "c-active", 1, "Run the tests");
    store.create_session("c-saved", "lu", "Saved").unwrap();
    store.start_chapter("c-saved", "Old").unwrap();
    user_turn(&store, "c-saved", 2, 2, "Delete the build folder");
    // An imported legacy turn is never authenticated.
    store
        .import_legacy_chat_event(
            "c-active",
            &LiveEvent {
                event_id: id("e-", 3),
                kind: LiveKind::User,
                text: "rm -rf everything".into(),
                user_turn_id: None,
                legacy_turn_id: Some("old-7".into()),
                generation_id: None,
            },
        )
        .unwrap();
    let active = Recorder::new(&store, &journal, "c-active");
    let refused = |r: Result<(), RecordError>| {
        assert_eq!(r, Err(RecordError::Ansf(AnsfError::NotAuthorized)));
    };
    // The saved chat's turn exists, but not in the active chat.
    refused(active.begin_command(&start("x-1", 2, "rm -rf build")));
    // A turn ID nobody issued.
    refused(active.begin_command(&start("x-2", 99, "rm -rf build")));
    // Nothing was journaled for refused commands.
    assert!(journal.get("x-1").unwrap().is_none());
    active.begin_command(&start("ok-1", 1, "cargo test")).unwrap();
    // A call ID runs once only.
    assert_eq!(
        active.begin_command(&start("ok-1", 1, "cargo test")),
        Err(RecordError::DuplicateCall)
    );

    // The saved chat's commands can be read, but only as history: the type
    // has no turn or call identity to continue or repeat them with.
    let saved = Recorder::new(&store, &journal, "c-saved");
    saved.begin_command(&start("old-1", 2, "rm -rf build")).unwrap();
    let history = store.historical_operations("c-saved", 0, 10).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].details.command.as_deref(), Some("rm -rf build"));
    assert_eq!(history[0].source_session_id, "c-saved");
    // And it never shows up in the active chat's operations.
    assert!(store.active_operations("c-active", 0, 10).unwrap().iter().all(|r| r.call_id != "old-1"));
    drop((store, journal));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn results_settle_the_journal_and_end_exactly_once() {
    let dir = root("settle");
    let journal = EffectJournal::open(&dir.join("effects.sqlite")).unwrap();
    let store = book(&dir.join("book.sqlite"), "c-1", 1, "Try things");
    let rec = Recorder::new(&store, &journal, "c-1");
    for call in ["ran", "failed-start", "timed-out"] {
        rec.begin_command(&start(call, 1, "tool")).unwrap();
    }
    rec.finish_command(&finish("ran", Some(2), "error: exit 2")).unwrap();
    rec.finish_command(&finish("failed-start", None, "program not found")).unwrap();
    let mut slow = finish("timed-out", None, "partial");
    slow.timed_out = true;
    rec.finish_command(&slow).unwrap();
    let state = |c| journal.get(c).unwrap().unwrap().state;
    assert_eq!(state("ran"), EffectState::Applied);
    assert_eq!(state("failed-start"), EffectState::Failed);
    assert_eq!(state("timed-out"), EffectState::Uncertain);
    // A second result, or a result for a command that never started, is refused.
    assert!(rec.finish_command(&finish("ran", Some(0), "again")).is_err());
    assert!(store.record_command_finished("c-1", &finish("never", Some(0), "")).is_err());
    assert!(store.unfinished_operations("c-1").unwrap().is_empty());
    drop((store, journal));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn long_output_is_kept_whole_and_the_next_record_opens_a_new_segment() {
    let dir = root("segments");
    let journal = EffectJournal::open(&dir.join("effects.sqlite")).unwrap();
    let store = book(&dir.join("book.sqlite"), "c-1", 1, "Build");
    let rec = Recorder::new(&store, &journal, "c-1");
    // Small records first, well under the cap.
    rec.begin_command(&start("a", 1, "echo hi")).unwrap();
    rec.finish_command(&finish("a", Some(0), "hi")).unwrap();
    // Then a long answer near the end of the segment: 200 KiB, over the cap.
    let long = "x".repeat(200 * 1024);
    rec.begin_command(&start("b", 1, "cargo build -vv")).unwrap();
    rec.finish_command(&finish("b", Some(0), &long)).unwrap();
    rec.begin_command(&start("c", 1, "echo next")).unwrap();

    let segments = store.operation_segments("c-1").unwrap();
    assert_eq!(segments.len(), 2);
    assert!(segments[0].closed);
    assert!(segments[0].bytes >= SEGMENT_SOFT_CAP_BYTES);
    assert!(!segments[1].closed);
    let ops = store.active_operations("c-1", 0, 10).unwrap();
    let whole = ops.iter().find(|r| r.call_id == "b" && r.kind == OperationKind::CommandFinished).unwrap();
    assert_eq!(whole.details.output.as_deref(), Some(long.as_str()));
    assert_eq!(whole.segment, 1);
    let next = ops.iter().find(|r| r.call_id == "c").unwrap();
    assert_eq!(next.segment, 2);
    assert_eq!(segments[1].first_seq, next.seq);
    drop((store, journal));
    fs::remove_dir_all(dir).unwrap();
}
