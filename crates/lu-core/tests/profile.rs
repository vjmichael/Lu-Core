use lu_core::{
    ansf::AnsfStore,
    ansf_goals::{GoalEvent, GoalEventKind},
    ansf_live_chat::{LiveEvent, LiveKind},
    contracts::{ProtoPayload, ToolInvocation},
    journal::EffectJournal,
    memory::{MemoryInjector, DEFAULT_BUDGET_CHARS},
    profile::{
        Augmentation, MemoryMode, ModelRef, ProfileError, ProfileStore, Route, RouteRequest, TaskKind,
        MODEL_DRIVEN_BUDGET_CHARS,
    },
    recorder::Recorder,
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static N: AtomicU32 = AtomicU32::new(1);
fn base(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lu-profile-{name}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}
/// A journal effect: applied (Some(true)), failed (Some(false)) or still running (None).
fn effect(journal: &EffectJournal, passed: Option<bool>) -> String {
    let call_id = format!("call-{}", N.fetch_add(1, Ordering::SeqCst));
    journal
        .prepare(&ToolInvocation {
            call_id: call_id.clone(),
            tool_id: "run".into(),
            workspace_id: None,
            input: ProtoPayload { type_url: "lu/test".into(), bytes: call_id.as_bytes().to_vec() },
        })
        .unwrap();
    journal.mark_executing(&call_id).unwrap();
    match passed {
        Some(true) => journal.mark_applied(&call_id, b"ok").unwrap(),
        Some(false) => journal.mark_failed(&call_id).unwrap(),
        None => {}
    }
    call_id
}
fn outcomes(p: &ProfileStore, j: &EffectJournal, m: &ModelRef, kind: TaskKind, aug: Augmentation, pass: u32, fail: u32) {
    for i in 0..pass + fail {
        p.record_outcome(m, kind, aug, &effect(j, Some(i < pass)), j).unwrap();
    }
}

#[test]
fn outcomes_come_only_from_the_journal() {
    let dir = base("evidence");
    let p = ProfileStore::open(&dir.join("profile.sqlite")).unwrap();
    let j = EffectJournal::open(&dir.join("effects.sqlite")).unwrap();
    let m = ModelRef::new("qwen3.6-14b", "Q4_K_M");
    p.register_model(&m, false).unwrap();
    let sched = TaskKind::Scheduling;
    let none = Augmentation::None;

    assert!(matches!(p.record_outcome(&m, sched, none, "never-ran", &j), Err(ProfileError::NoEvidence(_))));
    let running = effect(&j, None);
    assert!(matches!(p.record_outcome(&m, sched, none, &running, &j), Err(ProfileError::NotVerified(_))));
    let uncertain = effect(&j, None);
    j.mark_uncertain(&uncertain).unwrap();
    assert!(matches!(p.record_outcome(&m, sched, none, &uncertain, &j), Err(ProfileError::NotVerified(_))));

    let ok = effect(&j, Some(true));
    assert!(p.record_outcome(&m, sched, none, &ok, &j).unwrap());
    // Counting again is a no-op; counting it as something else is refused.
    assert!(p.record_outcome(&m, sched, none, &ok, &j).unwrap());
    assert!(matches!(p.record_outcome(&m, TaskKind::MultiStepMath, none, &ok, &j), Err(ProfileError::AlreadyCounted(_))));
    let bad = effect(&j, Some(false));
    assert!(!p.record_outcome(&m, sched, none, &bad, &j).unwrap());
    let s = p.stats(&m, sched).unwrap();
    assert_eq!((s.plain.passed, s.plain.failed, s.augmented.samples()), (1, 1, 0));

    // A different quantization is a different, empty profile.
    let q8 = ModelRef::new("qwen3.6-14b", "Q8_0");
    let fresh = effect(&j, Some(true));
    assert!(matches!(p.record_outcome(&q8, sched, none, &fresh, &j), Err(ProfileError::UnknownModel(_))));
    p.register_model(&q8, false).unwrap();
    assert_eq!(p.stats(&q8, sched).unwrap().plain.samples(), 0);
}

#[test]
fn untrained_versus_trained_model_same_continuity() {
    let dir = base("trained");
    let p = ProfileStore::open(&dir.join("profile.sqlite")).unwrap();
    let untrained = ModelRef::new("qwen3.6-14b", "Q4_K_M");
    let trained = ModelRef::new("qwen3.8-27b", "Q4_K_M");
    p.register_model(&untrained, false).unwrap();
    p.register_model(&trained, false).unwrap();

    // Both start on the Lu-driven path.
    for m in [&untrained, &trained] {
        let plan = p.memory_plan(m).unwrap();
        assert_eq!((plan.mode, plan.budget_chars), (MemoryMode::LuDriven, DEFAULT_BUDGET_CHARS));
    }
    // What the host observed over eight turns.
    for t in 0..8 {
        p.record_memory_use(&untrained, &format!("t{t}"), false).unwrap();
        p.record_memory_use(&trained, &format!("t{t}"), t != 3).unwrap();
    }
    // One observation per turn: repeating a turn changes nothing.
    p.record_memory_use(&trained, "t3", true).unwrap();
    let a = p.memory_plan(&untrained).unwrap();
    let b = p.memory_plan(&trained).unwrap();
    assert_eq!((a.mode, a.budget_chars), (MemoryMode::LuDriven, DEFAULT_BUDGET_CHARS));
    assert_eq!((b.mode, b.budget_chars, b.observed, b.unprompted), (MemoryMode::ModelDriven, MODEL_DRIVEN_BUDGET_CHARS, 8, 7));

    // The user sees the same continuity: both blocks carry the goal and the
    // open question, the trained model's within its smaller budget.
    let book = AnsfStore::open(&dir.join("book.sqlite")).unwrap();
    book.create_session("c-1", "lu", "Installer").unwrap();
    book.start_chapter("c-1", "Start").unwrap();
    let goal = |n: u32, kind, q: Option<u32>, text: &str| {
        book.append_native_goal_record(
            "c-1",
            &GoalEvent {
                event_id: format!("m-{n:032x}"),
                goal_id: format!("goal-{:032x}", 1),
                question_id: q.map(|q| format!("q-{q:032x}")),
                kind,
                text: text.into(),
                choice: None,
                comment: None,
            },
        )
        .unwrap();
    };
    goal(1, GoalEventKind::SetGoal, None, "Ship a signed Windows installer for Lu");
    goal(2, GoalEventKind::AskQuestion, Some(1), "Use the test certificate or the release one?");
    for n in 1..40u32 {
        let user = n % 2 == 1;
        book.append_host_chat_event(
            "c-1",
            &LiveEvent {
                event_id: format!("e-{n:032x}"),
                kind: if user { LiveKind::User } else { LiveKind::AssistantFinal },
                text: format!("Message {n} about the installer layout and its many details, kept long on purpose."),
                user_turn_id: user.then(|| format!("g-{n:032x}")),
                legacy_turn_id: None,
                generation_id: None,
            },
        )
        .unwrap();
    }
    let j = EffectJournal::open(&dir.join("effects.sqlite")).unwrap();
    let rec = Recorder::new(&book, &j, "c-1");
    let memory = MemoryInjector::new(&rec).unwrap();
    let turn = format!("g-{:032x}", 39);
    let full = memory.build(&turn, &[], a.budget_chars).unwrap();
    let small = memory.build(&turn, &[], b.budget_chars).unwrap();
    assert!(small.text.chars().count() <= MODEL_DRIVEN_BUDGET_CHARS);
    for block in [&full, &small] {
        assert!(block.text.contains("Ship a signed Windows installer"), "{}", block.text);
        assert!(block.text.contains("test certificate or the release one"), "{}", block.text);
    }
    assert!(full.items.len() >= small.items.len());

    // If the trained model stops recalling on its own, Lu takes over again.
    for t in 8..14 {
        p.record_memory_use(&trained, &format!("t{t}"), false).unwrap();
    }
    assert_eq!(p.memory_plan(&trained).unwrap().mode, MemoryMode::LuDriven);
}

#[test]
fn cognitum_is_offered_only_where_the_record_shows_it_helps() {
    let dir = base("augment");
    let p = ProfileStore::open(&dir.join("profile.sqlite")).unwrap();
    let j = EffectJournal::open(&dir.join("effects.sqlite")).unwrap();
    let small = ModelRef::new("qwen3.6-4b", "Q4_K_M");
    p.register_model(&small, false).unwrap();
    let req = |kind| RouteRequest { kind, has_images: false };
    let offered = |kind| matches!(p.route(&small, &req(kind), true).unwrap(), Route::Main { offer_cognitum: true, .. });

    // No record yet: nothing assumed.
    assert!(!offered(TaskKind::Scheduling));
    // Scheduling: 1/4 alone, 4/4 with Cognitum -> offered.
    outcomes(&p, &j, &small, TaskKind::Scheduling, Augmentation::None, 1, 3);
    assert!(!offered(TaskKind::Scheduling), "no augmented samples yet");
    outcomes(&p, &j, &small, TaskKind::Scheduling, Augmentation::Cognitum, 4, 0);
    assert!(offered(TaskKind::Scheduling));
    // Puzzles: Cognitum did not help -> not offered.
    outcomes(&p, &j, &small, TaskKind::ReasoningPuzzle, Augmentation::None, 3, 1);
    outcomes(&p, &j, &small, TaskKind::ReasoningPuzzle, Augmentation::Cognitum, 2, 2);
    assert!(!offered(TaskKind::ReasoningPuzzle));
    // Not connected: never offered, even where it helps.
    assert!(matches!(p.route(&small, &req(TaskKind::Scheduling), false).unwrap(), Route::Main { offer_cognitum: false, .. }));
}

#[test]
fn exact_work_goes_to_cognitum_and_core_runs_without_it() {
    let dir = base("exact");
    let p = ProfileStore::open(&dir.join("profile.sqlite")).unwrap();
    let m = ModelRef::new("qwen3.8-27b", "Q4_K_M");
    p.register_model(&m, false).unwrap();
    let req = RouteRequest { kind: TaskKind::ExactArithmetic, has_images: false };
    assert_eq!(p.route(&m, &req, true).unwrap(), Route::Cognitum);
    match p.route(&m, &req, false).unwrap() {
        Route::Main { note: Some(note), .. } => assert!(note.contains("not checked by an engine")),
        other => panic!("{other:?}"),
    }
    assert!(TaskKind::ALL.iter().all(|k| TaskKind::parse(k.as_str()) == Some(*k)));
    let unknown = ModelRef::new("not-registered", "Q4_K_M");
    assert!(matches!(p.route(&unknown, &req, true), Err(ProfileError::UnknownModel(_))));
}

#[test]
fn images_go_to_a_multimodal_model() {
    let dir = base("images");
    let p = ProfileStore::open(&dir.join("profile.sqlite")).unwrap();
    let j = EffectJournal::open(&dir.join("effects.sqlite")).unwrap();
    let text = ModelRef::new("qwen3.8-27b", "Q4_K_M");
    p.register_model(&text, false).unwrap();
    let req = RouteRequest { kind: TaskKind::General, has_images: true };
    assert_eq!(p.route(&text, &req, false).unwrap(), Route::NoMultimodalModel);

    let vl_a = ModelRef::new("qwen3-vl-8b", "Q4_K_M");
    let vl_b = ModelRef::new("gemma-vision-12b", "Q4_K_M");
    p.register_model(&vl_a, true).unwrap();
    p.register_model(&vl_b, true).unwrap();
    outcomes(&p, &j, &vl_a, TaskKind::General, Augmentation::None, 1, 3);
    outcomes(&p, &j, &vl_b, TaskKind::General, Augmentation::None, 3, 1);
    assert_eq!(p.route(&text, &req, false).unwrap(), Route::Multimodal(vl_b.clone()));
    // A multimodal current model keeps its own images.
    assert_eq!(p.route(&vl_a, &req, false).unwrap(), Route::Multimodal(vl_a));
    // Without images, the text model keeps the turn.
    let plain = RouteRequest { kind: TaskKind::General, has_images: false };
    assert!(matches!(p.route(&text, &plain, false).unwrap(), Route::Main { .. }));
}

#[test]
fn a_struggling_model_gets_a_stronger_one_suggested() {
    let dir = base("stronger");
    let p = ProfileStore::open(&dir.join("profile.sqlite")).unwrap();
    let j = EffectJournal::open(&dir.join("effects.sqlite")).unwrap();
    let weak = ModelRef::new("qwen3.6-4b", "Q4_K_M");
    let mid = ModelRef::new("qwen3.6-14b", "Q4_K_M");
    let strong = ModelRef::new("qwen3.8-27b", "Q4_K_M");
    let unproven = ModelRef::new("new-model", "Q4_K_M");
    for m in [&weak, &mid, &strong, &unproven] {
        p.register_model(m, false).unwrap();
    }
    let math = TaskKind::MultiStepMath;
    outcomes(&p, &j, &weak, math, Augmentation::None, 1, 4);
    outcomes(&p, &j, &mid, math, Augmentation::None, 3, 2);
    outcomes(&p, &j, &strong, math, Augmentation::None, 5, 0);
    outcomes(&p, &j, &unproven, math, Augmentation::None, 1, 0);
    let s = p.suggest_stronger(&weak, math).unwrap().unwrap();
    assert_eq!(s.model, strong);
    assert_eq!((s.current.passed, s.current.failed), (1, 4));
    // The route carries it.
    match p.route(&weak, &RouteRequest { kind: math, has_images: false }, false).unwrap() {
        Route::Main { suggest: Some(s), .. } => assert_eq!(s.model, strong),
        other => panic!("{other:?}"),
    }
    // Doing fine, or too few samples: nothing suggested.
    assert!(p.suggest_stronger(&mid, math).unwrap().is_none());
    assert!(p.suggest_stronger(&unproven, math).unwrap().is_none());
    assert!(p.suggest_stronger(&weak, TaskKind::Scheduling).unwrap().is_none());
}

#[test]
fn the_record_survives_a_restart() {
    let dir = base("restart");
    let path = dir.join("profile.sqlite");
    let j = EffectJournal::open(&dir.join("effects.sqlite")).unwrap();
    let m = ModelRef::new("qwen3.6-14b", "Q4_K_M");
    {
        let p = ProfileStore::open(&path).unwrap();
        p.register_model(&m, false).unwrap();
        outcomes(&p, &j, &m, TaskKind::Coding, Augmentation::None, 2, 1);
    }
    let p = ProfileStore::open(&path).unwrap();
    let s = p.stats(&m, TaskKind::Coding).unwrap();
    assert_eq!((s.plain.passed, s.plain.failed), (2, 1));
}
