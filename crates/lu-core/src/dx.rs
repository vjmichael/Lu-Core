//! DX: Lu's read-only diagnostics and benchmarks.
//!
//! Design: docs/design/DX_DIAGNOSTICS_SPEC.md (reconstructed from the old
//! DX-00..DX-15 record; approved by the owner on Nov 23, 2026).
//!
//! - **Read-only.** Checks only read Lu's stores and files. They never
//!   repair, install, restart or grant anything. A fix is advice.
//! - **Passive by default.** [`passive_checks`] reads records and costs
//!   nothing. Active checks (`dependency_check` reads files; `device_check`,
//!   `load_check` and the benchmarks start processes) run only when called,
//!   which the app does only when the user starts them.
//! - **Private by default.** A finding's `evidence` is sanitized (no paths,
//!   tokens, keys or e-mail addresses); anything private goes in
//!   `private_detail`, which is shown only on the user's own machine and
//!   never exported.
//! - **Honest.** Missing or contradictory evidence gives
//!   [`Severity::NotEnoughEvidence`], never a guessed cause.
//! - **Export** writes sanitized reports into `<lu data>/dx/`.
//!
//! Benchmarks fix their scoring before the run, keep raw outputs, and a
//! re-score is stored beside the original, never over it. A checker other
//! than the built-in exact-answer one (Cognitum, when connected) plugs in
//! through [`Checker`]; Lu core does not depend on it.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Connection};

use crate::engines::{list_devices, EngineBuild, EngineStore, LlamaServer, RunSettings, Variant};
use crate::environment::{EnvironmentStore, FactStatus};
use crate::journal::EffectJournal;
use crate::origin::OriginGate;
use crate::resources::{Engine, Governor};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Ok,
    /// Something is wrong; `cause` and `fix` say what and what to do.
    Problem,
    /// The records do not show enough to say; `cause` says what is missing.
    NotEnoughEvidence,
}
impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Problem => "problem",
            Self::NotEnoughEvidence => "not enough evidence",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Stable check name, e.g. `engine.devices`.
    pub check: String,
    pub severity: Severity,
    /// One or two plain sentences.
    pub summary: String,
    pub cause: Option<String>,
    pub fix: Option<String>,
    /// Sanitized facts behind the finding (safe to export).
    pub evidence: Vec<String>,
    /// Paths and raw lines, for the user's own private view only.
    pub private_detail: Vec<String>,
}
impl Finding {
    fn new(check: &str, severity: Severity, summary: impl Into<String>) -> Self {
        Self { check: check.into(), severity, summary: sanitize(&summary.into()), cause: None, fix: None, evidence: Vec::new(), private_detail: Vec::new() }
    }
    fn cause(mut self, c: impl Into<String>) -> Self {
        self.cause = Some(sanitize(&c.into()));
        self
    }
    fn fix(mut self, f: impl Into<String>) -> Self {
        self.fix = Some(sanitize(&f.into()));
        self
    }
    fn evidence(mut self, e: impl Into<String>) -> Self {
        self.evidence.push(sanitize(&e.into()));
        self
    }
    fn private(mut self, p: impl Into<String>) -> Self {
        self.private_detail.push(p.into());
        self
    }
}

// ---------------------------------------------------------------- privacy

/// Remove what must not leave the machine: file paths, long hex or base64
/// strings (keys, tokens, hashes), bearer/API keys and e-mail addresses.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, raw) in text.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&sanitize_word(raw));
    }
    // "Bearer <x>" and "api-key <x>" pairs.
    let lower = out.to_ascii_lowercase();
    for key in ["bearer ", "--api-key ", "api_key=", "apikey=", "token="] {
        if let Some(at) = lower.find(key) {
            let start = at + key.len();
            let end = out[start..].find(' ').map_or(out.len(), |e| start + e);
            if end > start {
                out.replace_range(start..end, "<secret>");
            }
        }
    }
    out
}

fn sanitize_word(word: &str) -> String {
    let trimmed = word.trim_matches(|c: char| matches!(c, '"' | '\'' | '(' | ')' | ',' | ';' | '[' | ']' | '<' | '>'));
    if trimmed.is_empty() {
        return word.to_owned();
    }
    let b = trimmed.as_bytes();
    let windows_path = b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
    let unc = trimmed.starts_with("\\\\");
    let unix_path = ["/home/", "/Users/", "/tmp/", "/var/", "/etc/", "/mnt/", "/opt/", "/root/", "/media/", "/srv/", "/usr/", "~/"]
        .iter()
        .any(|p| trimmed.starts_with(p));
    let replacement = if windows_path || unc || unix_path {
        // Keep the file name: it says what, not where.
        let name = trimmed.rsplit(['\\', '/']).find(|s| !s.is_empty()).unwrap_or("");
        Some(if name.contains('.') && name.len() < 80 { format!("<path>/{name}") } else { "<path>".to_owned() })
    } else if trimmed.contains('@') && trimmed.split('@').nth(1).is_some_and(|d| d.contains('.')) {
        Some("<email>".to_owned())
    } else if trimmed.len() >= 32 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        Some("<hex>".to_owned())
    } else if trimmed.len() >= 40 && trimmed.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '-' | '_')) && trimmed.chars().any(|c| c.is_ascii_digit()) {
        Some("<secret>".to_owned())
    } else {
        None
    };
    match replacement {
        Some(r) => word.replace(trimmed, &r),
        None => word.to_owned(),
    }
}

// ---------------------------------------------------------------- PE imports

/// DLL names a Windows executable or DLL imports (normal and delay-load).
pub fn pe_imports(data: &[u8]) -> Result<Vec<String>, String> {
    let u16_at = |o: usize| data.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]])).ok_or("truncated");
    let u32_at = |o: usize| data.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).ok_or("truncated");
    if data.get(0..2) != Some(b"MZ") {
        return Err("not a Windows executable".into());
    }
    let pe = u32_at(0x3c)? as usize;
    if data.get(pe..pe + 4) != Some(b"PE\0\0") {
        return Err("no PE header".into());
    }
    let sections = u16_at(pe + 6)? as usize;
    let opt_size = u16_at(pe + 20)? as usize;
    let opt = pe + 24;
    let magic = u16_at(opt)?;
    let dirs = opt + if magic == 0x20b { 112 } else { 96 };
    let import_rva = u32_at(dirs + 8)? as usize;
    let delay_rva = u32_at(dirs + 13 * 8)? as usize;
    let table = opt + opt_size;
    let to_offset = |rva: usize| -> Option<usize> {
        (0..sections).find_map(|i| {
            let s = table + i * 40;
            let vsize = u32_at(s + 8).ok()? as usize;
            let va = u32_at(s + 12).ok()? as usize;
            let rsize = u32_at(s + 16).ok()? as usize;
            let raw = u32_at(s + 20).ok()? as usize;
            (va <= rva && rva < va + vsize.max(rsize)).then(|| rva - va + raw)
        })
    };
    let name_at = |rva: usize| -> Option<String> {
        let o = to_offset(rva)?;
        let end = data[o..].iter().position(|&b| b == 0)? + o;
        std::str::from_utf8(&data[o..end]).ok().map(str::to_owned)
    };
    let mut out = Vec::new();
    if import_rva != 0 {
        let mut o = to_offset(import_rva).ok_or("bad import table")?;
        for _ in 0..4096 {
            let name = u32_at(o + 12)? as usize;
            if name == 0 {
                break;
            }
            out.extend(name_at(name));
            o += 20;
        }
    }
    if delay_rva != 0 {
        if let Some(mut o) = to_offset(delay_rva) {
            for _ in 0..4096 {
                let name = u32_at(o + 4)? as usize;
                if name == 0 {
                    break;
                }
                out.extend(name_at(name));
                o += 32;
            }
        }
    }
    Ok(out)
}

