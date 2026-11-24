//! Inference backends: llama.cpp builds side by side (CPU, Vulkan, ROCm),
//! an updater with rollback, backend choice per model, and the running
//! `llama-server` that piece 11's model owner loads and unloads.
//!
//! - **Pinned builds only.** The builds Lu may install are listed in
//!   `pins/llama-engines.json` with the SHA-256 GitHub publishes for each
//!   asset. An install downloads into staging, resumes after a failure,
//!   checks size and hash, extracts (archive.rs, with its path checks),
//!   runs `llama-server --version` as a smoke test, and only then registers
//!   the build. A failed install never touches a working engine.
//! - **Switch and roll back.** Several versions of a variant stay
//!   installed; one is active. Updating activates the new one and keeps the
//!   old, so rollback is one call. A model can be pinned to a version.
//! - **Choosing a backend.** Order for a model: its pin, else the backend
//!   that last worked for it, else ROCm, Vulkan, then CPU among the
//!   installed builds (no hardware gate on ROCm, as decided). A backend
//!   that fails is recorded and the next one is tried; the reason is kept
//!   in plain words.
//! - **Per-model record.** Each load and completion records the backend,
//!   version, load time and tokens per second. That feeds the model profile.
//! - **The engine.** [`LlamaServer`] runs `llama-server` bound to 127.0.0.1
//!   with a random API key, in an owned process tree (nothing outlives an
//!   unload), and waits for `/health`. It implements [`resources::Engine`],
//!   so only the model owner loads it.
//!
//! Reviewed, not ported: Lu-AI-GUI persistent_llama_backend.rs (its
//! loopback + API-key server and argument list are kept in spirit; its
//! single hard-coded Vulkan package and JSON manifest are replaced by the
//! pinned list and SQLite registry), compute_runtime.rs.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::archive::{self, ArchiveLimits};
use crate::origin::{Decision, EffectKind, OriginGate, Proposal, Source, SourceKind};
use crate::process_tree::ProcessTree;
use crate::recorder::Recorder;
use crate::resources::{self, Admission, Governor, Need};

pub const PINS_JSON: &str = include_str!("../pins/llama-engines.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Variant {
    Rocm,
    Vulkan,
    Cpu,
}
impl Variant {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rocm => "rocm",
            Self::Vulkan => "vulkan",
            Self::Cpu => "cpu",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [Self::Rocm, Self::Vulkan, Self::Cpu].into_iter().find(|v| v.as_str() == s)
    }
    pub fn uses_gpu(self) -> bool {
        self != Self::Cpu
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Pin {
    pub version: String,
    pub variant: Variant,
    pub platform: String,
    pub asset: String,
    pub bytes: u64,
    pub sha256: String,
}
impl Pin {
    pub fn url(&self) -> String {
        format!("https://github.com/ggml-org/llama.cpp/releases/download/{}/{}", self.version, self.asset)
    }
}

#[derive(Deserialize)]
struct PinFile {
    schema: String,
    builds: Vec<Pin>,
}

pub const PACKS_JSON: &str = include_str!("../pins/rocm-runtime.json");

/// One file a runtime pack takes from its source.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PackFile {
    /// Path inside the source archive, e.g. `bin/rocblas.dll`.
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PackSource {
    pub url: String,
    pub bytes: u64,
    pub sha256: String,
}

/// Libraries an engine build needs but does not include (for ROCm builds:
/// hipBLAS, rocBLAS and friends with their GPU kernels), taken file by file
/// from a pinned upstream archive so Lu runs out of the box.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RuntimePack {
    pub name: String,
    /// The engine variant it serves.
    #[serde(rename = "for")]
    pub for_variant: Variant,
    pub platform: String,
    /// GPU architectures its kernels cover.
    pub gfx: Vec<String>,
    /// GPU names (as the driver reports them) this pack is for.
    pub gpu_names: Vec<String>,
    pub source: PackSource,
    pub files: Vec<PackFile>,
}
impl RuntimePack {
    /// Total size of the files it installs.
    pub fn installed_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.bytes).sum()
    }
    /// Whether this pack serves a GPU with this name.
    pub fn serves(&self, gpu_name: &str) -> bool {
        let g = gpu_name.to_ascii_lowercase();
        self.gpu_names.iter().any(|n| g.contains(&n.to_ascii_lowercase()))
    }
}

#[derive(Deserialize)]
struct PackPinFile {
    schema: String,
    packs: Vec<RuntimePack>,
}

/// Parse a runtime-pack pin list (JSON at the edge); keep valid entries.
pub fn parse_packs(json: &str) -> Result<Vec<RuntimePack>, EngineError> {
    let file: PackPinFile = serde_json::from_str(json).map_err(|e| EngineError::Invalid(format!("pack list: {e}")))?;
    if file.schema != "lu-runtime-packs-v1" {
        return Err(EngineError::Invalid(format!("pack list schema {}", file.schema)));
    }
    let hex64 = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
    Ok(file
        .packs
        .into_iter()
        .filter(|p| {
            hex64(&p.source.sha256)
                && !p.files.is_empty()
                && p.name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
                && p.files.iter().all(|f| hex64(&f.sha256) && crate::archive::safe_relative(&f.path).is_ok())
        })
        .collect())
}

/// The runtime packs shipped with Lu for this platform.
pub fn shipped_packs() -> Vec<RuntimePack> {
    parse_packs(PACKS_JSON).unwrap_or_default().into_iter().filter(|p| p.platform == platform()).collect()
}

/// This machine's platform name in the pin list.
pub fn platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "windows-x64",
        ("linux", "x86_64") => "linux-x64",
        ("windows", "aarch64") => "windows-arm64",
        ("linux", "aarch64") => "linux-arm64",
        _ => "unsupported",
    }
}

/// Parse a pin list (JSON at the edge) and keep the valid entries.
pub fn parse_pins(json: &str) -> Result<Vec<Pin>, EngineError> {
    let file: PinFile = serde_json::from_str(json).map_err(|e| EngineError::Invalid(format!("pin list: {e}")))?;
    if file.schema != "lu-engine-pins-v1" {
        return Err(EngineError::Invalid(format!("pin list schema {}", file.schema)));
    }
    Ok(file
        .builds
        .into_iter()
        .filter(|p| {
            p.sha256.len() == 64
                && p.sha256.bytes().all(|b| b.is_ascii_hexdigit())
                && p.bytes > 0
                && !p.asset.contains(['/', '\\'])
                && p.version.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        })
        .collect())
}

/// The pins shipped with Lu for this platform.
pub fn shipped_pins() -> Vec<Pin> {
    parse_pins(PINS_JSON).unwrap_or_default().into_iter().filter(|p| p.platform == platform()).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    Invalid(String),
    Network(String),
    HashMismatch { expected: String, actual: String },
    /// The build downloaded but does not run here; nothing was changed.
    SmokeFailed(String),
    /// The engine could not load the model on this backend, in plain words.
    LoadFailed { variant: Variant, why: String },
    /// The engine process died.
    Crashed(String),
    NotInstalled(String),
    NoBackend(Vec<String>),
    Held { held_id: i64, notify_user: bool },
    NoRoom(Vec<String>),
    Cancelled,
    Io(String),
    Database(String),
}
impl From<std::io::Error> for EngineError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}
impl From<rusqlite::Error> for EngineError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Database(e.to_string())
    }
}
impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LoadFailed { variant, why } => write!(f, "the {} engine could not load the model: {why}", variant.as_str()),
            Self::Crashed(w) => write!(f, "the engine stopped unexpectedly: {w}"),
            other => write!(f, "{other:?}"),
        }
    }
}
impl std::error::Error for EngineError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineBuild {
    pub variant: Variant,
    pub version: String,
    pub dir: PathBuf,
    pub server: PathBuf,
    pub active: bool,
    /// What the install check found: version line and the devices the
    /// build reported (`...; devices: ROCm0: ...`).
    pub report: String,
}

/// A result kept for a model on a backend.
#[derive(Debug, Clone, PartialEq)]
pub struct BackendRecord {
    pub variant: Variant,
    pub version: String,
    pub ok: bool,
    pub load_ms: Option<u64>,
    pub tokens_per_second: Option<f64>,
    pub note: Option<String>,
}

// ---------------------------------------------------------------- downloads

/// AMD's ROCm download server (runtime pack sources).
fn amd_host(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("").to_ascii_lowercase();
    host == "stable.repo.amd.com" || host == "repo.radeon.com"
}

fn github_host(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("").to_ascii_lowercase();
    !host.contains(['@', ':']) && (host == "github.com" || host.ends_with(".githubusercontent.com"))
}

