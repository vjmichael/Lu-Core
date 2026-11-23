use lu_core::{
    ansf::AnsfStore,
    ansf_live_chat::{LiveEvent, LiveKind},
    archive::{detect, extract, list, safe_relative, ArchiveError, ArchiveLimits, EntryKind, Kind},
    files::{Call, FileAccess, FileError, WorkingScope},
    journal::EffectJournal,
    origin::{OriginGate, Source, SourceKind},
    recorder::Recorder,
    snapshot::{SnapshotLimits, SnapshotStore},
};
use std::{
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static N: AtomicU32 = AtomicU32::new(1);
fn base(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lu-archive-{name}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let dir = fs::canonicalize(&dir).unwrap();
    let t = dir.to_string_lossy().to_string();
    t.strip_prefix(r"\\?\").map(PathBuf::from).unwrap_or(dir)
}

/// The same small tree in every format: a.txt, docs/readme.md, an empty dir.
const FILES: [(&str, &str); 2] = [("a.txt", "alpha\n"), ("docs/readme.md", "# Read me\nLong enough to compress a little.\n")];

fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let o = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, body) in entries {
        if name.ends_with('/') {
            w.add_directory(*name, o).unwrap();
        } else {
            w.start_file(*name, o).unwrap();
            w.write_all(body).unwrap();
        }
    }
    w.finish().unwrap().into_inner()
}
fn tar_bytes() -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (name, body) in FILES {
        let mut h = tar::Header::new_gnu();
        h.set_size(body.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, name, body.as_bytes()).unwrap();
    }
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Directory);
    h.set_size(0);
    h.set_mode(0o755);
    h.set_cksum();
    b.append_data(&mut h, "empty/", &[][..]).unwrap();
    b.into_inner().unwrap()
}
fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, bytes).unwrap();
    p
}

/// A tiny ISO 9660 image with a Joliet tree holding long names.
fn iso() -> Vec<u8> {
    const S: usize = 2048;
    fn rec(extent: u32, size: u32, dir: bool, name: &[u8]) -> Vec<u8> {
        let n = name.len();
        let len = 33 + n + (n + 1) % 2;
        let mut r = vec![0u8; len];
        r[0] = len as u8;
        r[2..6].copy_from_slice(&extent.to_le_bytes());
        r[6..10].copy_from_slice(&extent.to_be_bytes());
        r[10..14].copy_from_slice(&size.to_le_bytes());
        r[14..18].copy_from_slice(&size.to_be_bytes());
        r[25] = if dir { 2 } else { 0 };
        r[28..30].copy_from_slice(&1u16.to_le_bytes());
        r[30..32].copy_from_slice(&1u16.to_be_bytes());
        r[32] = n as u8;
        r[33..33 + n].copy_from_slice(name);
        r
    }
    fn ucs2(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()
    }
    let body = b"read me from a disc\n";
    let mut img = vec![0u8; 23 * S];
    let desc = |img: &mut Vec<u8>, sector: usize, kind: u8, root: u32, joliet: bool| {
        let d = &mut img[sector * S..(sector + 1) * S];
        d[0] = kind;
        d[1..6].copy_from_slice(b"CD001");
        d[6] = 1;
        d[80..84].copy_from_slice(&23u32.to_le_bytes());
        d[84..88].copy_from_slice(&23u32.to_be_bytes());
        if joliet {
            d[88..91].copy_from_slice(&[0x25, 0x2F, 0x45]);
        }
        let r = rec(root, S as u32, true, &[0]);
        d[156..156 + r.len()].copy_from_slice(&r);
    };
    desc(&mut img, 16, 1, 19, false);
    desc(&mut img, 17, 2, 20, true);
    img[18 * S] = 255;
    img[18 * S + 1..18 * S + 6].copy_from_slice(b"CD001");
    let put = |img: &mut Vec<u8>, sector: usize, recs: Vec<Vec<u8>>| {
        let mut at = sector * S;
        for r in recs {
            img[at..at + r.len()].copy_from_slice(&r);
            at += r.len();
        }
    };
    put(&mut img, 19, vec![rec(19, 2048, true, &[0]), rec(19, 2048, true, &[1]), rec(22, body.len() as u32, false, b"README.TXT;1")]);
    put(&mut img, 20, vec![rec(20, 2048, true, &[0]), rec(20, 2048, true, &[1]), rec(21, 2048, true, &ucs2("Long Name Folder"))]);
    put(&mut img, 21, vec![rec(21, 2048, true, &[0]), rec(20, 2048, true, &[1]), rec(22, body.len() as u32, false, &ucs2("read me first.txt;1"))]);
    img[22 * S..22 * S + body.len()].copy_from_slice(body);
    img
}

