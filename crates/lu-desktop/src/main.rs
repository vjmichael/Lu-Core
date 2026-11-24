//! Lu desktop: windows over Lu core.
//!
//! The windows only see plain, user-facing data: conversations, folders,
//! models and their settings, downloads and engines. Lu's own machinery
//! (origin gate, journals, memory blocks, snapshots) works underneath and
//! does not appear in the interface.
//!
//! One model thread owns the loaded engine and runs chat turns, streaming
//! `chat-event`s to the windows. Downloads and engine installs run on their
//! own threads with progress events. Each click that changes something is
//! recorded as the user's instruction in a control conversation, so Lu's
//! origin gate treats it as the user's own request.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod catalog;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use catalog::{find_gguf, sibling_projector, Catalog, ModelEntry, ModelSettings};
use lu_core::{
    ansf::AnsfStore,
    ansf_live_chat::{LiveEvent, LiveKind},
    dx::{self, DxStore, ExactAnswer, Finding, Sources},
    chat::{folders_named, image_data_url, is_image, new_id, ChatConfig, ChatEvent, ChatLoop, DEFAULT_SYSTEM_PROMPT},
    conversations::{ConversationLibrary, SearchHit},
    engines::{load_with_fallback, reasoning_levels, shipped_packs, shipped_pins, EngineSource, EngineStore, LlamaServer, RunSettings, Variant},
    environment::EnvironmentStore,
    files::{FileAccess, WorkingScope},
    gguf,
    hf::{FileLead, Hub, InstallRequest, ModelInstaller},
    journal::EffectJournal,
    origin::OriginGate,
    recorder::Recorder,
    resources::{self, measure, Governor},
    runner::{CancelToken, CommandRunner},
    snapshot::{SnapshotLimits, SnapshotStore},
};
use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

/// Lu's stores, shared by every window for the life of the app.
struct Base {
    data: PathBuf,
    library: ConversationLibrary,
    catalog: Catalog,
    journal: EffectJournal,
    gate: OriginGate,
    governor: Governor,
    snapshots: SnapshotStore,
    environment: EnvironmentStore,
    hub: Hub,
    source: EngineSource,
    /// Where the user's clicks are recorded as their instructions.
    control: AnsfStore,
    /// DX: diagnostics and benchmark records, exports in <data>/dx.
    dx: DxStore,
}
const CONTROL: &str = "c-00000000000000000000000000000001";

impl Base {
    fn open(data: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(data).map_err(|e| e.to_string())?;
        let control = AnsfStore::open(&data.join("control.sqlite")).map_err(|e| format!("{e:?}"))?;
        if control.create_session(CONTROL, "lu", "Settings and downloads").is_ok() {
            control.start_chapter(CONTROL, "Start").map_err(|e| format!("{e:?}"))?;
        }
        Ok(Self {
            library: ConversationLibrary::open(&data.join("conversations")).map_err(|e| e.to_string())?,
            catalog: Catalog::open(&data.join("models.sqlite"))?,
            journal: EffectJournal::open(&data.join("effects.sqlite")).map_err(|e| format!("{e:?}"))?,
            gate: OriginGate::open(&data.join("origin.sqlite")).map_err(|e| format!("{e:?}"))?,
            governor: Governor::open(&data.join("resources.sqlite")).map_err(|e| e.to_string())?,
            snapshots: SnapshotStore::open(&data.join("snapshots")).map_err(|e| format!("{e:?}"))?,
            environment: EnvironmentStore::open(&data.join("environment.sqlite")).map_err(|e| format!("{e:?}"))?,
            hub: Hub::official(),
            source: EngineSource::github(),
            control,
            dx: DxStore::open(data)?,
            data: data.to_path_buf(),
        })
    }

    /// Record a click as the user's instruction; returns its turn ID.
    fn instruction(&self, text: &str) -> Result<String, String> {
        let turn = new_id('g');
        self.control
            .append_host_chat_event(CONTROL, &LiveEvent { event_id: new_id('e'), kind: LiveKind::User, text: text.into(), user_turn_id: Some(turn.clone()), legacy_turn_id: None, generation_id: None })
            .map_err(|e| format!("{e:?}"))?;
        Ok(turn)
    }

    fn workspace(&self) -> PathBuf {
        let p = self.catalog.setting("workspace").map(PathBuf::from).unwrap_or_else(|| self.data.join("workspace"));
        let _ = std::fs::create_dir_all(&p);
        p
    }
}

/// Development builds keep their own data (never the owner's real Lu data).
fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("LU_DATA") {
        return PathBuf::from(d);
    }
    let base = if cfg!(windows) { std::env::var_os("LOCALAPPDATA") } else { std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share").into_os_string()) };
    PathBuf::from(base.unwrap_or_default()).join(if cfg!(windows) { "Lu-Dev" } else { "lu-dev" })
}

// ---------------------------------------------------------------- model thread

#[derive(Debug, Clone, Serialize, Default)]
struct ModelStatus {
    /// `unloaded`, `loading`, `loaded` or `error`.
    state: String,
    model_id: Option<i64>,
    model: Option<String>,
    backend: Option<String>,
    /// 0..1 while loading (an estimate from earlier loads of this model).
    progress: f64,
    message: Option<String>,
    busy: bool,
    /// Reasoning settings the loaded model takes (empty: none).
    reasoning: Vec<String>,
    /// The load was refused for lack of memory; the user may load anyway.
    can_force: bool,
}

/// The memory a model takes, estimated from its file: weights (with the
/// image file), the context cache for its context length, and buffers.
fn estimate_mib(model: &ModelEntry) -> u64 {
    let weights = model.bytes.div_ceil(1024 * 1024);
    let projector = model.projector.as_ref().and_then(|p| std::fs::metadata(p).ok()).map_or(0, |m| m.len().div_ceil(1024 * 1024));
    let kv = gguf::inspect(Path::new(&model.path)).ok().and_then(|i| i.kv_cache_mib(u64::from(model.settings.context))).unwrap_or(weights / 4);
    weights + projector + kv + 512 + weights / 20
}

enum Job {
    /// Load a model; `force` loads even when Lu measured it does not fit.
    Load(i64, bool),
    Unload,
    Send { chat: String, text: String, attachments: Vec<String>, reasoning: Option<String> },
    /// A benchmark on the loaded model: `speed` or `memory`.
    Bench(String),
}

