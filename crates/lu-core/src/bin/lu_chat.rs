//! `lu-chat`: talk to Lu in a terminal until the desktop app exists.
//!
//!   lu-chat install <cpu|vulkan|rocm> [--rocm-source <therock tarball>]
//!   lu-chat [--model <file.gguf>] [--workspace <folder>] [--session <name>]
//!
//! Data lives in a development folder of its own (`%LOCALAPPDATA%\Lu-Dev`,
//! or `~/.local/share/lu-dev`; override with LU_DATA), never in LM Studio's
//! or the owner's real Lu data. Commands and file writes are limited to the
//! workspace folder (default: `<data>/workspace`). Type /quit to leave.

use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
    sync::atomic::AtomicBool,
};

use lu_core::{
    ansf::AnsfStore,
    chat::{new_id, ChatConfig, ChatEvent, ChatLoop},
    engines::{load_with_fallback, shipped_packs, shipped_pins, EngineSource, EngineStore, RunSettings, Variant},
    environment::EnvironmentStore,
    files::{FileAccess, WorkingScope},
    journal::EffectJournal,
    origin::OriginGate,
    recorder::Recorder,
    resources::{measure, Governor},
    runner::CommandRunner,
    snapshot::{SnapshotLimits, SnapshotStore},
};

fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("LU_DATA") {
        return PathBuf::from(d);
    }
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("Lu-Dev")
    } else {
        std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join(".local/share/lu-dev")
    }
}

