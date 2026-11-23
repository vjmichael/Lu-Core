use lu_core::{
    ansf::AnsfStore,
    ansf_live_chat::{LiveEvent, LiveKind},
    gguf,
    hf::{parse_review, parse_search, quant_of, shard_of, valid_repo, FileLead, Hub, HubError, InstallRequest, ModelInstaller},
    journal::EffectJournal,
    origin::OriginGate,
    recorder::Recorder,
    resources::{Governor, Sample},
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

const REV: &str = "0123456789abcdef0123456789abcdef01234567";

fn base(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lu-hf-{name}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}
fn sha(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}

/// How the stand-in Hub serves a file.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    /// The first download breaks after this many bytes; later ones resume.
    DropAfter(usize),
    /// Serves different bytes than published.
    Corrupt,
    /// Ignores Range and always sends the whole file.
    NoRange,
}

struct FakeHub {
    url: String,
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    mode: Arc<Mutex<Mode>>,
    hits: Arc<AtomicUsize>,
    ranges: Arc<Mutex<Vec<String>>>,
}

fn fake_hub(info: String, tree: String, search: String) -> FakeHub {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let files: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
    let mode = Arc::new(Mutex::new(Mode::Normal));
    let hits = Arc::new(AtomicUsize::new(0));
    let ranges: Arc<Mutex<Vec<String>>> = Arc::default();
    let (f2, m2, h2, r2) = (files.clone(), mode.clone(), hits.clone(), ranges.clone());
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut reader = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let mut range = None;
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                    break;
                }
                if let Some(v) = h.to_ascii_lowercase().strip_prefix("range: bytes=") {
                    range = v.trim().trim_end_matches('-').parse::<usize>().ok();
                }
            }
            let reply = |s: &mut std::net::TcpStream, status: &str, headers: &str, body: &[u8]| {
                let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n", body.len());
                let _ = s.write_all(body);
            };
            if path.starts_with("/api/models?search=") {
                reply(&mut s, "200 OK", "Content-Type: application/json\r\n", search.as_bytes());
            } else if path.starts_with("/api/models/owner/repo/tree/") {
                reply(&mut s, "200 OK", "Content-Type: application/json\r\n", tree.as_bytes());
            } else if path == "/api/models/owner/repo" {
                reply(&mut s, "200 OK", "Content-Type: application/json\r\n", info.as_bytes());
            } else if let Some(file) = path.strip_prefix(&format!("/owner/repo/resolve/{REV}/")) {
                // Like the real Hub: a redirect to the file store.
                reply(&mut s, "302 Found", &format!("Location: /cdn/{file}\r\n"), b"");
            } else if path == "/evil-redirect" {
                reply(&mut s, "302 Found", "Location: https://evil.example/x.gguf\r\n", b"");
            } else if let Some(file) = path.strip_prefix("/cdn/") {
                let data = f2.lock().unwrap().get(file).cloned();
                let Some(mut data) = data else {
                    reply(&mut s, "404 Not Found", "", b"");
                    continue;
                };
                let mode = *m2.lock().unwrap();
                r2.lock().unwrap().push(range.map(|r| format!("{r}-")).unwrap_or_default());
                let n = h2.fetch_add(1, Ordering::SeqCst);
                if mode == Mode::Corrupt {
                    let last = data.len() - 1;
                    data[last] ^= 0xFF;
                }
                match (range, mode) {
                    (Some(from), m) if m != Mode::NoRange => {
                        let total = data.len();
                        reply(&mut s, "206 Partial Content", &format!("Content-Range: bytes {from}-{}/{total}\r\n", total - 1), &data[from..]);
                    }
                    (_, Mode::DropAfter(k)) if n == 0 => {
                        // Promise the whole file, send part of it, hang up.
                        let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", data.len());
                        let _ = s.write_all(&data[..k]);
                    }
                    _ => reply(&mut s, "200 OK", "", &data),
                }
            } else {
                reply(&mut s, "404 Not Found", "", b"");
            }
        }
    });
    FakeHub { url, files, mode, hits, ranges }
}

