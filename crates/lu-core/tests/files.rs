use lu_core::{
    ansf::AnsfStore,
    ansf_live_chat::{LiveEvent, LiveKind},
    ansf_operations::OperationKind,
    files::{Call, Content, FileAccess, FileError, ReadResult, WorkingScope, WriteMode, WriteRequest},
    journal::{EffectJournal, EffectState},
    origin::OriginGate,
    recorder::Recorder,
    snapshot::{SnapshotLimits, SnapshotStore},
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

struct Env {
    base: PathBuf,
    project: PathBuf,
    book: AnsfStore,
    journal: EffectJournal,
    snapshots: SnapshotStore,
    gate: OriginGate,
}
static CALLS: AtomicU32 = AtomicU32::new(0);
fn call() -> Call {
    Call { call_id: format!("call-{}", CALLS.fetch_add(1, Ordering::SeqCst)), turn_id: format!("g-{:032x}", 1) }
}
fn env(name: &str) -> Env {
    let base = std::env::temp_dir().join(format!(
        "lu-files-{name}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    let project = base.join("project");
    fs::create_dir_all(&project).unwrap();
    let project = dunce(fs::canonicalize(&project).unwrap());
    let book = AnsfStore::open(&base.join("book.sqlite")).unwrap();
    book.create_session("c-1", "lu", "Chat").unwrap();
    book.start_chapter("c-1", "Start").unwrap();
    book.append_host_chat_event(
        "c-1",
        &LiveEvent {
            event_id: format!("e-{:032x}", 1),
            kind: LiveKind::User,
            text: "Work on the project files".into(),
            user_turn_id: Some(format!("g-{:032x}", 1)),
            legacy_turn_id: None,
            generation_id: None,
        },
    )
    .unwrap();
    let journal = EffectJournal::open(&base.join("effects.sqlite")).unwrap();
    let snapshots = SnapshotStore::open(&base.join("lu-data")).unwrap();
    let gate = OriginGate::open(&base.join("origin.sqlite")).unwrap();
    Env { base, project, book, journal, snapshots, gate }
}
fn dunce(p: PathBuf) -> PathBuf {
    let t = p.to_string_lossy().to_string();
    t.strip_prefix(r"\\?\").map(PathBuf::from).unwrap_or(p)
}
macro_rules! files {
    ($env:expr, $rec:ident, $fa:ident) => {
        let $rec = Recorder::new(&$env.book, &$env.journal, "c-1");
        let $fa = FileAccess::new(
            &$rec,
            &$env.snapshots,
            &$env.gate,
            WorkingScope::new(std::slice::from_ref(&$env.project)).unwrap(),
            SnapshotLimits::default(),
        );
    };
}
const TEN_LINES: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";

#[test]
fn staged_write_catches_a_corrupting_edit_before_it_replaces_the_file() {
    let e = env("staged");
    files!(e, rec, fa);
    let path = e.project.join("notes.txt");
    fs::write(&path, TEN_LINES).unwrap();

    // The model means to change line 3 only, but sends a truncated file:
    // the failure the owner saw with a weaker model.
    let bad = call();
    let result = fa.write(
        &bad,
        &WriteRequest {
            path: path.clone(),
            content: Content::Text("one\ntwo\nTHREE\n".into()),
            mode: WriteMode::Replace { expected_sha256: None },
            intended_lines: Some((3, 3)),
        },
    );
    assert!(matches!(result, Err(FileError::CheckFailed(_))), "{result:?}");
    assert_eq!(fs::read_to_string(&path).unwrap(), TEN_LINES);
    assert_eq!(e.journal.get(&bad.call_id).unwrap().unwrap().state, EffectState::Failed);
    // Nothing is left behind in the folder.
    assert_eq!(fs::read_dir(&e.project).unwrap().count(), 1);

    // The same edit done properly passes, and the original stays recoverable.
    let good = call();
    let receipt = fa.edit_lines(&good, &path, 3, 3, "THREE\n", None).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), TEN_LINES.replace("three", "THREE"));
    assert!(receipt.undo_available);
    let snap: i64 = receipt.snapshot.as_deref().unwrap().trim_start_matches("snap:").parse().unwrap();
    e.snapshots.restore(snap, "notes.txt").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), TEN_LINES);

    // The record shows the change with hashes before and after.
    let ops = e.book.active_operations("c-1", 0, 50).unwrap();
    let change = ops.iter().find(|r| r.call_id == good.call_id && r.kind == OperationKind::FileChanged).unwrap();
    assert_eq!(change.details.sha256_before, receipt.sha256_before);
    assert_eq!(change.details.sha256_after.as_deref(), Some(receipt.sha256.as_str()));
    assert!(ops.iter().all(|r| !(r.call_id == bad.call_id && r.kind == OperationKind::FileChanged)));
}

#[test]
fn a_file_changed_since_the_model_read_it_is_not_overwritten() {
    let e = env("stale");
    files!(e, rec, fa);
    let path = e.project.join("a.txt");
    fs::write(&path, "first").unwrap();
    let seen = match fa.read(&call(), &path).unwrap() {
        ReadResult::Text { sha256, .. } => sha256,
        other => panic!("{other:?}"),
    };
    fs::write(&path, "someone else edited this").unwrap();
    let result = fa.write(
        &call(),
        &WriteRequest {
            path: path.clone(),
            content: Content::Text("model's version".into()),
            mode: WriteMode::Replace { expected_sha256: Some(seen) },
            intended_lines: None,
        },
    );
    assert!(matches!(result, Err(FileError::Stale { .. })));
    assert_eq!(fs::read_to_string(&path).unwrap(), "someone else edited this");
}