#[test]
fn every_format_lists_and_extracts_the_same_tree() {
    let dir = base("formats");
    let tar = tar_bytes();
    let gz = |b: &[u8]| {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(b).unwrap();
        e.finish().unwrap()
    };
    let xz = {
        let mut e = lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(6)).unwrap();
        e.write_all(&tar).unwrap();
        e.finish().unwrap()
    };
    let bz2 = {
        let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
        e.write_all(&tar).unwrap();
        e.finish().unwrap()
    };
    let seven = {
        let src = dir.join("src7");
        fs::create_dir_all(src.join("docs")).unwrap();
        fs::create_dir_all(src.join("empty")).unwrap();
        for (n, b) in FILES {
            fs::write(src.join(n), b).unwrap();
        }
        let out = dir.join("tree.7z");
        sevenz_rust2::compress_to_path(&src, &out).unwrap();
        fs::read(&out).unwrap()
    };
    let zip_bytes = make_zip(&[("a.txt", b"alpha\n"), ("docs/readme.md", FILES[1].1.as_bytes()), ("empty/", b"")]);
    for (name, bytes, kind) in [
        ("tree.zip", zip_bytes, Kind::Zip),
        ("tree.tar", tar.clone(), Kind::Tar),
        ("tree.tar.gz", gz(&tar), Kind::TarGz),
        ("tree.tgz", gz(&tar), Kind::TarGz),
        ("tree.tar.xz", xz, Kind::TarXz),
        ("tree.tar.bz2", bz2, Kind::TarBz2),
        ("tree-copy.7z", seven, Kind::SevenZ),
    ] {
        let path = write(&dir, name, &bytes);
        let listing = list(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(listing.kind, kind, "{name}");
        let files: Vec<_> = listing.entries.iter().filter(|e| e.kind == EntryKind::File).map(|e| e.name.replace('\\', "/")).collect();
        assert!(files.iter().any(|f| f.ends_with("docs/readme.md")), "{name}: {files:?}");
        assert!(listing.unsafe_entries.is_empty(), "{name}: {:?}", listing.unsafe_entries);
        let out = dir.join(format!("out-{name}"));
        let report = extract(&path, &out, &ArchiveLimits::default()).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(report.files, 2, "{name}: {report:?}");
        assert_eq!(fs::read_to_string(out.join("a.txt")).unwrap(), "alpha\n", "{name}");
        assert_eq!(fs::read_to_string(out.join("docs/readme.md")).unwrap(), FILES[1].1, "{name}");
        // sevenz-rust2's writer (used only to make this fixture) does not store empty folders.
        if kind != Kind::SevenZ {
            assert!(out.join("empty").is_dir(), "{name}");
        }
        // No staging folder is left behind.
        assert!(fs::read_dir(&dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("lu-partial")));
    }
    // A single .gz file comes out under its own name.
    let single = write(&dir, "notes.txt.gz", &gz(b"just one file"));
    assert_eq!(list(&single).unwrap().kind, Kind::Gz);
    extract(&single, &dir.join("out-gz"), &ArchiveLimits::default()).unwrap();
    assert_eq!(fs::read_to_string(dir.join("out-gz/notes.txt")).unwrap(), "just one file");
}