/// A repo with a single-file Q4_K_M model, a two-part Q8_0 model and a projector.
fn repo() -> (FakeHub, Vec<u8>, Vec<u8>) {
    let model = gguf::minimal("qwen3", "Qwen3 Test", 32768, 300_000);
    let mmproj = gguf::minimal("clip", "Qwen3 Projector", 0, 1000);
    let part1 = gguf::minimal("qwen3", "Qwen3 Test Q8 part 1", 32768, 5000);
    let part2 = gguf::minimal("qwen3", "Qwen3 Test Q8 part 2", 32768, 5000);
    let lfs = |path: &str, b: &[u8]| format!(r#"{{"type":"file","path":"{path}","size":{},"lfs":{{"oid":"{}","size":{}}}}}"#, b.len(), sha(b), b.len());
    let tree = format!(
        "[{},{},{},{},{},{}]",
        lfs("qwen3-test-Q4_K_M.gguf", &model),
        lfs("mmproj-qwen3-test-f16.gguf", &mmproj),
        lfs("Q8_0/qwen3-test-Q8_0-00001-of-00002.gguf", &part1),
        lfs("Q8_0/qwen3-test-Q8_0-00002-of-00002.gguf", &part2),
        r#"{"type":"file","path":"README.md","size":10}"#,
        r#"{"type":"file","path":"unhashed-Q2_K.gguf","size":4096}"#,
    );
    let info = format!(r#"{{"id":"owner/repo","sha":"{REV}","cardData":{{"license":"apache-2.0"}},"gated":false}}"#);
    let search = r#"[{"id":"owner/repo","downloads":1200,"likes":30,"cardData":{"license":"apache-2.0"}},
                     {"id":"meta/gated","downloads":99,"gated":"manual"},
                     {"id":"bad/../path","downloads":1}]"#;
    let hub = fake_hub(info, tree, search.into());
    {
        let mut f = hub.files.lock().unwrap();
        f.insert("qwen3-test-Q4_K_M.gguf".into(), model.clone());
        f.insert("mmproj-qwen3-test-f16.gguf".into(), mmproj.clone());
        f.insert("Q8_0/qwen3-test-Q8_0-00001-of-00002.gguf".into(), part1);
        f.insert("Q8_0/qwen3-test-Q8_0-00002-of-00002.gguf".into(), part2);
    }
    (hub, model, mmproj)
}

struct Lu {
    dir: PathBuf,
    book: AnsfStore,
    journal: EffectJournal,
    gate: OriginGate,
    governor: Governor,
}
fn lu(name: &str, disk_free_mib: u64) -> Lu {
    let dir = base(name);
    let book = AnsfStore::open(&dir.join("book.sqlite")).unwrap();
    book.create_session("c-1", "lu", "Chat").unwrap();
    book.start_chapter("c-1", "Start").unwrap();
    let governor = Governor::with_sampler(
        &dir.join("resources.sqlite"),
        Box::new(move |_| Sample { disk_free_mib: Some(disk_free_mib), ..Sample::default() }),
    )
    .unwrap();
    Lu {
        journal: EffectJournal::open(&dir.join("effects.sqlite")).unwrap(),
        gate: OriginGate::open(&dir.join("origin.sqlite")).unwrap(),
        governor,
        book,
        dir,
    }
}
fn say(lu: &Lu, n: u32, text: &str) -> String {
    let turn = format!("g-{n:032x}");
    lu.book
        .append_host_chat_event(
            "c-1",
            &LiveEvent { event_id: format!("e-{n:032x}"), kind: LiveKind::User, text: text.into(), user_turn_id: Some(turn.clone()), legacy_turn_id: None, generation_id: None },
        )
        .unwrap();
    turn
}
fn request(files: Vec<FileLead>, projector: Option<FileLead>) -> InstallRequest {
    InstallRequest { repo_id: "owner/repo".into(), revision: REV.into(), files, projector }
}

#[test]
fn names_quantizations_and_parts_are_read_safely() {
    assert!(valid_repo("unsloth/Qwen3.8-27B-GGUF"));
    for bad in ["a/b/c", "a/../b", "a", "/a/b", "a/b?x=1", "a/.hidden"] {
        assert!(!valid_repo(bad), "{bad}");
    }
    assert_eq!(quant_of("Qwen3.8-27B-Q4_K_M.gguf").as_deref(), Some("Q4_K_M"));
    assert_eq!(quant_of("gemma-4-12b-it-IQ3_XS.gguf").as_deref(), Some("IQ3_XS"));
    assert_eq!(quant_of("model-BF16-00001-of-00002.gguf").as_deref(), Some("BF16"));
    assert_eq!(quant_of("readme.gguf"), None);
    assert_eq!(shard_of("Q8_0/model-Q8_0-00002-of-00003.gguf"), Some((2, 3)));
    assert_eq!(shard_of("model-Q8_0.gguf"), None);
    let leads = parse_search(br#"[{"id":"a/b","private":true},{"id":"c/d","tags":["license:mit"]}]"#).unwrap();
    assert!(leads[0].unavailable.as_deref().unwrap().contains("private"));
    assert_eq!((leads[1].license.as_str(), leads[1].unavailable.is_none()), ("mit", true));
}

#[test]
fn search_and_review_show_choices_parts_projectors_and_what_is_unavailable() {
    let (hub, model, _) = repo();
    let lu = lu("review", 600_000);
    let turn = say(&lu, 1, "Find me a Qwen3 GGUF");
    let rec = Recorder::new(&lu.book, &lu.journal, "c-1");
    let h = Hub::local_test(&hub.url).unwrap();
    let inst = ModelInstaller::new(&lu.dir.join("models"), &h, &rec, &lu.gate, &lu.governor).unwrap();
    let leads = inst.search(&turn, "qwen3 test", 10).unwrap();
    assert_eq!(leads.len(), 2, "the unsafe repo name is dropped");
    assert!(leads[1].unavailable.as_deref().unwrap().contains("gated"));
    let review = inst.review(&turn, "owner/repo", None).unwrap();
    assert_eq!(review.revision, REV);
    assert_eq!(review.license, "apache-2.0");
    assert_eq!(review.choices.len(), 2);
    assert_eq!(review.choices[0].quant.as_deref(), Some("Q8_0"));
    assert_eq!(review.choices[0].files.len(), 2, "both parts kept together");
    assert_eq!(review.choices[1].files[0].bytes, model.len() as u64);
    assert_eq!(review.projectors.len(), 1);
    assert_eq!(review.skipped.len(), 1, "{:?}", review.skipped);
    assert!(review.skipped[0].1.contains("SHA-256"));
    // A split set with a part missing is not offered.
    let info = format!(r#"{{"id":"o/r","sha":"{REV}"}}"#);
    let tree = r#"[{"type":"file","path":"m-Q8_0-00001-of-00003.gguf","size":4096,"lfs":{"oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}]"#;
    let r = parse_review(info.as_bytes(), tree.as_bytes(), "o/r").unwrap();
    assert!(r.choices.is_empty() && r.skipped[0].1.contains("only 1"));
    let tree = r#"[{"type":"file","path":"imatrix_unsloth.gguf","size":4096,"lfs":{"oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}]"#;
    let r = parse_review(info.as_bytes(), tree.as_bytes(), "o/r").unwrap();
    assert!(r.choices.is_empty() && r.skipped[0].1.contains("not a model"));
    // Hub content is untrusted: installing in the same turn is held.
    let choice = review.choices[1].files.clone();
    let err = inst.install("dl-1", &turn, &request(choice, None), &AtomicBool::new(false), &mut |_, _, _| {}).unwrap_err();
    assert!(matches!(err, HubError::Held { notify_user: true, .. }), "{err:?}");
    assert!(!lu.dir.join("models/owner/repo/qwen3-test-Q4_K_M.gguf").exists());
}

#[test]
fn the_users_instruction_installs_with_a_paired_projector() {
    let (hub, model, mmproj) = repo();
    let lu = lu("install", 600_000);
    let first = say(&lu, 1, "Find me a Qwen3 GGUF");
    let rec = Recorder::new(&lu.book, &lu.journal, "c-1");
    let h = Hub::local_test(&hub.url).unwrap();
    let inst = ModelInstaller::new(&lu.dir.join("models"), &h, &rec, &lu.gate, &lu.governor).unwrap();
    let review = inst.review(&first, "owner/repo", None).unwrap();
    // The user's next turn says which one.
    let yes = say(&lu, 2, "Download the Q4_K_M one with its vision projector");
    let mut seen = Vec::new();
    let done = inst
        .install("dl-2", &yes, &request(review.choices[1].files.clone(), Some(review.projectors[0].clone())), &AtomicBool::new(false), &mut |f, d, t| seen.push((f.to_owned(), d, t)))
        .unwrap();
    let path = lu.dir.join("models/owner/repo/qwen3-test-Q4_K_M.gguf");
    assert_eq!(done.model[0].path, path);
    assert_eq!(fs::read(&path).unwrap(), model);
    assert_eq!(done.model[0].info.architecture.as_deref(), Some("qwen3"));
    assert_eq!(done.model[0].info.context_length, Some(32768));
    let proj = done.projector.unwrap();
    assert!(proj.info.is_projector());
    assert_eq!(fs::read(&proj.path).unwrap(), mmproj);
    assert_eq!(inst.projector_for(&path).unwrap(), Some(proj.path.clone()));
    assert!(seen.last().is_some_and(|(_, d, t)| d == t));
    // The file is marked as coming from outside the PC.
    assert!(lu.gate.external_file(&path).unwrap().unwrap().reference.contains("huggingface.co/owner/repo"));
    // Nothing left in staging; installing again does not download again.
    assert_eq!(fs::read_dir(lu.dir.join("models/.staging")).unwrap().count(), 0);
    let hits = hub.hits.load(Ordering::SeqCst);
    let again = inst.install("dl-3", &yes, &request(review.choices[1].files.clone(), None), &AtomicBool::new(false), &mut |_, _, _| {}).unwrap();
    assert!(again.model[0].already_installed);
    assert_eq!(hub.hits.load(Ordering::SeqCst), hits);
    assert_eq!(inst.installed().unwrap().len(), 1);
    // A split model's names were copied from the Hub, not typed by the user:
    // held, then approved in the user's next message.
    let split_req = request(review.choices[0].files.clone(), None);
    let held = match inst.install("dl-4", &yes, &split_req, &AtomicBool::new(false), &mut |_, _, _| {}) {
        Err(HubError::Held { held_id, .. }) => held_id,
        other => panic!("{other:?}"),
    };
    // The approval must come from a later user turn, for the same action.
    assert!(inst.install_approved(held, &yes, "dl-5", &split_req, &AtomicBool::new(false), &mut |_, _, _| {}).is_err());
    let ok = say(&lu, 3, "Yes, get the Q8_0 too");
    let other = request(review.choices[1].files.clone(), None);
    assert!(inst.install_approved(held, &ok, "dl-5", &other, &AtomicBool::new(false), &mut |_, _, _| {}).is_err());
    let split = inst.install_approved(held, &ok, "dl-5", &split_req, &AtomicBool::new(false), &mut |_, _, _| {}).unwrap();
    assert_eq!(split.model.len(), 2);
    assert!(lu.dir.join("models/owner/repo/Q8_0/qwen3-test-Q8_0-00002-of-00002.gguf").exists());
}

#[test]
fn a_broken_download_resumes_and_a_bad_hash_is_deleted() {
    let (hub, model, _) = repo();
    let lu = lu("resume", 600_000);
    let turn = say(&lu, 1, "Download owner/repo Q4_K_M");
    let rec = Recorder::new(&lu.book, &lu.journal, "c-1");
    let h = Hub::local_test(&hub.url).unwrap();
    let inst = ModelInstaller::new(&lu.dir.join("models"), &h, &rec, &lu.gate, &lu.governor).unwrap();
    let file = FileLead { path: "qwen3-test-Q4_K_M.gguf".into(), bytes: model.len() as u64, sha256: sha(&model) };
    let part = lu.dir.join(format!("models/.staging/{}.part", file.sha256));

    *hub.mode.lock().unwrap() = Mode::DropAfter(100_000);
    let err = inst.install("dl-1", &turn, &request(vec![file.clone()], None), &AtomicBool::new(false), &mut |_, _, _| {}).unwrap_err();
    assert!(matches!(err, HubError::Network(ref w) if w.contains("resume")), "{err:?}");
    assert_eq!(fs::metadata(&part).unwrap().len(), 100_000, "the staged part is kept");
    assert!(!lu.dir.join("models/owner/repo/qwen3-test-Q4_K_M.gguf").exists());
    let done = inst.install("dl-2", &turn, &request(vec![file.clone()], None), &AtomicBool::new(false), &mut |_, _, _| {}).unwrap();
    assert!(done.model[0].resumed);
    assert_eq!(hub.ranges.lock().unwrap().last().unwrap(), "100000-", "it asked for the rest only");
    assert_eq!(fs::read(&done.model[0].path).unwrap(), model);

    // Wrong bytes: caught by the hash, staged file deleted, nothing installed.
    let lu2 = self::lu("corrupt", 600_000);
    let t2 = say(&lu2, 1, "Download owner/repo Q4_K_M");
    let rec2 = Recorder::new(&lu2.book, &lu2.journal, "c-1");
    let inst2 = ModelInstaller::new(&lu2.dir.join("models"), &h, &rec2, &lu2.gate, &lu2.governor).unwrap();
    *hub.mode.lock().unwrap() = Mode::Corrupt;
    let err = inst2.install("dl-1", &t2, &request(vec![file.clone()], None), &AtomicBool::new(false), &mut |_, _, _| {}).unwrap_err();
    assert!(matches!(err, HubError::HashMismatch { .. }), "{err:?}");
    assert_eq!(fs::read_dir(lu2.dir.join("models/.staging")).unwrap().count(), 0);
    assert!(!lu2.dir.join("models/owner/repo/qwen3-test-Q4_K_M.gguf").exists());

    // A server that ignores Range: Lu starts over cleanly instead of appending.
    let lu3 = self::lu("norange", 600_000);
    let t3 = say(&lu3, 1, "Download owner/repo Q4_K_M");
    let rec3 = Recorder::new(&lu3.book, &lu3.journal, "c-1");
    let inst3 = ModelInstaller::new(&lu3.dir.join("models"), &h, &rec3, &lu3.gate, &lu3.governor).unwrap();
    fs::write(lu3.dir.join(format!("models/.staging/{}.part", file.sha256)), &model[..5000]).unwrap();
    *hub.mode.lock().unwrap() = Mode::NoRange;
    let done = inst3.install("dl-1", &t3, &request(vec![file.clone()], None), &AtomicBool::new(false), &mut |_, _, _| {}).unwrap();
    assert!(!done.model[0].resumed);
    assert_eq!(fs::read(&done.model[0].path).unwrap(), model);

    // Cancel keeps the part for later.
    let lu4 = self::lu("cancel", 600_000);
    let t4 = say(&lu4, 1, "Download owner/repo Q4_K_M");
    let rec4 = Recorder::new(&lu4.book, &lu4.journal, "c-1");
    let inst4 = ModelInstaller::new(&lu4.dir.join("models"), &h, &rec4, &lu4.gate, &lu4.governor).unwrap();
    *hub.mode.lock().unwrap() = Mode::Normal;
    let stop = AtomicBool::new(true);
    assert_eq!(inst4.install("dl-1", &t4, &request(vec![file], None), &stop, &mut |_, _, _| {}), Err(HubError::Cancelled));
}

#[test]
fn not_gguf_no_disk_and_foreign_hosts_are_refused() {
    let (hub, model, _) = repo();
    let h = Hub::local_test(&hub.url).unwrap();
    // A file that is not GGUF, even with the right hash, is not installed.
    let lu1 = lu("notgguf", 600_000);
    let t = say(&lu1, 1, "Download owner/repo");
    let rec = Recorder::new(&lu1.book, &lu1.journal, "c-1");
    let inst = ModelInstaller::new(&lu1.dir.join("models"), &h, &rec, &lu1.gate, &lu1.governor).unwrap();
    let text = b"<html>not a model</html> padding padding padding".to_vec();
    hub.files.lock().unwrap().insert("fake-Q4_0.gguf".into(), text.clone());
    let fake = FileLead { path: "fake-Q4_0.gguf".into(), bytes: text.len() as u64, sha256: sha(&text) };
    assert!(matches!(inst.install("dl-1", &t, &request(vec![fake], None), &AtomicBool::new(false), &mut |_, _, _| {}), Err(HubError::NotGguf { .. })));
    assert!(!lu1.dir.join("models/owner/repo/fake-Q4_0.gguf").exists());

    // Not enough disk: measured, refused with the numbers, nothing fetched.
    let lu2 = lu("nodisk", 5);
    let t = say(&lu2, 1, "Download owner/repo");
    let rec = Recorder::new(&lu2.book, &lu2.journal, "c-1");
    let inst = ModelInstaller::new(&lu2.dir.join("models"), &h, &rec, &lu2.gate, &lu2.governor).unwrap();
    let file = FileLead { path: "qwen3-test-Q4_K_M.gguf".into(), bytes: model.len() as u64, sha256: sha(&model) };
    let hits = hub.hits.load(Ordering::SeqCst);
    match inst.install("dl-1", &t, &request(vec![file], None), &AtomicBool::new(false), &mut |_, _, _| {}) {
        Err(HubError::NoRoom(w)) => assert!(w[0].starts_with("disk"), "{w:?}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(hub.hits.load(Ordering::SeqCst), hits);

    // Only the Hub: a test hub must be loopback, and the real one only talks HTTPS to huggingface.co / hf.co.
    assert!(Hub::local_test("http://example.com").is_err());
    assert!(matches!(Hub::official().review("../etc", None), Err(HubError::Invalid(_))));
}

#[test]
#[ignore = "talks to the real Hugging Face Hub"]
fn real_hub_search_and_review() {
    let hub = Hub::official();
    let leads = hub.search("Qwen3 GGUF", 5).unwrap();
    for l in &leads {
        println!("{} license={} downloads={} {:?}", l.repo_id, l.license, l.downloads, l.unavailable);
    }
    let first = leads.iter().find(|l| l.unavailable.is_none()).expect("an open repo");
    let r = hub.review(&first.repo_id, None).unwrap();
    println!("{} @ {}", r.repo_id, r.revision);
    for c in &r.choices {
        println!("  {:<40} {:>8} MiB  {}", c.label, c.total_bytes >> 20, c.files[0].sha256);
    }
    for p in &r.projectors {
        println!("  projector {} {} MiB", p.path, p.bytes >> 20);
    }
    assert!(!r.choices.is_empty());
}
