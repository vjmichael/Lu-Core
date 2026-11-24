//! Hugging Face model search and staged download.
//!
//! - **Search and review** read public metadata: repo, license, downloads,
//!   GGUF files with their size and SHA-256 (from the LFS pointer), grouped
//!   by quantization, multi-part files kept together, vision projectors
//!   (mmproj) listed separately. Gated and private repos are shown as not
//!   available; there is no token handling yet.
//! - **Results are leads, not orders.** Everything read from the Hub is
//!   untrusted content in that turn (origin gate), so a download is an
//!   install that runs only on the user's own instruction; one proposed
//!   right after a search is held until the user says so.
//! - **Staged download.** Each file goes to `<models>/.staging/<sha256>.part`,
//!   resumes after a failure with an HTTP range request, is hashed while it
//!   is written, must match the published size and SHA-256, must pass the
//!   GGUF check, and only then is renamed into `<models>/<owner>/<repo>/`.
//!   A mismatch deletes the staged file; nothing half-downloaded is ever
//!   in the model folder.
//! - **Disk is measured first** (resources), and the files are marked as
//!   coming from outside the PC.
//!
//! Ported from Lu-AI-GUI src-tauri/lu-core/src/model_provider_catalog.rs
//! (name, revision, digest and gated/private rules kept). Not ported:
//! model_provider_transport.rs, which ran a pinned curl binary; Lu now
//! downloads in-process (ureq + rustls), follows redirects itself, and only
//! to HTTPS hosts under huggingface.co or hf.co.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::Duration,
};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::ansf_operations::{CommandFinish, CommandStart};
use crate::gguf::{self, GgufInfo};
use crate::origin::{Decision, EffectKind, OriginGate, Proposal, Source, SourceKind};
use crate::recorder::Recorder;
use crate::resources::{Admission, Governor, Need};

pub const HUB: &str = "https://huggingface.co";
const MAX_METADATA: u64 = 8 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const MAX_MODEL_BYTES: u64 = 1 << 40;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubError {
    Network(String),
    Http(u16, String),
    /// The Hub answered with something Lu does not accept.
    Invalid(String),
    /// Gated or private: shown, not downloadable without a token.
    Unavailable(String),
    /// The downloaded bytes are not what the Hub published; deleted.
    HashMismatch { file: String, expected: String, actual: String },
    NotGguf { file: String, why: String },
    /// Stopped by the user; the staged part is kept for resuming.
    Cancelled,
    /// Held by the origin gate: asked for by untrusted content.
    Held { held_id: i64, notify_user: bool },
    /// Not enough disk (or unmeasurable), with the numbers.
    NoRoom(Vec<String>),
    /// A different file already has this name in the model folder.
    Conflict(PathBuf),
    Io(String),
    Database(String),
}
impl From<std::io::Error> for HubError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}
impl From<rusqlite::Error> for HubError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Database(e.to_string())
    }
}
impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for HubError {}

// ---------------------------------------------------------------- names

fn segment_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 96 && !s.starts_with('.') && !s.ends_with('.') && !s.contains("..") && s.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}
/// `owner/name`, nothing else.
pub fn valid_repo(s: &str) -> bool {
    let mut p = s.split('/');
    matches!((p.next(), p.next(), p.next()), (Some(a), Some(b), None) if segment_ok(a) && segment_ok(b))
}
pub fn valid_revision(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}
fn valid_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}
/// A GGUF path in a repo: at most one folder deep, safe segments only.
fn valid_file(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    parts.len() <= 2 && parts.iter().all(|p| segment_ok(p)) && path.to_ascii_lowercase().ends_with(".gguf")
}

/// The quantization label in a file name: `Q4_K_M`, `IQ3_XS`, `BF16`, ...
pub fn quant_of(file: &str) -> Option<String> {
    let stem = file.rsplit('/').next().unwrap_or(file);
    let stem = stem.strip_suffix(".gguf").or_else(|| stem.strip_suffix(".GGUF")).unwrap_or(stem);
    let tokens: Vec<&str> = stem.split(['-', '.']).collect();
    tokens.iter().rev().map(|t| t.to_ascii_uppercase()).find(|t| {
        let t = t.as_str();
        matches!(t, "F16" | "F32" | "BF16" | "FP16")
            || ((t.starts_with('Q') || t.starts_with("IQ") || t.starts_with("TQ"))
                && t.trim_start_matches(['I', 'T', 'Q']).chars().next().is_some_and(|c| c.is_ascii_digit()))
    })
}
/// `(part, of)` for a split file named `...-00001-of-00003.gguf`.
pub fn shard_of(file: &str) -> Option<(u32, u32)> {
    let stem = file.strip_suffix(".gguf")?;
    let (head, of) = stem.rsplit_once("-of-")?;
    let part = head.rsplit('-').next()?;
    let (p, n) = (part.parse().ok()?, of.parse().ok()?);
    (p >= 1 && p <= n && part.len() == 5 && of.len() == 5).then_some((p, n))
}
fn is_projector_name(file: &str) -> bool {
    file.rsplit('/').next().unwrap_or(file).to_ascii_lowercase().contains("mmproj")
}