/// Where a library Lu cannot find normally comes from, in plain words.
fn origin_of(dll: &str) -> &'static str {
    let d = dll.to_ascii_lowercase();
    if d.starts_with("hipblas") || d.starts_with("rocblas") || d.starts_with("rocsolver") || d.starts_with("hipblaslt") || d.starts_with("amdhip") || d.starts_with("amd_comgr") || d.starts_with("rocm") {
        "AMD's ROCm libraries (Lu's ROCm runtime pack, or AMD's HIP SDK)"
    } else if d == "vulkan-1.dll" {
        "the Vulkan loader, installed with the graphics driver"
    } else if d.starts_with("msvcp140") || d.starts_with("vcruntime140") || d.starts_with("concrt140") {
        "the Microsoft Visual C++ runtime"
    } else if d.starts_with("cudart") || d.starts_with("cublas") {
        "NVIDIA's CUDA runtime"
    } else {
        "an unknown package"
    }
}

/// System folders Windows searches for a DLL.
fn system_dirs() -> Vec<PathBuf> {
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    vec![root.join("System32"), root.clone()]
}

/// The libraries a build's GPU module needs that cannot be found: in the
/// build's own folder, in `extra_dirs` (Lu's runtime packs, the HIP SDK's
/// `bin`), or in Windows' system folders. Returns (missing DLL, needed by).
pub fn missing_libraries(start: &Path, extra_dirs: &[PathBuf]) -> Result<Vec<(String, String)>, String> {
    let dir = start.parent().ok_or("no folder")?.to_path_buf();
    let mut search = vec![dir];
    search.extend(extra_dirs.iter().cloned());
    let system = system_dirs();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut missing = Vec::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::from([start.to_path_buf()]);
    while let Some(file) = queue.pop_front() {
        let data = fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        let by = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        for dll in pe_imports(&data)? {
            let key = dll.to_ascii_lowercase();
            if key.starts_with("api-ms-") || key.starts_with("ext-ms-") || !seen.insert(key.clone()) {
                continue;
            }
            let find = |dirs: &[PathBuf]| dirs.iter().map(|d| d.join(&dll)).find(|p| p.is_file());
            if let Some(found) = find(&search) {
                queue.push_back(found);
            } else if find(&system).is_none() {
                missing.push((dll, by.clone()));
            }
        }
    }
    Ok(missing)
}

// ---------------------------------------------------------------- passive checks

/// The stores DX may read. Any of them may be absent; checks that need an
/// absent one say so instead of guessing.
#[derive(Default)]
pub struct Sources<'a> {
    pub engines: Option<&'a EngineStore<'a>>,
    pub governor: Option<&'a Governor>,
    pub journal: Option<&'a EffectJournal>,
    pub gate: Option<(&'a OriginGate, &'a str)>,
    pub environment: Option<&'a EnvironmentStore>,
}

fn gpu_devices_in(report: &str) -> Option<Vec<String>> {
    let list = report.split("devices: ").nth(1)?;
    if list.trim() == "none" {
        return Some(Vec::new());
    }
    Some(list.split(", ").filter(|d| !d.to_ascii_uppercase().starts_with("CPU")).map(str::to_owned).collect())
}

/// Every passive check. Reads only.
pub fn passive_checks(src: &Sources<'_>) -> Vec<Finding> {
    let mut out = Vec::new();
    out.extend(check_engines(src));
    out.extend(check_recent_loads(src));
    out.push(check_uncertain(src));
    out.push(check_held(src));
    out.extend(check_resources(src));
    out.push(check_environment(src));
    out
}

fn missing_source(check: &str, what: &str) -> Finding {
    Finding::new(check, Severity::NotEnoughEvidence, format!("Cannot check: the {what} record is not available.")).cause(format!("{what} not opened"))
}

fn check_engines(src: &Sources<'_>) -> Vec<Finding> {
    let Some(store) = src.engines else { return vec![missing_source("engine.installed", "engine")] };
    let builds = match store.installed() {
        Ok(b) => b,
        Err(e) => return vec![Finding::new("engine.installed", Severity::NotEnoughEvidence, "The engine record could not be read.").cause(e.to_string())],
    };
    if builds.is_empty() {
        return vec![Finding::new("engine.installed", Severity::Problem, "No inference engine is installed, so no model can run.")
            .fix("Install an engine build (CPU, Vulkan or ROCm) from Lu's pinned list.")];
    }
    let mut out = Vec::new();
    for b in &builds {
        let name = format!("{} {}", b.variant.as_str(), b.version);
        let mut f = match (b.variant.uses_gpu(), gpu_devices_in(&b.report)) {
            (true, Some(gpus)) if gpus.is_empty() => Finding::new("engine.devices", Severity::Problem, format!("The {name} engine sees no GPU, so it would run on the CPU."))
                .cause("the build's GPU module did not find a usable GPU when it was installed")
                .fix("Run the engine dependency check to see which libraries are missing."),
            (true, Some(gpus)) => {
                let mut f = Finding::new("engine.devices", Severity::Ok, format!("The {name} engine sees {} GPU(s).", gpus.len()));
                for g in gpus {
                    f = f.evidence(g);
                }
                f
            }
            (true, None) => Finding::new("engine.devices", Severity::NotEnoughEvidence, format!("The {name} engine was installed before Lu recorded devices."))
                .cause("no device list in its install record")
                .fix("Run the device check for this engine."),
            (false, _) => Finding::new("engine.devices", Severity::Ok, format!("The {name} engine runs on the CPU.")),
        };
        f = f.evidence(if b.active { "active".to_owned() } else { "installed, not active".to_owned() }).private(b.server.display().to_string());
        out.push(f);
    }
    out
}

fn check_recent_loads(src: &Sources<'_>) -> Vec<Finding> {
    let Some(store) = src.engines else { return Vec::new() };
    let results = match store.recent_results(200) {
        Ok(r) => r,
        Err(e) => return vec![Finding::new("load.recent", Severity::NotEnoughEvidence, "The load record could not be read.").cause(e.to_string())],
    };
    if results.is_empty() {
        return vec![Finding::new("load.recent", Severity::NotEnoughEvidence, "No model has been loaded yet, so there is nothing to compare.")];
    }
    // Newest result per model.
    let mut by_model: BTreeMap<String, Vec<&crate::engines::BackendRecord>> = BTreeMap::new();
    for (m, r) in &results {
        by_model.entry(m.clone()).or_default().push(r);
    }
    let mut out = Vec::new();
    for (model, rs) in by_model {
        let label = model_label(&model);
        let last = rs[0];
        let backend = format!("{} {}", last.variant.as_str(), last.version);
        let f = if !last.ok {
            let note = last.note.clone().unwrap_or_default();
            let mut f = Finding::new("load.last", Severity::Problem, format!("{label} failed to load on {backend}."))
                .cause(note.split(". Engine log").next().unwrap_or(&note).to_owned())
                .private(note.clone());
            if note.contains("ran on the CPU") {
                f = f.fix("Lu fell back to the next backend. Run the engine dependency check for this build.");
            }
            f
        } else {
            let earlier: Vec<f64> = rs[1..].iter().filter(|r| r.ok && r.variant == last.variant).filter_map(|r| r.tokens_per_second).collect();
            match (last.tokens_per_second, median(&earlier)) {
                (Some(now), Some(before)) if now < before * 0.5 => Finding::new("load.speed", Severity::Problem, format!("{label} on {backend} is much slower than before: {now:.1} tokens/s against {before:.1}."))
                    .cause("speed dropped by more than half compared with its earlier runs on this backend")
                    .fix("Check memory pressure and GPU leases; run the speed benchmark to confirm."),
                (Some(now), _) => Finding::new("load.last", Severity::Ok, format!("{label} runs on {backend} at {now:.1} tokens/s.")),
                (None, _) => Finding::new("load.last", Severity::Ok, format!("{label} loaded on {backend}.")),
            }
        };
        out.push(f.evidence(format!("load time {} ms", last.load_ms.map_or("unknown".to_owned(), |v| v.to_string()))));
    }
    out
}