/// Downloads and engine installs in progress, so any window or tab that
/// opens later shows them correctly. Keyed by `download:<repo>/<file>` or
/// `engine:<id>`.
static TRANSFERS: Mutex<BTreeMap<String, Value>> = Mutex::new(BTreeMap::new());
fn transfer(key: &str, v: Option<Value>) {
    if let Ok(mut t) = TRANSFERS.lock() {
        match v {
            Some(v) => {
                t.insert(key.to_owned(), v);
            }
            None => {
                t.remove(key);
            }
        }
    }
}
/// The answer being written right now, per conversation, so a conversation
/// reopened mid-answer shows it. Also which conversations are waiting.
static LIVE: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());
static WAITING: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// The conversation being answered now, and how to stop it.
static CURRENT: Mutex<Option<(String, CancelToken)>> = Mutex::new(None);
/// Waiting messages the user stopped before they started.
static SKIP: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Approvals Lu is waiting for: id -> the user's answer.
static APPROVALS: Mutex<BTreeMap<String, mpsc::Sender<String>>> = Mutex::new(BTreeMap::new());
/// How long Lu waits for an approval before ending the turn.
const APPROVAL_SECONDS: u64 = 120;

struct App {
    base: &'static Base,
    jobs: Mutex<mpsc::Sender<Job>>,
    status: Arc<Mutex<ModelStatus>>,
}

fn set_status(app: &AppHandle, status: &Arc<Mutex<ModelStatus>>, f: impl FnOnce(&mut ModelStatus)) {
    let snapshot = {
        let mut s = status.lock().expect("status");
        f(&mut s);
        s.clone()
    };
    let _ = app.emit("model-status", snapshot);
}

fn variant_of(name: &str) -> Option<Variant> {
    Variant::parse(name)
}