// ---------------------------------------------------------------- results

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelLead {
    pub repo_id: String,
    pub license: String,
    pub downloads: u64,
    pub likes: u64,
    /// `None` when downloadable; otherwise why not (gated, private).
    pub unavailable: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileLead {
    /// Path inside the repo.
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

/// One downloadable model: a quantization, maybe in several parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub label: String,
    pub quant: Option<String>,
    /// All parts, in order.
    pub files: Vec<FileLead>,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoReview {
    pub repo_id: String,
    /// The exact commit; downloads are pinned to it.
    pub revision: String,
    pub license: String,
    pub unavailable: Option<String>,
    pub choices: Vec<ModelChoice>,
    /// Vision projectors in the repo. Pairing is the user's choice.
    pub projectors: Vec<FileLead>,
    /// Incomplete split sets and files without a published hash.
    pub skipped: Vec<(String, String)>,
    pub page: String,
}

fn restricted(v: &Value) -> Option<String> {
    if v.get("private").and_then(Value::as_bool).unwrap_or(false) {
        return Some("private repository".into());
    }
    match v.get("gated") {
        Some(Value::Bool(false)) | None | Some(Value::Null) => None,
        Some(g) => Some(format!("gated ({}): needs accepting terms and a token, which Lu does not handle yet", g.as_str().unwrap_or("yes"))),
    }
}
fn license_of(v: &Value) -> String {
    v.pointer("/cardData/license")
        .and_then(|l| l.as_str().map(str::to_owned).or_else(|| l.as_array().and_then(|a| a.first()).and_then(Value::as_str).map(str::to_owned)))
        .filter(|l| l.len() <= 80)
        .or_else(|| {
            v.get("tags")?.as_array()?.iter().filter_map(Value::as_str).find_map(|t| t.strip_prefix("license:").map(str::to_owned))
        })
        .unwrap_or_else(|| "unknown".into())
}

pub fn parse_search(raw: &[u8]) -> Result<Vec<ModelLead>, HubError> {
    let v: Value = serde_json::from_slice(raw).map_err(|e| HubError::Invalid(format!("search results: {e}")))?;
    let items = v.as_array().ok_or_else(|| HubError::Invalid("search results are not a list".into()))?;
    Ok(items
        .iter()
        .filter_map(|item| {
            let id = item.get("id").or_else(|| item.get("modelId"))?.as_str()?;
            valid_repo(id).then(|| ModelLead {
                repo_id: id.to_owned(),
                license: license_of(item),
                downloads: item.get("downloads").and_then(Value::as_u64).unwrap_or(0),
                likes: item.get("likes").and_then(Value::as_u64).unwrap_or(0),
                unavailable: restricted(item),
            })
        })
        .collect())
}

pub fn parse_review(info: &[u8], tree: &[u8], repo_id: &str) -> Result<RepoReview, HubError> {
    let v: Value = serde_json::from_slice(info).map_err(|e| HubError::Invalid(format!("model info: {e}")))?;
    if v.get("id").and_then(Value::as_str) != Some(repo_id) {
        return Err(HubError::Invalid("the model info is for a different repository".into()));
    }
    let revision = v.get("sha").and_then(Value::as_str).filter(|s| valid_revision(s)).ok_or_else(|| HubError::Invalid("no pinned revision".into()))?.to_ascii_lowercase();
    let t: Value = serde_json::from_slice(tree).map_err(|e| HubError::Invalid(format!("file list: {e}")))?;
    let entries = t.as_array().ok_or_else(|| HubError::Invalid("the file list is not a list".into()))?;
    let mut skipped = Vec::new();
    let mut files = Vec::new();
    for e in entries {
        if e.get("type").and_then(Value::as_str) != Some("file") {
            continue;
        }
        let Some(path) = e.get("path").and_then(Value::as_str) else { continue };
        if !path.to_ascii_lowercase().ends_with(".gguf") {
            continue;
        }
        if !valid_file(path) {
            skipped.push((path.to_owned(), "an unsafe or nested path".into()));
            continue;
        }
        let lower = path.to_ascii_lowercase();
        if lower.starts_with("mtp/") || lower.contains("/mtp") || lower.rsplit('/').next().is_some_and(|n| n.starts_with("mtp-") || n.starts_with("mtp_")) {
            skipped.push((path.to_owned(), "a draft head for faster generation (MTP), not a model on its own".into()));
            continue;
        }
        if path.to_ascii_lowercase().contains("imatrix") {
            skipped.push((path.to_owned(), "an importance matrix (quantization data), not a model".into()));
            continue;
        }
        let size = e.pointer("/lfs/size").or_else(|| e.get("size")).and_then(Value::as_u64).unwrap_or(0);
        let digest = e.pointer("/lfs/oid").or_else(|| e.pointer("/lfs/sha256")).and_then(Value::as_str);
        match digest {
            Some(d) if valid_sha256(d) && (gguf::HEADER_BYTES..=MAX_MODEL_BYTES).contains(&size) => {
                files.push(FileLead { path: path.to_owned(), bytes: size, sha256: d.to_ascii_lowercase() })
            }
            _ => skipped.push((path.to_owned(), "no published SHA-256 to check it against".into())),
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let (projectors, models): (Vec<_>, Vec<_>) = files.into_iter().partition(|f| is_projector_name(&f.path));
    // Group split files; a set with a missing part is not offered.
    let mut choices: Vec<ModelChoice> = Vec::new();
    let mut sets: std::collections::BTreeMap<String, (u32, Vec<FileLead>)> = Default::default();
    for f in models {
        match shard_of(&f.path) {
            Some((_, of)) => {
                // "dir/name-Q8_0-00001-of-00002.gguf" -> "dir/name-Q8_0"
                let key = f.path.rsplit_once("-of-").and_then(|(head, _)| head.rsplit_once('-')).map_or(f.path.clone(), |(k, _)| k.to_owned());
                sets.entry(key).or_insert((of, Vec::new())).1.push(f);
            }
            None => choices.push(ModelChoice {
                label: f.path.clone(),
                quant: quant_of(&f.path),
                total_bytes: f.bytes,
                files: vec![f],
            }),
        }
    }
    for (key, (of, mut parts)) in sets {
        parts.sort_by_key(|f| shard_of(&f.path).map(|s| s.0));
        let complete = parts.len() == of as usize && parts.iter().enumerate().all(|(i, f)| shard_of(&f.path) == Some((i as u32 + 1, of)));
        if complete {
            choices.push(ModelChoice { label: format!("{key} ({of} parts)"), quant: quant_of(&parts[0].path), total_bytes: parts.iter().map(|f| f.bytes).sum(), files: parts });
        } else {
            skipped.push((key, format!("split into {of} parts but only {} are listed", parts.len())));
        }
    }
    choices.sort_by_key(|c| c.total_bytes);
    Ok(RepoReview {
        repo_id: repo_id.to_owned(),
        page: format!("{HUB}/{repo_id}/tree/{revision}"),
        revision,
        license: license_of(&v),
        unavailable: restricted(&v),
        choices,
        projectors,
        skipped,
    })
}

// ---------------------------------------------------------------- transport

/// The Hub over HTTPS. Redirects are followed by Lu, only to HTTPS hosts
/// under huggingface.co or hf.co (the file CDN lives there).
pub struct Hub {
    base: String,
    agent: ureq::Agent,
    /// Plain-HTTP loopback base, for tests only.
    local_test: bool,
}

impl Hub {
    pub fn official() -> Self {
        Self::build(HUB.to_owned(), false)
    }
    /// A local stand-in for the Hub (tests). Only loopback HTTP is accepted.
    pub fn local_test(base: &str) -> Result<Self, HubError> {
        let host = base.strip_prefix("http://").and_then(|r| r.split(['/', ':']).next()).unwrap_or("");
        if !matches!(host, "127.0.0.1" | "localhost") {
            return Err(HubError::Invalid("a test hub must be on loopback".into()));
        }
        Ok(Self::build(base.trim_end_matches('/').to_owned(), true))
    }
    fn build(base: String, local_test: bool) -> Self {
        let agent = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .https_only(!local_test)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .timeout_recv_body(None)
            .user_agent("Lu/0.1")
            .build()
            .into();
        Self { base, agent, local_test }
    }

    fn host_allowed(&self, url: &str) -> bool {
        if self.local_test && url.starts_with(&self.base) {
            return true;
        }
        let Some(rest) = url.strip_prefix("https://") else { return false };
        let host = rest.split(['/', '?', '#']).next().unwrap_or("").to_ascii_lowercase();
        if host.contains('@') || host.contains(':') {
            return false;
        }
        host == "huggingface.co" || host.ends_with(".huggingface.co") || host == "hf.co" || host.ends_with(".hf.co")
    }

    /// GET with Lu's own redirect checks. `from` asks for a byte range.
    fn get(&self, url: &str, from: Option<u64>) -> Result<ureq::http::Response<ureq::Body>, HubError> {
        let mut url = url.to_owned();
        for _ in 0..=MAX_REDIRECTS {
            if !self.host_allowed(&url) {
                return Err(HubError::Invalid(format!("refused to contact {url}: not the Hugging Face Hub")));
            }
            let mut req = self.agent.get(&url);
            if let Some(n) = from {
                req = req.header("Range", &format!("bytes={n}-"));
            }
            let resp = req.call().map_err(|e| HubError::Network(e.to_string()))?;
            let status = resp.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                let loc = resp.headers().get("location").and_then(|h| h.to_str().ok()).ok_or_else(|| HubError::Invalid("a redirect without a location".into()))?;
                url = if loc.starts_with("https://") || loc.starts_with("http://") {
                    loc.to_owned()
                } else if loc.starts_with('/') {
                    let origin_end = url.find("://").map(|i| i + 3).and_then(|s| url[s..].find('/').map(|j| s + j)).unwrap_or(url.len());
                    format!("{}{loc}", &url[..origin_end])
                } else {
                    return Err(HubError::Invalid(format!("an unusable redirect: {loc}")));
                };
                continue;
            }
            return Ok(resp);
        }
        Err(HubError::Invalid("too many redirects".into()))
    }

    fn get_json(&self, path: &str) -> Result<Vec<u8>, HubError> {
        let resp = self.get(&format!("{}{path}", self.base), None)?;
        let status = resp.status().as_u16();
        let body = resp.into_body().with_config().limit(MAX_METADATA).read_to_vec().map_err(|e| HubError::Network(e.to_string()))?;
        match status {
            200 => Ok(body),
            401 | 403 => Err(HubError::Unavailable("the Hub refused access (gated or private)".into())),
            404 => Err(HubError::Http(404, "not found on the Hub".into())),
            s => Err(HubError::Http(s, String::from_utf8_lossy(&body[..body.len().min(200)]).into_owned())),
        }
    }

    /// Search GGUF models by name, most downloaded first.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<ModelLead>, HubError> {
        let q = query.trim();
        if q.is_empty() || q.len() > 96 || !q.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'-' | b'_' | b'.' | b'/')) {
            return Err(HubError::Invalid("search with letters, digits, spaces and - _ . / only".into()));
        }
        let encoded = q.split_whitespace().collect::<Vec<_>>().join("%20").replace('/', "%2F");
        let raw = self.get_json(&format!("/api/models?search={encoded}&filter=gguf&sort=downloads&direction=-1&limit={}&cardData=true", limit.clamp(1, 50)))?;
        parse_search(&raw)
    }

    /// The files in a repository at its current (or a given) commit.
    pub fn review(&self, repo_id: &str, revision: Option<&str>) -> Result<RepoReview, HubError> {
        if !valid_repo(repo_id) {
            return Err(HubError::Invalid(format!("{repo_id:?} is not owner/name")));
        }
        let info_path = match revision {
            Some(r) if valid_revision(r) => format!("/api/models/{repo_id}/revision/{r}"),
            Some(r) => return Err(HubError::Invalid(format!("{r:?} is not a commit hash"))),
            None => format!("/api/models/{repo_id}"),
        };
        let info = self.get_json(&info_path)?;
        let sha = serde_json::from_slice::<Value>(&info).ok().and_then(|v| v.get("sha").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
        if !valid_revision(&sha) {
            return Err(HubError::Invalid("the Hub gave no commit hash".into()));
        }
        let tree = self.get_json(&format!("/api/models/{repo_id}/tree/{sha}?recursive=true"))?;
        parse_review(&info, &tree, repo_id)
    }
}

// ---------------------------------------------------------------- installing

/// What the user chose to install: one model (all its parts) and maybe a
/// projector, pinned to a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRequest {
    pub repo_id: String,
    pub revision: String,
    pub files: Vec<FileLead>,
    pub projector: Option<FileLead>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledFile {
    pub path: PathBuf,
    pub repo_file: String,
    pub bytes: u64,
    pub sha256: String,
    pub info: GgufInfo,
    pub resumed: bool,
    /// Already there with the same hash; not downloaded again.
    pub already_installed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub model: Vec<InstalledFile>,
    pub projector: Option<InstalledFile>,
}

/// A model in the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledModel {
    pub path: PathBuf,
    pub repo_id: String,
    pub repo_file: String,
    pub architecture: Option<String>,
    pub context_length: Option<u64>,
    /// The vision projector the user paired with it.
    pub projector: Option<PathBuf>,
}