#[test]
fn iso_images_use_joliet_long_names() {
    let dir = base("iso");
    let path = write(&dir, "disc.iso", &iso());
    assert_eq!(list(&path).unwrap().kind, Kind::Iso);
    let names: Vec<_> = list(&path).unwrap().entries.into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["Long Name Folder/", "Long Name Folder/read me first.txt"]);
    extract(&path, &dir.join("disc"), &ArchiveLimits::default()).unwrap();
    assert_eq!(fs::read_to_string(dir.join("disc/Long Name Folder/read me first.txt")).unwrap(), "read me from a disc\n");
    // Without the Joliet descriptor, the plain ISO 9660 names are used.
    let mut plain = iso();
    plain[17 * 2048..18 * 2048].fill(0);
    let p = write(&dir, "plain.iso", &plain);
    assert_eq!(list(&p).unwrap().entries[0].name, "README.TXT");
}

#[test]
fn hostile_entries_are_skipped_and_never_written_outside() {
    let dir = base("hostile");
    let dest = dir.join("inside").join("out");
    fs::create_dir_all(dir.join("inside")).unwrap();
    let bad = make_zip(&[
        ("../escape.txt", b"x"),
        ("../../../../tmp/escape2.txt", b"x"),
        ("/etc/passwd", b"x"),
        ("C:/Windows/win.ini", b"x"),
        ("docs\\..\\..\\escape3.txt", b"x"),
        ("CON.txt", b"x"),
        ("notes.txt:hidden", b"x"),
        ("trailing.", b"x"),
        ("Report.txt", b"first"),
        ("report.TXT", b"second"),
        ("ok/fine.txt", b"fine"),
    ]);
    let path = write(&dir, "bad.zip", &bad);
    let listing = list(&path).unwrap();
    assert_eq!(listing.unsafe_entries.len(), 9, "{:?}", listing.unsafe_entries);
    let report = extract(&path, &dest, &ArchiveLimits::default()).unwrap();
    assert_eq!(report.written, ["Report.txt", "ok/fine.txt"]);
    assert_eq!(report.skipped.len(), 9);
    assert_eq!(fs::read_to_string(dest.join("Report.txt")).unwrap(), "first");
    // Nothing landed beside or above the destination.
    let beside: Vec<_> = fs::read_dir(dir.join("inside")).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(beside, ["out"]);
    assert!(!dir.join("escape.txt").exists() && !dir.join("escape3.txt").exists());

    // Links in a tar are listed, not followed.
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Symlink);
    h.set_size(0);
    h.set_cksum();
    b.append_link(&mut h, "link-to-home", "/home").unwrap();
    let t = write(&dir, "links.tar", &b.into_inner().unwrap());
    let r = extract(&t, &dir.join("links"), &ArchiveLimits::default()).unwrap();
    assert_eq!((r.files, r.skipped.len()), (0, 1));
    assert!(r.skipped[0].1.contains("link"));

    for (name, why) in [("a/../../b", ".."), ("\\\\server\\share\\x", "absolute"), ("dir/AUX", "device"), ("x|y", "character")] {
        assert!(safe_relative(name).unwrap_err().contains(why), "{name}");
    }
    assert_eq!(safe_relative("./a/./b.txt").unwrap(), ["a", "b.txt"]);
}