#[test]
fn universal_write_and_read_handle_text_and_binary() {
    let e = env("universal");
    files!(e, rec, fa);
    // "Create this file" runs with no confirmation step.
    let text = e.project.join("docs/readme.md");
    fa.write(
        &call(),
        &WriteRequest { path: text.clone(), content: Content::Text("# Lu\n".into()), mode: WriteMode::CreateNew, intended_lines: None },
    )
    .unwrap();
    assert!(matches!(
        fa.write(&call(), &WriteRequest { path: text.clone(), content: Content::Text("x".into()), mode: WriteMode::CreateNew, intended_lines: None }),
        Err(FileError::AlreadyExists)
    ));
    // A PNG header given as base64, and the same bytes as hex.
    let png = [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D];
    let b64 = e.project.join("a.png");
    let hex = e.project.join("b.png");
    let as_b64 = "iVBORw0KGgoAAAAN";
    let as_hex = png.iter().map(|b| format!("{b:02x}")).collect::<String>();
    fa.write(&call(), &WriteRequest { path: b64.clone(), content: Content::Base64(as_b64.into()), mode: WriteMode::CreateNew, intended_lines: None }).unwrap();
    fa.write(&call(), &WriteRequest { path: hex.clone(), content: Content::Hex(as_hex), mode: WriteMode::CreateNew, intended_lines: None }).unwrap();
    assert_eq!(fs::read(&b64).unwrap(), png);
    assert_eq!(fs::read(&hex).unwrap(), png);
    match fa.read(&call(), &b64).unwrap() {
        ReadResult::Binary { kind, size, hex_preview, .. } => {
            assert_eq!(kind, "image/png");
            assert_eq!(size, 12);
            assert!(hex_preview.starts_with("89504e47"));
        }
        other => panic!("{other:?}"),
    }
    match fa.read(&call(), &text).unwrap() {
        ReadResult::Text { content, lines, .. } => {
            assert_eq!(content, "# Lu\n");
            assert_eq!(lines, 1);
        }
        other => panic!("{other:?}"),
    }
    // A big text file is read by line range instead.
    let big = e.project.join("big.log");
    let body: String = (1..=500_000).map(|n| format!("line {n}\n")).collect();
    fs::write(&big, &body).unwrap();
    assert!(matches!(fa.read(&call(), &big), Err(FileError::TooLarge { .. })));
    assert_eq!(fa.read_lines(&call(), &big, 150_000, 150_001).unwrap(), "line 150000\nline 150001\n");
}

#[test]
fn writes_outside_the_working_scope_are_held_and_reads_are_free() {
    let e = env("scope");
    files!(e, rec, fa);
    let outside = e.base.join("elsewhere.txt");
    fs::write(&outside, "outside").unwrap();
    let held = call();
    let result = fa.write(
        &held,
        &WriteRequest { path: outside.clone(), content: Content::Text("changed".into()), mode: WriteMode::CreateOrReplace, intended_lines: None },
    );
    assert!(matches!(result, Err(FileError::Held { .. })), "{result:?}");
    assert_eq!(fs::read_to_string(&outside).unwrap(), "outside");
    assert!(e.journal.get(&held.call_id).unwrap().is_none());
    // A path that climbs out with .. is refused too.
    let sneaky = e.project.join("..").join("elsewhere.txt");
    assert!(fa.trash(&call(), &sneaky).is_err());
    assert!(outside.exists());
    // Reading outside the scope is allowed.
    assert!(matches!(fa.read(&call(), &outside).unwrap(), ReadResult::Text { .. }));
}

#[test]
fn delete_is_recoverable_and_copy_and_move_are_checked() {
    let e = env("trash");
    files!(e, rec, fa);
    let path = e.project.join("keep.txt");
    fs::write(&path, "precious").unwrap();
    let entry = fa.trash(&call(), &path).unwrap();
    assert!(!path.exists());
    fa.restore_from_trash(&call(), &entry).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "precious");
    // A second restore of the same entry, or over a taken path, is refused.
    assert!(fa.restore_from_trash(&call(), &entry).is_err());

    let copy = e.project.join("sub/copy.txt");
    fa.copy(&call(), &path, &copy, false).unwrap();
    assert_eq!(fs::read_to_string(&copy).unwrap(), "precious");
    assert!(matches!(fa.copy(&call(), &path, &copy, false), Err(FileError::AlreadyExists)));
    let moved = e.project.join("moved.txt");
    fa.move_file(&call(), &copy, &moved).unwrap();
    assert!(!copy.exists());
    assert_eq!(fs::read_to_string(&moved).unwrap(), "precious");
    assert!(matches!(fa.move_file(&call(), &moved, &path), Err(FileError::AlreadyExists)));
    let (entries, truncated) = fa.list(&call(), &e.project).unwrap();
    assert!(!truncated);
    let names: Vec<_> = entries.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["sub", "keep.txt", "moved.txt"]);
}
