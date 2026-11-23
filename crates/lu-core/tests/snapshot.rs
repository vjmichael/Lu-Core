use lu_core::{
    ansf::AnsfStore,
    ansf_live_chat::{LiveEvent, LiveKind},
    ansf_operations::{CommandFinish, CommandStart, OperationKind},
    journal::EffectJournal,
    recorder::Recorder,
    snapshot::{diff, Change, Restorable, SnapshotError, SnapshotKind, SnapshotLimits, SnapshotStore},
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lu-snap-{name}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}
fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn command_scope_check_reports_changes_and_restores_a_failed_file() {
    let base = root("scope");
    let (project, store_dir) = (base.join("project"), base.join("lu-data/snapshots"));
    write(&project.join("a.txt"), "original a");
    write(&project.join("b.txt"), "original b");
    write(&project.join("sub/c.txt"), "unchanged c");
    let store = SnapshotStore::open(&store_dir).unwrap();

    // Recorder setup: one authenticated user turn.
    let book = AnsfStore::open(&base.join("book.sqlite")).unwrap();
    book.create_session("c-1", "lu", "Chat").unwrap();
    book.start_chapter("c-1", "Start").unwrap();
    book.append_host_chat_event(
        "c-1",
        &LiveEvent {
            event_id: format!("e-{:032x}", 1),
            kind: LiveKind::User,
            text: "Run the formatter".into(),
            user_turn_id: Some(format!("g-{:032x}", 1)),
            legacy_turn_id: None,
            generation_id: None,
        },
    )
    .unwrap();
    let journal = EffectJournal::open(&base.join("effects.sqlite")).unwrap();
    let rec = Recorder::new(&book, &journal, "c-1");

    let before = store.take(&project, SnapshotLimits::default()).unwrap();
    assert_eq!(before.kind, SnapshotKind::Copy);
    assert_eq!(before.files, 3);
    rec.begin_command(&CommandStart {
        call_id: "fmt".into(),
        turn_id: format!("g-{:032x}", 1),
        command: "formatter --all".into(),
        cwd: project.to_string_lossy().into(),
    })
    .unwrap();
    // The "command" edits a, deletes b and creates d. Lu cannot see inside it.
    write(&project.join("a.txt"), "garbled a");
    fs::remove_file(project.join("b.txt")).unwrap();
    write(&project.join("d.txt"), "new d");
    rec.finish_command(&CommandFinish {
        call_id: "fmt".into(),
        exit_code: Some(0),
        timed_out: false,
        cancelled: false,
        duration_ms: 5,
        output: String::new(),
    })
    .unwrap();

    let changes = diff(&store.manifest(before.id).unwrap(), &store.scan(&project).unwrap());
    let summary: Vec<_> = changes
        .iter()
        .map(|c| match c {
            Change::Added { path, .. } => format!("+{path}"),
            Change::Removed { path, .. } => format!("-{path}"),
            Change::Modified { path, .. } => format!("~{path}"),
        })
        .collect();
    assert_eq!(summary, ["~a.txt", "-b.txt", "+d.txt"]);
    rec.record_changes("fmt", &before, &changes).unwrap();
    let recorded: Vec<_> = book
        .active_operations("c-1", 0, 20)
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == OperationKind::FileChanged)
        .collect();
    assert_eq!(recorded.len(), 3);
    assert!(recorded.iter().all(|r| r.details.snapshot_ref.as_deref() == Some(before.reference().as_str())));

    // a.txt fails its check: restore it (and the deleted b.txt) from the snapshot.
    store.restore(before.id, "a.txt").unwrap();
    store.restore(before.id, "b.txt").unwrap();
    assert_eq!(fs::read_to_string(project.join("a.txt")).unwrap(), "original a");
    assert_eq!(fs::read_to_string(project.join("b.txt")).unwrap(), "original b");
    // The new file has no original to restore.
    assert_eq!(store.restore(before.id, "d.txt"), Err(SnapshotError::NotRestorable));
    assert_eq!(store.restore(before.id, "../outside.txt"), Err(SnapshotError::InvalidPath));
    let after = diff(&store.manifest(before.id).unwrap(), &store.scan(&project).unwrap());
    assert_eq!(after.len(), 1);
    drop((book, journal, store));
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn copy_limits_detect_changes_but_refuse_restore_and_unchanged_content_is_stored_once() {
    let base = root("limits");
    let (project, store_dir) = (base.join("project"), base.join("store"));
    write(&project.join("small.txt"), "small");
    write(&project.join("large.bin"), &"x".repeat(4096));
    let store = SnapshotStore::open(&store_dir).unwrap();
    let limits = SnapshotLimits { max_total_copy_bytes: 1024 * 1024, max_file_copy_bytes: 1024 };
    let first = store.take(&project, limits).unwrap();
    assert_eq!(first.not_restorable, 1);
    assert_eq!(first.copied_bytes, 5);
    assert_eq!(store.restorable(first.id, "large.bin").unwrap(), Restorable::No);
    assert_eq!(store.restorable(first.id, "small.txt").unwrap(), Restorable::Blob);
    // Same content again: nothing new is copied.
    let second = store.take(&project, limits).unwrap();
    assert_eq!(second.copied_bytes, 0);

    write(&project.join("large.bin"), &"y".repeat(4096));
    let changes = diff(&store.manifest(first.id).unwrap(), &store.scan(&project).unwrap());
    assert_eq!(changes.len(), 1);
    assert_eq!(store.restore(first.id, "large.bin"), Err(SnapshotError::NotRestorable));
    drop(store);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn git_scope_uses_a_pinned_commit_without_touching_the_branch_or_index() {
    let base = root("git");
    let (repo, store_dir) = (base.join("repo"), base.join("store"));
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    git(&repo, &["config", "user.name", "Lu test"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    write(&repo.join(".gitignore"), "target/\n");
    write(&repo.join("src/main.rs"), "fn main() {}\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    // Uncommitted work and an untracked file exist before the command.
    write(&repo.join("src/main.rs"), "fn main() { println!(\"v2\"); }\n");
    write(&repo.join("notes.txt"), "untracked notes\n");
    write(&repo.join("target/big.o"), "ignored build output");
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let status = git(&repo, &["status", "--porcelain"]);

    let store = SnapshotStore::open(&store_dir).unwrap();
    let snap = store.take(&repo, SnapshotLimits::default()).unwrap();
    assert_eq!(snap.kind, SnapshotKind::Git);
    assert_eq!(store.restorable(snap.id, "src/main.rs").unwrap(), Restorable::Git);
    assert_eq!(store.restorable(snap.id, "notes.txt").unwrap(), Restorable::Blob);
    assert!(!store.manifest(snap.id).unwrap().contains_key("target/big.o"));
    // Taking the snapshot changed nothing the user can see.
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&repo, &["status", "--porcelain"]), status);
    assert!(git(&repo, &["for-each-ref", "refs/lu/snapshots/"]).contains(snap.git_commit.as_deref().unwrap()));

    // A bad edit to both files, then restore.
    write(&repo.join("src/main.rs"), "broken");
    write(&repo.join("notes.txt"), "broken");
    store.restore(snap.id, "src/main.rs").unwrap();
    store.restore(snap.id, "notes.txt").unwrap();
    assert_eq!(fs::read_to_string(repo.join("src/main.rs")).unwrap(), "fn main() { println!(\"v2\"); }\n");
    assert_eq!(fs::read_to_string(repo.join("notes.txt")).unwrap(), "untracked notes\n");
    assert!(diff(&store.manifest(snap.id).unwrap(), &store.scan(&repo).unwrap()).is_empty());
    drop(store);
    let _ = fs::remove_dir_all(base);
}