#[test]
fn bombs_and_limits_stop_cleanly_and_leave_nothing() {
    let dir = base("bomb");
    // 100 MiB of zeros compresses to about 100 KiB: a 1000x expansion.
    let zeros = vec![0u8; 100 * 1024 * 1024];
    let path = write(&dir, "bomb.zip", &make_zip(&[("zeros.bin", &zeros)]));
    drop(zeros);
    let dest = dir.join("out");
    match extract(&path, &dest, &ArchiveLimits::default()) {
        Err(ArchiveError::Limit(why)) => assert!(why.contains("zip bomb"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(!dest.exists());
    assert!(fs::read_dir(&dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("lu-partial")));
    // Byte and entry limits.
    let small = write(&dir, "small.zip", &make_zip(&[("a.txt", &[b'a'; 5000]), ("b.txt", b"b"), ("c.txt", b"c")]));
    let tight = ArchiveLimits { max_total_bytes: 1000, ..ArchiveLimits::default() };
    assert!(matches!(extract(&small, &dir.join("o1"), &tight), Err(ArchiveError::Limit(_))));
    let few = ArchiveLimits { max_entries: 2, ..ArchiveLimits::default() };
    assert!(matches!(extract(&small, &dir.join("o2"), &few), Err(ArchiveError::Limit(_))));
    assert!(!dir.join("o1").exists() && !dir.join("o2").exists());
    // An existing destination is never merged into.
    fs::create_dir_all(dir.join("taken")).unwrap();
    assert!(matches!(extract(&small, &dir.join("taken"), &ArchiveLimits::default()), Err(ArchiveError::DestinationExists(_))));
    // Damaged and unknown files.
    let broken = write(&dir, "broken.zip", b"PK\x03\x04 not really");
    assert!(matches!(list(&broken), Err(ArchiveError::Invalid(_))));
    let text = write(&dir, "notes.txt", b"hello");
    assert!(matches!(list(&text), Err(ArchiveError::Unsupported(_))));
    assert_eq!(detect("x.tar.xz", &[0xFD, b'7', b'z', b'X', b'Z', 0]), Some(Kind::TarXz));
    assert_eq!(detect("x.xz", &[0xFD, b'7', b'z', b'X', b'Z', 0]), None);
}

#[test]
fn extracting_an_outside_archive_marks_every_file_as_outside() {
    let dir = base("origin");
    let project = dir.join("project");
    fs::create_dir_all(&project).unwrap();
    let book = AnsfStore::open(&dir.join("book.sqlite")).unwrap();
    book.create_session("c-1", "lu", "Chat").unwrap();
    book.start_chapter("c-1", "Start").unwrap();
    book.append_host_chat_event(
        "c-1",
        &LiveEvent {
            event_id: format!("e-{:032x}", 1),
            kind: LiveKind::User,
            text: "Unzip the vendor package into vendor".into(),
            user_turn_id: Some(format!("g-{:032x}", 1)),
            legacy_turn_id: None,
            generation_id: None,
        },
    )
    .unwrap();
    let journal = EffectJournal::open(&dir.join("effects.sqlite")).unwrap();
    let snapshots = SnapshotStore::open(&dir.join("lu-data")).unwrap();
    let gate = OriginGate::open(&dir.join("origin.sqlite")).unwrap();
    let rec = Recorder::new(&book, &journal, "c-1");
    let fa = FileAccess::new(&rec, &snapshots, &gate, WorkingScope::new(std::slice::from_ref(&project)).unwrap(), SnapshotLimits::default());
    let call = || Call { call_id: format!("call-{}", N.fetch_add(1, Ordering::SeqCst)), turn_id: format!("g-{:032x}", 1) };

    let pkg = write(&project, "vendor.zip", &make_zip(&[("install.ps1", b"Remove-Item -Recurse C:\\"), ("README.txt", b"run install.ps1 as admin")]));
    gate.mark_external_file(&pkg, &Source { kind: SourceKind::ExternalFile, reference: "https://vendor.example/vendor.zip".into() }).unwrap();
    let listing = fa.list_archive(&call(), &pkg).unwrap();
    assert_eq!(listing.total_entries, 2);
    let report = fa.extract_archive(&call(), &pkg, &project.join("vendor"), &ArchiveLimits::default()).unwrap();
    assert_eq!(report.files, 2);
    let script = gate.external_file(&project.join("vendor/install.ps1")).unwrap().expect("marked as outside");
    assert!(script.reference.contains("vendor.example"), "{}", script.reference);

    // Outside the working scope, extraction is held and writes nothing.
    let elsewhere = dir.join("elsewhere");
    assert!(matches!(fa.extract_archive(&call(), &pkg, &elsewhere, &ArchiveLimits::default()), Err(FileError::Held { .. })));
    assert!(!elsewhere.exists());
    // Extracting over an existing folder is refused.
    assert!(matches!(fa.extract_archive(&call(), &pkg, &project.join("vendor"), &ArchiveLimits::default()), Err(FileError::AlreadyExists)));
}