/// Where builds come from: GitHub, or a loopback stand-in in tests.
pub struct EngineSource {
    agent: ureq::Agent,
    /// Replaces `https://github.com/ggml-org/llama.cpp/releases/download` in tests.
    local_base: Option<String>,
}
impl EngineSource {
    pub fn github() -> Self {
        Self { agent: agent(false), local_base: None }
    }
    pub fn local_test(base: &str) -> Result<Self, EngineError> {
        let host = base.strip_prefix("http://").and_then(|r| r.split(['/', ':']).next()).unwrap_or("");
        if !matches!(host, "127.0.0.1" | "localhost") {
            return Err(EngineError::Invalid("a test source must be on loopback".into()));
        }
        Ok(Self { agent: agent(true), local_base: Some(base.trim_end_matches('/').to_owned()) })
    }
    fn url(&self, pin: &Pin) -> String {
        match &self.local_base {
            Some(b) => format!("{b}/{}/{}", pin.version, pin.asset),
            None => pin.url(),
        }
    }
    fn allowed(&self, url: &str) -> bool {
        self.local_base.as_ref().is_some_and(|b| url.starts_with(b.as_str())) || github_host(url) || amd_host(url)
    }
    /// Where a runtime pack's source tarball is fetched from.
    fn pack_url(&self, pack: &RuntimePack) -> String {
        match &self.local_base {
            Some(b) => format!("{b}/packs/{}", pack.source.url.rsplit('/').next().unwrap_or("pack.tar.gz")),
            None => pack.source.url.clone(),
        }
    }
    /// Download `pin` to `part`, resuming, checking size and SHA-256.
    fn fetch(&self, pin: &Pin, part: &Path, cancel: &AtomicBool, progress: &mut dyn FnMut(u64, u64)) -> Result<(), EngineError> {
        self.fetch_file(&self.url(pin), &pin.asset, pin.bytes, &pin.sha256, part, cancel, progress)
    }

    /// Download a pinned file to `part`, resuming, checking size and SHA-256.
    #[allow(clippy::too_many_arguments)]
    fn fetch_file(&self, first_url: &str, label: &str, bytes: u64, sha256: &str, part: &Path, cancel: &AtomicBool, progress: &mut dyn FnMut(u64, u64)) -> Result<(), EngineError> {
        let have = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
        if have > bytes {
            fs::remove_file(part)?;
        }
        let have = have.min(bytes);
        let mut hasher = Sha256::new();
        let mut done = 0u64;
        let mut out = if have == bytes {
            None
        } else {
            let mut url = first_url.to_owned();
            let mut resp = None;
            for _ in 0..6 {
                if !self.allowed(&url) {
                    return Err(EngineError::Invalid(format!("refused to download from {url}: not GitHub or AMD's ROCm server")));
                }
                let mut req = self.agent.get(&url);
                if have > 0 {
                    req = req.header("Range", &format!("bytes={have}-"));
                }
                let r = req.call().map_err(|e| EngineError::Network(e.to_string()))?;
                if matches!(r.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                    url = r.headers().get("location").and_then(|h| h.to_str().ok()).unwrap_or("").to_owned();
                    if url.starts_with('/') {
                        url = format!("{}{url}", self.local_base.clone().unwrap_or_else(|| "https://github.com".into()));
                    }
                    continue;
                }
                resp = Some(r);
                break;
            }
            let resp = resp.ok_or_else(|| EngineError::Network("too many redirects".into()))?;
            let file = match resp.status().as_u16() {
                206 if have > 0 => {
                    let mut existing = File::open(part)?;
                    let mut buf = vec![0u8; 1 << 20];
                    loop {
                        let k = existing.read(&mut buf)?;
                        if k == 0 {
                            break;
                        }
                        hasher.update(&buf[..k]);
                    }
                    done = have;
                    OpenOptions::new().append(true).open(part)?
                }
                200 => File::create(part)?,
                s => return Err(EngineError::Network(format!("GitHub answered {s} for {}", label))),
            };
            Some((file, resp.into_body().into_reader()))
        };
        if let Some((file, body)) = out.as_mut() {
            let mut buf = vec![0u8; 1 << 20];
            loop {
                if cancel.load(Ordering::Acquire) {
                    file.sync_all()?;
                    return Err(EngineError::Cancelled);
                }
                let k = body.read(&mut buf).map_err(|e| EngineError::Network(format!("{e} (the staged part is kept; it will resume)")))?;
                if k == 0 {
                    break;
                }
                done += k as u64;
                if done > bytes {
                    drop(out);
                    fs::remove_file(part)?;
                    return Err(EngineError::Invalid(format!("{} is larger than pinned", label)));
                }
                file.write_all(&buf[..k])?;
                hasher.update(&buf[..k]);
                progress(done, bytes);
            }
            file.sync_all()?;
        } else {
            let mut f = File::open(part)?;
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let k = f.read(&mut buf)?;
                if k == 0 {
                    break;
                }
                done += k as u64;
                hasher.update(&buf[..k]);
            }
        }
        drop(out);
        if done != bytes {
            return Err(EngineError::Network(format!("{} stopped at {done} of {} bytes (the staged part is kept; it will resume)", label, bytes)));
        }
        let actual = format!("{:x}", hasher.finalize());
        if actual != sha256 {
            fs::remove_file(part)?;
            return Err(EngineError::HashMismatch { expected: sha256.to_owned(), actual });
        }
        Ok(())
    }
}

fn agent(local: bool) -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .https_only(!local)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .user_agent("Lu/0.1")
        .build()
        .into()
}

fn server_name() -> &'static str {
    if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    }
}
/// Find `llama-server` in an extracted build (release archives put it at
/// the top or under build/bin).
fn find_server(dir: &Path, depth: usize) -> Option<PathBuf> {
    let direct = dir.join(server_name());
    if direct.is_file() {
        return Some(direct);
    }
    if depth == 0 {
        return None;
    }
    let mut subdirs: Vec<PathBuf> = fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    subdirs.sort();
    subdirs.into_iter().find_map(|d| find_server(&d, depth - 1))
}

/// Run the server with `args` in an owned process tree (60 s limit) and
/// return whether it succeeded and everything it printed.
fn run_capture(server: &Path, args: &[&str], lib_dirs: &[PathBuf]) -> Result<(bool, String), String> {
    let mut cmd = Command::new(server);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    add_lib_dirs(&mut cmd, lib_dirs);
    if let Some(dir) = server.parent() {
        cmd.current_dir(dir);
    }
    let (mut child, tree) = ProcessTree::spawn(cmd).map_err(|e| format!("it does not start: {e}"))?;
    // Read both pipes on threads so a chatty process cannot block on a full pipe.
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let t_out = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = out.as_mut() {
            let _ = o.read_to_string(&mut s);
        }
        s
    });
    let t_err = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(e) = err.as_mut() {
            let _ = e.read_to_string(&mut s);
        }
        s
    });
    let until = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break s;
        }
        if Instant::now() > until {
            tree.kill();
            return Err(format!("it did not finish {} within 60 s", args.join(" ")));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let text = format!("{}{}", t_out.join().unwrap_or_default(), t_err.join().unwrap_or_default());
    Ok((status.success(), text))
}

/// Put runtime-pack folders first on a process's library search path
/// (`PATH` on Windows, `LD_LIBRARY_PATH` elsewhere).
fn add_lib_dirs(cmd: &mut Command, lib_dirs: &[PathBuf]) {
    if lib_dirs.is_empty() {
        return;
    }
    let var = if cfg!(windows) { "PATH" } else { "LD_LIBRARY_PATH" };
    let mut dirs: Vec<PathBuf> = lib_dirs.to_vec();
    if let Some(old) = std::env::var_os(var) {
        dirs.extend(std::env::split_paths(&old));
    }
    if let Ok(joined) = std::env::join_paths(dirs) {
        cmd.env(var, joined);
    }
}

/// Run `server --version`; its version line.
fn smoke(server: &Path) -> Result<String, String> {
    let (ok, text) = run_capture(server, &["--version"], &[])?;
    let line = text.lines().find(|l| l.to_ascii_lowercase().contains("version")).map(str::to_owned);
    match (ok, line) {
        (true, Some(l)) => Ok(l.trim().to_owned()),
        _ => Err(format!("--version failed: {}", text.lines().last().unwrap_or("no output").trim())),
    }
}

