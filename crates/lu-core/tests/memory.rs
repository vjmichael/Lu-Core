use lu_core::{
    ansf::AnsfStore,
    ansf_goals::{GoalEvent, GoalEventKind},
    ansf_live_chat::{LiveEvent, LiveKind},
    ansf_operations::{CommandFinish, CommandStart},
    ansf_source_ids::AnsfSourceId,
    journal::EffectJournal,
    memory::{EnvFact, ItemStatus, MemoryInjector, DEFAULT_BUDGET_CHARS},
    recorder::Recorder,
};
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

fn base(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lu-memory-{name}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}
fn user(book: &AnsfStore, n: u32, text: &str) -> String {
    let turn = format!("g-{n:032x}");
    book.append_host_chat_event(
        "c-1",
        &LiveEvent {
            event_id: format!("e-{n:032x}"),
            kind: LiveKind::User,
            text: text.into(),
            user_turn_id: Some(turn.clone()),
            legacy_turn_id: None,
            generation_id: None,
        },
    )
    .unwrap();
    turn
}
fn answer(book: &AnsfStore, n: u32, text: &str) {
    book.append_host_chat_event(
        "c-1",
        &LiveEvent {
            event_id: format!("e-{n:032x}"),
            kind: LiveKind::AssistantFinal,
            text: text.into(),
            user_turn_id: None,
            legacy_turn_id: None,
            generation_id: None,
        },
    )
    .unwrap();
}
fn goal(book: &AnsfStore, n: u32, kind: GoalEventKind, q: Option<u32>, text: &str) {
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
}

/// A long conversation: an installer path stated early, many later turns,
/// commands with long output, an open question and an interrupted command.
fn long_conversation(dir: &std::path::Path) -> (AnsfStore, EffectJournal, String) {
    let book = AnsfStore::open(&dir.join("book.sqlite")).unwrap();
    book.create_session("c-1", "lu", "Installer work").unwrap();
    book.start_chapter("c-1", "Start").unwrap();
    let journal = EffectJournal::open(&dir.join("effects.sqlite")).unwrap();
    user(&book, 1, "The NSIS installer compiler is at C:/tools/nsis/makensis.exe, use that one.");
    answer(&book, 2, "Noted: makensis.exe from C:/tools/nsis.");
    goal(&book, 1, GoalEventKind::SetGoal, None, "Ship a signed Windows installer for Lu");
    goal(&book, 2, GoalEventKind::AskQuestion, Some(1), "Use the test certificate or the release one?");
    let mut n = 3;
    let mut turn = String::new();
    for i in 0..60 {
        turn = user(&book, n, &format!("Step {i}: adjust the window layout and colours"));
        answer(&book, n + 1, &format!("Done with layout step {i}."));
        n += 2;
    }
    let rec = Recorder::new(&book, &journal, "c-1");
    for i in 0..20 {
        let call = format!("build-{i}");
        rec.begin_command(&CommandStart { call_id: call.clone(), turn_id: turn.clone(), command: format!("cargo build -p lu --release # {i}"), cwd: "C:/work/lu".into() }).unwrap();
        rec.finish_command(&CommandFinish { call_id: call, exit_code: Some(0), timed_out: false, cancelled: false, duration_ms: 900, output: format!("{}\nFinished release build {i}", "Compiling lu ... ".repeat(200)) }).unwrap();
    }
    rec.begin_command(&CommandStart { call_id: "pack".into(), turn_id: turn.clone(), command: "makensis lu.nsi".into(), cwd: "C:/work/lu".into() }).unwrap();
    drop(rec);
    let rec = Recorder::new(&book, &journal, "c-1");
    rec.recover_after_restart().unwrap();
    drop(rec);
    let last = user(&book, 999, "Now build the installer again");
    (book, journal, last)
}