fn arg(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

fn main() {
    if let Err(e) = run() {
        eprintln!("lu-chat: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let data = data_dir();
    std::fs::create_dir_all(&data).map_err(|e| e.to_string())?;
    let session_name = arg(&args, "--session").unwrap_or_else(|| "terminal".into());
    let session = format!("c-{:0>32}", session_name.bytes().map(|b| format!("{b:02x}")).collect::<String>().chars().take(32).collect::<String>());
    let book = AnsfStore::open(&data.join("ansf.sqlite")).map_err(|e| format!("{e:?}"))?;
    // A new session the first time; an existing one carries on.
    if book.create_session(&session, "lu", &session_name).is_ok() {
        book.start_chapter(&session, "Start").map_err(|e| format!("{e:?}"))?;
    }
    let journal = EffectJournal::open(&data.join("effects.sqlite")).map_err(|e| format!("{e:?}"))?;
    let gate = OriginGate::open(&data.join("origin.sqlite")).map_err(|e| format!("{e:?}"))?;
    let governor = Governor::open(&data.join("resources.sqlite")).map_err(|e| e.to_string())?;
    let rec = Recorder::new(&book, &journal, &session);
    let source = EngineSource::github();
    let engines = EngineStore::open(&data.join("engines"), shipped_pins(), &source, &rec, &gate, &governor).map_err(|e| e.to_string())?;

    if args.first().map(String::as_str) == Some("install") {
        let variant = args.get(1).and_then(|v| Variant::parse(v)).ok_or("install cpu, vulkan or rocm")?;
        // The install is the user's own instruction, typed here.
        let turn = new_id('g');
        book.append_host_chat_event(
            &session,
            &lu_core::ansf_live_chat::LiveEvent { event_id: new_id('e'), kind: lu_core::ansf_live_chat::LiveKind::User, text: format!("install the {} engine", variant.as_str()), user_turn_id: Some(turn.clone()), legacy_turn_id: None, generation_id: None },
        )
        .map_err(|e| format!("{e:?}"))?;
        let mut shown = 101;
        let mut progress = |d: u64, t: u64| {
            let p = d * 100 / t.max(1);
            if p / 10 != shown / 10 {
                shown = p;
                eprint!("\r{p:>3}%");
            }
        };
        if variant == Variant::Rocm {
            let gpus = measure(None).gpus;
            let pack = shipped_packs().into_iter().find(|p| gpus.iter().any(|g| p.serves(&g.name))).ok_or("no ROCm runtime pack for this GPU")?;
            eprintln!("Installing {} ({} MB)...", pack.name, pack.installed_bytes() / 1_000_000);
            let local = arg(&args, "--rocm-source").map(PathBuf::from);
            engines.install_pack(&new_id('c'), &turn, &pack, local.as_deref(), &AtomicBool::new(false), &mut progress).map_err(|e| e.to_string())?;
            eprintln!();
        }
        let pin = engines.pins().iter().find(|p| p.variant == variant).cloned().ok_or("no pinned build for this platform")?;
        eprintln!("Installing llama.cpp {} {} ({} MB)...", pin.version, variant.as_str(), pin.bytes / 1_000_000);
        let b = engines.install(&new_id('c'), &turn, &pin, true, &AtomicBool::new(false), &mut progress).map_err(|e| e.to_string())?;
        eprintln!("\nInstalled: {}", b.report);
        return Ok(());
    }

    let model = arg(&args, "--model").ok_or("give a model: lu-chat --model <file.gguf>")?;
    let workspace = arg(&args, "--workspace").map(PathBuf::from).unwrap_or_else(|| data.join("workspace"));
    std::fs::create_dir_all(&workspace).map_err(|e| e.to_string())?;
    let workspace = std::fs::canonicalize(&workspace).map_err(|e| e.to_string())?;
    let workspace = PathBuf::from(workspace.to_string_lossy().trim_start_matches(r"\\?\").to_owned());
    let snapshots = SnapshotStore::open(&data.join("snapshots")).map_err(|e| format!("{e:?}"))?;
    let env = EnvironmentStore::open(&data.join("environment.sqlite")).map_err(|e| format!("{e:?}"))?;
    let files = FileAccess::new(&rec, &snapshots, &gate, WorkingScope::new(std::slice::from_ref(&workspace)).map_err(|e| format!("{e:?}"))?, SnapshotLimits::default());
    let runner = CommandRunner::new(&rec, &snapshots, &gate, SnapshotLimits::default(), &data.join("runner")).map_err(|e| format!("{e:?}"))?;
    let gpu = !measure(None).gpus.is_empty();
    eprintln!("Loading {}...", PathBuf::from(&model).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    let (mut engine, failed) = load_with_fallback(&engines, &model, gpu, &RunSettings { context: 16384, ..RunSettings::default() }, &data.join("logs")).map_err(|e| e.to_string())?;
    for f in failed {
        eprintln!("(skipped {f})");
    }
    eprintln!("Ready on {} {}. Workspace: {}. Type /quit to leave.\n", engine.build.variant.as_str(), engine.build.version, workspace.display());
    let chat = ChatLoop { recorder: &rec, files: &files, runner: &runner, environment: Some(&env), profile: None, config: ChatConfig::new(&workspace) };
    let stdin = io::stdin();
    loop {
        eprint!("you> ");
        io::stderr().flush().ok();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            break;
        }
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        if text == "/quit" {
            break;
        }
        print!("lu> ");
        let out = chat.turn(&mut engine, text, &mut |ev| match ev {
            ChatEvent::Text(t) => {
                print!("{t}");
                io::stdout().flush().ok();
            }
            ChatEvent::ActionStarted { what, .. } => eprint!("\n  [{what}]"),
            ChatEvent::ActionFinished { ok, .. } => eprint!("{}", if ok { " ok\n" } else { " failed\n" }),
            ChatEvent::Held { what, reason } => eprint!("\n  [held: {what}: {reason}]\n"),
            ChatEvent::Done => {}
        });
        match out {
            Ok(o) => {
                println!();
                if let Some(why) = o.stopped {
                    eprintln!("  (stopped: {why})");
                }
                if let Some(s) = o.tokens_per_second {
                    eprintln!("  ({s:.1} tokens/s)\n");
                }
            }
            Err(e) => eprintln!("\n  error: {e}\n"),
        }
    }
    Ok(())
}
