use lu_core::{
    ansf::AnsfStore,
    ansf_live_chat::{LiveEvent, LiveKind},
    environment::{EnvironmentStore, FactStatus},
    journal::EffectJournal,
    memory::{ItemStatus, MemoryInjector, DEFAULT_BUDGET_CHARS},
    recorder::Recorder,
};
use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn base(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lu-env-{name}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}
/// A new conversation book with one user turn; returns (book, turn).
fn chat(path: &Path, session: &str, text: &str) -> (AnsfStore, String) {
    let book = AnsfStore::open(path).unwrap();
    book.create_session(session, "lu", "Chat").unwrap();
    book.start_chapter(session, "Start").unwrap();
    let turn = format!("g-{:032x}", 1);
    book.append_host_chat_event(
        session,
        &LiveEvent {
            event_id: format!("e-{:032x}", 1),
            kind: LiveKind::User,
            text: text.into(),
            user_turn_id: Some(turn.clone()),
            legacy_turn_id: None,
            generation_id: None,
        },
    )
    .unwrap();
    (book, turn)
}

#[test]
fn toolchain_amnesia_a_fresh_chat_knows_and_rechecks_what_was_supplied() {
    let dir = base("amnesia");
    let env = EnvironmentStore::open(&dir.join("environment.sqlite")).unwrap();
    let tools = dir.join("tools/nsis");
    fs::create_dir_all(&tools).unwrap();
    let makensis = tools.join("makensis.exe");
    fs::write(&makensis, b"MZ fake compiler").unwrap();

    // Chat A: the user supplies the installer; Lu probes the machine.
    let (book_a, _) = chat(&dir.join("a.sqlite"), "c-a", &format!("The installer compiler is {}", makensis.display()));
    let said = book_a.recent_context_entries("c-a", 1).unwrap()[0].stable_source_id().unwrap();
    let reported = env.record_reported("installer:nsis", "NSIS installer compiler (makensis)", Some(&makensis), &said).unwrap();
    assert_eq!(reported.status, FactStatus::Unverified);
    let git = env.probe_tool("git", "--version").unwrap();
    assert_eq!(git.status, FactStatus::Verified);
    assert!(git.value.chars().any(|c| c.is_ascii_digit()), "{}", git.value);

    // Chat B, days later, different conversation and book: the memory block
    // already knows, and the facts were checked before being given out.
    let (book_b, turn_b) = chat(&dir.join("b.sqlite"), "c-b", "Build the Windows installer");
    let journal = EffectJournal::open(&dir.join("effects.sqlite")).unwrap();
    let rec = Recorder::new(&book_b, &journal, "c-b");
    let memory = MemoryInjector::new(&rec).unwrap();
    let facts = env.memory_facts(Duration::from_secs(3600)).unwrap();
    let block = memory.build(&turn_b, &facts, DEFAULT_BUDGET_CHARS).unwrap();
    let nsis = block.items.iter().find(|i| i.source == "env:installer:nsis").unwrap_or_else(|| panic!("{}", block.text));
    assert_eq!(nsis.status, ItemStatus::Verified, "a reported fact is checked before use");
    assert!(nsis.text.contains("makensis.exe"));
    assert!(block.items.iter().any(|i| i.source == "env:tool:git" && i.status == ItemStatus::Verified));
    // The fact still points at where the user said it.
    assert_eq!(env.get("installer:nsis").unwrap().unwrap().source, said);
}

#[test]
fn a_contradicted_fact_is_marked_never_silently_kept() {
    let dir = base("contradict");
    let env = EnvironmentStore::open(&dir.join("environment.sqlite")).unwrap();
    let tool = dir.join("qemu-img.exe");
    fs::write(&tool, b"v1").unwrap();
    env.record_reported("tool-path:qemu-img", "QEMU disk image tool", Some(&tool), "ansf:v1:Yy0x:5").unwrap();
    assert_eq!(env.verify("tool-path:qemu-img").unwrap().status, FactStatus::Verified);

    // The file is replaced: still there, but flagged as changed.
    thread::sleep(Duration::from_millis(1100));
    fs::write(&tool, b"version two, larger").unwrap();
    let changed = env.verify("tool-path:qemu-img").unwrap();
    assert_eq!(changed.status, FactStatus::Verified);
    assert!(changed.value.contains("file changed since last check"), "{}", changed.value);

    // The file is removed: the fact is contradicted, with what it was.
    fs::remove_file(&tool).unwrap();
    let facts = env.facts_for_use(Duration::ZERO, false).unwrap();
    let gone = facts.iter().find(|f| f.key == "tool-path:qemu-img").unwrap();
    assert_eq!(gone.status, FactStatus::Contradicted);
    assert!(gone.note.as_deref().unwrap().contains("no longer found"));
    let memory = env.memory_facts(Duration::from_secs(3600)).unwrap();
    assert_eq!(memory.iter().find(|f| f.source == "env:tool-path:qemu-img").unwrap().status, ItemStatus::Contradicted);
    // Every observation is kept.
    let history = env.history("tool-path:qemu-img").unwrap();
    let statuses: Vec<_> = history.iter().map(|f| f.status).collect();
    assert_eq!(statuses[0], FactStatus::Unverified);
    assert_eq!(*statuses.last().unwrap(), FactStatus::Contradicted);
    assert!(history.len() >= 4);
}

#[test]
fn probes_find_installed_tools_and_record_missing_ones() {
    let dir = base("probe");
    let env = EnvironmentStore::open(&dir.join("environment.sqlite")).unwrap();
    let missing = env.probe_tool("lu-no-such-tool-7f3a", "--version").unwrap();
    assert_eq!(missing.status, FactStatus::Missing);
    let facts = env.probe_known_tools().unwrap();
    let git = facts.iter().find(|f| f.key == "tool:git").unwrap();
    assert_eq!(git.status, FactStatus::Verified);
    assert!(git.path.as_deref().unwrap().is_file());
    // Missing tools are known to Lu but not put in the model's memory.
    let memory = env.memory_facts(Duration::from_secs(3600)).unwrap();
    assert!(memory.iter().all(|f| f.source != "env:tool:lu-no-such-tool-7f3a"));
    assert!(memory.iter().any(|f| f.source == "env:tool:git"));
}

#[test]
#[ignore = "prints what the probes find on this machine"]
fn print_probes() {
    let dir = base("print");
    let env = EnvironmentStore::open(&dir.join("environment.sqlite")).unwrap();
    for f in env.probe_known_tools().unwrap() {
        println!("{:<28} {:<12} {}", f.key, format!("{:?}", f.status), f.value);
    }
}