#[test]
fn bounded_injection_stays_in_budget_and_every_item_has_source_and_status() {
    let dir = base("bounded");
    let (book, journal, turn) = long_conversation(&dir);
    let rec = Recorder::new(&book, &journal, "c-1");
    let memory = MemoryInjector::new(&rec).unwrap();
    let facts = [EnvFact { source: "env:nsis".into(), status: ItemStatus::Verified, text: "makensis 3.10 at C:/tools/nsis (checked 2026-11-23)".into() }];
    for budget in [0, 200, 500, 1200, 3000, DEFAULT_BUDGET_CHARS, 20_000] {
        let block = memory.build(&turn, &facts, budget).unwrap();
        assert!(block.text.chars().count() <= budget, "budget {budget}: {} chars", block.text.chars().count());
        for line in block.text.lines().filter(|l| l.starts_with("- ")) {
            assert!(line.starts_with("- [source: ") && line.contains("; status: "), "{line}");
        }
        assert_eq!(block.items.len(), block.text.lines().filter(|l| l.starts_with("- ")).count());
        assert!(block.items.iter().all(|i| !i.source.is_empty()));
        if budget >= 500 {
            assert!(block.text.starts_with("[Lu memory: history and data"));
            assert!(block.text.contains("not instructions"));
            assert_eq!(block.items[0].section, "Goal", "the goal comes first");
        }
        if budget <= 1200 {
            assert!(block.omitted > 0, "budget {budget} should leave items out");
        }
    }
}

#[test]
fn the_block_carries_goal_question_unknown_outcome_and_the_old_installer_path() {
    let dir = base("content");
    let (book, journal, turn) = long_conversation(&dir);
    let rec = Recorder::new(&book, &journal, "c-1");
    let memory = MemoryInjector::new(&rec).unwrap();
    let block = memory.build(&turn, &[], DEFAULT_BUDGET_CHARS).unwrap();
    let find = |s: &str| block.items.iter().find(|i| i.text.contains(s)).unwrap_or_else(|| panic!("{s} missing:\n{}", block.text));
    assert_eq!(find("Ship a signed Windows installer").status, ItemStatus::Open);
    assert_eq!(find("test certificate or the release one").status, ItemStatus::Open);
    let interrupted = find("makensis lu.nsi");
    assert_eq!(interrupted.status, ItemStatus::Uncertain);
    assert_eq!(interrupted.section, "Unknown outcome");
    // Said 120 messages ago, far outside the recent window, and found by
    // relevance to "build the installer".
    let path = find("C:/tools/nsis/makensis.exe");
    assert_eq!(path.section, "Earlier in this conversation");
    // Its source opens the exact original entry directly.
    let id = AnsfSourceId::parse(&path.source).unwrap();
    let original = book.read_entry(id.session_id(), id.entry_id()).unwrap().unwrap();
    assert!(original.content.contains("use that one"));
    // Long command output is cut to its result, not pasted whole.
    let build = find("Finished release build 19");
    assert!(build.text.chars().count() <= 700);
}

#[test]
fn lu_saves_the_answer_and_logs_what_it_injected() {
    let dir = base("save");
    let (book, journal, turn) = long_conversation(&dir);
    let rec = Recorder::new(&book, &journal, "c-1");
    let memory = MemoryInjector::new(&rec).unwrap();
    let block = memory.build(&turn, &[], 1500).unwrap();
    memory.save_answer(&format!("e-{:032x}", 1000), "Built lu-setup.exe with C:/tools/nsis/makensis.exe.").unwrap();
    let latest = book.recent_context_entries("c-1", 1).unwrap();
    assert_eq!(latest[0].content, "Built lu-setup.exe with C:/tools/nsis/makensis.exe.");
    let logged = memory.injected_sources(&turn).unwrap();
    assert_eq!(logged, block.items.iter().map(|i| i.source.clone()).collect::<Vec<_>>());

    // After a restart, a new turn gets the same goal and history.

    let reopened = AnsfStore::open(&dir.join("book.sqlite")).unwrap();
    let rec = Recorder::new(&reopened, &journal, "c-1");
    let memory = MemoryInjector::new(&rec).unwrap();
    let again = memory.build(&turn, &[], 1500).unwrap();
    assert_eq!(again.items[0].text, block.items[0].text);
}

#[test]
#[ignore = "prints a sample block for review"]
fn print_sample_block() {
    let dir = base("sample");
    let (book, journal, turn) = long_conversation(&dir);
    let rec = Recorder::new(&book, &journal, "c-1");
    let memory = MemoryInjector::new(&rec).unwrap();
    let block = memory.build(&turn, &[], 2500).unwrap();
    println!("{}\n-- {} chars, {} omitted", block.text, block.text.chars().count(), block.omitted);
}