/// The devices a build can use, from `llama-server --list-devices`
/// (lines like `ROCm0: AMD Radeon AI PRO R9700 (32624 MiB, 32000 MiB free)`).
/// Returns the device lines and the full output, for reporting.
pub fn list_devices(server: &Path, lib_dirs: &[PathBuf]) -> Result<(Vec<String>, String), String> {
    let (_, text) = run_capture(server, &["--list-devices"], lib_dirs)?;
    let mut devices = Vec::new();
    let mut in_list = false;
    for line in text.lines() {
        if line.trim_start().starts_with("Available devices") {
            in_list = true;
            continue;
        }
        if in_list {
            let t = line.trim();
            if t.contains(':') && t.split(':').next().is_some_and(|n| !n.is_empty() && !n.contains(' ')) {
                devices.push(t.to_owned());
            }
        }
    }
    Ok((devices, text))
}

/// GPU devices among `devices` (anything that is not the CPU).
fn gpu_devices(devices: &[String]) -> Vec<String> {
    devices.iter().filter(|d| !d.to_ascii_uppercase().starts_with("CPU")).cloned().collect()
}

/// `(offloaded, total)` from a llama.cpp load log ("offloaded 65/65 layers to GPU").
pub fn offloaded_layers(log: &str) -> Option<(u32, u32)> {
    log.lines().rev().find_map(|l| {
        let at = l.find("offloaded ")?;
        let rest = &l[at + "offloaded ".len()..];
        let (n, rest) = rest.split_once('/')?;
        let m: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        Some((n.trim().parse().ok()?, m.parse().ok()?))
    })
}

// ---------------------------------------------------------------- the store

/// Installed engine builds, model pins and per-model backend results.
pub struct EngineStore<'a> {
    root: PathBuf,
    db: Mutex<Connection>,
    pins: Vec<Pin>,
    source: &'a EngineSource,
    recorder: &'a Recorder<'a>,
    gate: &'a OriginGate,
    governor: &'a Governor,
}

impl<'a> EngineStore<'a> {
    pub fn open(root: &Path, pins: Vec<Pin>, source: &'a EngineSource, recorder: &'a Recorder<'a>, gate: &'a OriginGate, governor: &'a Governor) -> Result<Self, EngineError> {
        fs::create_dir_all(root.join(".staging"))?;
        let db = Connection::open(root.join("engines.sqlite"))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS lu_engine_builds (
                variant TEXT NOT NULL, version TEXT NOT NULL,
                dir TEXT NOT NULL, server TEXT NOT NULL, sha256 TEXT NOT NULL,
                version_line TEXT NOT NULL,
                active INTEGER NOT NULL CHECK(active IN (0,1)),
                installed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                activated_seq INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(variant, version)
            );
            CREATE TABLE IF NOT EXISTS lu_runtime_packs (
                name TEXT PRIMARY KEY NOT NULL, variant TEXT NOT NULL, dir TEXT NOT NULL,
                source_sha256 TEXT NOT NULL, files INTEGER NOT NULL,
                installed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS lu_model_engine_pins (
                model TEXT PRIMARY KEY NOT NULL, variant TEXT NOT NULL, version TEXT
            );
            CREATE TABLE IF NOT EXISTS lu_backend_results (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                model TEXT NOT NULL, variant TEXT NOT NULL, version TEXT NOT NULL,
                ok INTEGER NOT NULL CHECK(ok IN (0,1)),
                load_ms INTEGER, tokens_per_second REAL, note TEXT,
                at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TRIGGER IF NOT EXISTS lu_backend_results_no_update
              BEFORE UPDATE ON lu_backend_results BEGIN SELECT RAISE(ABORT, 'results are immutable'); END;",
        )?;
        Ok(Self { root: root.to_path_buf(), db: Mutex::new(db), pins, source, recorder, gate, governor })
    }
    fn db(&self) -> Result<std::sync::MutexGuard<'_, Connection>, EngineError> {
        self.db.lock().map_err(|_| EngineError::Database("lock poisoned".into()))
    }

    pub fn pins(&self) -> &[Pin] {
        &self.pins
    }