fn model_thread(app: AppHandle, base: &'static Base, status: Arc<Mutex<ModelStatus>>, jobs: mpsc::Receiver<Job>) {
    let mut engine: Option<(LlamaServer, ModelEntry)> = None;
    for job in jobs {
        match job {
            Job::Unload => {
                if let Some((mut e, _)) = engine.take() {
                    let _ = lu_core::resources::Engine::unload(&mut e);
                }
                set_status(&app, &status, |s| *s = ModelStatus { state: "unloaded".into(), ..Default::default() });
            }
            Job::Load(id, force) => {
                if let Some((mut e, _)) = engine.take() {
                    let _ = lu_core::resources::Engine::unload(&mut e);
                }
                let model = match base.catalog.get(id) {
                    // An imported model whose image support file sits next to it: pair them.
                    Ok(mut m) => {
                        if m.projector.is_none() {
                            if let Some(p) = sibling_projector(Path::new(&m.path)) {
                                if base.catalog.set_projector(id, &p).is_ok() {
                                    m.projector = Some(p.to_string_lossy().into_owned());
                                }
                            }
                        }
                        m
                    }
                    Err(e) => {
                        set_status(&app, &status, |s| *s = ModelStatus { state: "error".into(), message: Some(e), ..Default::default() });
                        continue;
                    }
                };
                set_status(&app, &status, |s| *s = ModelStatus { state: "loading".into(), model_id: Some(id), model: Some(model.name.clone()), ..Default::default() });
                let turn = base.instruction(&format!("Load the model {}", model.name)).unwrap_or_default();
                let rec = Recorder::new(&base.control, &base.journal, CONTROL);
                let store = match EngineStore::open(&base.data.join("engines"), shipped_pins(), &base.source, &rec, &base.gate, &base.governor) {
                    Ok(s) => s,
                    Err(e) => {
                        set_status(&app, &status, |s| *s = ModelStatus { state: "error".into(), message: Some(e.to_string()), ..Default::default() });
                        continue;
                    }
                };
                let _ = turn;
                // The backend the user chose for this model, or Lu's order.
                match variant_of(&model.settings.backend) {
                    Some(v) => {
                        let _ = store.pin_model(&model.path, v, None);
                    }
                    None => {
                        let _ = store.unpin_model(&model.path);
                    }
                }
                // Progress estimate from the last successful load of this model.
                let expected_ms = store.records(&model.path).ok().and_then(|r| r.into_iter().find(|x| x.ok).and_then(|x| x.load_ms)).unwrap_or_else(|| 5_000 + model.bytes / 1_500_000);
                let done = Arc::new(AtomicBool::new(false));
                let ticker = {
                    let (app, status, done) = (app.clone(), status.clone(), done.clone());
                    thread::spawn(move || {
                        let start = Instant::now();
                        while !done.load(Ordering::Relaxed) {
                            let frac = (start.elapsed().as_millis() as f64 / expected_ms.max(1) as f64).min(0.95);
                            set_status(&app, &status, |s| s.progress = frac);
                            thread::sleep(Duration::from_millis(200));
                        }
                    })
                };
                let gpu = !measure(None).gpus.is_empty();
                // Does it fit? Measured from its last load with these
                // settings, or estimated from the file (weights + context cache).
                let on_gpu = gpu && model.settings.backend != "cpu";
                let key = format!("{}#ctx{}", model.path, model.settings.context);
                let estimate = estimate_mib(&model);
                let fp = base.governor.measured_footprint(&key).ok().flatten();
                let (need_ram, need_vram) = match (&fp, on_gpu) {
                    (Some(f), _) => (f.ram_mib, f.vram_mib),
                    (None, true) => (1024, estimate),
                    (None, false) => (estimate, 0),
                };
                if !force {
                    let need = resources::Need { ram_mib: need_ram, vram_mib: need_vram, ..Default::default() };
                    match base.governor.admit(&format!("load {}", model.name), &need) {
                        Ok(resources::Admission::Admit) => {}
                        Ok(resources::Admission::Refuse(why)) | Ok(resources::Admission::Unmeasured(why)) => {
                            done.store(true, Ordering::Relaxed);
                            let _ = ticker.join();
                            let gb = |m: u64| format!("{:.1} GB", m as f64 / 1024.0);
                            let what = if on_gpu { format!("about {} of graphics memory", gb(need_vram)) } else { format!("about {} of memory", gb(need_ram)) };
                            let msg = format!(
                                "{} needs {what}{}, more than is free now, so Lu did not load it (loading it could slow the computer to a crawl). Close other programs that use the GPU, pick a smaller version, or lower its context length. Details: {}",
                                model.name,
                                if fp.is_some() { " (measured last time)" } else { " (estimated)" },
                                why.join("; ")
                            );
                            set_status(&app, &status, |s| *s = ModelStatus { state: "error".into(), model_id: Some(id), model: Some(model.name.clone()), message: Some(msg), can_force: true, ..Default::default() });
                            continue;
                        }
                        Err(_) => {}
                    }
                }
                let before = measure(None);
                let settings = RunSettings {
                    context: model.settings.context,
                    gpu_layers: model.settings.gpu_layers,
                    projector: model.projector.as_ref().map(PathBuf::from),
                    ..RunSettings::default()
                };
                let result = load_with_fallback(&store, &model.path, gpu, &settings, &base.data.join("logs"));
                done.store(true, Ordering::Relaxed);
                let _ = ticker.join();
                match result {
                    Ok((server, _failed)) => {
                        // Remember what this load really took, for next time.
                        let after = measure(None);
                        let vram = match (before.gpus.first().and_then(|g| g.used_mib), after.gpus.first().and_then(|g| g.used_mib)) {
                            (Some(a), Some(b)) => b.saturating_sub(a),
                            _ => 0,
                        };
                        let ram = match (before.ram_available_mib, after.ram_available_mib) {
                            (Some(a), Some(b)) => a.saturating_sub(b),
                            _ => 0,
                        };
                        if vram + ram > 256 {
                            let _ = base.governor.record_footprint(&key, ram, vram);
                        }
                        let backend = match server.build.variant {
                            Variant::Rocm => "ROCm",
                            Variant::Vulkan => "Vulkan",
                            Variant::Cpu => "CPU",
                        }
                        .to_owned();
                        let reasoning = reasoning_levels(&server.chat_template().unwrap_or_default()).into_iter().map(str::to_owned).collect();
                        set_status(&app, &status, |s| *s = ModelStatus { state: "loaded".into(), model_id: Some(id), model: Some(model.name.clone()), backend: Some(backend), progress: 1.0, reasoning, ..Default::default() });
                        engine = Some((server, model));
                    }
                    Err(e) => {
                        let msg = plain_load_error(&e.to_string());
                        set_status(&app, &status, |s| *s = ModelStatus { state: "error".into(), model: Some(model.name.clone()), message: Some(msg), ..Default::default() });
                    }
                }
            }
            Job::Bench(kind) => {
                let Some((server, model)) = engine.as_mut() else {
                    let _ = app.emit("dx-event", json!({"kind": kind, "error": "Load a model first."}));
                    continue;
                };
                set_status(&app, &status, |s| s.busy = true);
                let label = model.name.clone();
                let result = if kind == "memory" {
                    let scratch = base.data.join("dx-scratch");
                    let _ = std::fs::create_dir_all(&scratch);
                    base.dx.memory_benchmark(server, &model.path, &dx::memory_set(), &ExactAnswer, 6, &scratch).map(|r| {
                        json!({"run": r.id, "model": label, "backend": r.backend,
                            "arms": r.arms.iter().map(|(a, c, j, t)| json!({"arm": a, "correct": c, "judged": j, "total": t})).collect::<Vec<_>>(),
                            "results": r.results.iter().map(|(a, x)| json!({"arm": a, "task": x.task, "correct": x.correct, "answer": x.answer.chars().take(400).collect::<String>()})).collect::<Vec<_>>()})
                    })
                } else {
                    base.dx.benchmark(server, &model.path, &dx::starter_tasks(), &ExactAnswer, 1024).map(|r| {
                        let (c, j, t) = r.score();
                        json!({"run": r.id, "model": label, "backend": r.backend, "correct": c, "judged": j, "total": t, "speed": r.mean_speed(),
                            "results": r.results.iter().map(|x| json!({"task": x.task, "kind": x.kind, "correct": x.correct, "speed": x.tokens_per_second, "answer": x.answer.chars().take(400).collect::<String>()})).collect::<Vec<_>>()})
                    })
                };
                set_status(&app, &status, |s| s.busy = false);
                let _ = app.emit("dx-event", match result {
                    Ok(mut v) => {
                        v["kind"] = json!(kind);
                        v
                    }
                    Err(e) => json!({"kind": kind, "error": e}),
                });
            }
            Job::Send { chat, text, attachments, reasoning } => {
                if let Ok(mut w) = WAITING.lock() {
                    if let Some(i) = w.iter().position(|c| *c == chat) {
                        w.remove(i);
                    }
                }
                let skipped = SKIP.lock().map(|mut s| s.iter().position(|c| *c == chat).map(|i| s.remove(i)).is_some()).unwrap_or(false);
                if skipped {
                    let _ = app.emit("chat-event", json!({"chat": chat, "type": "finished", "stopped": "you stopped it before it started"}));
                    continue;
                }
                let _ = app.emit("chat-event", json!({"chat": chat, "type": "started"}));
                let Some((server, model)) = engine.as_mut() else {
                    let _ = app.emit("chat-event", json!({"chat": chat, "type": "error", "text": "Load a model first."}));
                    continue;
                };
                set_status(&app, &status, |s| s.busy = true);
                let cancel = CancelToken::default();
                if let Ok(mut c) = CURRENT.lock() {
                    *c = Some((chat.clone(), cancel.clone()));
                }
                let result = run_turn(&app, base, server, model, &chat, &text, &attachments, reasoning.as_deref(), cancel);
                if let Ok(mut c) = CURRENT.lock() {
                    *c = None;
                }
                if let Err(e) = result {
                    let _ = app.emit("chat-event", json!({"chat": chat, "type": "error", "text": e}));
                }
                set_status(&app, &status, |s| s.busy = false);
            }
        }
    }
}

fn plain_load_error(e: &str) -> String {
    if e.contains("no engine build is installed") {
        "No engine is installed yet. Open Models › Engines to install one.".into()
    } else if e.contains("Not started") || e.contains("VRAM") {
        format!("Not enough memory to load this model. {e}")
    } else {
        format!("The model could not be loaded. {e}")
    }
}

