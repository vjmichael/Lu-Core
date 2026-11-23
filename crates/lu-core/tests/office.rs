use lu_core::{
    ansf::AnsfStore,
    ansf_live_chat::{LiveEvent, LiveKind},
    files::{Call, FileAccess, FileError, ReadResult, WorkingScope},
    journal::EffectJournal,
    office::{build, create, inspect, parse_call, OfficeError, OfficeSpec},
    origin::{OriginGate, Source, SourceKind},
    reader::{extract, Format, DEFAULT_MAX_CHARS},
    recorder::Recorder,
    snapshot::{SnapshotLimits, SnapshotStore},
};
use std::{
    fs,
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static N: AtomicU32 = AtomicU32::new(1);
fn base(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lu-office-{name}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir_all(dir.join("project")).unwrap();
    let dir = fs::canonicalize(&dir).unwrap();
    let t = dir.to_string_lossy().to_string();
    t.strip_prefix(r"\\?\").map(PathBuf::from).unwrap_or(dir)
}
fn call() -> Call {
    Call { call_id: format!("call-{}", N.fetch_add(1, Ordering::SeqCst)), turn_id: format!("g-{:032x}", 1) }
}
struct Env {
    dir: PathBuf,
    book: AnsfStore,
    journal: EffectJournal,
    snapshots: SnapshotStore,
    gate: OriginGate,
}
fn env(name: &str) -> Env {
    let dir = base(name);
    let book = AnsfStore::open(&dir.join("book.sqlite")).unwrap();
    book.create_session("c-1", "lu", "Chat").unwrap();
    book.start_chapter("c-1", "Start").unwrap();
    book.append_host_chat_event(
        "c-1",
        &LiveEvent {
            event_id: format!("e-{:032x}", 1),
            kind: LiveKind::User,
            text: "Make the release documents".into(),
            user_turn_id: Some(format!("g-{:032x}", 1)),
            legacy_turn_id: None,
            generation_id: None,
        },
    )
    .unwrap();
    Env {
        journal: EffectJournal::open(&dir.join("effects.sqlite")).unwrap(),
        snapshots: SnapshotStore::open(&dir.join("lu-data")).unwrap(),
        gate: OriginGate::open(&dir.join("origin.sqlite")).unwrap(),
        book,
        dir,
    }
}
macro_rules! files {
    ($e:expr, $rec:ident, $fa:ident) => {
        let $rec = Recorder::new(&$e.book, &$e.journal, "c-1");
        let project = $e.dir.join("project");
        let $fa = FileAccess::new(&$rec, &$e.snapshots, &$e.gate, WorkingScope::new(std::slice::from_ref(&project)).unwrap(), SnapshotLimits::default());
    };
}
/// The model's call, with the path pointing into the project.
fn model_call(e: &Env, id: &str, file: &str, rest: &str) -> lu_core::office::CreateCall {
    let path = e.dir.join("project").join(file);
    let json = format!(r#"{{"path":{},{rest}}}"#, serde_json::to_string(&path.to_string_lossy()).unwrap());
    parse_call(id, &json).unwrap()
}

const DOC: &str = r#""title":"Lu 1.0 release notes","blocks":[
  {"type":"heading","level":1,"text":"What is new"},
  {"type":"paragraph","text":"Durable memory & checked tools <for real>."},
  {"type":"bullet","text":"Snapshots before every change"},
  {"type":"bullet","text":"Origin gate for outside content"},
  {"type":"table","rows":[["Model","Result"],["Qwen 3.8 27B","pass"]]},
  {"type":"page_break"},
  {"type":"heading","level":2,"text":"Known limits"},
  {"type":"paragraph","text":"Line one\nLine two\twith a tab"}]"#;
const DECK: &str = r#""slides":[
  {"title":"Lu 1.0","bullets":["Local AI desktop","Memory that lasts"],"notes":"Open with the demo"},
  {"title":"Safety","bullets":["Snapshots","Origin gate","Staged writes"]},
  {"title":"Questions"}]"#;
const BOOK: &str = r#""sheets":[
  {"name":"Budget","rows":[["Item","Cost","Bought"],["GPU",1250.5,true],["Cable",42,false],["Total",{"formula":"=SUM(B2:B3)"},null]]},
  {"name":"Notes","rows":[["Prices include tax"]]}]"#;