    pub fn installed(&self) -> Result<Vec<EngineBuild>, EngineError> {
        let db = self.db()?;
        let mut q = db.prepare("SELECT variant, version, dir, server, active, version_line FROM lu_engine_builds ORDER BY variant, installed_at")?;
        let rows = q.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, bool>(4)?, r.get::<_, String>(5)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (v, version, dir, server, active, report) = row?;
            if let Some(variant) = Variant::parse(&v) {
                out.push(EngineBuild { variant, version, dir: PathBuf::from(dir), server: PathBuf::from(server), active, report });
            }
        }
        Ok(out)
    }
    pub fn active(&self, variant: Variant) -> Result<Option<EngineBuild>, EngineError> {
        Ok(self.installed()?.into_iter().find(|b| b.variant == variant && b.active))
    }
    fn build(&self, variant: Variant, version: &str) -> Result<Option<EngineBuild>, EngineError> {
        Ok(self.installed()?.into_iter().find(|b| b.variant == variant && b.version == version))
    }

    /// Pinned builds not installed yet (newer ones first).
    pub fn available(&self) -> Result<Vec<Pin>, EngineError> {
        let installed = self.installed()?;
        let mut out: Vec<Pin> = self.pins.iter().filter(|p| !installed.iter().any(|b| b.variant == p.variant && b.version == p.version)).cloned().collect();
        out.sort_by(|a, b| b.version.cmp(&a.version).then(a.variant.cmp(&b.variant)));
        Ok(out)
    }

    fn summary(pin: &Pin) -> String {
        format!("install engine llama.cpp {} {} ({})", pin.version, pin.variant.as_str(), pin.asset)
    }

    /// Install a pinned build (the user's instruction; the origin gate
    /// decides). Becomes active if `activate`, or if it is the first of
    /// its variant. A failure at any step leaves the active build as it was.
    pub fn install(&self, call_id: &str, turn_id: &str, pin: &Pin, activate: bool, cancel: &AtomicBool, progress: &mut dyn FnMut(u64, u64)) -> Result<EngineBuild, EngineError> {
        if !self.pins.contains(pin) {
            return Err(EngineError::Invalid(format!("{} is not in the pinned list", pin.asset)));
        }
        let proposal = Proposal { session_id: self.recorder.session_id().to_owned(), turn_id: turn_id.to_owned(), call_id: call_id.to_owned(), effect: EffectKind::Install, text: Self::summary(pin) };
        match self.gate.decide(self.recorder.book(), &proposal).map_err(|e| EngineError::Database(format!("{e:?}")))? {
            Decision::Run => {}
            Decision::Hold { held_id, notify, .. } => return Err(EngineError::Held { held_id, notify_user: notify }),
            Decision::Refuse { source } => return Err(EngineError::Invalid(format!("refused: asked for by {}", source.reference))),
        }
        self.install_unchecked(call_id, turn_id, pin, activate, cancel, progress)
    }

    /// Install a held build after the user approved it in a later message.
    #[allow(clippy::too_many_arguments)]
    pub fn install_approved(&self, held_id: i64, approving_turn: &str, call_id: &str, pin: &Pin, activate: bool, cancel: &AtomicBool, progress: &mut dyn FnMut(u64, u64)) -> Result<EngineBuild, EngineError> {
        let held = self.gate.held(held_id).map_err(|e| EngineError::Database(format!("{e:?}")))?.ok_or_else(|| EngineError::Invalid("no such held action".into()))?;
        if held.effect != EffectKind::Install || held.text != Self::summary(pin) {
            return Err(EngineError::Invalid("the approval is for a different action".into()));
        }
        self.gate.decide_held(self.recorder.book(), held_id, approving_turn, true).map_err(|e| EngineError::Invalid(format!("approval not accepted: {e:?}")))?;
        self.install_unchecked(call_id, approving_turn, pin, activate, cancel, progress)
    }

    fn install_unchecked(&self, call_id: &str, turn_id: &str, pin: &Pin, activate: bool, cancel: &AtomicBool, progress: &mut dyn FnMut(u64, u64)) -> Result<EngineBuild, EngineError> {
        if let Some(b) = self.build(pin.variant, &pin.version)? {
            return Ok(b);
        }
        // The archive plus its extracted copy (about the same again).
        let need = Need { disk_mib: (pin.bytes * 3).div_ceil(1024 * 1024), disk_path: Some(self.root.clone()), ..Need::default() };
        match self.governor.admit(&Self::summary(pin), &need).map_err(|e| EngineError::Database(format!("{e:?}")))? {
            Admission::Admit => {}
            Admission::Refuse(w) | Admission::Unmeasured(w) => return Err(EngineError::NoRoom(w)),
        }
        self.recorder
            .begin_command(&crate::ansf_operations::CommandStart { call_id: call_id.to_owned(), turn_id: turn_id.to_owned(), command: Self::summary(pin), cwd: self.root.to_string_lossy().into_owned() })
            .map_err(|e| EngineError::Database(format!("{e:?}")))?;
        let started = Instant::now();
        let result = self.download_extract_check(pin, activate, cancel, progress);
        let (code, output) = match &result {
            Ok(b) => (Some(0), format!("installed {} {} at {}", b.variant.as_str(), b.version, b.dir.display())),
            Err(EngineError::Cancelled) => (None, "cancelled; the staged download is kept".into()),
            Err(e) => (Some(1), format!("not installed: {e}")),
        };
        self.recorder
            .finish_command(&crate::ansf_operations::CommandFinish {
                call_id: call_id.to_owned(),
                exit_code: code,
                timed_out: false,
                cancelled: matches!(result, Err(EngineError::Cancelled)),
                duration_ms: started.elapsed().as_millis() as u64,
                output,
            })
            .map_err(|e| EngineError::Database(format!("{e:?}")))?;
        result
    }

    fn download_extract_check(&self, pin: &Pin, activate: bool, cancel: &AtomicBool, progress: &mut dyn FnMut(u64, u64)) -> Result<EngineBuild, EngineError> {
        let part = self.root.join(".staging").join(&pin.asset);
        self.source.fetch(pin, &part, cancel, progress)?;
        let dir = self.root.join(pin.variant.as_str()).join(&pin.version);
        let unpacked = self.root.join(".staging").join(format!("{}-{}", pin.variant.as_str(), pin.version));
        if unpacked.exists() {
            fs::remove_dir_all(&unpacked)?;
        }
        let fail = |e: EngineError, unpacked: &Path| {
            let _ = fs::remove_dir_all(unpacked);
            e
        };
        archive::extract(&part, &unpacked, &ArchiveLimits::default()).map_err(|e| fail(EngineError::Invalid(format!("extracting {}: {e}", pin.asset)), &unpacked))?;
        let server = find_server(&unpacked, 3).ok_or_else(|| fail(EngineError::SmokeFailed(format!("{} has no {}", pin.asset, server_name())), &unpacked))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for e in fs::read_dir(server.parent().unwrap_or(&unpacked))?.flatten() {
                let p = e.path();
                if p.is_file() {
                    let mode = if p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("llama-")) { 0o755 } else { 0o644 };
                    fs::set_permissions(&p, fs::Permissions::from_mode(mode))?;
                }
            }
        }
        let version_line = smoke(&server).map_err(|why| fail(EngineError::SmokeFailed(why), &unpacked))?;
        // A GPU build that sees no GPU here is not installed: it would only
        // run on the CPU while claiming to be ROCm or Vulkan.
        let lib_dirs = self.runtime_dirs(pin.variant)?;
        let (devices, listing) = list_devices(&server, &lib_dirs).map_err(|why| fail(EngineError::SmokeFailed(why), &unpacked))?;
        if pin.variant.uses_gpu() && gpu_devices(&devices).is_empty() {
            let lines: Vec<&str> = listing.lines().filter(|l| !l.trim().is_empty()).collect();
            let shown = lines[lines.len().saturating_sub(12)..].join(" | ");
            return Err(fail(
                EngineError::SmokeFailed(format!(
                    "this {} build finds no usable GPU on this machine, so it would run on the CPU. Its device list: {shown}",
                    pin.variant.as_str()
                )),
                &unpacked,
            ));
        }
        let version_line = format!("{version_line}; devices: {}", if devices.is_empty() { "none".to_owned() } else { devices.join(", ") });
        // Into place: the extracted folder is renamed, then registered.
        let rel = server.strip_prefix(&unpacked).map_err(|_| EngineError::Invalid("server path".into()))?.to_path_buf();
        if dir.exists() {
            fs::remove_dir_all(&dir)?; // a leftover from an install that never registered
        }
        fs::create_dir_all(dir.parent().unwrap_or(&self.root))?;
        fs::rename(&unpacked, &dir)?;
        let _ = fs::remove_file(&part);
        let server = dir.join(rel);
        // Engine files came from outside the PC.
        let source = Source { kind: SourceKind::RemoteService, reference: pin.url() };
        self.gate.mark_external_file(&server, &source).map_err(|e| EngineError::Database(format!("{e:?}")))?;
        let first = self.active(pin.variant)?.is_none();
        self.db()?.execute(
            "INSERT INTO lu_engine_builds(variant, version, dir, server, sha256, version_line, active) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            params![pin.variant.as_str(), pin.version, dir.to_string_lossy(), server.to_string_lossy(), pin.sha256, version_line],
        )?;
        if activate || first {
            self.activate(pin.variant, &pin.version)?;
        }
        self.build(pin.variant, &pin.version)?.ok_or_else(|| EngineError::Database("build vanished".into()))
    }

    /// Installed runtime packs: (name, variant, folder).
    pub fn installed_packs(&self) -> Result<Vec<(String, Variant, PathBuf)>, EngineError> {
        let db = self.db()?;
        let mut q = db.prepare("SELECT name, variant, dir FROM lu_runtime_packs ORDER BY name")?;
        let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (name, v, dir) = row?;
            if let Some(variant) = Variant::parse(&v) {
                out.push((name, variant, PathBuf::from(dir)));
            }
        }
        Ok(out)
    }

    /// Library folders of the installed packs for a variant, for the
    /// engine's search path.
    pub fn runtime_dirs(&self, variant: Variant) -> Result<Vec<PathBuf>, EngineError> {
        Ok(self.installed_packs()?.into_iter().filter(|(_, v, _)| *v == variant).map(|(_, _, d)| d.join("bin")).collect())
    }

    fn pack_summary(pack: &RuntimePack) -> String {
        format!("install runtime pack {} ({} files, {} MB, from {})", pack.name, pack.files.len(), pack.installed_bytes() / 1_000_000, pack.source.url)
    }

    /// Install a runtime pack (the user's instruction; the origin gate
    /// decides). `preloaded` is a copy of the source archive already on
    /// this machine (an installer or an earlier download); it is used only
    /// if its size and SHA-256 match the pin.
    #[allow(clippy::too_many_arguments)]
    pub fn install_pack(&self, call_id: &str, turn_id: &str, pack: &RuntimePack, preloaded: Option<&Path>, cancel: &AtomicBool, progress: &mut dyn FnMut(u64, u64)) -> Result<PathBuf, EngineError> {
        let proposal = Proposal { session_id: self.recorder.session_id().to_owned(), turn_id: turn_id.to_owned(), call_id: call_id.to_owned(), effect: EffectKind::Install, text: Self::pack_summary(pack) };
        match self.gate.decide(self.recorder.book(), &proposal).map_err(|e| EngineError::Database(format!("{e:?}")))? {
            Decision::Run => {}
            Decision::Hold { held_id, notify, .. } => return Err(EngineError::Held { held_id, notify_user: notify }),
            Decision::Refuse { source } => return Err(EngineError::Invalid(format!("refused: asked for by {}", source.reference))),
        }
        self.install_pack_unchecked(call_id, turn_id, pack, preloaded, cancel, progress)
    }

    /// Install a held runtime pack after the user approved it in a later message.
    #[allow(clippy::too_many_arguments)]
    pub fn install_pack_approved(&self, held_id: i64, approving_turn: &str, call_id: &str, pack: &RuntimePack, preloaded: Option<&Path>, cancel: &AtomicBool, progress: &mut dyn FnMut(u64, u64)) -> Result<PathBuf, EngineError> {
        let held = self.gate.held(held_id).map_err(|e| EngineError::Database(format!("{e:?}")))?.ok_or_else(|| EngineError::Invalid("no such held action".into()))?;
        if held.effect != EffectKind::Install || held.text != Self::pack_summary(pack) {
            return Err(EngineError::Invalid("the approval is for a different action".into()));
        }
        self.gate.decide_held(self.recorder.book(), held_id, approving_turn, true).map_err(|e| EngineError::Invalid(format!("approval not accepted: {e:?}")))?;
        self.install_pack_unchecked(call_id, approving_turn, pack, preloaded, cancel, progress)
    }

    fn install_pack_unchecked(&self, call_id: &str, turn_id: &str, pack: &RuntimePack, preloaded: Option<&Path>, cancel: &AtomicBool, progress: &mut dyn FnMut(u64, u64)) -> Result<PathBuf, EngineError> {
        if let Some((_, _, dir)) = self.installed_packs()?.into_iter().find(|(n, _, _)| n == &pack.name) {
            return Ok(dir);
        }
        let part = self.root.join(".staging").join(format!("{}.source", pack.name));
        let have = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        let copy = preloaded.filter(|p| fs::metadata(p).ok().map(|m| m.len()) == Some(pack.source.bytes) && have < pack.source.bytes);
        // Room for the source archive (unless already here) and the files.
        let download = if copy.is_some() || have == pack.source.bytes { 0 } else { pack.source.bytes - have };
        let need = Need { disk_mib: (download + pack.source.bytes * u64::from(copy.is_some()) + pack.installed_bytes()).div_ceil(1024 * 1024), disk_path: Some(self.root.clone()), ..Need::default() };
        match self.governor.admit(&Self::pack_summary(pack), &need).map_err(|e| EngineError::Database(format!("{e:?}")))? {
            Admission::Admit => {}
            Admission::Refuse(w) | Admission::Unmeasured(w) => return Err(EngineError::NoRoom(w)),
        }
        self.recorder
            .begin_command(&crate::ansf_operations::CommandStart { call_id: call_id.to_owned(), turn_id: turn_id.to_owned(), command: Self::pack_summary(pack), cwd: self.root.to_string_lossy().into_owned() })
            .map_err(|e| EngineError::Database(format!("{e:?}")))?;
        let started = Instant::now();
        let result = (|| {
            if let Some(src) = copy {
                fs::copy(src, &part)?;
            }
            self.source.fetch_file(&self.source.pack_url(pack), &pack.name, pack.source.bytes, &pack.source.sha256, &part, cancel, progress)?;
            let dir = self.extract_pack(pack, &part)?;
            // The source archive is large and no longer needed.
            let _ = fs::remove_file(&part);
            Ok(dir)
        })();
        let (code, output) = match &result {
            Ok(d) => (Some(0), format!("installed {} ({} files) at {}", pack.name, pack.files.len(), d.display())),
            Err(EngineError::Cancelled) => (None, "cancelled; the staged download is kept".into()),
            Err(e) => (Some(1), format!("not installed: {e}")),
        };
        self.recorder
            .finish_command(&crate::ansf_operations::CommandFinish {
                call_id: call_id.to_owned(),
                exit_code: code,
                timed_out: false,
                cancelled: matches!(result, Err(EngineError::Cancelled)),
                duration_ms: started.elapsed().as_millis() as u64,
                output,
            })
            .map_err(|e| EngineError::Database(format!("{e:?}")))?;
        result
    }

    /// Take exactly the pinned files out of the source archive, checking
    /// each one's size and SHA-256, then move the pack into place.
    fn extract_pack(&self, pack: &RuntimePack, source: &Path) -> Result<PathBuf, EngineError> {
        let staging = self.root.join(".staging").join(format!("{}-unpacked", pack.name));
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        let fail = |e: EngineError| {
            let _ = fs::remove_dir_all(&staging);
            e
        };
        let wanted: BTreeMap<&str, &PackFile> = pack.files.iter().map(|f| (f.path.as_str(), f)).collect();
        let mut done: BTreeSet<String> = BTreeSet::new();
        let reader = flate2::read::MultiGzDecoder::new(std::io::BufReader::new(File::open(source)?));
        let mut tar = tar::Archive::new(reader);
        let entries = tar.entries().map_err(|e| fail(EngineError::Invalid(format!("pack source: {e}"))))?;
        for entry in entries {
            let mut entry = entry.map_err(|e| fail(EngineError::Invalid(format!("pack source: {e}"))))?;
            let raw = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
            let name = raw.strip_prefix("./").unwrap_or(&raw).to_owned();
            let Some(file) = wanted.get(name.as_str()) else { continue };
            if entry.header().entry_type() != tar::EntryType::Regular {
                return Err(fail(EngineError::Invalid(format!("{name} is not a plain file in the source"))));
            }
            let parts = crate::archive::safe_relative(&name).map_err(|why| fail(EngineError::Invalid(format!("{name}: {why}"))))?;
            let target = parts.iter().fold(staging.clone(), |p, c| p.join(c));
            fs::create_dir_all(target.parent().unwrap_or(&staging)).map_err(|e| fail(e.into()))?;
            let mut out = File::create(&target).map_err(|e| fail(e.into()))?;
            let mut hasher = Sha256::new();
            let mut buf = vec![0u8; 1 << 20];
            let mut n = 0u64;
            loop {
                let k = entry.read(&mut buf).map_err(|e| fail(EngineError::Invalid(format!("{name}: {e}"))))?;
                if k == 0 {
                    break;
                }
                n += k as u64;
                if n > file.bytes {
                    return Err(fail(EngineError::Invalid(format!("{name} is larger than pinned"))));
                }
                out.write_all(&buf[..k]).map_err(|e| fail(e.into()))?;
                hasher.update(&buf[..k]);
            }
            out.sync_all().map_err(|e| fail(e.into()))?;
            let actual = format!("{:x}", hasher.finalize());
            if n != file.bytes || actual != file.sha256 {
                return Err(fail(EngineError::HashMismatch { expected: format!("{} {}", file.path, file.sha256), actual }));
            }
            done.insert(name);
        }
        let missing: Vec<&str> = pack.files.iter().map(|f| f.path.as_str()).filter(|p| !done.contains(*p)).collect();
        if !missing.is_empty() {
            return Err(fail(EngineError::Invalid(format!("the source lacks {} pinned file(s), e.g. {}", missing.len(), missing[0]))));
        }
        let dir = self.root.join("runtime").join(&pack.name);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(dir.parent().unwrap_or(&self.root))?;
        fs::rename(&staging, &dir)?;
        let source = Source { kind: SourceKind::RemoteService, reference: pack.source.url.clone() };
        for f in pack.files.iter().filter(|f| f.path.to_ascii_lowercase().ends_with(".dll") || f.path.to_ascii_lowercase().ends_with(".so")) {
            let p = f.path.split('/').fold(dir.clone(), |p, c| p.join(c));
            self.gate.mark_external_file(&p, &source).map_err(|e| EngineError::Database(format!("{e:?}")))?;
        }
        self.db()?.execute(
            "INSERT INTO lu_runtime_packs(name, variant, dir, source_sha256, files) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![pack.name, pack.for_variant.as_str(), dir.to_string_lossy(), pack.source.sha256, pack.files.len() as i64],
        )?;
        Ok(dir)
    }

    /// Make an installed version the active one for its variant.
    pub fn activate(&self, variant: Variant, version: &str) -> Result<(), EngineError> {
        let mut db = self.db()?;
        let tx = db.transaction()?;
        let exists: Option<i64> = tx.query_row("SELECT 1 FROM lu_engine_builds WHERE variant = ?1 AND version = ?2", params![variant.as_str(), version], |r| r.get(0)).optional()?;
        if exists.is_none() {
            return Err(EngineError::NotInstalled(format!("{} {version}", variant.as_str())));
        }
        let seq: i64 = tx.query_row("SELECT COALESCE(MAX(activated_seq), 0) + 1 FROM lu_engine_builds", [], |r| r.get(0))?;
        tx.execute("UPDATE lu_engine_builds SET active = 0 WHERE variant = ?1", params![variant.as_str()])?;
        tx.execute("UPDATE lu_engine_builds SET active = 1, activated_seq = ?3 WHERE variant = ?1 AND version = ?2", params![variant.as_str(), version, seq])?;
        tx.commit()?;
        Ok(())
    }

    /// Go back to the version that was active before the current one.
    pub fn rollback(&self, variant: Variant) -> Result<EngineBuild, EngineError> {
        let previous: Option<String> = self
            .db()?
            .query_row(
                "SELECT version FROM lu_engine_builds WHERE variant = ?1 AND active = 0 AND activated_seq > 0 ORDER BY activated_seq DESC LIMIT 1",
                params![variant.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        let version = previous.ok_or_else(|| EngineError::NotInstalled(format!("no earlier {} build to roll back to", variant.as_str())))?;
        self.activate(variant, &version)?;
        self.build(variant, &version)?.ok_or_else(|| EngineError::Database("build vanished".into()))
    }

    /// Pin a model to a backend (and optionally an exact version).
    pub fn pin_model(&self, model: &str, variant: Variant, version: Option<&str>) -> Result<(), EngineError> {
        self.db()?.execute(
            "INSERT INTO lu_model_engine_pins(model, variant, version) VALUES (?1, ?2, ?3)
             ON CONFLICT(model) DO UPDATE SET variant = excluded.variant, version = excluded.version",
            params![model, variant.as_str(), version],
        )?;
        Ok(())
    }
    pub fn unpin_model(&self, model: &str) -> Result<(), EngineError> {
        self.db()?.execute("DELETE FROM lu_model_engine_pins WHERE model = ?1", params![model])?;
        Ok(())
    }

    pub fn record(&self, model: &str, r: &BackendRecord) -> Result<(), EngineError> {
        self.db()?.execute(
            "INSERT INTO lu_backend_results(model, variant, version, ok, load_ms, tokens_per_second, note) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![model, r.variant.as_str(), r.version, r.ok, r.load_ms.map(|v| v as i64), r.tokens_per_second, r.note],
        )?;
        Ok(())
    }
    /// What happened with this model, newest first.
    pub fn records(&self, model: &str) -> Result<Vec<BackendRecord>, EngineError> {
        let db = self.db()?;
        let mut q = db.prepare("SELECT variant, version, ok, load_ms, tokens_per_second, note FROM lu_backend_results WHERE model = ?1 ORDER BY seq DESC")?;
        let rows = q.query_map(params![model], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?, r.get::<_, Option<i64>>(3)?, r.get::<_, Option<f64>>(4)?, r.get::<_, Option<String>>(5)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (v, version, ok, load_ms, tps, note) = row?;
            if let Some(variant) = Variant::parse(&v) {
                out.push(BackendRecord { variant, version, ok, load_ms: load_ms.map(|v| v as u64), tokens_per_second: tps, note });
            }
        }
        Ok(out)
    }

    /// The latest backend results across all models, newest first.
    pub fn recent_results(&self, limit: usize) -> Result<Vec<(String, BackendRecord)>, EngineError> {
        let db = self.db()?;
        let mut q = db.prepare(
            "SELECT model, variant, version, ok, load_ms, tokens_per_second, note FROM lu_backend_results ORDER BY seq DESC LIMIT ?1",
        )?;
        let rows = q.query_map(params![limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, bool>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<f64>>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (model, v, version, ok, load_ms, tps, note) = row?;
            if let Some(variant) = Variant::parse(&v) {
                out.push((model, BackendRecord { variant, version, ok, load_ms: load_ms.map(|v| v as u64), tokens_per_second: tps, note }));
            }
        }
        Ok(out)
    }

    /// The builds to try for a model, in order, each with why it is there.
    pub fn order_for(&self, model: &str, gpu_present: bool) -> Result<Vec<(EngineBuild, String)>, EngineError> {
        let pin: Option<(String, Option<String>)> = self
            .db()?
            .query_row("SELECT variant, version FROM lu_model_engine_pins WHERE model = ?1", params![model], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if let Some((v, version)) = pin {
            let variant = Variant::parse(&v).ok_or_else(|| EngineError::Invalid(format!("pinned to unknown backend {v}")))?;
            let build = match &version {
                Some(ver) => self.build(variant, ver)?,
                None => self.active(variant)?,
            };
            let build = build.ok_or_else(|| EngineError::NotInstalled(format!("{model} is pinned to {v} {}, which is not installed", version.clone().unwrap_or_default())))?;
            return Ok(vec![(build, "pinned for this model".into())]);
        }
        let records = self.records(model)?;
        let mut order: Vec<Variant> = if gpu_present { vec![Variant::Rocm, Variant::Vulkan, Variant::Cpu] } else { vec![Variant::Cpu] };
        // The backend that last worked for this model goes first; one whose
        // last attempt failed goes last.
        if let Some(ok) = records.iter().find(|r| r.ok).map(|r| r.variant).filter(|v| order.contains(v)) {
            order.retain(|v| *v != ok);
            order.insert(0, ok);
        }
        let last_failed: Vec<Variant> = order.iter().copied().filter(|v| records.iter().find(|r| r.variant == *v).is_some_and(|r| !r.ok)).collect();
        order.retain(|v| !last_failed.contains(v));
        order.extend(last_failed);
        let mut out = Vec::new();
        for v in order {
            if let Some(b) = self.active(v)? {
                let why = match records.iter().find(|r| r.variant == v) {
                    Some(r) if r.ok => "worked last time".to_owned(),
                    Some(r) => format!("failed last time ({})", r.note.clone().unwrap_or_default()),
                    None => "default order".to_owned(),
                };
                out.push((b, why));
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- the engine

/// How a model is run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSettings {
    pub context: u32,
    /// Layers on the GPU (`None`: all for GPU builds, none for CPU).
    pub gpu_layers: Option<u32>,
    pub threads: Option<u32>,
    /// A vision projector the user paired with the model.
    pub projector: Option<PathBuf>,
    /// How long a load may take before it is given up.
    pub load_timeout: Duration,
    /// Runtime-pack folders put first on the engine's library search path.
    pub lib_dirs: Vec<PathBuf>,
}
impl Default for RunSettings {
    fn default() -> Self {
        Self { context: 8192, gpu_layers: None, threads: None, projector: None, load_timeout: Duration::from_secs(600), lib_dirs: Vec::new() }
    }
}

/// Reasoning effort values Lu knows, lowest first.
pub const EFFORTS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// The reasoning settings a chat template takes, in the order to show
/// them: `low`/`medium`/`high` when it reads `reasoning_effort`, `off`/`on`
/// when it reads `enable_thinking`, nothing otherwise.
pub fn reasoning_levels(template: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    if template.contains("reasoning_effort") {
        // The values the template itself names (it refuses any other),
        // lowest first; the usual three when it names none.
        let named: Vec<&'static str> = EFFORTS.iter().copied().filter(|e| template.contains(&format!("'{e}'")) || template.contains(&format!("\"{e}\""))).collect();
        if named.is_empty() {
            out.extend(["low", "medium", "high"]);
        } else {
            out.extend(named);
        }
    }
    if template.contains("enable_thinking") {
        out.insert(0, "off");
        if out.len() == 1 {
            out.push("on");
        }
    }
    out
}

/// One message of a chat, as the engine's chat API takes it (JSON only at
/// this boundary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    /// `system`, `user`, `assistant` or `tool`.
    pub role: String,
    pub content: String,
    /// Tool calls the assistant made (assistant messages only).
    pub tool_calls: Vec<ToolCall>,
    /// The call this message answers (tool messages only).
    pub tool_call_id: Option<String>,
    /// Images shown to the model with this message, as `data:` URLs
    /// (needs a model loaded with its image support file).
    pub images: Vec<String>,
}
impl ChatMessage {
    pub fn new(role: &str, content: impl Into<String>) -> Self {
        Self { role: role.into(), content: content.into(), tool_calls: Vec::new(), tool_call_id: None, images: Vec::new() }
    }
    fn to_json(&self) -> serde_json::Value {
        let mut m = serde_json::json!({ "role": self.role, "content": self.content });
        if !self.images.is_empty() {
            let mut parts = vec![serde_json::json!({ "type": "text", "text": self.content })];
            parts.extend(self.images.iter().map(|u| serde_json::json!({ "type": "image_url", "image_url": { "url": u } })));
            m["content"] = serde_json::Value::Array(parts);
        }
        if !self.tool_calls.is_empty() {
            m["tool_calls"] = serde_json::Value::Array(
                self.tool_calls
                    .iter()
                    .map(|c| serde_json::json!({ "id": c.id, "type": "function", "function": { "name": c.name, "arguments": c.arguments } }))
                    .collect(),
            );
        }
        if let Some(id) = &self.tool_call_id {
            m["tool_call_id"] = serde_json::Value::String(id.clone());
        }
        m
    }
}

/// A tool call the model made: the function name and its arguments as the
/// model wrote them (JSON text, parsed and checked by the caller).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// A tool the model may call, with its parameters as JSON Schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: String,
}

/// Per-request generation options.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChatOptions {
    pub max_tokens: u32,
    /// `None` uses the model's own default.
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    /// How hard the model thinks, for models whose chat template takes it:
    /// `off`, `on`, `low`, `medium`, `high` (see [`reasoning_levels`]).
    pub reasoning: Option<&'static str>,
}
impl Default for ChatOptions {
    fn default() -> Self {
        Self { max_tokens: 4096, temperature: None, top_p: None, reasoning: None }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatReply {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    /// `stop`, `tool_calls`, `length`, or `loop` when Lu stopped a repeating stream.
    pub finish_reason: String,
    pub prompt_tokens_per_second: Option<f64>,
    pub tokens_per_second: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub text: String,
    pub tokens_per_second: Option<f64>,
}

struct Running {
    child: Child,
    tree: ProcessTree,
    port: u16,
    key: String,
    log: PathBuf,
    model: String,
    loaded_ms: u64,
}

/// `llama-server` from one engine build. The model owner loads and unloads
/// it (it implements [`resources::Engine`]); models are GGUF file paths.
pub struct LlamaServer {
    pub build: EngineBuild,
    pub settings: RunSettings,
    running: Option<Running>,
    agent: ureq::Agent,
    log_dir: PathBuf,
}

fn free_port() -> std::io::Result<u16> {
    Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}
fn random_key() -> String {
    let mut h = Sha256::new();
    h.update(std::process::id().to_le_bytes());
    h.update(format!("{:?}", Instant::now()).as_bytes());
    h.update(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0).to_le_bytes());
    let addr = &h as *const _ as usize;
    h.update(addr.to_le_bytes());
    format!("{:x}", h.finalize())
}
/// GPU memory in use on all GPUs, when measurable.
fn gpu_used_mib() -> Option<u64> {
    let gpus = resources::measure(None).gpus;
    if gpus.is_empty() {
        return None;
    }
    gpus.iter().map(|g| g.used_mib).sum()
}
fn tail(path: &Path, lines: usize) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    all[all.len().saturating_sub(lines)..].join(" | ")
}

impl LlamaServer {
    pub fn new(build: EngineBuild, settings: RunSettings, log_dir: &Path) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(2)))
            .timeout_global(Some(Duration::from_secs(600)))
            .build()
            .into();
        Self { build, settings, running: None, agent, log_dir: log_dir.to_path_buf() }
    }

    pub fn loaded_model(&self) -> Option<&str> {
        self.running.as_ref().map(|r| r.model.as_str())
    }
    /// The engine's own log for the running model.
    pub fn log_path(&self) -> Option<&Path> {
        self.running.as_ref().map(|r| r.log.as_path())
    }
    /// Log lines about devices and GPU offload, to see where the model runs.
    pub fn log_tail(&self, lines: usize) -> String {
        self.log_path().map(|p| tail(p, lines)).unwrap_or_default()
    }
    pub fn device_report(&self) -> Vec<String> {
        let Some(path) = self.log_path() else { return Vec::new() };
        let text = fs::read_to_string(path).unwrap_or_default();
        let keys = ["offloaded", "device", "rocm", "hip", "vulkan", "gfx", "vram", "cpu_mapped", "rocm0", "vulkan0", "error", "warning", "fall"];
        text.lines().filter(|l| { let l = l.to_ascii_lowercase(); keys.iter().any(|k| l.contains(k)) }).map(|l| l.trim().to_owned()).take(40).collect()
    }
    pub fn load_ms(&self) -> Option<u64> {
        self.running.as_ref().map(|r| r.loaded_ms)
    }

    /// The process is still there.
    pub fn alive(&mut self) -> bool {
        match self.running.as_mut() {
            Some(r) => matches!(r.child.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn start(&mut self, model: &str) -> Result<(), EngineError> {
        let variant = self.build.variant;
        let fail = |why: String| EngineError::LoadFailed { variant, why };
        if !Path::new(model).is_file() {
            return Err(fail(format!("{model} is not a file")));
        }
        fs::create_dir_all(&self.log_dir)?;
        let port = free_port()?;
        let key = random_key();
        let log = self.log_dir.join(format!("llama-server-{}-{port}.log", variant.as_str()));
        let log_file = File::create(&log)?;
        let layers = self.settings.gpu_layers.unwrap_or(if variant.uses_gpu() { 999 } else { 0 });
        let mut cmd = Command::new(&self.build.server);
        cmd.arg("--model").arg(model);
        cmd.args(["--host", "127.0.0.1", "--port", &port.to_string(), "--api-key", &key]);
        cmd.args(["--ctx-size", &self.settings.context.to_string(), "--n-gpu-layers", &layers.to_string(), "--parallel", "1", "--no-webui", "--jinja"]);
        // Fully on the GPU: do not keep a mapped copy of the file in RAM too.
        if variant.uses_gpu() && self.settings.gpu_layers.is_none() {
            // b12651 replaced --no-mmap with --load-mode.
            cmd.args(["--load-mode", "none"]);
        }
        if let Some(t) = self.settings.threads {
            cmd.args(["--threads", &t.to_string()]);
        }
        if let Some(p) = &self.settings.projector {
            cmd.arg("--mmproj").arg(p);
        }
        if let Some(dir) = self.build.server.parent() {
            cmd.current_dir(dir);
        }
        add_lib_dirs(&mut cmd, &self.settings.lib_dirs);
        cmd.stdin(Stdio::null()).stdout(Stdio::from(log_file.try_clone()?)).stderr(Stdio::from(log_file));
        let used_before = gpu_used_mib();
        let started = Instant::now();
        let (mut child, tree) = ProcessTree::spawn(cmd).map_err(|e| fail(format!("{} does not start: {e}", self.build.server.display())))?;
        let health = format!("http://127.0.0.1:{port}/health");
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(fail(format!("the engine exited ({status}) while loading: {}", tail(&log, 3))));
            }
            if let Ok(resp) = self.agent.get(&health).call() {
                if resp.status().as_u16() == 200 {
                    break;
                }
            }
            if started.elapsed() > self.settings.load_timeout {
                tree.kill();
                let _ = child.wait();
                return Err(fail(format!("not ready after {} s: {}", self.settings.load_timeout.as_secs(), tail(&log, 3))));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let loaded_ms = started.elapsed().as_millis() as u64;
        // A GPU backend must really have put the model on the GPU. llama.cpp
        // quietly runs on the CPU when it cannot use the GPU; that is a
        // failure of this backend, not a slow success.
        if variant.uses_gpu() && layers > 0 {
            let text = fs::read_to_string(&log).unwrap_or_default();
            let grew = used_before.zip(gpu_used_mib()).map(|(b, a)| a.saturating_sub(b));
            let verdict = match (offloaded_layers(&text), grew) {
                (Some((0, total)), _) => Err(format!("no layers went to the GPU (0 of {total}); it ran on the CPU")),
                (Some(_), _) => Ok(()),
                (None, Some(g)) if g >= 256 => Ok(()),
                (None, Some(g)) => Err(format!("GPU memory grew by only {g} MiB while loading; it ran on the CPU")),
                (None, None) => Err("could not confirm the model is on the GPU (no offload line in the log, GPU memory not measurable)".into()),
            };
            if let Err(why) = verdict {
                tree.kill();
                let _ = child.wait();
                return Err(fail(format!("{why}. Engine log: {}", tail(&log, 4))));
            }
        }
        self.running = Some(Running { child, tree, port, key, log, model: model.to_owned(), loaded_ms });
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(mut r) = self.running.take() {
            r.tree.kill();
            let _ = r.child.wait();
        }
    }

    /// A chat turn through the engine's chat API, streamed: each piece of
    /// text goes to `on_text` as it arrives. Tool calls are collected and
    /// returned, not run. The stream loop guard stops a reply that repeats
    /// itself; the partial reply is returned with finish reason `loop`.
    pub fn chat(&mut self, messages: &[ChatMessage], tools: &[ToolSpec], options: &ChatOptions, on_text: &mut dyn FnMut(&str)) -> Result<ChatReply, EngineError> {
        self.chat_with_stop(messages, tools, options, None, on_text)
    }

    /// The model's chat template (from the running server), to see which
    /// reasoning settings it takes.
    pub fn chat_template(&self) -> Option<String> {
        let r = self.running.as_ref()?;
        let url = format!("http://127.0.0.1:{}/props", r.port);
        let resp = self.agent.get(&url).header("Authorization", &format!("Bearer {}", r.key)).call().ok()?;
        let text = resp.into_body().with_config().limit(8 << 20).read_to_string().ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        v.get("chat_template").and_then(|t| t.as_str()).map(str::to_owned)
    }

    /// [`chat`](Self::chat) that ends early (finish reason `stopped`) when
    /// `stop` is set; closing the stream ends the generation in the engine.
    pub fn chat_with_stop(&mut self, messages: &[ChatMessage], tools: &[ToolSpec], options: &ChatOptions, stop: Option<&AtomicBool>, on_text: &mut dyn FnMut(&str)) -> Result<ChatReply, EngineError> {
        let stopped = || stop.is_some_and(|s| s.load(Ordering::SeqCst));
        let r = self.running.as_ref().ok_or_else(|| EngineError::Invalid("no model loaded".into()))?;
        let url = format!("http://127.0.0.1:{}/v1/chat/completions", r.port);
        let key = r.key.clone();
        let log = r.log.clone();
        let mut body = serde_json::json!({
            "messages": messages.iter().map(ChatMessage::to_json).collect::<Vec<_>>(),
            "stream": true,
            "max_tokens": options.max_tokens,
            "cache_prompt": true,
            "timings_per_token": false,
        });
        if let Some(t) = options.temperature {
            body["temperature"] = serde_json::json!(t);
        }
        if let Some(p) = options.top_p {
            body["top_p"] = serde_json::json!(p);
        }
        if let Some(level) = options.reasoning {
            // Templates read the keys they know and ignore the others.
            let mut kw = serde_json::json!({ "enable_thinking": level != "off" });
            if !matches!(level, "on" | "off") {
                kw["reasoning_effort"] = serde_json::json!(level);
            }
            body["chat_template_kwargs"] = kw;
        }
        if !tools.is_empty() {
            let mut list = Vec::new();
            for t in tools {
                let params: serde_json::Value = serde_json::from_str(&t.parameters).map_err(|e| EngineError::Invalid(format!("tool {}: {e}", t.name)))?;
                list.push(serde_json::json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": params } }));
            }
            body["tools"] = serde_json::Value::Array(list);
        }
        let sent = self.agent.post(&url).header("Authorization", &format!("Bearer {key}")).header("Content-Type", "application/json").send(body.to_string().as_bytes());
        let resp = match sent {
            Ok(r) => r,
            Err(e) => {
                let until = Instant::now() + Duration::from_secs(2);
                while self.alive() && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(50));
                }
                if !self.alive() {
                    self.stop();
                    return Err(EngineError::Crashed(format!("{e}; {}", tail(&log, 3))));
                }
                return Err(EngineError::Network(e.to_string()));
            }
        };
        let status = resp.status().as_u16();
        if status != 200 {
            let text = resp.into_body().with_config().limit(1 << 20).read_to_string().unwrap_or_default();
            return Err(EngineError::Invalid(format!("the engine answered {status}: {}", &text[..text.len().min(300)])));
        }
        let mut reader = std::io::BufReader::new(resp.into_body().into_reader());
        let mut reply = ChatReply { content: String::new(), tool_calls: Vec::new(), finish_reason: "stop".into(), prompt_tokens_per_second: None, tokens_per_second: None };
        let mut calls: BTreeMap<u64, ToolCall> = BTreeMap::new();
        let mut guard = crate::loop_guard::StreamLoopGuard::default();
        let mut reasoning_chars = 0usize;
        let mut saw_done = false;
        let mut line = String::new();
        loop {
            if stopped() {
                reply.finish_reason = "stopped".into();
                break;
            }
            line.clear();
            let n = match std::io::BufRead::read_line(&mut reader, &mut line) {
                Ok(n) => n,
                Err(e) => {
                    if !self.alive() {
                        self.stop();
                        return Err(EngineError::Crashed(format!("{e}; {}", tail(&log, 3))));
                    }
                    return Err(EngineError::Network(e.to_string()));
                }
            };
            if n == 0 {
                break;
            }
            let Some(data) = line.trim_end().strip_prefix("data: ") else { continue };
            if data == "[DONE]" {
                saw_done = true;
                break;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { continue };
            if let Some(t) = v.get("timings") {
                reply.prompt_tokens_per_second = t.get("prompt_per_second").and_then(|x| x.as_f64());
                reply.tokens_per_second = t.get("predicted_per_second").and_then(|x| x.as_f64());
            }
            let Some(choice) = v.pointer("/choices/0") else { continue };
            if let Some(f) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                reply.finish_reason = f.to_owned();
            }
            let Some(delta) = choice.get("delta") else { continue };
            if let Some(r) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
                reasoning_chars += r.chars().count();
            }
            if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    if guard.observe_chunk(text) != crate::loop_guard::StreamLoopVerdict::Continue {
                        reply.finish_reason = "loop".into();
                        break;
                    }
                    reply.content.push_str(text);
                    on_text(text);
                }
            }
            if let Some(list) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                for c in list {
                    let index = c.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                    let entry = calls.entry(index).or_insert_with(|| ToolCall { id: String::new(), name: String::new(), arguments: String::new() });
                    if let Some(id) = c.get("id").and_then(|i| i.as_str()) {
                        entry.id = id.to_owned();
                    }
                    if let Some(name) = c.pointer("/function/name").and_then(|i| i.as_str()) {
                        entry.name.push_str(name);
                    }
                    if let Some(args) = c.pointer("/function/arguments").and_then(|i| i.as_str()) {
                        entry.arguments.push_str(args);
                    }
                }
            }
        }
        // One line per reply in the engine's log, so a short or cut answer
        // can be traced to how it ended (no message text is written).
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&log) {
            use std::io::Write as _;
            let _ = writeln!(
                f,
                "lu reply: finish={} done={} content_chars={} reasoning_chars={} tool_calls={}",
                reply.finish_reason,
                saw_done,
                reply.content.chars().count(),
                reasoning_chars,
                calls.len()
            );
        }
        reply.tool_calls = calls.into_values().enumerate().map(|(i, mut c)| {
            if c.id.is_empty() {
                c.id = format!("call_{i}");
            }
            c
        }).collect();
        Ok(reply)
    }

    /// A plain completion (used for the speed record and smoke checks; chat
    /// goes through the model boundary).
    pub fn complete(&mut self, prompt: &str, max_tokens: u32) -> Result<Completion, EngineError> {
        let r = self.running.as_ref().ok_or_else(|| EngineError::Invalid("no model loaded".into()))?;
        let body = serde_json::json!({ "prompt": prompt, "n_predict": max_tokens, "cache_prompt": true }).to_string();
        let url = format!("http://127.0.0.1:{}/completion", r.port);
        let key = r.key.clone();
        let log = r.log.clone();
        let result = self.agent.post(&url).header("Authorization", &format!("Bearer {key}")).header("Content-Type", "application/json").send(body.as_bytes());
        let resp = match result {
            Ok(resp) => resp,
            Err(e) => {
                // A dying process drops the connection before it is reaped;
                // give it a moment to finish exiting before deciding.
                let until = Instant::now() + Duration::from_secs(2);
                while self.alive() && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(50));
                }
                if !self.alive() {
                    self.stop();
                    return Err(EngineError::Crashed(format!("{e}; {}", tail(&log, 3))));
                }
                return Err(EngineError::Network(e.to_string()));
            }
        };
        let status = resp.status().as_u16();
        let text = resp.into_body().with_config().limit(16 * 1024 * 1024).read_to_string().map_err(|e| EngineError::Network(e.to_string()))?;
        if status != 200 {
            return Err(EngineError::Invalid(format!("the engine answered {status}: {}", &text[..text.len().min(300)])));
        }
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| EngineError::Invalid(format!("engine reply: {e}")))?;
        Ok(Completion {
            text: v.get("content").and_then(|c| c.as_str()).unwrap_or_default().to_owned(),
            tokens_per_second: v.pointer("/timings/predicted_per_second").and_then(|t| t.as_f64()),
        })
    }
}