/// A reasoning setting the UI sent, as the engine names it.
fn reasoning_static(r: Option<&str>) -> Option<&'static str> {
    match r? {
        "off" => Some("off"),
        "on" => Some("on"),
        other => lu_core::engines::EFFORTS.iter().copied().find(|e| *e == other),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_turn(app: &AppHandle, base: &'static Base, server: &mut LlamaServer, model: &ModelEntry, chat: &str, typed: &str, attachments: &[String], reasoning: Option<&str>, cancel: CancelToken) -> Result<(), String> {
    // Attached files stay where they are: the message names them (which also
    // lets Lu work in their folders for this message), and images are shown
    // to the model when it can see.
    let mut images = Vec::new();
    let mut text = typed.to_owned();
    let mut notes = Vec::new();
    for a in attachments {
        let p = PathBuf::from(a);
        if is_image(&p) {
            if model.projector.is_some() {
                match image_data_url(&p) {
                    Ok(u) => images.push(u),
                    Err(e) => notes.push(format!("{a}: not shown ({e})")),
                }
            } else {
                notes.push(format!("{a}: this model cannot see images, so it was not shown"));
            }
        }
    }
    if !attachments.is_empty() {
        text.push('\n');
        for a in attachments {
            text.push_str(&format!("\n[Attached] {a}"));
        }
    }
    base.library.note_message(chat, "user", &text).map_err(|e| e.to_string())?;
    for n in notes {
        let _ = app.emit("chat-event", json!({"chat": chat, "type": "note", "text": n}));
    }
    let text = if images.is_empty() { text } else { format!("{text}\n(The attached image{} shown to you with this message.)", if images.len() > 1 { "s are" } else { " is" }) };
    let text = text.as_str();
    let _ = app.emit("library-changed", ());
    let book = base.library.book(chat).map_err(|e| e.to_string())?;
    let rec = Recorder::new(&book, &base.journal, chat);
    let workspace = base.workspace();
    let workspace = std::fs::canonicalize(&workspace).map(|p| PathBuf::from(p.to_string_lossy().trim_start_matches(r"\\?\").to_owned())).unwrap_or(workspace);
    // Where Lu may work: the workspace, folders the user allowed before, and
    // folders the user names in this very message (their words are the OK).
    let mut roots = vec![workspace.clone()];
    for p in base.catalog.allowed_folders().into_iter().chain(folders_named(text)).chain(attachments.iter().filter_map(|a| Path::new(a).parent().map(Path::to_path_buf))) {
        let p = std::fs::canonicalize(&p).map(|c| PathBuf::from(c.to_string_lossy().trim_start_matches(r"\\?\").to_owned())).unwrap_or(p);
        if !roots.contains(&p) {
            roots.push(p);
        }
    }
    let files = FileAccess::new(&rec, &base.snapshots, &base.gate, WorkingScope::new(&roots).map_err(|e| format!("{e:?}"))?, SnapshotLimits::default());
    let runner = CommandRunner::new(&rec, &base.snapshots, &base.gate, SnapshotLimits::default(), &base.data.join("runner")).map_err(|e| format!("{e:?}"))?;
    let mut config = ChatConfig::new(&workspace);
    config.system_prompt = model.settings.system_prompt.clone().filter(|p| !p.trim().is_empty()).unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_owned());
    config.temperature = model.settings.temperature;
    config.top_p = model.settings.top_p;
    config.reasoning = reasoning_static(reasoning);
    config.images = images;
    // Pause and ask: the user has APPROVAL_SECONDS to answer, or the turn ends.
    let approve_chat = chat.to_owned();
    let approve_cancel = cancel.clone();
    let approve = move |what: &str, folder: &str| -> bool {
        let id = new_id('h');
        let (tx, rx) = mpsc::channel();
        if let Ok(mut a) = APPROVALS.lock() {
            a.insert(id.clone(), tx);
        }
        let _ = app.emit("chat-event", json!({"chat": approve_chat, "type": "approval", "id": id, "what": what, "folder": folder, "seconds": APPROVAL_SECONDS}));
        let deadline = Instant::now() + Duration::from_secs(APPROVAL_SECONDS);
        let answer = loop {
            if approve_cancel.is_cancelled() {
                break "stopped".to_owned();
            }
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(a) => break a,
                Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
                Err(_) => break "timeout".to_owned(),
            }
        };
        if let Ok(mut a) = APPROVALS.lock() {
            a.remove(&id);
        }
        let ok = answer == "once" || answer == "always";
        if answer == "always" {
            let _ = base.instruction(&format!("Allow Lu to work in {folder}"));
            let _ = base.catalog.allow_folder(Path::new(folder));
        } else if ok {
            let _ = base.instruction(&format!("Allow Lu to work in {folder} for this answer"));
        }
        let _ = app.emit("chat-event", json!({"chat": approve_chat, "type": "approval-done", "id": id, "answer": answer}));
        ok
    };
    let lp = ChatLoop { recorder: &rec, files: &files, runner: &runner, environment: Some(&base.environment), profile: None, config, cancel, approve: Some(&approve), web: None, links: Default::default() };
    let chat_id = chat.to_owned();
    if let Ok(mut l) = LIVE.lock() {
        l.insert(chat_id.clone(), String::new());
    }
    let out = lp
        .turn(server, text, &mut |ev| {
            let payload = match ev {
                ChatEvent::Text(t) => {
                    if let Ok(mut l) = LIVE.lock() {
                        l.entry(chat_id.clone()).or_default().push_str(&t);
                    }
                    json!({"chat": chat_id, "type": "text", "text": t})
                }
                ChatEvent::ActionStarted { what, .. } => json!({"chat": chat_id, "type": "action", "text": what}),
                ChatEvent::ActionFinished { ok, what, .. } => json!({"chat": chat_id, "type": "action-done", "ok": ok, "text": what}),
                ChatEvent::Held { what, folder, .. } => match folder {
                    Some(f) => json!({"chat": chat_id, "type": "held", "folder": f, "text": format!("{what} needs a folder Lu has not been allowed to work in: {f}")}),
                    None => json!({"chat": chat_id, "type": "held", "text": format!("{what} is waiting for your OK. Reply to approve it.")}),
                },
                ChatEvent::Done => json!({"chat": chat_id, "type": "done"}),
            };
            let _ = app.emit("chat-event", payload);
        })
        .map_err(|e| e.to_string());
    if let Ok(mut l) = LIVE.lock() {
        l.remove(chat);
    }
    let out = out?;
    base.library.note_message(chat, "assistant", &out.answer).map_err(|e| e.to_string())?;
    let _ = app.emit("chat-event", json!({"chat": chat, "type": "finished", "speed": out.tokens_per_second, "stopped": out.stopped}));
    let _ = app.emit("library-changed", ());
    Ok(())
}

// ---------------------------------------------------------------- commands: conversations

type R<T> = Result<T, String>;
fn s<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[tauri::command]
fn library(app: State<App>) -> R<Value> {
    let (folders, chats) = app.base.library.list().map_err(s)?;
    Ok(json!({
        "folders": folders.iter().map(|f| json!({"id": f.id, "name": f.name})).collect::<Vec<_>>(),
        "chats": chats.iter().map(|c| json!({"id": c.id, "title": c.title, "folder": c.folder, "updated": c.updated_at})).collect::<Vec<_>>(),
    }))
}
#[tauri::command]
fn new_chat(app: State<App>, folder: Option<i64>) -> R<String> {
    app.base.library.create(folder).map_err(s)
}
#[tauri::command]
fn rename_chat(app: State<App>, id: String, title: String) -> R<()> {
    app.base.library.rename(&id, &title).map_err(s)
}
#[tauri::command]
fn delete_chat(app: State<App>, id: String) -> R<()> {
    app.base.library.delete(&id).map(|_| ()).map_err(s)
}
#[tauri::command]
fn new_folder(app: State<App>, name: String) -> R<i64> {
    app.base.library.create_folder(&name).map_err(s)
}
#[tauri::command]
fn rename_folder(app: State<App>, id: i64, name: String) -> R<()> {
    app.base.library.rename_folder(id, &name).map_err(s)
}
#[tauri::command]
fn delete_folder(app: State<App>, id: i64, with_chats: bool) -> R<()> {
    app.base.library.delete_folder(id, with_chats).map_err(s)
}
#[tauri::command]
fn move_chat(app: State<App>, id: String, folder: Option<i64>) -> R<()> {
    app.base.library.move_to(&id, folder).map_err(s)
}
#[tauri::command]
fn search_chats(app: State<App>, query: String) -> R<Vec<Value>> {
    let hits: Vec<SearchHit> = app.base.library.search(&query, 50).map_err(s)?;
    Ok(hits.into_iter().map(|h| json!({"id": h.chat, "title": h.title, "snippet": h.snippet})).collect())
}
#[tauri::command]
fn chat_messages(app: State<App>, id: String) -> R<Vec<Value>> {
    let m = app.base.library.messages(&id).map_err(s)?;
    Ok(m.into_iter().map(|(role, text)| json!({"role": role, "text": text})).collect())
}
#[tauri::command]
fn trash(app: State<App>) -> R<Vec<Value>> {
    Ok(app.base.library.trashed().map_err(s)?.into_iter().map(|t| json!({"id": t.trash_id, "title": t.title, "deleted": t.deleted_at})).collect())
}
#[tauri::command]
fn restore_chat(app: State<App>, trash_id: String) -> R<String> {
    app.base.library.restore(&trash_id).map_err(s)
}
#[tauri::command]
fn erase_chat(app: State<App>, trash_id: String) -> R<()> {
    app.base.library.empty_trash_item(&trash_id).map_err(s)
}
#[tauri::command]
fn send_message(handle: AppHandle, app: State<App>, chat: String, text: String, attachments: Option<Vec<String>>, reasoning: Option<String>) -> R<()> {
    let attachments = attachments.unwrap_or_default();
    if text.trim().is_empty() && attachments.is_empty() {
        return Err("nothing to send".into());
    }
    let busy = app.status.lock().map_err(s)?.busy || !WAITING.lock().map_err(s)?.is_empty();
    WAITING.lock().map_err(s)?.push(chat.clone());
    if busy {
        let _ = handle.emit("chat-event", json!({"chat": chat, "type": "queued"}));
    }
    app.jobs.lock().map_err(s)?.send(Job::Send { chat, text, attachments, reasoning }).map_err(s)
}

/// Stop the answer being written in a conversation (or its waiting message).
#[tauri::command]
fn stop_chat(chat: String) -> R<()> {
    if let Some((c, token)) = CURRENT.lock().map_err(s)?.as_ref() {
        if *c == chat {
            token.cancel();
            return Ok(());
        }
    }
    if WAITING.lock().map_err(s)?.contains(&chat) {
        SKIP.lock().map_err(s)?.push(chat);
    }
    Ok(())
}

/// The user's answer to an approval: `once`, `always` or `deny`.
#[tauri::command]
fn answer_approval(id: String, choice: String) -> R<()> {
    if !matches!(choice.as_str(), "once" | "always" | "deny") {
        return Err("unknown answer".into());
    }
    let tx = APPROVALS.lock().map_err(s)?.get(&id).cloned().ok_or("This question is no longer waiting.")?;
    tx.send(choice).map_err(s)
}

/// What a conversation is doing now: its answer so far, or that it waits.
#[tauri::command]
fn chat_live(id: String) -> R<Value> {
    let text = LIVE.lock().map_err(s)?.get(&id).cloned();
    let waiting = WAITING.lock().map_err(s)?.contains(&id);
    Ok(json!({"writing": text.is_some(), "text": text, "waiting": waiting}))
}

// ---------------------------------------------------------------- commands: models

#[tauri::command]
fn model_status(app: State<App>) -> R<ModelStatus> {
    Ok(app.status.lock().map_err(s)?.clone())
}
#[tauri::command]
fn load_model(app: State<App>, id: i64, force: Option<bool>) -> R<()> {
    app.jobs.lock().map_err(s)?.send(Job::Load(id, force.unwrap_or(false))).map_err(s)
}
#[tauri::command]
fn unload_model(app: State<App>) -> R<()> {
    app.jobs.lock().map_err(s)?.send(Job::Unload).map_err(s)
}
#[tauri::command]
fn models(app: State<App>) -> R<Vec<ModelEntry>> {
    app.base.catalog.list()
}
#[tauri::command]
fn save_model_settings(app: State<App>, id: i64, settings: ModelSettings) -> R<()> {
    app.base.catalog.set_settings(id, &settings)
}
#[tauri::command]
fn rename_model(app: State<App>, id: i64, name: String) -> R<()> {
    app.base.catalog.rename(id, &name)
}
#[tauri::command]
fn remove_model(app: State<App>, id: i64) -> R<()> {
    app.base.catalog.remove(id)
}

/// Delete a downloaded model's files from disk (and take it off the list).
/// Imported models live in the user's own folders; Lu never deletes those.
#[tauri::command]
fn delete_model(handle: AppHandle, app: State<App>, id: i64) -> R<u64> {
    let m = app.base.catalog.get(id)?;
    let models_dir = app.base.data.join("models");
    let path = PathBuf::from(&m.path);
    if m.source != "downloaded" || !path.starts_with(&models_dir) {
        return Err("This model was imported from your own folder, so Lu will not delete its file. Use Remove to take it off the list.".into());
    }
    if app.status.lock().map_err(s)?.model_id == Some(id) {
        return Err("Unload this model first.".into());
    }
    app.base.instruction(&format!("Delete the model files of {}", m.name))?;
    let mut freed = 0;
    let mut gone = vec![path.clone()];
    if let Some(p) = m.projector.as_ref().map(PathBuf::from).filter(|p| p.starts_with(&models_dir)) {
        gone.push(p);
    }
    // Split models: every shard next to the first one.
    if let (Some(dir), Some(stem)) = (path.parent(), path.file_name().and_then(|n| n.to_str()).and_then(|n| n.split("-00001-of-").next())) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n != stem && n.starts_with(stem) && n.contains("-of-") && n.ends_with(".gguf") && !gone.contains(&e.path()) {
                    gone.push(e.path());
                }
            }
        }
    }
    for p in &gone {
        if let Ok(md) = std::fs::metadata(p) {
            std::fs::remove_file(p).map_err(|e| format!("Could not delete {}: {e}", p.display()))?;
            freed += md.len();
        }
    }
    app.base.catalog.remove(id)?;
    let _ = handle.emit("models-changed", ());
    Ok(freed)
}

/// Add a GGUF file the user already has (it stays where it is).
#[tauri::command]
fn import_model(app: State<App>, path: String) -> R<i64> {
    let p = PathBuf::from(path.trim().trim_matches('"'));
    let info = gguf::inspect(&p).map_err(|e| format!("This is not a model file Lu can use: {e}"))?;
    if info.is_projector() {
        return Err("This is a vision projector, not a model. Import the model, then pair the projector in its settings.".into());
    }
    if info.architecture.as_deref().is_some_and(|a| a.ends_with("mtp") || a.ends_with("_mtp")) || p.file_name().is_some_and(|n| n.to_string_lossy().to_ascii_lowercase().starts_with("mtp")) {
        return Err("This file is a draft head for faster generation (MTP). It only works next to its main model and cannot be loaded on its own.".into());
    }
    let name = info.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| p.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    app.base.catalog.add(&p, &name, info.architecture.as_deref(), info.context_length, info.file_bytes, "imported", None, sibling_projector(&p).as_deref())
}

/// Model files found in LM Studio's folder (or another folder), to import.
#[tauri::command]
async fn scan_models(folder: Option<String>) -> R<Vec<Value>> {
    let dir = folder.map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).unwrap_or_default()).join(".lmstudio").join("models"));
    Ok(find_gguf(&dir, 500)
        .into_iter()
        .map(|p| json!({"path": p.to_string_lossy(), "name": p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), "bytes": std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0)}))
        .collect())
}