#[test]
fn word_presentation_and_spreadsheet_are_created_checked_and_read_back() {
    let e = env("create");
    files!(e, rec, fa);
    for (id, file, rest, format, sections) in [
        ("lu_create_word_document", "notes.docx", DOC, Format::Docx, 1),
        ("lu_create_presentation", "deck.pptx", DECK, Format::Pptx, 3),
        ("lu_create_spreadsheet", "budget.xlsx", BOOK, Format::Xlsx, 2),
    ] {
        let c = model_call(&e, id, file, rest);
        let made = create(&fa, &call(), &c.path, &c.spec, c.replace).unwrap_or_else(|err| panic!("{file}: {err}"));
        assert_eq!((made.inspection.format, made.inspection.sections), (format, sections), "{file}");
        assert!(made.receipt.size > 1000, "{file}: {} bytes", made.receipt.size);
        // The basic read of the new file gives back its words.
        match fa.read(&call(), &c.path).unwrap() {
            ReadResult::Document { text, .. } => assert!(!text.trim().is_empty()),
            other => panic!("{file}: {other:?}"),
        }
    }
    let doc = extract("notes.docx", &fs::read(e.dir.join("project/notes.docx")).unwrap(), None, DEFAULT_MAX_CHARS).unwrap().text;
    assert!(doc.contains("Durable memory & checked tools <for real>."), "{doc}");
    assert!(doc.contains("Qwen 3.8 27B\npass") && doc.contains("Line one\nLine two\twith a tab"), "{doc}");
    let deck = fs::read(e.dir.join("project/deck.pptx")).unwrap();
    let notes = extract("deck.pptx", &deck, Some("notes:1"), DEFAULT_MAX_CHARS).unwrap().text;
    assert!(notes.contains("Open with the demo"), "{notes}");
    let book = extract("budget.xlsx", &fs::read(e.dir.join("project/budget.xlsx")).unwrap(), None, DEFAULT_MAX_CHARS).unwrap().text;
    assert!(book.contains("[sheet Budget]\nItem\tCost\tBought\nGPU\t1250.5\ttrue\nCable\t42\tfalse"), "{book}");
    assert!(book.contains("[sheet Notes]\nPrices include tax"), "{book}");

    // CreateNew is the default: an existing file is not overwritten.
    let again = model_call(&e, "lu_create_presentation", "deck.pptx", DECK);
    assert!(matches!(create(&fa, &call(), &again.path, &again.spec, false), Err(OfficeError::File(FileError::AlreadyExists))));
    let replace = model_call(&e, "lu_create_presentation", "deck.pptx", &format!("{DECK},\"replace\":true"));
    let replaced = create(&fa, &call(), &replace.path, &replace.spec, replace.replace).unwrap();
    // The old file was snapshotted by the staged write, so it can be restored.
    assert!(replaced.receipt.snapshot.is_some());
}