/// A model's file name, whichever system wrote the path.
fn model_label(model: &str) -> String {
    model.rsplit(['/', char::from(92)]).find(|s| !s.is_empty()).unwrap_or(model).to_owned()
}
fn median(v: &[f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(s[s.len() / 2])
}

fn check_uncertain(src: &Sources<'_>) -> Finding {
    let Some(j) = src.journal else { return missing_source("effects.uncertain", "effect journal") };
    match j.uncertain_all() {
        Ok(list) if list.is_empty() => Finding::new("effects.uncertain", Severity::Ok, "No action has an unknown outcome."),
        Ok(list) => {
            let mut f = Finding::new("effects.uncertain", Severity::Problem, format!("{} action(s) were cut off and their outcome is unknown.", list.len()))
                .cause("Lu stopped (crash, timeout or cancel) before these actions confirmed their result")
                .fix("Check the files or programs they touched before repeating them; a snapshot can restore files.");
            for (_, tool, _) in list.iter().take(10) {
                f = f.evidence(format!("tool {tool}"));
            }
            for (call, _, _) in list.iter().take(10) {
                f = f.private(call.clone());
            }
            f
        }
        Err(e) => Finding::new("effects.uncertain", Severity::NotEnoughEvidence, "The effect journal could not be read.").cause(format!("{e:?}")),
    }
}

fn check_held(src: &Sources<'_>) -> Finding {
    let Some((gate, session)) = src.gate else { return missing_source("origin.held", "origin gate") };
    match gate.held_actions(session) {
        Ok(all) => {
            let waiting: Vec<_> = all.iter().filter(|h| h.status == "held").collect();
            if waiting.is_empty() {
                return Finding::new("origin.held", Severity::Ok, "No action is waiting for your approval in this chat.");
            }
            let mut f = Finding::new("origin.held", Severity::Problem, format!("{} action(s) are waiting for your approval in this chat.", waiting.len()))
                .cause("they were asked for after outside content was read, so Lu holds them until you say so")
                .fix("Approve or refuse each one in your next message.");
            for h in waiting.iter().take(10) {
                f = f.evidence(format!("{:?} held because of {:?} content", h.effect, h.source.kind)).private(h.text.clone());
            }
            f
        }
        Err(e) => Finding::new("origin.held", Severity::NotEnoughEvidence, "The origin record could not be read.").cause(format!("{e:?}")),
    }
}

fn check_resources(src: &Sources<'_>) -> Vec<Finding> {
    let Some(gov) = src.governor else { return vec![missing_source("resources", "resource")] };
    let mut out = Vec::new();
    match gov.reserves() {
        Ok(r) if !r.owner_set => out.push(
            Finding::new("resources.reserves", Severity::Ok, format!("Lu keeps {} MiB RAM, {} MiB VRAM and {} MiB disk free; these are provisional.", r.ram_mib, r.vram_mib, r.disk_mib))
                .fix("Set them after measuring real model loads on this machine."),
        ),
        Ok(r) => out.push(Finding::new("resources.reserves", Severity::Ok, format!("Lu keeps {} MiB RAM, {} MiB VRAM and {} MiB disk free (set by you).", r.ram_mib, r.vram_mib, r.disk_mib))),
        Err(e) => out.push(Finding::new("resources.reserves", Severity::NotEnoughEvidence, "The resource settings could not be read.").cause(e.to_string())),
    }
    let sample = gov.sample(None);
    let mut f = match (sample.ram_available_mib, sample.ram_total_mib) {
        (Some(a), Some(t)) => Finding::new("resources.now", Severity::Ok, format!("{a} MiB of {t} MiB RAM is free.")),
        _ => Finding::new("resources.now", Severity::NotEnoughEvidence, "Free RAM could not be measured."),
    };
    for g in &sample.gpus {
        f = f.evidence(match g.free_mib() {
            Some(free) => format!("{}: {free} MiB of {} MiB VRAM free", g.name, g.total_mib),
            None => format!("{}: VRAM use not measurable", g.name),
        });
    }
    out.push(f);
    if let Ok(events) = gov.events() {
        let recent: Vec<_> = events.iter().rev().take(50).filter(|(k, _)| matches!(k.as_str(), "refused" | "unmeasured" | "pressure" | "expire")).collect();
        if !recent.is_empty() {
            let mut f = Finding::new("resources.events", Severity::Problem, format!("{} recent refusal or pressure event(s).", recent.len())).cause("memory, VRAM or disk was short, or a plug-in did not renew its GPU lease");
            for (_, text) in recent.iter().take(5) {
                f = f.evidence(text.clone());
            }
            out.push(f);
        }
    }
    out
}

fn check_environment(src: &Sources<'_>) -> Finding {
    let Some(env) = src.environment else { return missing_source("environment", "environment") };
    match env.all() {
        Ok(facts) => {
            let bad: Vec<_> = facts.iter().filter(|f| f.status == FactStatus::Contradicted).collect();
            if bad.is_empty() {
                return Finding::new("environment.contradicted", Severity::Ok, format!("{} environment fact(s), none contradicted.", facts.len()));
            }
            let mut f = Finding::new("environment.contradicted", Severity::Problem, format!("{} environment fact(s) no longer hold.", bad.len()))
                .cause("a later check found a tool or file changed or gone")
                .fix("Tell Lu where the tool is now, or reinstall it.");
            for b in bad.iter().take(10) {
                f = f.evidence(b.key.clone()).private(b.note.clone().unwrap_or_default());
            }
            f
        }
        Err(e) => Finding::new("environment.contradicted", Severity::NotEnoughEvidence, "The environment record could not be read.").cause(format!("{e:?}")),
    }
}

// ---------------------------------------------------------------- active checks

/// Read an engine build's files and report missing libraries. Reads only
/// (starts nothing). `extra_dirs` are runtime packs Lu would add to the
/// engine's search path.
pub fn dependency_check(build: &EngineBuild, extra_dirs: &[PathBuf]) -> Finding {
    let name = format!("{} {}", build.variant.as_str(), build.version);
    if !cfg!(windows) {
        return Finding::new("engine.dependencies", Severity::NotEnoughEvidence, format!("The dependency check reads Windows builds; {name} is checked by the device check on this system."));
    }
    let dir = build.server.parent().map(Path::to_path_buf).unwrap_or_default();
    let module = match build.variant {
        Variant::Rocm => dir.join("ggml-hip.dll"),
        Variant::Vulkan => dir.join("ggml-vulkan.dll"),
        Variant::Cpu => build.server.clone(),
    };
    let start = if module.is_file() { module } else { build.server.clone() };
    match missing_libraries(&start, extra_dirs) {
        Ok(missing) if missing.is_empty() => Finding::new("engine.dependencies", Severity::Ok, format!("Every library the {name} engine needs is present.")),
        Ok(missing) => {
            let names: Vec<&str> = missing.iter().map(|(d, _)| d.as_str()).collect();
            let mut sources: Vec<&str> = missing.iter().map(|(d, _)| origin_of(d)).collect();
            sources.dedup();
            let mut f = Finding::new("engine.dependencies", Severity::Problem, format!("The {name} engine is missing {}, so its GPU module cannot load and it runs on the CPU.", names.join(" and ")))
                .cause(format!("these come from {}", sources.join("; ")))
                .fix(match build.variant {
                    Variant::Rocm => "Install Lu's ROCm runtime pack for this GPU (or AMD's HIP SDK). Lu uses Vulkan meanwhile.",
                    Variant::Vulkan => "Update the graphics driver, which installs the Vulkan loader.",
                    Variant::Cpu => "Install the Microsoft Visual C++ runtime.",
                });
            for (dll, by) in &missing {
                f = f.evidence(format!("{dll} (needed by {by})"));
            }
            f
        }
        Err(e) => Finding::new("engine.dependencies", Severity::NotEnoughEvidence, format!("The {name} engine's files could not be read.")).cause(e),
    }
}

/// Ask a build which devices it can use (starts the build briefly).
pub fn device_check(build: &EngineBuild, lib_dirs: &[PathBuf]) -> Finding {
    let name = format!("{} {}", build.variant.as_str(), build.version);
    match list_devices(&build.server, lib_dirs) {
        Ok((devices, output)) => {
            let gpus: Vec<&String> = devices.iter().filter(|d| !d.to_ascii_uppercase().starts_with("CPU")).collect();
            let mut f = if build.variant.uses_gpu() && gpus.is_empty() {
                Finding::new("engine.device_check", Severity::Problem, format!("The {name} engine finds no GPU.")).fix("Run the dependency check to see what it is missing.")
            } else {
                Finding::new("engine.device_check", Severity::Ok, format!("The {name} engine finds {} device(s).", devices.len()))
            };
            for d in &devices {
                f = f.evidence(d.clone());
            }
            f.private(output)
        }
        Err(e) => Finding::new("engine.device_check", Severity::Problem, format!("The {name} engine did not run.")).cause(e),
    }
}

/// Load a model on one build, report where it went, unload it.
pub fn load_check(build: &EngineBuild, model: &Path, settings: &RunSettings, log_dir: &Path) -> Finding {
    let name = format!("{} {}", build.variant.as_str(), build.version);
    let label = model_label(&model.to_string_lossy());
    let mut server = LlamaServer::new(build.clone(), settings.clone(), log_dir);
    match server.load(&model.to_string_lossy()) {
        Ok(()) => {
            let speed = server.complete("Hello", 8).ok().and_then(|c| c.tokens_per_second);
            let report = server.device_report();
            let mut f = Finding::new(
                "engine.load_check",
                Severity::Ok,
                format!("{label} loaded on {name} in {} ms{}.", server.load_ms().unwrap_or(0), speed.map(|s| format!(", {s:.1} tokens/s")).unwrap_or_default()),
            );
            for l in report.iter().filter(|l| l.contains("offloaded")).take(3) {
                f = f.evidence(l.clone());
            }
            let _ = server.unload();
            f.private(report.join("\n"))
        }
        Err(e) => Finding::new("engine.load_check", Severity::Problem, format!("{label} did not load on {name}.")).cause(e.split(". Engine log").next().unwrap_or(&e).to_owned()).private(e),
    }
}

// ---------------------------------------------------------------- benchmarks

/// One task with a known answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub kind: String,
    pub prompt: String,
    pub expected: String,
}