// ---------------------------------------------------------------- commands: Hugging Face

#[tauri::command]
async fn hf_search(app: State<'_, App>, query: String) -> R<Vec<Value>> {
    let leads = app.base.hub.search(&query, 30).map_err(s)?;
    Ok(leads.into_iter().map(|l| json!({"repo": l.repo_id, "license": l.license, "downloads": l.downloads, "likes": l.likes, "unavailable": l.unavailable})).collect())
}
#[tauri::command]
async fn hf_files(app: State<'_, App>, repo: String) -> R<Value> {
    let r = app.base.hub.review(&repo, None).map_err(s)?;
    Ok(json!({
        "repo": r.repo_id, "revision": r.revision, "license": r.license, "unavailable": r.unavailable,
        "choices": r.choices.iter().map(|c| json!({"label": c.label, "quant": c.quant, "bytes": c.total_bytes, "files": c.files.iter().map(|f| json!({"path": f.path, "bytes": f.bytes, "sha256": f.sha256})).collect::<Vec<_>>()})).collect::<Vec<_>>(),
        "projectors": r.projectors.iter().map(|f| json!({"path": f.path, "bytes": f.bytes, "sha256": f.sha256})).collect::<Vec<_>>(),
    }))
}
#[derive(serde::Deserialize)]
struct FileArg {
    path: String,
    bytes: u64,
    sha256: String,
}
#[tauri::command]
fn hf_download(handle: AppHandle, app: State<App>, repo: String, revision: String, files: Vec<FileArg>, projector: Option<FileArg>) -> R<()> {
    let base = app.base;
    let lead = |f: FileArg| FileLead { path: f.path, bytes: f.bytes, sha256: f.sha256 };
    let req = InstallRequest { repo_id: repo.clone(), revision, files: files.into_iter().map(lead).collect(), projector: projector.map(lead) };
    let names: Vec<String> = req.files.iter().chain(req.projector.as_ref()).map(|f| f.path.clone()).collect();
    // The click is the user's instruction: name exactly what is installed.
    let summary = format!("install model {}@{}: {}", req.repo_id, &req.revision[..req.revision.len().min(12)], names.join(", "));
    let turn = base.instruction(&format!("Download {summary}"))?;
    let total: u64 = req.files.iter().chain(req.projector.as_ref()).map(|f| f.bytes).sum();
    let key = format!("download:{}/{}", repo, names.first().cloned().unwrap_or_default());
    let history = base.catalog.download_started(&repo, &names.join(", "), total)?;
    let first_file = names.first().cloned().unwrap_or_default();
    transfer(&key, Some(json!({"kind": "download", "repo": repo, "file": first_file, "done": 0, "total": total})));
    let _ = handle.emit("download-event", json!({"repo": repo, "file": first_file, "done": 0, "total": total, "started": true}));
    thread::spawn(move || {
        let rec = Recorder::new(&base.control, &base.journal, CONTROL);
        let emit = |v: Value| {
            let _ = handle.emit("download-event", v);
        };
        let result = ModelInstaller::new(&base.data.join("models"), &base.hub, &rec, &base.gate, &base.governor).and_then(|inst| {
            let mut last = 0u64;
            inst.install(&new_id('c'), &turn, &req, &AtomicBool::new(false), &mut |file, done, total| {
                let pct = done * 1000 / total.max(1);
                if pct != last {
                    last = pct;
                    let v = json!({"kind": "download", "repo": repo, "file": file, "first": first_file, "done": done, "total": total});
                    transfer(&key, Some(v.clone()));
                    emit(v);
                }
            })
        });
        match result {
            Ok(installed) => {
                let first = &installed.model[0];
                let name = first.info.name.clone().unwrap_or_else(|| first.repo_file.clone());
                let _ = base.catalog.add(&first.path, &name, first.info.architecture.as_deref(), first.info.context_length, installed.model.iter().map(|f| f.bytes).sum(), "downloaded", Some(&repo), installed.projector.as_ref().map(|p| p.path.as_path()));
                transfer(&key, None);
                let _ = base.catalog.download_ended(history, true, None);
                emit(json!({"repo": repo, "file": first_file, "finished": true}));
            }
            Err(e) => {
                transfer(&key, None);
                let _ = base.catalog.download_ended(history, false, Some(&e.to_string()));
                emit(json!({"repo": repo, "file": first_file, "error": e.to_string()}));
            }
        }
    });
    Ok(())
}