/// Progress: (repo file, bytes done, bytes total).
pub type Progress<'p> = &'p mut dyn FnMut(&str, u64, u64);

/// Installs models into a folder and keeps the registry of what is there.
pub struct ModelInstaller<'a> {
    root: PathBuf,
    db: Mutex<Connection>,
    hub: &'a Hub,
    recorder: &'a Recorder<'a>,
    gate: &'a OriginGate,
    governor: &'a Governor,
}

impl<'a> ModelInstaller<'a> {
    pub fn new(root: &Path, hub: &'a Hub, recorder: &'a Recorder<'a>, gate: &'a OriginGate, governor: &'a Governor) -> Result<Self, HubError> {
        fs::create_dir_all(root.join(".staging"))?;
        let db = Connection::open(root.join("models.sqlite"))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS lu_installed_models (
                path TEXT PRIMARY KEY NOT NULL,
                repo_id TEXT NOT NULL, revision TEXT NOT NULL, repo_file TEXT NOT NULL,
                sha256 TEXT NOT NULL, bytes INTEGER NOT NULL,
                architecture TEXT, name TEXT, context_length INTEGER,
                is_projector INTEGER NOT NULL CHECK(is_projector IN (0,1)),
                projector_path TEXT,
                installed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );",
        )?;
        Ok(Self { root: root.to_path_buf(), db: Mutex::new(db), hub, recorder, gate, governor })
    }

    fn db(&self) -> Result<std::sync::MutexGuard<'_, Connection>, HubError> {
        self.db.lock().map_err(|_| HubError::Database("lock poisoned".into()))
    }

    /// Search; the results are untrusted content in this turn.
    pub fn search(&self, turn_id: &str, query: &str, limit: usize) -> Result<Vec<ModelLead>, HubError> {
        let leads = self.hub.search(query, limit)?;
        let text: Vec<String> = leads.iter().map(|l| format!("{} {}", l.repo_id, l.license)).collect();
        self.untrusted(turn_id, &format!("{HUB}/models?search={query}"), &text.join("\n"))?;
        Ok(leads)
    }
    /// Review a repository; its contents are untrusted content in this turn.
    pub fn review(&self, turn_id: &str, repo_id: &str, revision: Option<&str>) -> Result<RepoReview, HubError> {
        let review = self.hub.review(repo_id, revision)?;
        let text: Vec<&str> = review.choices.iter().flat_map(|c| c.files.iter().map(|f| f.path.as_str())).collect();
        self.untrusted(turn_id, &review.page, &text.join("\n"))?;
        Ok(review)
    }
    fn untrusted(&self, turn_id: &str, reference: &str, text: &str) -> Result<(), HubError> {
        self.gate
            .register_content(self.recorder.session_id(), turn_id, &Source { kind: SourceKind::RemoteService, reference: reference.to_owned() }, text)
            .map_err(|e| HubError::Database(format!("{e:?}")))
    }

    fn destination(&self, repo_id: &str, file: &str) -> PathBuf {
        let mut p = self.root.clone();
        for part in repo_id.split('/').chain(file.split('/')) {
            p.push(part);
        }
        p
    }

    fn summary(req: &InstallRequest) -> String {
        let all: Vec<&str> = req.files.iter().chain(req.projector.as_ref()).map(|f| f.path.as_str()).collect();
        format!("install model {}@{}: {}", req.repo_id, &req.revision[..12], all.join(", "))
    }
    fn check_request(req: &InstallRequest) -> Result<(), HubError> {
        if !valid_repo(&req.repo_id) || !valid_revision(&req.revision) || req.files.is_empty() {
            return Err(HubError::Invalid("an install needs owner/name, a commit hash and at least one file".into()));
        }
        if req.files.iter().chain(req.projector.as_ref()).any(|f| !valid_file(&f.path) || !valid_sha256(&f.sha256)) {
            return Err(HubError::Invalid("a file name or hash is not valid".into()));
        }
        Ok(())
    }

    /// Download and install, after the origin gate and a disk check. File
    /// names copied from the Hub are not the user's words, so an install the
    /// model proposes is usually held: the user is told, and approves it with
    /// [`install_approved`](Self::install_approved) in a later message.
    pub fn install(&self, call_id: &str, turn_id: &str, req: &InstallRequest, cancel: &AtomicBool, progress: Progress<'_>) -> Result<Installed, HubError> {
        Self::check_request(req)?;
        let summary = Self::summary(req);
        let proposal = Proposal { session_id: self.recorder.session_id().to_owned(), turn_id: turn_id.to_owned(), call_id: call_id.to_owned(), effect: EffectKind::Install, text: summary };
        match self.gate.decide(self.recorder.book(), &proposal).map_err(|e| HubError::Database(format!("{e:?}")))? {
            Decision::Run => {}
            Decision::Hold { held_id, notify, .. } => return Err(HubError::Held { held_id, notify_user: notify }),
            Decision::Refuse { source } => return Err(HubError::Invalid(format!("refused: asked for by {}", source.reference))),
        }
        self.run_install(call_id, turn_id, req, cancel, progress)
    }

    /// The user approved a held install in a later message (`approving_turn`).
    /// The approval covers exactly the held action: the same repository,
    /// commit and files, or it is refused.
    pub fn install_approved(&self, held_id: i64, approving_turn: &str, call_id: &str, req: &InstallRequest, cancel: &AtomicBool, progress: Progress<'_>) -> Result<Installed, HubError> {
        Self::check_request(req)?;
        let held = self.gate.held(held_id).map_err(|e| HubError::Database(format!("{e:?}")))?.ok_or_else(|| HubError::Invalid("no such held action".into()))?;
        if held.effect != EffectKind::Install || held.text != Self::summary(req) || held.session_id != self.recorder.session_id() {
            return Err(HubError::Invalid("the approval is for a different action".into()));
        }
        self.gate
            .decide_held(self.recorder.book(), held_id, approving_turn, true)
            .map_err(|e| HubError::Invalid(format!("approval not accepted: {e:?}")))?;
        self.run_install(call_id, approving_turn, req, cancel, progress)
    }

    fn run_install(&self, call_id: &str, turn_id: &str, req: &InstallRequest, cancel: &AtomicBool, progress: Progress<'_>) -> Result<Installed, HubError> {
        let summary = Self::summary(req);
        let all: Vec<&FileLead> = req.files.iter().chain(req.projector.as_ref()).collect();
        // Room for what is not staged yet, plus the copy made by the rename
        // only if staging is on another disk (it is not: same folder).
        let staged: u64 = all.iter().map(|f| fs::metadata(self.part(f)).map(|m| m.len()).unwrap_or(0)).sum();
        let need_mib = all.iter().map(|f| f.bytes).sum::<u64>().saturating_sub(staged).div_ceil(1024 * 1024);
        let need = Need { disk_mib: need_mib, disk_path: Some(self.root.clone()), ..Need::default() };
        match self.governor.admit(&summary, &need).map_err(|e| HubError::Database(format!("{e:?}")))? {
            Admission::Admit => {}
            Admission::Refuse(w) | Admission::Unmeasured(w) => return Err(HubError::NoRoom(w)),
        }
        self.recorder
            .begin_command(&CommandStart { call_id: call_id.to_owned(), turn_id: turn_id.to_owned(), command: summary, cwd: self.root.to_string_lossy().into_owned() })
            .map_err(|e| HubError::Database(format!("{e:?}")))?;
        let started = std::time::Instant::now();
        let result = (|| {
            let mut model = Vec::new();
            for f in &req.files {
                model.push(self.install_file(req, f, false, cancel, progress)?);
            }
            let projector = match &req.projector {
                Some(p) => Some(self.install_file(req, p, true, cancel, progress)?),
                None => None,
            };
            if let Some(p) = &projector {
                // The user chose this pairing; record it on the first part.
                self.db()?.execute("UPDATE lu_installed_models SET projector_path = ?1 WHERE path = ?2", params![key(&p.path), key(&model[0].path)])?;
            }
            Ok(Installed { model, projector })
        })();
        let (code, output) = match &result {
            Ok(i) => (Some(0), format!("installed {} file(s){}", i.model.len(), if i.projector.is_some() { " and a projector" } else { "" })),
            Err(HubError::Cancelled) => (None, "cancelled; staged parts kept for resuming".into()),
            Err(e) => (Some(1), format!("failed: {e}")),
        };
        self.recorder
            .finish_command(&CommandFinish {
                call_id: call_id.to_owned(),
                exit_code: code,
                timed_out: false,
                cancelled: matches!(result, Err(HubError::Cancelled)),
                duration_ms: started.elapsed().as_millis() as u64,
                output,
            })
            .map_err(|e| HubError::Database(format!("{e:?}")))?;
        result
    }

    fn part(&self, f: &FileLead) -> PathBuf {
        self.root.join(".staging").join(format!("{}.part", f.sha256))
    }

    fn install_file(&self, req: &InstallRequest, f: &FileLead, projector: bool, cancel: &AtomicBool, progress: Progress<'_>) -> Result<InstalledFile, HubError> {
        let dest = self.destination(&req.repo_id, &f.path);
        if dest.exists() {
            let (sha, bytes) = sha_file(&dest)?;
            if sha != f.sha256 {
                return Err(HubError::Conflict(dest));
            }
            let info = gguf::inspect(&dest).map_err(|e| HubError::NotGguf { file: f.path.clone(), why: e.to_string() })?;
            self.register(req, f, &dest, &info, projector)?;
            return Ok(InstalledFile { path: dest, repo_file: f.path.clone(), bytes, sha256: sha, info, resumed: false, already_installed: true });
        }
        let part = self.part(f);
        let resumed = self.fetch(req, f, &part, cancel, progress)?;
        let info = match gguf::inspect(&part) {
            Ok(i) => i,
            Err(e) => {
                let _ = fs::remove_file(&part);
                return Err(HubError::NotGguf { file: f.path.clone(), why: e.to_string() });
            }
        };
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&part, &dest)?;
        let source = Source { kind: SourceKind::RemoteService, reference: format!("{HUB}/{}/blob/{}/{}", req.repo_id, req.revision, f.path) };
        self.gate.mark_external_file(&dest, &source).map_err(|e| HubError::Database(format!("{e:?}")))?;
        self.register(req, f, &dest, &info, projector)?;
        Ok(InstalledFile { path: dest, repo_file: f.path.clone(), bytes: f.bytes, sha256: f.sha256.clone(), info, resumed, already_installed: false })
    }

    /// Download into the staging part, resuming what is there. Returns
    /// whether it resumed. On success the part has the published size and hash.
    fn fetch(&self, req: &InstallRequest, f: &FileLead, part: &Path, cancel: &AtomicBool, progress: Progress<'_>) -> Result<bool, HubError> {
        let have = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
        if have > f.bytes {
            fs::remove_file(part)?;
        }
        let have = have.min(f.bytes);
        let mut hasher = Sha256::new();
        let mut resumed = false;
        let mut done = 0u64;
        if have == f.bytes {
            // Fully staged earlier: only verify.
            let (sha, _) = sha_file(part)?;
            return if sha == f.sha256 {
                Ok(true)
            } else {
                fs::remove_file(part)?;
                Err(HubError::HashMismatch { file: f.path.clone(), expected: f.sha256.clone(), actual: sha })
            };
        }
        let url = format!("{}/{}/resolve/{}/{}", self.hub.base, req.repo_id, req.revision, f.path);
        let resp = self.hub.get(&url, (have > 0).then_some(have))?;
        let status = resp.status().as_u16();
        let mut out = match (status, have) {
            (206, n) if n > 0 => {
                let expected = format!("bytes {n}-{}/{}", f.bytes - 1, f.bytes);
                let got = resp.headers().get("content-range").and_then(|h| h.to_str().ok()).unwrap_or("");
                if got != expected {
                    return Err(HubError::Invalid(format!("resume answered {got:?}, expected {expected:?}")));
                }
                // Hash what is already staged, then append.
                let mut existing = File::open(part)?;
                let mut buf = vec![0u8; 1 << 20];
                loop {
                    let k = existing.read(&mut buf)?;
                    if k == 0 {
                        break;
                    }
                    hasher.update(&buf[..k]);
                }
                resumed = true;
                done = n;
                OpenOptions::new().append(true).open(part)?
            }
            // No range support (or nothing staged): start over.
            (200, _) => File::create(part)?,
            (401 | 403, _) => return Err(HubError::Unavailable("the Hub refused the download (gated or private)".into())),
            (s, _) => return Err(HubError::Http(s, format!("downloading {}", f.path))),
        };
        let mut body = resp.into_body().into_reader();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            if cancel.load(Ordering::Acquire) {
                out.sync_all()?;
                return Err(HubError::Cancelled);
            }
            let k = match body.read(&mut buf) {
                Ok(k) => k,
                Err(e) => {
                    out.sync_all()?;
                    return Err(HubError::Network(format!("{}: {e} (staged part kept; it will resume)", f.path)));
                }
            };
            if k == 0 {
                break;
            }
            done += k as u64;
            if done > f.bytes {
                drop(out);
                fs::remove_file(part)?;
                return Err(HubError::Invalid(format!("{} is larger than the {} bytes the Hub published", f.path, f.bytes)));
            }
            out.write_all(&buf[..k])?;
            hasher.update(&buf[..k]);
            progress(&f.path, done, f.bytes);
        }
        out.sync_all()?;
        drop(out);
        if done != f.bytes {
            return Err(HubError::Network(format!("{} ended at {done} of {} bytes (staged part kept; it will resume)", f.path, f.bytes)));
        }
        let actual = format!("{:x}", hasher.finalize());
        if actual != f.sha256 {
            fs::remove_file(part)?;
            return Err(HubError::HashMismatch { file: f.path.clone(), expected: f.sha256.clone(), actual });
        }
        Ok(resumed)
    }

    fn register(&self, req: &InstallRequest, f: &FileLead, dest: &Path, info: &GgufInfo, projector: bool) -> Result<(), HubError> {
        self.db()?.execute(
            "INSERT INTO lu_installed_models(path, repo_id, revision, repo_file, sha256, bytes, architecture, name, context_length, is_projector)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(path) DO UPDATE SET revision = excluded.revision, sha256 = excluded.sha256",
            params![
                key(dest),
                req.repo_id,
                req.revision,
                f.path,
                f.sha256,
                f.bytes as i64,
                info.architecture,
                info.name,
                info.context_length.map(|c| c as i64),
                projector || info.is_projector()
            ],
        )?;
        Ok(())
    }

    /// Installed models (projectors appear as their model's pairing).
    pub fn installed(&self) -> Result<Vec<InstalledModel>, HubError> {
        let db = self.db()?;
        let mut q = db.prepare(
            "SELECT path, repo_id, repo_file, architecture, context_length, projector_path
             FROM lu_installed_models WHERE is_projector = 0 ORDER BY path",
        )?;
        let rows = q.query_map([], |r| {
            Ok(InstalledModel {
                path: PathBuf::from(r.get::<_, String>(0)?),
                repo_id: r.get(1)?,
                repo_file: r.get(2)?,
                architecture: r.get(3)?,
                context_length: r.get::<_, Option<i64>>(4)?.map(|c| c as u64),
                projector: r.get::<_, Option<String>>(5)?.map(PathBuf::from),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The projector the user paired with a model, if any.
    pub fn projector_for(&self, model: &Path) -> Result<Option<PathBuf>, HubError> {
        Ok(self
            .db()?
            .query_row("SELECT projector_path FROM lu_installed_models WHERE path = ?1", params![key(model)], |r| r.get::<_, Option<String>>(0))
            .optional()?
            .flatten()
            .map(PathBuf::from))
    }
}

/// One spelling per path (separators, `.`), so lookups match what was stored.
fn key(path: &Path) -> String {
    path.components().collect::<PathBuf>().to_string_lossy().into_owned()
}

fn sha_file(path: &Path) -> Result<(String, u64), HubError> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        let k = f.read(&mut buf)?;
        if k == 0 {
            break;
        }
        n += k as u64;
        h.update(&buf[..k]);
    }
    Ok((format!("{:x}", h.finalize()), n))
}