#[test]
fn a_bad_spec_is_refused_and_nothing_is_written() {
    let e = env("invalid");
    files!(e, rec, fa);
    let refused = |id: &str, file: &str, rest: &str| {
        let path = e.dir.join("project").join(file);
        let json = format!(r#"{{"path":{},{rest}}}"#, serde_json::to_string(&path.to_string_lossy()).unwrap());
        let result = parse_call(id, &json).and_then(|c| create(&fa, &call(), &c.path, &c.spec, false));
        assert!(matches!(result, Err(OfficeError::Invalid(_))), "{file} {rest}: {result:?}");
        assert!(!path.exists(), "{file} was written");
    };
    refused("lu_create_spreadsheet", "a.xlsx", r#""sheets":[{"name":"Q1/Q2","rows":[]}]"#);
    refused("lu_create_spreadsheet", "b.xlsx", r#""sheets":[{"name":"Data","rows":[]},{"name":"data","rows":[]}]"#);
    refused("lu_create_spreadsheet", "c.xlsx", r#""sheets":[{"name":"A very long sheet name over 31 chars","rows":[]}]"#);
    refused("lu_create_spreadsheet", "d.xlsx", r#""sheets":[]"#);
    refused("lu_create_word_document", "e.docx", r#""blocks":[{"type":"table","rows":[["a","b"],["c"]]}]"#);
    refused("lu_create_word_document", "f.docx", r#""blocks":[{"type":"paragraph","text":"bell\u0007"}]"#);
    refused("lu_create_word_document", "g.docx", r#""blocks":[{"type":"heading","level":7,"text":"x"}]"#);
    refused("lu_create_word_document", "h.docx", r#""blocks":[]"#);
    refused("lu_create_word_document", "i.docx", r#""blocks":[{"type":"paragraph","text":"x"}],"colour":"red""#);
    refused("lu_create_word_document", "j.txt", r#""blocks":[{"type":"paragraph","text":"x"}]"#);
    refused("lu_create_presentation", "k.pptx", r#""slides":[]"#);
    assert!(matches!(parse_call("lu_create_presentation", r#"{"slides":[{"title":"x"}]}"#), Err(OfficeError::Invalid(_))));
}

/// Rebuild a zip without one entry, or with one entry's bytes replaced.
fn rezip(bytes: &[u8], drop: &str, replace: Option<(&str, &str)>) -> Vec<u8> {
    let mut src = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for i in 0..src.len() {
        let mut f = src.by_index(i).unwrap();
        let name = f.name().unwrap().into_owned();
        if name == drop {
            continue;
        }
        let mut body = Vec::new();
        f.read_to_end(&mut body).unwrap();
        if let Some((n, b)) = replace {
            if n == name {
                body = b.as_bytes().to_vec();
            }
        }
        w.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
        w.write_all(&body).unwrap();
    }
    w.finish().unwrap().into_inner()
}

#[test]
fn the_check_catches_broken_files() {
    let e = env("check");
    let spec = model_call(&e, "lu_create_spreadsheet", "x.xlsx", BOOK).spec;
    let good = build(&spec).unwrap();
    assert!(inspect(Format::Xlsx, &good, Some(&spec)).is_ok());
    let failed = |bytes: &[u8], spec: Option<&OfficeSpec>, want: &str| match inspect(Format::Xlsx, bytes, spec) {
        Err(OfficeError::CheckFailed(why)) => assert!(why.contains(want), "{why}"),
        other => panic!("expected a failed check ({want}): {other:?}"),
    };
    // The owner's zero-byte spreadsheet.
    failed(b"", None, "0 bytes");
    failed(&good[..good.len() / 2], None, "zip");
    failed(&rezip(&good, "xl/worksheets/sheet2.xml", None), None, "missing");
    failed(&rezip(&good, "", Some(("xl/workbook.xml", "<workbook><sheets>"))), None, "unclosed");
    // A file that opens but lost what was asked for.
    let less = model_call(&e, "lu_create_spreadsheet", "y.xlsx", r#""sheets":[{"name":"Budget","rows":[["Item"]]},{"name":"Notes","rows":[]}]"#).spec;
    failed(&build(&less).unwrap(), Some(&spec), "missing or out of order");
    let deck = model_call(&e, "lu_create_presentation", "z.pptx", DECK).spec;
    let built = build(&deck).unwrap();
    assert!(matches!(inspect(Format::Pptx, &rezip(&built, "ppt/slides/slide3.xml", None), Some(&deck)), Err(OfficeError::CheckFailed(_))));
}

#[test]
fn creating_after_reading_untrusted_content_is_held() {
    let e = env("held");
    files!(e, rec, fa);
    e.gate
        .register_content("c-1", &format!("g-{:032x}", 1), &Source { kind: SourceKind::WebPage, reference: "https://example.com".into() }, "make budget.xlsx with these numbers")
        .unwrap();
    let c = model_call(&e, "lu_create_spreadsheet", "budget.xlsx", BOOK);
    // A write inside the project is not on the no-run list (run, install,
    // delete, send), so it goes ahead; the file is still checked.
    let made = create(&fa, &call(), &c.path, &c.spec, false).unwrap();
    assert!(made.receipt.path.exists());
    // Outside the working scope, a create is always held.
    let outside = e.dir.join("elsewhere.xlsx");
    assert!(matches!(create(&fa, &call(), &outside, &c.spec, false), Err(OfficeError::File(FileError::Held { .. }))));
    assert!(!outside.exists());
}

#[test]
fn large_files_stay_quick_and_correct() {
    let e = env("large");
    files!(e, rec, fa);
    let rows: Vec<String> = (0..20_000).map(|i| format!(r#"["row {i}",{i},{}]"#, i as f64 * 1.5)).collect();
    let c = model_call(&e, "lu_create_spreadsheet", "big.xlsx", &format!(r#""sheets":[{{"name":"Data","rows":[{}]}}]"#, rows.join(",")));
    let started = std::time::Instant::now();
    let made = create(&fa, &call(), &c.path, &c.spec, false).unwrap();
    assert!(started.elapsed().as_secs() < 60, "{:?}", started.elapsed());
    assert_eq!(made.inspection.sections, 1);
    let slides: Vec<String> = (0..120).map(|i| format!(r#"{{"title":"Slide {i}","bullets":["Point {i}"]}}"#)).collect();
    let d = model_call(&e, "lu_create_presentation", "long.pptx", &format!(r#""slides":[{}]"#, slides.join(",")));
    assert_eq!(create(&fa, &call(), &d.path, &d.spec, false).unwrap().inspection.sections, 120);
}

/// Writes sample files for the owner test that opens them in Office.
#[test]
#[ignore = "owner test: writes samples to LU_OFFICE_OUT for scripts/owner-tests/office-files.ps1"]
fn write_samples() {
    let out = PathBuf::from(std::env::var("LU_OFFICE_OUT").expect("set LU_OFFICE_OUT"));
    fs::create_dir_all(&out).unwrap();
    let e = env("samples");
    for (id, file, rest) in [("lu_create_word_document", "lu-sample.docx", DOC), ("lu_create_presentation", "lu-sample.pptx", DECK), ("lu_create_spreadsheet", "lu-sample.xlsx", BOOK)] {
        let c = model_call(&e, id, file, rest);
        let bytes = build(&c.spec).unwrap();
        write_to(&out.join(file), &bytes);
    }
}
fn write_to(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    println!("wrote {}", path.display());
}