// ---------------------------------------------------------------- commands: engines

#[tauri::command]
async fn engines(app: State<'_, App>) -> R<Value> {
    let base = app.base;
    let rec = Recorder::new(&base.control, &base.journal, CONTROL);
    let store = EngineStore::open(&base.data.join("engines"), shipped_pins(), &base.source, &rec, &base.gate, &base.governor).map_err(s)?;
    let installed = store.installed().map_err(s)?;
    let packs = store.installed_packs().map_err(s)?;
    let gpus = measure(None).gpus;
    let names = |v: Variant| match v {
        Variant::Rocm => "ROCm",
        Variant::Vulkan => "Vulkan",
        Variant::Cpu => "CPU",
    };
    let rows: Vec<Value> = [Variant::Rocm, Variant::Vulkan, Variant::Cpu]
        .iter()
        .map(|&v| {
            let active = installed.iter().find(|b| b.variant == v && b.active);
            let pin = store.pins().iter().find(|p| p.variant == v);
            let versions: Vec<String> = installed.iter().filter(|b| b.variant == v).map(|b| b.version.clone()).collect();
            json!({
                "id": v.as_str(), "name": names(v),
                "installed": active.map(|b| b.version.clone()),
                "versions": versions,
                "available": pin.map(|p| p.version.clone()),
                "size": pin.map(|p| p.bytes),
                "needs_pack": v == Variant::Rocm && !packs.iter().any(|(_, pv, _)| *pv == Variant::Rocm),
            })
        })
        .collect();
    Ok(json!({"engines": rows, "gpus": gpus.iter().map(|g| json!({"name": g.name, "total": g.total_mib, "used": g.used_mib})).collect::<Vec<_>>()}))
}

