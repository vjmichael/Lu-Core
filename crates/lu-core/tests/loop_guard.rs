use lu_core::{
    ansf::AnsfStore,
    ansf_live_chat::{LiveEvent, LiveKind},
    ansf_operations::OperationKind,
    journal::EffectJournal,
    loop_guard::{
        FailedCheck, Recovery, RecoveryOutcome, StreamLoopGuard, StreamLoopVerdict, ToolLoopGuard, ToolLoopLimits,
        ToolLoopVerdict, DEFAULT_REPAIR_CAP,
    },
    recorder::Recorder,
    snapshot::{SnapshotLimits, SnapshotStore},
};
use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

const GOOD: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";

#[test]
fn repeated_failed_repairs_restore_each_time_then_stop_the_model() {
    let base = std::env::temp_dir().join(format!(
        "lu-guard-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    let project = base.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("lib.rs"), GOOD).unwrap();
    let book = AnsfStore::open(&base.join("book.sqlite")).unwrap();
    book.create_session("c-1", "lu", "Chat").unwrap();
    book.start_chapter("c-1", "Start").unwrap();
    let turn = format!("g-{:032x}", 1);
    book.append_host_chat_event(
        "c-1",
        &LiveEvent {
            event_id: format!("e-{:032x}", 1),
            kind: LiveKind::User,
            text: "Fix the add function".into(),
            user_turn_id: Some(turn.clone()),
            legacy_turn_id: None,
            generation_id: None,
        },
    )
    .unwrap();
    let journal = EffectJournal::open(&base.join("effects.sqlite")).unwrap();
    let snapshots = SnapshotStore::open(&base.join("lu-data")).unwrap();
    let rec = Recorder::new(&book, &journal, "c-1");
    let db = base.join("repairs.sqlite");
    let paths = vec!["lib.rs".to_string()];

    // One attempt: snapshot, a bad edit by the model, a failed check.
    let attempt = |recovery: &Recovery, n: u32| {
        let snap = snapshots.take(&project, SnapshotLimits::default()).unwrap();
        fs::write(project.join("lib.rs"), format!("pub fn add(a: i32, b: i32) -> i32 {{ garbage {n}")).unwrap();
        recovery
            .check_failed(&FailedCheck {
                call_id: &format!("restore-{n}"),
                turn_id: &turn,
                job: "fix lib.rs",
                snapshot_id: snap.id,
                paths: &paths,
                what_failed: "cargo check: expected `}`",
            })
            .unwrap()
    };
    {
        let recovery = Recovery::new(&rec, &snapshots, DEFAULT_REPAIR_CAP, &db).unwrap();
        for n in 1..=2 {
            let outcome = attempt(&recovery, n);
            assert_eq!(outcome, RecoveryOutcome::Retry { restored: paths.clone(), failures: n, remaining: 3 - n });
            // Restore, don't patch: the damaged file is back to the last good state.
            assert_eq!(fs::read_to_string(project.join("lib.rs")).unwrap(), GOOD);
        }
    }
    // The count survives a restart.
    let recovery = Recovery::new(&rec, &snapshots, DEFAULT_REPAIR_CAP, &db).unwrap();
    assert_eq!(recovery.failures("fix lib.rs").unwrap(), 2);
    assert!(!recovery.is_stopped("fix lib.rs").unwrap());
    let outcome = attempt(&recovery, 3);
    assert_eq!(outcome, RecoveryOutcome::Stop { restored: paths.clone(), failures: 3 });
    assert_eq!(fs::read_to_string(project.join("lib.rs")).unwrap(), GOOD);
    assert!(recovery.is_stopped("fix lib.rs").unwrap());
    // Other jobs are counted separately.
    assert_eq!(recovery.failures("other job").unwrap(), 0);

    // Every restore is in the record: what failed, what was put back, and
    // that the model was stopped.
    let ops = book.active_operations("c-1", 0, 50).unwrap();
    let restores: Vec<_> = ops.iter().filter(|r| r.kind == OperationKind::FileChanged && r.call_id.starts_with("restore-")).collect();
    assert_eq!(restores.len(), 3);
    let last = ops.iter().rfind(|r| r.call_id == "restore-3" && r.kind == OperationKind::CommandFinished).unwrap();
    let summary = last.details.output.as_deref().unwrap();
    assert!(summary.contains("cargo check: expected `}`") && summary.contains("model stopped"), "{summary}");

    // A new instruction from the user starts the count again.
    recovery.reset("fix lib.rs").unwrap();
    assert_eq!(recovery.failures("fix lib.rs").unwrap(), 0);
    assert!(!recovery.is_stopped("fix lib.rs").unwrap());
    drop(recovery);

}

#[test]
fn stream_guard_stops_repetition_but_not_normal_text() {
    let mut looping = StreamLoopGuard::default();
    let mut verdict = StreamLoopVerdict::Continue;
    for _ in 0..2000 {
        verdict = looping.observe_chunk("I will fix it. ");
        if verdict != StreamLoopVerdict::Continue {
            break;
        }
    }
    assert_eq!(verdict, StreamLoopVerdict::RepeatedOutput);

    let mut prose = StreamLoopGuard::default();
    for n in 0..3000 {
        let line = format!("Step {n}: read file_{n}.rs, then update its tests and note the result.\n");
        assert_eq!(prose.observe_chunk(&line), StreamLoopVerdict::Continue, "stopped at line {n}");
    }

    let mut silent = StreamLoopGuard::default();
    let stops = (0..64).map(|_| silent.observe_chunk("")).last().unwrap();
    assert_eq!(stops, StreamLoopVerdict::RepeatedOutput);
}

#[test]
fn tool_guard_stops_the_same_action_and_runaway_turns() {
    let mut turn = ToolLoopGuard::new(ToolLoopLimits::default());
    assert_eq!(turn.observe("execute", "cargo test"), ToolLoopVerdict::Continue);
    assert_eq!(turn.observe("read", "src/lib.rs"), ToolLoopVerdict::Continue);
    assert_eq!(turn.observe("execute", "cargo test"), ToolLoopVerdict::Continue);
    assert_eq!(turn.observe("execute", "cargo test"), ToolLoopVerdict::SameAction { count: 3 });

    let mut busy = ToolLoopGuard::new(ToolLoopLimits { same_action: 3, per_turn: 50 });
    let last = (0..50).map(|n| busy.observe("read", &format!("file_{n}.rs"))).last().unwrap();
    assert_eq!(last, ToolLoopVerdict::TooManyActions { count: 50 });
}