/// Decides whether an answer is right. The built-in one compares the line
/// after `ANSWER:`; Cognitum, when connected, can supply another.
pub trait Checker {
    /// Name and version, stored with every score so a later re-score is
    /// told apart from the original.
    fn name(&self) -> String;
    /// `None` when the answer cannot be judged (it is counted, not guessed).
    fn check(&self, task: &Task, answer: &str) -> Option<bool>;
}

pub struct ExactAnswer;
impl Checker for ExactAnswer {
    fn name(&self) -> String {
        "exact-answer-v1".into()
    }
    fn check(&self, task: &Task, answer: &str) -> Option<bool> {
        let line = answer.lines().rev().find_map(|l| l.trim().strip_prefix("ANSWER:").or_else(|| l.trim().strip_prefix("Answer:")))?;
        let norm = |s: &str| s.trim().trim_end_matches(['.', '!']).to_ascii_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
        Some(norm(line) == norm(&task.expected))
    }
}

/// A small fixed set for the first model-profile task kinds.
pub fn starter_tasks() -> Vec<Task> {
    let t = |id: &str, kind: &str, prompt: &str, expected: &str| Task { id: id.into(), kind: kind.into(), prompt: format!("{prompt}\nEnd with one line: ANSWER: <the answer>"), expected: expected.into() };
    vec![
        t("arith-1", "exact-arithmetic", "What is 4817 multiplied by 263?", "1266871"),
        t("arith-2", "exact-arithmetic", "What is 98765 minus 43210, divided by 5?", "11111"),
        t("math-1", "multi-step-math", "A box holds 12 eggs. A farm collects 1,000 eggs and fills as many boxes as it can. How many eggs are left over?", "4"),
        t("math-2", "multi-step-math", "Train A leaves at 09:40 and travels 150 km at 60 km/h. At what time (HH:MM) does it arrive?", "12:10"),
        t("sched-1", "scheduling", "Tasks: B needs A done first, C needs B, D needs A. List an order that does all four with A first and D last, letters separated by commas.", "A, B, C, D"),
        t("sched-2", "scheduling", "Meetings: 9:00-10:00, 9:30-11:00, 10:30-11:30, 11:00-12:00. What is the largest number that can be attended without overlap?", "2"),
        t("puzzle-1", "reasoning-puzzle", "Ann is taller than Ben. Ben is taller than Cal. Who is shortest?", "Cal"),
        t("puzzle-2", "reasoning-puzzle", "If all bloops are razzies and all razzies are lazzies, are all bloops lazzies? Answer yes or no.", "yes"),
    ]
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskResult {
    pub task: String,
    pub kind: String,
    pub answer: String,
    pub correct: Option<bool>,
    pub tokens_per_second: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BenchRun {
    pub id: i64,
    pub model: String,
    pub backend: String,
    pub checker: String,
    pub results: Vec<TaskResult>,
}
impl BenchRun {
    /// (correct, judged, total).
    pub fn score(&self) -> (usize, usize, usize) {
        let judged = self.results.iter().filter(|r| r.correct.is_some()).count();
        let correct = self.results.iter().filter(|r| r.correct == Some(true)).count();
        (correct, judged, self.results.len())
    }
    pub fn mean_speed(&self) -> Option<f64> {
        let v: Vec<f64> = self.results.iter().filter_map(|r| r.tokens_per_second).collect();
        (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
    }
}

// ---------------------------------------------------------------- the store

/// A stored finding row: (id, check, severity, summary, cause, fix).
type FindingRow = (i64, String, String, String, Option<String>, Option<String>);
/// A stored score: (task, checker, correct, original).
pub type Score = (String, String, Option<bool>, bool);

/// DX's own records: runs, findings and benchmark results, immutable.
pub struct DxStore {
    db: Mutex<Connection>,
    root: PathBuf,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl DxStore {
    /// `lu_data` is Lu's data folder; DX keeps `dx.sqlite` and exports in
    /// `<lu_data>/dx/`.
    pub fn open(lu_data: &Path) -> Result<Self, String> {
        let root = lu_data.join("dx");
        fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        let db = Connection::open(root.join("dx.sqlite")).map_err(|e| e.to_string())?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS dx_runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                kind TEXT NOT NULL CHECK(kind IN ('passive','active','benchmark')),
                started_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS dx_findings (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id INTEGER NOT NULL REFERENCES dx_runs(id),
                check_name TEXT NOT NULL, severity TEXT NOT NULL,
                summary TEXT NOT NULL, cause TEXT, fix TEXT
            );
            CREATE TABLE IF NOT EXISTS dx_evidence (
                finding_id INTEGER NOT NULL REFERENCES dx_findings(id),
                seq INTEGER NOT NULL, text TEXT NOT NULL,
                private INTEGER NOT NULL CHECK(private IN (0,1))
            );
            CREATE TABLE IF NOT EXISTS dx_bench_results (
                run_id INTEGER NOT NULL REFERENCES dx_runs(id),
                model TEXT NOT NULL, backend TEXT NOT NULL,
                task TEXT NOT NULL, kind TEXT NOT NULL,
                raw_answer TEXT NOT NULL, tokens_per_second REAL
            );
            CREATE TABLE IF NOT EXISTS dx_bench_scores (
                run_id INTEGER NOT NULL REFERENCES dx_runs(id),
                task TEXT NOT NULL, checker TEXT NOT NULL,
                correct INTEGER CHECK(correct IN (0,1)),
                original INTEGER NOT NULL CHECK(original IN (0,1)),
                scored_at INTEGER NOT NULL
            );
            CREATE TRIGGER IF NOT EXISTS dx_findings_no_update BEFORE UPDATE ON dx_findings BEGIN SELECT RAISE(ABORT, 'immutable'); END;
            CREATE TRIGGER IF NOT EXISTS dx_findings_no_delete BEFORE DELETE ON dx_findings BEGIN SELECT RAISE(ABORT, 'immutable'); END;
            CREATE TRIGGER IF NOT EXISTS dx_bench_results_no_update BEFORE UPDATE ON dx_bench_results BEGIN SELECT RAISE(ABORT, 'immutable'); END;
            CREATE TRIGGER IF NOT EXISTS dx_bench_scores_no_update BEFORE UPDATE ON dx_bench_scores BEGIN SELECT RAISE(ABORT, 'immutable'); END;",
        )
        .map_err(|e| e.to_string())?;
        Ok(Self { db: Mutex::new(db), root })
    }
    pub fn dx_folder(&self) -> &Path {
        &self.root
    }
    fn db(&self) -> Result<std::sync::MutexGuard<'_, Connection>, String> {
        self.db.lock().map_err(|_| "lock".to_owned())
    }

    /// Store a run's findings; returns the run ID.
    pub fn record(&self, kind: &str, findings: &[Finding]) -> Result<i64, String> {
        let mut db = self.db()?;
        let tx = db.transaction().map_err(|e| e.to_string())?;
        tx.execute("INSERT INTO dx_runs(kind, started_at) VALUES (?1, ?2)", params![kind, now_secs() as i64]).map_err(|e| e.to_string())?;
        let run = tx.last_insert_rowid();
        for f in findings {
            tx.execute(
                "INSERT INTO dx_findings(run_id, check_name, severity, summary, cause, fix) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![run, f.check, f.severity.as_str(), f.summary, f.cause, f.fix],
            )
            .map_err(|e| e.to_string())?;
            let fid = tx.last_insert_rowid();
            for (i, e) in f.evidence.iter().enumerate() {
                tx.execute("INSERT INTO dx_evidence VALUES (?1, ?2, ?3, 0)", params![fid, i as i64, e]).map_err(|e| e.to_string())?;
            }
            for (i, p) in f.private_detail.iter().enumerate() {
                tx.execute("INSERT INTO dx_evidence VALUES (?1, ?2, ?3, 1)", params![fid, (1000 + i) as i64, p]).map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(run)
    }

    /// Findings of a run; `private` includes the private detail.
    pub fn findings(&self, run: i64, private: bool) -> Result<Vec<Finding>, String> {
        let db = self.db()?;
        let mut q = db.prepare("SELECT id, check_name, severity, summary, cause, fix FROM dx_findings WHERE run_id = ?1 ORDER BY id").map_err(|e| e.to_string())?;
        let rows: Vec<FindingRow> = q
            .query_map(params![run], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for (id, check, sev, summary, cause, fix) in rows {
            let severity = match sev.as_str() {
                "ok" => Severity::Ok,
                "problem" => Severity::Problem,
                _ => Severity::NotEnoughEvidence,
            };
            let mut e = db.prepare("SELECT text, private FROM dx_evidence WHERE finding_id = ?1 ORDER BY seq").map_err(|e| e.to_string())?;
            let ev: Vec<(String, bool)> = e.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?))).map_err(|e| e.to_string())?.collect::<Result<_, _>>().map_err(|e| e.to_string())?;
            out.push(Finding {
                check,
                severity,
                summary,
                cause,
                fix,
                evidence: ev.iter().filter(|(_, p)| !p).map(|(t, _)| t.clone()).collect(),
                private_detail: if private { ev.iter().filter(|(_, p)| *p).map(|(t, _)| t.clone()).collect() } else { Vec::new() },
            });
        }
        Ok(out)
    }

    /// Run `tasks` on a loaded model, score them with `checker` (fixed
    /// before the run), and keep raw answers. Active: the user starts it.
    pub fn benchmark(&self, server: &mut LlamaServer, model: &str, tasks: &[Task], checker: &dyn Checker, max_tokens: u32) -> Result<BenchRun, String> {
        let backend = format!("{} {}", server.build.variant.as_str(), server.build.version);
        let checker_name = checker.name();
        let mut results = Vec::new();
        for task in tasks {
            let (answer, tps) = match server.complete(&task.prompt, max_tokens) {
                Ok(c) => (c.text, c.tokens_per_second),
                Err(e) => (format!("[no answer: {e}]"), None),
            };
            let correct = checker.check(task, &answer);
            results.push(TaskResult { task: task.id.clone(), kind: task.kind.clone(), answer, correct, tokens_per_second: tps });
        }
        let mut db = self.db()?;
        let tx = db.transaction().map_err(|e| e.to_string())?;
        tx.execute("INSERT INTO dx_runs(kind, started_at) VALUES ('benchmark', ?1)", params![now_secs() as i64]).map_err(|e| e.to_string())?;
        let run = tx.last_insert_rowid();
        for r in &results {
            tx.execute(
                "INSERT INTO dx_bench_results VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![run, model_label(model), backend, r.task, r.kind, r.answer, r.tokens_per_second],
            )
            .map_err(|e| e.to_string())?;
            tx.execute("INSERT INTO dx_bench_scores VALUES (?1, ?2, ?3, ?4, 1, ?5)", params![run, r.task, checker_name, r.correct, now_secs() as i64])
                .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(BenchRun { id: run, model: model_label(model), backend, checker: checker_name, results })
    }

    /// Score a stored run again with another checker. The original scores
    /// and raw answers stay; the new scores are stored beside them, marked
    /// as not original.
    pub fn rescore(&self, run: i64, tasks: &[Task], checker: &dyn Checker) -> Result<Vec<(String, Option<bool>)>, String> {
        let db = self.db()?;
        let mut q = db.prepare("SELECT task, raw_answer FROM dx_bench_results WHERE run_id = ?1").map_err(|e| e.to_string())?;
        let rows: Vec<(String, String)> = q.query_map(params![run], |r| Ok((r.get(0)?, r.get(1)?))).map_err(|e| e.to_string())?.collect::<Result<_, _>>().map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for (task_id, answer) in rows {
            let Some(task) = tasks.iter().find(|t| t.id == task_id) else { continue };
            let c = checker.check(task, &answer);
            db.execute("INSERT INTO dx_bench_scores VALUES (?1, ?2, ?3, ?4, 0, ?5)", params![run, task_id, checker.name(), c, now_secs() as i64]).map_err(|e| e.to_string())?;
            out.push((task_id, c));
        }
        Ok(out)
    }

    /// All scores of a run: (task, checker, correct, original).
    pub fn scores(&self, run: i64) -> Result<Vec<Score>, String> {
        let db = self.db()?;
        let mut q = db.prepare("SELECT task, checker, correct, original FROM dx_bench_scores WHERE run_id = ?1 ORDER BY rowid").map_err(|e| e.to_string())?;
        let rows = q.query_map(params![run], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
    }

    /// Write a sanitized report of a run to `<lu data>/dx/`: a readable
    /// text file and a JSON file (JSON only at this export boundary).
    /// Private detail is never written. Returns the two paths.
    pub fn export(&self, run: i64) -> Result<(PathBuf, PathBuf), String> {
        let findings = self.findings(run, false)?;
        let stamp = now_secs();
        let mut text = format!("Lu DX report, run {run}\n\n");
        for f in &findings {
            text.push_str(&format!("[{}] {}: {}\n", f.severity.as_str(), f.check, sanitize(&f.summary)));
            if let Some(c) = &f.cause {
                text.push_str(&format!("    cause: {}\n", sanitize(c)));
            }
            if let Some(x) = &f.fix {
                text.push_str(&format!("    fix: {}\n", sanitize(x)));
            }
            for e in &f.evidence {
                text.push_str(&format!("    - {}\n", sanitize(e)));
            }
        }
        let json = serde_json::json!({
            "schema": "lu-dx-report-v1",
            "run": run,
            "findings": findings.iter().map(|f| serde_json::json!({
                "check": f.check,
                "severity": f.severity.as_str(),
                "summary": sanitize(&f.summary),
                "cause": f.cause.as_deref().map(sanitize),
                "fix": f.fix.as_deref().map(sanitize),
                "evidence": f.evidence.iter().map(|e| sanitize(e)).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        });
        let txt_path = self.root.join(format!("dx-report-{run}-{stamp}.txt"));
        let json_path = self.root.join(format!("dx-report-{run}-{stamp}.json"));
        fs::write(&txt_path, text).map_err(|e| e.to_string())?;
        fs::write(&json_path, serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        Ok((txt_path, json_path))
    }
}

// ---------------------------------------------------------------- tool self-test

/// Run Lu's own basic tools, through the same checked layer the model uses
/// (origin gate, staged writes, snapshots, trash), on a scratch folder under
/// `scratch_root` with its own throwaway record, gate and snapshots. The
/// user's files and conversations are never touched. Active: the user
/// starts it. The scratch folder is removed afterwards.
pub fn tool_self_test(scratch_root: &Path) -> Vec<Finding> {
    use crate::ansf::AnsfStore;
    use crate::ansf_live_chat::{LiveEvent, LiveKind};
    use crate::chat::new_id;
    use crate::files::{Call, Content, FileAccess, FileError, ReadResult, WorkingScope, WriteMode, WriteRequest};
    use crate::recorder::Recorder;
    use crate::runner::{CancelToken, CommandRunner, RunRequest};
    use crate::snapshot::{SnapshotLimits, SnapshotStore};
    use std::io::Write as _;

    let dir = scratch_root.join(format!("lu-selftest-{}-{}", std::process::id(), now_secs()));
    let work = dir.join("work");
    let setup = (|| -> Result<_, String> {
        fs::create_dir_all(&work).map_err(|e| e.to_string())?;
        let work = fs::canonicalize(&work).map_err(|e| e.to_string())?;
        let work = PathBuf::from(work.to_string_lossy().trim_start_matches(r"\\?\").to_owned());
        let book = AnsfStore::open(&dir.join("book.sqlite")).map_err(|e| format!("{e:?}"))?;
        let journal = EffectJournal::open(&dir.join("effects.sqlite")).map_err(|e| format!("{e:?}"))?;
        let gate = OriginGate::open(&dir.join("origin.sqlite")).map_err(|e| format!("{e:?}"))?;
        let snapshots = SnapshotStore::open(&dir.join("snapshots")).map_err(|e| format!("{e:?}"))?;
        Ok((work, book, journal, gate, snapshots))
    })();
    let (work, book, journal, gate, snapshots) = match setup {
        Ok(s) => s,
        Err(e) => {
            let _ = fs::remove_dir_all(&dir);
            return vec![Finding::new("selftest.setup", Severity::Problem, "The self-test could not set up its scratch folder.").cause(e).fix("Check that Lu's data folder is writable and has free space.")];
        }
    };
    let session = new_id('c');
    let turn = new_id('g');
    let mut out = Vec::new();
    let ok_setup = book.create_session(&session, "lu", "Self-test").is_ok()
        && book.start_chapter(&session, "Self-test").is_ok()
        && book
            .append_host_chat_event(&session, &LiveEvent { event_id: new_id('e'), kind: LiveKind::User, text: format!("Lu self-test in {}", work.display()), user_turn_id: Some(turn.clone()), legacy_turn_id: None, generation_id: None })
            .is_ok();
    let rec = Recorder::new(&book, &journal, session.as_str());
    let files = match WorkingScope::new(std::slice::from_ref(&work)) {
        Ok(scope) => FileAccess::new(&rec, &snapshots, &gate, scope, SnapshotLimits::default()),
        Err(e) => {
            drop(rec);
            let _ = fs::remove_dir_all(&dir);
            return vec![Finding::new("selftest.setup", Severity::Problem, "The self-test could not set up its working folder.").cause(format!("{e:?}"))];
        }
    };
    if !ok_setup {
        out.push(Finding::new("selftest.setup", Severity::Problem, "The self-test could not record its own instruction.").fix("Run the passive checks; Lu's record store may be damaged."));
    }
    let mut n = 0;
    let mut call = || {
        n += 1;
        Call { call_id: format!("selftest-{n}"), turn_id: turn.clone() }
    };
    let mut step = |check: &str, what: &str, r: Result<(), String>| {
        out.push(match r {
            Ok(()) => Finding::new(check, Severity::Ok, format!("{what}: works.")),
            Err(e) => Finding::new(check, Severity::Problem, format!("{what}: failed.")).cause(e.clone()).fix("Export this report and look at the cause; Lu's tools may not work in chats either.").private(e),
        });
    };
    let text_of = |r: Result<ReadResult, FileError>| -> Result<String, String> {
        match r.map_err(|e| format!("{e:?}"))? {
            ReadResult::Text { content, .. } => Ok(content),
            other => Err(format!("read back as something else: {other:?}")),
        }
    };
    let note = work.join("note.txt");

    // 1. Write a new file, read it back.
    let r = files
        .write(&call(), &WriteRequest { path: note.clone(), content: Content::Text("first version\nline two\n".into()), mode: WriteMode::CreateNew, intended_lines: None })
        .map_err(|e| format!("{e:?}"))
        .and_then(|_| text_of(files.read(&call(), &note)))
        .and_then(|t| if t == "first version\nline two\n" { Ok(()) } else { Err(format!("read back {t:?}")) });
    step("selftest.write_read", "Writing a file and reading it back", r);

    // 2. Replace it; the previous version is kept and restores exactly.
    let r = (|| -> Result<(), String> {
        let receipt = files
            .write(&call(), &WriteRequest { path: note.clone(), content: Content::Text("second version\n".into()), mode: WriteMode::CreateOrReplace, intended_lines: None })
            .map_err(|e| format!("{e:?}"))?;
        let snap = receipt.snapshot.ok_or("no copy of the previous version was kept")?;
        let id: i64 = snap.trim_start_matches("snap:").parse().map_err(|_| format!("unexpected snapshot reference {snap}"))?;
        let rel = note.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        snapshots
            .restore(id, &rel)
            .or_else(|_| snapshots.restore(id, &note.to_string_lossy()))
            .map_err(|e| format!("restore: {e:?}"))?;
        let t = fs::read_to_string(&note).map_err(|e| e.to_string())?;
        if t == "first version\nline two\n" { Ok(()) } else { Err(format!("after restore the file holds {t:?}")) }
    })();
    step("selftest.snapshot_restore", "Keeping the previous version and restoring it", r);

    // 3. Edit one line.
    let r = files
        .edit_lines(&call(), &note, 2, 2, "line 2 (edited)", None)
        .map_err(|e| format!("{e:?}"))
        .and_then(|_| fs::read_to_string(&note).map_err(|e| e.to_string()))
        .and_then(|t| if t.lines().nth(1) == Some("line 2 (edited)") && t.starts_with("first version") { Ok(()) } else { Err(format!("after the edit the file holds {t:?}")) });
    step("selftest.edit", "Editing one line of a file", r);

    // 4. Delete to the trash and restore.
    let r = files
        .trash(&call(), &note)
        .map_err(|e| format!("{e:?}"))
        .and_then(|entry| if note.exists() { Err("the file is still there after deleting".into()) } else { Ok(entry) })
        .and_then(|entry| files.restore_from_trash(&call(), &entry).map_err(|e| format!("{e:?}")))
        .and_then(|_| if note.exists() { Ok(()) } else { Err("the file did not come back from the trash".into()) });
    step("selftest.trash", "Deleting to the trash and restoring", r);

    // 5. Look inside an archive and extract it.
    let r = (|| -> Result<(), String> {
        let zip_path = work.join("pack.zip");
        {
            let f = fs::File::create(&zip_path).map_err(|e| e.to_string())?;
            let mut z = zip::ZipWriter::new(f);
            z.start_file("inside/hello.txt", zip::write::SimpleFileOptions::default()).map_err(|e| e.to_string())?;
            z.write_all(b"hello from the archive").map_err(|e| e.to_string())?;
            z.finish().map_err(|e| e.to_string())?;
        }
        let listing = files.list_archive(&call(), &zip_path).map_err(|e| format!("{e:?}"))?;
        if !format!("{listing:?}").contains("hello.txt") {
            return Err(format!("the listing does not show the file: {listing:?}"));
        }
        let dest = work.join("unpacked");
        files.extract_archive(&call(), &zip_path, &dest, &crate::archive::ArchiveLimits::default()).map_err(|e| format!("{e:?}"))?;
        let t = fs::read_to_string(dest.join("inside").join("hello.txt")).map_err(|e| format!("extracted file: {e}"))?;
        if t == "hello from the archive" { Ok(()) } else { Err(format!("extracted {t:?}")) }
    })();
    step("selftest.archive", "Listing and extracting an archive", r);

    // 6. Run a command and read its output.
    let r = (|| -> Result<(), String> {
        let runner = CommandRunner::new(&rec, &snapshots, &gate, SnapshotLimits::default(), &dir.join("runner")).map_err(|e| format!("{e:?}"))?;
        let c = call();
        let cmd = if cfg!(windows) { "Write-Output lu-selftest-ok" } else { "echo lu-selftest-ok" };
        let res = runner
            .run(&RunRequest { call_id: c.call_id, turn_id: c.turn_id, command: cmd.into(), cwd: work.clone(), timeout: std::time::Duration::from_secs(60), snapshot: false }, &CancelToken::default())
            .map_err(|e| format!("{e:?}"))?;
        match (res.exit_code, res.output_for_model.contains("lu-selftest-ok")) {
            (Some(0), true) => Ok(()),
            (code, _) => Err(format!("exit code {code:?}, output {:?}", res.output_for_model.chars().take(200).collect::<String>())),
        }
    })();
    step("selftest.command", "Running a command", r);

    // 7. A write outside the allowed folder is held, not done.
    let outside = dir.join("outside.txt");
    let r = match files.write(&call(), &WriteRequest { path: outside.clone(), content: Content::Text("x".into()), mode: WriteMode::CreateNew, intended_lines: None }) {
        Err(FileError::Held { .. }) if !outside.exists() => Ok(()),
        Err(e) => Err(format!("refused for another reason: {e:?}")),
        Ok(_) => Err("the write outside the allowed folder went through".into()),
    };
    step("selftest.scope", "Holding a write outside the allowed folders", r);

    drop(files);
    drop(rec);
    drop(book);
    drop(journal);
    drop(gate);
    drop(snapshots);
    let _ = fs::remove_dir_all(&dir);
    out
}

// ---------------------------------------------------------------- memory benchmark

/// A conversation to remember and questions about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySet {
    /// Earlier (user, assistant) exchanges, oldest first. The facts sit in
    /// the first ones; later filler pushes them out of the recent window.
    pub history: Vec<(String, String)>,
    /// Questions with known answers (some about the facts, one control).
    pub questions: Vec<Task>,
}

/// The built-in set: four facts told early, twelve unrelated exchanges, and
/// questions about the facts plus one control question memory cannot help.
pub fn memory_set() -> MemorySet {
    let mut history = vec![
        ("By the way, my dog's name is Biscuit.".to_owned(), "Biscuit is a lovely name. Noted.".to_owned()),
        ("The NVP project deadline is March 14.".to_owned(), "Got it: NVP is due on March 14.".to_owned()),
        ("Remember that the staging server listens on port 8443.".to_owned(), "Noted: staging uses port 8443.".to_owned()),
        ("For charts I always want the color teal.".to_owned(), "Understood: teal for charts.".to_owned()),
    ];
    let filler = [
        ("What is the capital of Japan?", "Tokyo."),
        ("How many legs does a spider have?", "Eight."),
        ("Give me a synonym for quick.", "Fast."),
        ("What is 15 plus 27?", "42."),
        ("Which planet is known as the red planet?", "Mars."),
        ("How many days are in a leap year?", "366."),
        ("What language is spoken in Brazil?", "Portuguese."),
        ("Name a primary color.", "Blue."),
        ("What is the boiling point of water in Celsius?", "100 degrees."),
        ("How many minutes are in two hours?", "120."),
        ("What is the opposite of ancient?", "Modern."),
        ("Spell the word cat backwards.", "tac."),
    ];
    history.extend(filler.iter().map(|(a, b)| (a.to_string(), b.to_string())));
    let t = |id: &str, prompt: &str, expected: &str| Task { id: id.into(), kind: "memory".into(), prompt: format!("{prompt}\nIf you do not know, say so. End with one line: ANSWER: <the answer>"), expected: expected.into() };
    MemorySet {
        history,
        questions: vec![
            t("mem-dog", "What is my dog's name?", "Biscuit"),
            t("mem-deadline", "When is the NVP project deadline? Give the month and day.", "March 14"),
            t("mem-port", "Which port does the staging server listen on?", "8443"),
            t("mem-color", "Which color do I want for charts?", "teal"),
            t("mem-control", "What is 12 multiplied by 12?", "144"),
        ],
    }
}

/// The three ways the model is asked.
pub const MEMORY_ARMS: [&str; 3] = ["none", "recent", "memory"];

#[derive(Debug, Clone, PartialEq)]
pub struct MemoryBenchRun {
    pub id: i64,
    pub model: String,
    pub backend: String,
    /// Per arm: (arm, correct, judged, total).
    pub arms: Vec<(String, usize, usize, usize)>,
    pub results: Vec<(String, TaskResult)>,
}

impl DxStore {
    /// Memory, three arms: each question is asked with no earlier
    /// conversation ("none"), with the last `recent` messages only
    /// ("recent"), and with those plus Lu's memory block ("memory"), which
    /// brings back older messages relevant to the question. Each question
    /// gets its own throwaway record under `scratch_root`; the user's
    /// conversations are not used. Scoring is fixed before the run; raw
    /// answers are kept. Active: the user starts it.
    pub fn memory_benchmark(&self, server: &mut LlamaServer, model: &str, set: &MemorySet, checker: &dyn Checker, recent: usize, scratch_root: &Path) -> Result<MemoryBenchRun, String> {
        use crate::ansf::AnsfStore;
        use crate::ansf_live_chat::{LiveEvent, LiveKind};
        use crate::chat::new_id;
        use crate::engines::{ChatMessage, ChatOptions};
        use crate::memory::{MemoryInjector, DEFAULT_BUDGET_CHARS};
        use crate::recorder::Recorder;

        let backend = format!("{} {}", server.build.variant.as_str(), server.build.version);
        let checker_name = checker.name();
        let system = "You are a helpful assistant. Answer briefly.";
        let options = ChatOptions { max_tokens: 1024, temperature: Some(0.0), top_p: None, reasoning: Some("off") };
        let mut results: Vec<(String, TaskResult)> = Vec::new();
        for q in &set.questions {
            // A fresh record holding the earlier conversation, then the question.
            let dir = scratch_root.join(format!("lu-membench-{}-{}", std::process::id(), new_id('x')));
            fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let made = (|| -> Result<Vec<(String, TaskResult)>, String> {
                let book = AnsfStore::open(&dir.join("book.sqlite")).map_err(|e| format!("{e:?}"))?;
                let journal = EffectJournal::open(&dir.join("effects.sqlite")).map_err(|e| format!("{e:?}"))?;
                let session = new_id('c');
                book.create_session(&session, "lu", "Memory benchmark").map_err(|e| format!("{e:?}"))?;
                book.start_chapter(&session, "Benchmark").map_err(|e| format!("{e:?}"))?;
                let rec = Recorder::new(&book, &journal, session.as_str());
                let mut injector = MemoryInjector::new(&rec).map_err(|e| format!("{e:?}"))?;
                injector.recent_messages = recent;
                let user = |text: &str, turn: &str| {
                    book.append_host_chat_event(&session, &LiveEvent { event_id: new_id('e'), kind: LiveKind::User, text: text.into(), user_turn_id: Some(turn.into()), legacy_turn_id: None, generation_id: None }).map_err(|e| format!("{e:?}"))
                };
                for (u, a) in &set.history {
                    user(u, &new_id('g'))?;
                    injector.save_answer(&new_id('e'), a).map_err(|e| format!("{e:?}"))?;
                }
                // The recent window, taken before the question is added.
                let window: Vec<ChatMessage> = book
                    .recent_context_entries(&session, recent.clamp(1, 128))
                    .map_err(|e| format!("{e:?}"))?
                    .into_iter()
                    .map(|e| ChatMessage::new(if e.role == "user" { "user" } else { "assistant" }, e.content))
                    .collect();
                let turn = new_id('g');
                user(&q.prompt, &turn)?;
                let block = injector.build(&turn, &[], DEFAULT_BUDGET_CHARS).map_err(|e| format!("{e:?}"))?;
                let mut out = Vec::new();
                for arm in MEMORY_ARMS {
                    let mut sys = system.to_owned();
                    if arm == "memory" && !block.text.is_empty() {
                        sys.push_str("\n\n");
                        sys.push_str(&block.text);
                    }
                    let mut messages = vec![ChatMessage::new("system", sys)];
                    if arm != "none" {
                        messages.extend(window.iter().cloned());
                    }
                    messages.push(ChatMessage::new("user", q.prompt.clone()));
                    let (answer, tps) = match server.chat(&messages, &[], &options, &mut |_| {}) {
                        Ok(r) => (r.content, r.tokens_per_second),
                        Err(e) => (format!("[no answer: {e}]"), None),
                    };
                    let correct = checker.check(q, &answer);
                    out.push((arm.to_owned(), TaskResult { task: q.id.clone(), kind: format!("memory:{arm}"), answer, correct, tokens_per_second: tps }));
                }
                Ok(out)
            })();
            let _ = fs::remove_dir_all(&dir);
            results.extend(made?);
        }
        let mut db = self.db()?;
        let tx = db.transaction().map_err(|e| e.to_string())?;
        tx.execute("INSERT INTO dx_runs(kind, started_at) VALUES ('benchmark', ?1)", params![now_secs() as i64]).map_err(|e| e.to_string())?;
        let run = tx.last_insert_rowid();
        for (arm, r) in &results {
            let task = format!("{}@{arm}", r.task);
            tx.execute("INSERT INTO dx_bench_results VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![run, model_label(model), backend, task, r.kind, r.answer, r.tokens_per_second]).map_err(|e| e.to_string())?;
            tx.execute("INSERT INTO dx_bench_scores VALUES (?1, ?2, ?3, ?4, 1, ?5)", params![run, task, checker_name, r.correct, now_secs() as i64]).map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        let arms = MEMORY_ARMS
            .iter()
            .map(|arm| {
                let rs: Vec<&TaskResult> = results.iter().filter(|(a, _)| a == arm).map(|(_, r)| r).collect();
                (arm.to_string(), rs.iter().filter(|r| r.correct == Some(true)).count(), rs.iter().filter(|r| r.correct.is_some()).count(), rs.len())
            })
            .collect();
        Ok(MemoryBenchRun { id: run, model: model_label(model), backend, arms, results })
    }
}