#[tauri::command]
fn install_engine(handle: AppHandle, app: State<App>, variant: String) -> R<()> {
    let base = app.base;
    let v = Variant::parse(&variant).ok_or("unknown engine")?;
    let turn = base.instruction(&format!("Install the {} engine", v.as_str()))?;
    thread::spawn(move || {
        let key = format!("engine:{}", v.as_str());
        let emit = |x: Value| {
            if x.get("finished").is_some() || x.get("error").is_some() {
                transfer(&key, None);
            } else {
                let mut t = x.clone();
                t["kind"] = json!("engine");
                t["id"] = json!(v.as_str());
                transfer(&key, Some(t));
            }
            let _ = handle.emit("engine-event", x);
        };
        emit(json!({"id": v.as_str(), "stage": "Starting", "done": 0, "total": 1}));
        let rec = Recorder::new(&base.control, &base.journal, CONTROL);
        let result = (|| -> Result<(), String> {
            let store = EngineStore::open(&base.data.join("engines"), shipped_pins(), &base.source, &rec, &base.gate, &base.governor).map_err(s)?;
            if v == Variant::Rocm && store.runtime_dirs(Variant::Rocm).map_err(s)?.is_empty() {
                let gpus = measure(None).gpus;
                let pack = shipped_packs().into_iter().find(|p| gpus.iter().any(|g| p.serves(&g.name))).ok_or("This GPU has no ROCm support pack in Lu yet.")?;
                let mut last = 101;
                store
                    .install_pack(&new_id('c'), &turn, &pack, None, &AtomicBool::new(false), &mut |d, t| {
                        let pct = d * 100 / t.max(1);
                        if pct != last {
                            last = pct;
                            emit(json!({"id": "rocm", "stage": "GPU support files", "done": d, "total": t}));
                        }
                    })
                    .map_err(s)?;
            }
            let pin = store.pins().iter().find(|p| p.variant == v).cloned().ok_or("no build for this computer")?;
            let mut last = 101;
            store
                .install(&new_id('c'), &turn, &pin, true, &AtomicBool::new(false), &mut |d, t| {
                    let pct = d * 100 / t.max(1);
                    if pct != last {
                        last = pct;
                        emit(json!({"id": v.as_str(), "stage": "Engine", "done": d, "total": t}));
                    }
                })
                .map_err(s)?;
            Ok(())
        })();
        match result {
            Ok(()) => emit(json!({"id": v.as_str(), "finished": true})),
            Err(e) => emit(json!({"id": v.as_str(), "error": e})),
        }
    });
    Ok(())
}

#[tauri::command]
fn rollback_engine(app: State<App>, variant: String) -> R<String> {
    let base = app.base;
    let v = Variant::parse(&variant).ok_or("unknown engine")?;
    let rec = Recorder::new(&base.control, &base.journal, CONTROL);
    let store = EngineStore::open(&base.data.join("engines"), shipped_pins(), &base.source, &rec, &base.gate, &base.governor).map_err(s)?;
    store.rollback(v).map(|b| b.version).map_err(s)
}

// ---------------------------------------------------------------- commands: app

#[tauri::command]
fn app_settings(app: State<App>) -> R<Value> {
    Ok(json!({"workspace": app.base.workspace().to_string_lossy(), "data": app.base.data.to_string_lossy(), "allowed": app.base.catalog.allowed_folders()}))
}
// ---------------------------------------------------------------- commands: DX