impl resources::Engine for LlamaServer {
    fn load(&mut self, model: &str) -> Result<(), String> {
        self.stop();
        self.start(model).map_err(|e| e.to_string())
    }
    fn unload(&mut self) -> Result<(), String> {
        self.stop();
        Ok(())
    }
}

impl Drop for LlamaServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Load `model` on the first backend that works, recording each attempt.
/// Returns the running engine and the attempts that failed (in plain words).
pub fn load_with_fallback(store: &EngineStore<'_>, model: &str, gpu_present: bool, settings: &RunSettings, log_dir: &Path) -> Result<(LlamaServer, Vec<String>), EngineError> {
    let order = store.order_for(model, gpu_present)?;
    if order.is_empty() {
        return Err(EngineError::NoBackend(vec!["no engine build is installed".into()]));
    }
    let mut failures = Vec::new();
    for (build, _why) in order {
        let variant = build.variant;
        let version = build.version.clone();
        let mut settings = settings.clone();
        settings.lib_dirs.extend(store.runtime_dirs(variant)?);
        let mut server = LlamaServer::new(build, settings, log_dir);
        match server.start(model) {
            Ok(()) => {
                let load_ms = server.load_ms();
                // A short completion measures speed and proves it answers.
                let speed = match server.complete("Hello", 8) {
                    Ok(c) => c.tokens_per_second,
                    Err(e) => {
                        let why = e.to_string();
                        store.record(model, &BackendRecord { variant, version, ok: false, load_ms, tokens_per_second: None, note: Some(why.clone()) })?;
                        failures.push(format!("{}: {why}", variant.as_str()));
                        continue;
                    }
                };
                store.record(model, &BackendRecord { variant, version, ok: true, load_ms, tokens_per_second: speed, note: None })?;
                return Ok((server, failures));
            }
            Err(e) => {
                let why = match &e {
                    EngineError::LoadFailed { why, .. } => why.clone(),
                    other => other.to_string(),
                };
                store.record(model, &BackendRecord { variant, version, ok: false, load_ms: None, tokens_per_second: None, note: Some(why.clone()) })?;
                failures.push(format!("{}: {why}", variant.as_str()));
            }
        }
    }
    Err(EngineError::NoBackend(failures))
}