fn finding_json(f: &Finding) -> Value {
    json!({"check": f.check, "severity": f.severity.as_str(), "summary": f.summary, "cause": f.cause, "fix": f.fix, "evidence": f.evidence, "private": f.private_detail})
}
/// Read-only checks of Lu's own records (costs nothing, changes nothing).
#[tauri::command]
async fn dx_check(app: State<'_, App>) -> R<Value> {
    let base = app.base;
    let rec = Recorder::new(&base.control, &base.journal, CONTROL);
    let store = EngineStore::open(&base.data.join("engines"), shipped_pins(), &base.source, &rec, &base.gate, &base.governor).map_err(s)?;
    let findings = dx::passive_checks(&Sources { engines: Some(&store), governor: Some(&base.governor), journal: Some(&base.journal), gate: Some((&base.gate, CONTROL)), environment: Some(&base.environment) });
    let run = base.dx.record("passive", &findings)?;
    Ok(json!({"run": run, "findings": findings.iter().map(finding_json).collect::<Vec<_>>()}))
}
/// Lu's own tools on a scratch folder (the user's files are not touched).
#[tauri::command]
async fn dx_self_test(app: State<'_, App>) -> R<Value> {
    let base = app.base;
    let scratch = base.data.join("dx-scratch");
    std::fs::create_dir_all(&scratch).map_err(s)?;
    let findings = dx::tool_self_test(&scratch);
    let run = base.dx.record("active", &findings)?;
    Ok(json!({"run": run, "findings": findings.iter().map(finding_json).collect::<Vec<_>>()}))
}
/// Start a benchmark on the loaded model; the result arrives as `dx-event`.
#[tauri::command]
fn dx_benchmark(app: State<App>, kind: String) -> R<()> {
    if !matches!(kind.as_str(), "speed" | "memory") {
        return Err("unknown benchmark".into());
    }
    app.base.instruction(&format!("Run the {kind} benchmark"))?;
    app.jobs.lock().map_err(s)?.send(Job::Bench(kind)).map_err(s)
}
/// Write a sanitized report of a run into <data>/dx and show the folder.
#[tauri::command]
fn dx_export(app: State<App>, run: i64) -> R<String> {
    let (txt, _json) = app.base.dx.export(run)?;
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer.exe").arg("/select,").arg(&txt).spawn();
    Ok(txt.to_string_lossy().into_owned())
}

/// Open a web address in the user's own browser.
#[tauri::command]
fn open_link(url: String) -> R<()> {
    let u = url.trim();
    if !(u.starts_with("https://") || u.starts_with("http://")) || u.chars().any(|c| c.is_control() || c == '"') {
        return Err("Lu only opens web addresses (http or https).".into());
    }
    #[cfg(windows)]
    let r = std::process::Command::new("rundll32.exe").arg("url.dll,FileProtocolHandler").arg(u).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(u).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(u).spawn();
    r.map(|_| ()).map_err(|e| format!("Could not open the browser: {e}"))
}

#[tauri::command]
fn transfers() -> R<Vec<Value>> {
    Ok(TRANSFERS.lock().map_err(s)?.values().cloned().collect())
}
#[tauri::command]
fn download_history(app: State<App>) -> R<Vec<catalog::DownloadRow>> {
    app.base.catalog.downloads()
}
#[tauri::command]
fn clear_download_history(app: State<App>) -> R<()> {
    app.base.catalog.clear_download_history()
}
/// The user allows Lu to work in a folder from now on.
#[tauri::command]
fn allow_folder(app: State<App>, path: String) -> R<()> {
    let p = PathBuf::from(path.trim().trim_matches('"'));
    if !p.is_dir() {
        return Err("That folder does not exist.".into());
    }
    app.base.instruction(&format!("Allow Lu to work in {}", p.display()))?;
    app.base.catalog.allow_folder(&p)
}
#[tauri::command]
fn forget_folder(app: State<App>, path: String) -> R<()> {
    app.base.catalog.forget_folder(&path)
}

#[tauri::command]
fn set_workspace(app: State<App>, path: String) -> R<()> {
    let p = PathBuf::from(path.trim().trim_matches('"'));
    std::fs::create_dir_all(&p).map_err(|e| format!("That folder cannot be used: {e}"))?;
    app.base.catalog.set_setting("workspace", &p.to_string_lossy())
}

#[tauri::command]
/// Async on purpose: creating a window from a synchronous command
/// deadlocks on Windows (WebView2 needs the main thread the command holds).
async fn open_window(handle: AppHandle, kind: String, tab: Option<String>) -> R<()> {
    let (label, title, file, w, h) = match kind.as_str() {
        "models" => ("models", "Lu · Models", "models.html", 980.0, 680.0),
        "manager" => ("manager", "Lu · Conversations", "manager.html", 900.0, 640.0),
        "diagnostics" => ("diagnostics", "Lu · Diagnostics", "diagnostics.html", 900.0, 680.0),
        _ => return Err("unknown window".into()),
    };
    if let Some(win) = handle.get_webview_window(label) {
        let _ = win.set_focus();
        if let Some(t) = tab {
            let _ = win.emit("open-tab", t);
        }
        return Ok(());
    }
    let url = match tab {
        Some(t) => format!("{file}#{t}"),
        None => file.to_owned(),
    };
    WebviewWindowBuilder::new(&handle, label, WebviewUrl::App(url.into())).title(title).inner_size(w, h).min_inner_size(640.0, 420.0).build().map_err(s)?;
    Ok(())
}

fn main() {
    let data = data_dir();
    let base: &'static Base = match Base::open(&data) {
        Ok(b) => Box::leak(Box::new(b)),
        Err(e) => {
            eprintln!("Lu could not open its data folder {}: {e}", data.display());
            std::process::exit(1);
        }
    };
    let (tx, rx) = mpsc::channel();
    let status = Arc::new(Mutex::new(ModelStatus { state: "unloaded".into(), ..Default::default() }));
    let thread_status = status.clone();
    tauri::Builder::default()
        .manage(App { base, jobs: Mutex::new(tx), status })
        .setup(move |app| {
            let handle = app.handle().clone();
            thread::spawn(move || model_thread(handle, base, thread_status, rx));
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into())).title("Lu").inner_size(1180.0, 760.0).min_inner_size(720.0, 480.0).build()?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            library, new_chat, rename_chat, delete_chat, new_folder, rename_folder, delete_folder, move_chat, search_chats, chat_messages, trash, restore_chat, erase_chat, send_message, chat_live, stop_chat, answer_approval,
            model_status, load_model, unload_model, models, save_model_settings, rename_model, remove_model, delete_model, import_model, scan_models,
            hf_search, hf_files, hf_download, engines, install_engine, rollback_engine, app_settings, set_workspace, open_window, open_link, dx_check, dx_self_test, dx_benchmark, dx_export, transfers, download_history, clear_download_history, allow_folder, forget_folder
        ])
        .run(tauri::generate_context!())
        .expect("Lu failed to start");
}
