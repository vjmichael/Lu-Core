//! File access, one of the basic tools: universal read, universal write, and
//! list / inspect / copy / move / trash.
//!
//! Lu, not the model, enforces the write discipline:
//! 1. The new content is written to a temporary file in the same folder.
//! 2. It is read back and checked: exact size and SHA-256 of what was asked
//!    for, the file's hash still matching what the model last saw (when
//!    given), and, when the request names the lines it means to change,
//!    that no other line changed.
//! 3. Only then is the original snapshotted and the temporary file swapped
//!    into place with an atomic rename. The result is hashed again.
//!
//! A failed check leaves the original untouched and the effect FAILED (it
//! never happened). Every operation, reads included, is recorded through
//! the [`Recorder`] against the user turn that asked for it.
//!
//! Writes outside the working scope are held, not performed. Reads anywhere
//! are allowed. Deletes go to Lu's recoverable trash, never a hard delete.
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use sha2::{Digest, Sha256};

use crate::ansf_operations::{CommandFinish, CommandStart, FileChange};
use crate::recorder::{RecordError, Recorder};
use crate::origin::{path_text, Decision, EffectKind, OriginError, OriginGate, Proposal};
use crate::archive::{self, ArchiveError, ArchiveLimits, ExtractReport, Listing};
use crate::origin::{Source, SourceKind};
use crate::reader::{self, Extracted, ReaderError};
use crate::snapshot::{SnapshotError, SnapshotLimits, SnapshotStore, TrashEntry};

/// Largest content accepted in one write.
pub const MAX_WRITE_BYTES: usize = 64 * 1024 * 1024;
/// Largest text returned whole by `read`; bigger files are read by lines.
pub const MAX_READ_TEXT_BYTES: u64 = 4 * 1024 * 1024;
/// Bytes shown in a binary file's hex preview.
pub const HEX_PREVIEW_BYTES: usize = 256;
const MAX_LIST: usize = 2000;

/// The user turn an operation serves, and its unique call ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub call_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Text(String),
    /// Binary content as standard base64.
    Base64(String),
    /// Binary content as hexadecimal (whitespace ignored).
    Hex(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteMode {
    /// Fails if the file exists.
    CreateNew,
    /// Fails if the file is missing. With `expected_sha256`, also fails if the
    /// file changed since the model read it.
    Replace { expected_sha256: Option<String> },
    CreateOrReplace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRequest {
    pub path: PathBuf,
    pub content: Content,
    pub mode: WriteMode,
    /// Original line numbers (1-based, inclusive) the write is meant to
    /// change. Every other line must come out identical, or the write is
    /// refused. This catches an "edit" that truncates or garbles the file.
    pub intended_lines: Option<(usize, usize)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteReceipt {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
    pub sha256_before: Option<String>,
    /// Snapshot holding the previous content, e.g. `snap:12`.
    pub snapshot: Option<String>,
    /// False when the previous content was over the copy limit.
    pub undo_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadResult {
    Text { content: String, size: u64, sha256: String, lines: usize },
    /// A document (PDF, Word, Excel, PowerPoint, OpenDocument, EPUB, RTF):
    /// its extracted text. `sections` counts pages, sheets, slides or
    /// chapters; one of them can be read alone with `read_document`.
    Document { format: &'static str, text: String, truncated: bool, sections: usize, size: u64, sha256: String },
    /// A binary file: its likely type, size, hash and the first bytes in hex.
    /// `note` says why a recognized document could not be read as text.
    Binary { kind: &'static str, size: u64, sha256: String, hex_preview: String, note: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileError {
    /// Not done. Outside the working scope, or traced to untrusted content
    /// (then `held_id` names the held action and `notify_user` is true the
    /// first time that content caused a hold).
    Held { path: PathBuf, reason: String, held_id: Option<i64>, notify_user: bool },
    /// Never done: a send traced to untrusted content.
    Refused { reason: String },
    NotFound,
    AlreadyExists,
    /// The file changed since the model read it.
    Stale { expected: String, actual: String },
    /// A staged check failed; the original is untouched.
    CheckFailed(String),
    /// A text file too big to return whole; read it by line range.
    TooLarge { size: u64 },
    Invalid(String),
    Io(String),
    Snapshot(SnapshotError),
    Record(RecordError),
    Origin(OriginError),
    /// A document could not be read (encrypted, no text layer, invalid, ...).
    Document(ReaderError),
    /// An archive could not be listed or extracted; nothing was left behind.
    Archive(ArchiveError),
}
impl From<OriginError> for FileError {
    fn from(e: OriginError) -> Self {
        Self::Origin(e)
    }
}
impl From<std::io::Error> for FileError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::NotFound {
            Self::NotFound
        } else {
            Self::Io(e.to_string())
        }
    }
}
impl From<SnapshotError> for FileError {
    fn from(e: SnapshotError) -> Self {
        Self::Snapshot(e)
    }
}
impl From<RecordError> for FileError {
    fn from(e: RecordError) -> Self {
        Self::Record(e)
    }
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn sha_file(path: &Path) -> Result<(u64, String), FileError> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        size += n as u64;
    }
    Ok((size, format!("{:x}", h.finalize())))
}

/// Resolve an absolute path without requiring it to exist: the longest
/// existing ancestor is canonicalized, the rest must be plain names.
fn resolve(path: &Path) -> Result<PathBuf, FileError> {
    if !path.is_absolute() {
        return Err(FileError::Invalid("path must be absolute".into()));
    }
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        let name = existing.file_name().ok_or(FileError::Invalid("no existing ancestor".into()))?;
        rest.push(name.to_owned());
        existing = existing.parent().ok_or(FileError::Invalid("no existing ancestor".into()))?.to_path_buf();
    }
    let mut out = plain(fs::canonicalize(&existing)?);
    for name in rest.iter().rev() {
        let p = Path::new(name);
        if p.components().count() != 1 || !matches!(p.components().next(), Some(Component::Normal(_))) {
            return Err(FileError::Invalid("path has . or .. parts".into()));
        }
        out.push(name);
    }
    Ok(out)
}
/// Drop Windows' verbatim `\\?\` prefix for ordinary drive paths.
fn plain(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => path,
    }
}

/// The folders the user's instruction covers: the project folder plus any
/// folder the instruction names. A DEFAULT PENDING THE OWNER'S APPROVAL.
#[derive(Debug, Clone)]
pub struct WorkingScope {
    roots: Vec<PathBuf>,
}
impl WorkingScope {
    pub fn new(roots: &[PathBuf]) -> Result<Self, FileError> {
        let roots = roots.iter().map(|r| resolve(r)).collect::<Result<Vec<_>, _>>()?;
        Ok(Self { roots })
    }
    pub fn contains(&self, path: &Path) -> bool {
        self.roots.iter().any(|r| path.starts_with(r))
    }
}

fn decode(content: &Content) -> Result<Vec<u8>, FileError> {
    let bytes = match content {
        Content::Text(t) => t.as_bytes().to_vec(),
        Content::Base64(b) => STANDARD
            .decode(b.split_whitespace().collect::<String>())
            .map_err(|e| FileError::Invalid(format!("base64: {e}")))?,
        Content::Hex(h) => {
            let digits: Vec<u8> = h.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
            if !digits.len().is_multiple_of(2) {
                return Err(FileError::Invalid("hex: odd number of digits".into()));
            }
            digits
                .chunks(2)
                .map(|pair| {
                    std::str::from_utf8(pair)
                        .ok()
                        .and_then(|s| u8::from_str_radix(s, 16).ok())
                        .ok_or(FileError::Invalid("hex: not a hex digit".into()))
                })
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    if bytes.len() > MAX_WRITE_BYTES {
        return Err(FileError::Invalid("content over the write limit".into()));
    }
    Ok(bytes)
}

/// Lines with their endings, so joining them gives back the exact bytes.
fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// Check that only original lines `first..=last` differ between the two
/// texts. Returns a message naming the first unexpected difference.
fn check_intended_lines(old: &[u8], new: &[u8], first: usize, last: usize) -> Result<(), String> {
    let (Ok(old), Ok(new)) = (std::str::from_utf8(old), std::str::from_utf8(new)) else {
        return Err("intended lines need text files".into());
    };
    let (o, n) = (lines(old), lines(new));
    if first == 0 || last < first || last > o.len().max(1) {
        return Err(format!("line range {first}..={last} is outside a {}-line file", o.len()));
    }
    let prefix = first - 1;
    let suffix = o.len().saturating_sub(last);
    if n.len() < prefix + suffix {
        return Err(format!(
            "the new text has {} lines, too few to keep the {} lines outside {first}..={last}",
            n.len(),
            prefix + suffix
        ));
    }
    if let Some(i) = (0..prefix).find(|&i| o[i] != n[i]) {
        return Err(format!("line {} changed but is outside {first}..={last}", i + 1));
    }
    for k in 0..suffix {
        let (oi, ni) = (o.len() - suffix + k, n.len() - suffix + k);
        if o[oi] != n[ni] {
            return Err(format!("line {} changed but is outside {first}..={last}", oi + 1));
        }
    }
    Ok(())
}

/// Likely type from the first bytes.
fn sniff(head: &[u8], path: &Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    match head {
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [b'G', b'I', b'F', b'8', ..] => "image/gif",
        [b'%', b'P', b'D', b'F', ..] => "application/pdf",
        [0x1F, 0x8B, ..] => "application/gzip",
        [b'M', b'Z', ..] => "application/x-msdownload",
        [0x7F, b'E', b'L', b'F', ..] => "application/x-elf",
        [b'P', b'K', 3, 4, ..] => match ext.as_deref() {
            Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            Some("epub") => "application/epub+zip",
            Some("odt") => "application/vnd.oasis.opendocument.text",
            _ => "application/zip",
        },
        _ => "application/octet-stream",
    }
}

fn unique_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos())
    )
}

pub struct FileAccess<'a> {
    recorder: &'a Recorder<'a>,
    snapshots: &'a SnapshotStore,
    gate: &'a OriginGate,
    scope: WorkingScope,
    limits: SnapshotLimits,
}

impl<'a> FileAccess<'a> {
    pub fn new(
        recorder: &'a Recorder<'a>,
        snapshots: &'a SnapshotStore,
        gate: &'a OriginGate,
        scope: WorkingScope,
        limits: SnapshotLimits,
    ) -> Self {
        Self { recorder, snapshots, gate, scope, limits }
    }

    /// Ask the origin gate whether this action runs.
    fn allowed(&self, call: &Call, effect: EffectKind, what: &str, path: &Path) -> Result<(), FileError> {
        let proposal = Proposal {
            session_id: self.recorder.session_id().to_owned(),
            turn_id: call.turn_id.clone(),
            call_id: call.call_id.clone(),
            effect,
            text: path_text(what, path),
        };
        match self.gate.decide(self.recorder.book(), &proposal)? {
            Decision::Run => Ok(()),
            Decision::Hold { held_id, source, notify } => Err(FileError::Held {
                path: path.to_path_buf(),
                reason: format!("asked for by untrusted content: {:?} {}", source.kind, source.reference),
                held_id: Some(held_id),
                notify_user: notify,
            }),
            Decision::Refuse { source } => Err(FileError::Refused {
                reason: format!("a send asked for by untrusted content: {:?} {}", source.kind, source.reference),
            }),
        }
    }

    fn instruction(&self, call: &Call) -> Result<String, FileError> {
        Ok(self
            .recorder
            .book()
            .authenticated_chat_turn(self.recorder.session_id(), &call.turn_id)
            .map_err(|e| FileError::Origin(OriginError::Ansf(e)))?
            .map(|(text, _)| text)
            .unwrap_or_default())
    }

    /// A writable target: inside the working scope and allowed by the gate.
    fn writable_for(&self, call: &Call, effect: EffectKind, what: &str, path: &Path) -> Result<PathBuf, FileError> {
        let target = self.writable(path)?;
        self.allowed(call, effect, what, &target)?;
        Ok(target)
    }

    fn writable(&self, path: &Path) -> Result<PathBuf, FileError> {
        let target = resolve(path)?;
        if !self.scope.contains(&target) {
            return Err(FileError::Held {
                path: target,
                reason: "outside the working scope; the user's instruction did not name it".into(),
                held_id: None,
                notify_user: true,
            });
        }
        Ok(target)
    }

    /// Record the start of an operation, run it, and record its outcome.
    /// An Err from `op` means no effect happened (journal FAILED).
    fn recorded<T>(
        &self,
        call: &Call,
        what: &str,
        path: &Path,
        op: impl FnOnce() -> Result<(T, String), FileError>,
    ) -> Result<T, FileError> {
        let cwd = path.parent().unwrap_or(path).to_string_lossy().into_owned();
        self.recorder.begin_command(&CommandStart {
            call_id: call.call_id.clone(),
            turn_id: call.turn_id.clone(),
            command: format!("{what} \"{}\"", path.display()),
            cwd,
        })?;
        let started = SystemTime::now();
        let result = op();
        let duration_ms = started.elapsed().map_or(0, |d| d.as_millis() as u64);
        let (exit_code, output) = match &result {
            Ok((_, summary)) => (Some(0), summary.clone()),
            Err(e) => (None, format!("{e:?}")),
        };
        self.recorder.finish_command(&CommandFinish {
            call_id: call.call_id.clone(),
            exit_code,
            timed_out: false,
            cancelled: false,
            duration_ms,
            output,
        })?;
        result.map(|(value, _)| value)
    }

    fn changed(&self, call: &Call, path: &Path, before: Option<String>, after: Option<String>, snapshot: Option<String>)
        -> Result<(), FileError> {
        self.recorder.file_changed(&FileChange {
            call_id: call.call_id.clone(),
            path: path.to_string_lossy().into_owned(),
            sha256_before: before,
            sha256_after: after,
            snapshot_ref: snapshot,
        })?;
        Ok(())
    }

    /// Read any file: text as it is, binary as type, size, hash and preview.
    pub fn read(&self, call: &Call, path: &Path) -> Result<ReadResult, FileError> {
        let target = resolve(path)?;
        self.recorded(call, "lu.files.read", &target, || {
            let size = fs::metadata(&target)?.len();
            let mut head = vec![0u8; 8192.min(size as usize)];
            File::open(&target)?.read_exact(&mut head)?;
            let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            // A document's signature wins over looking like text: RTF is
            // ASCII, and its words matter, not its control codes.
            let document = reader::classify(&name, &head).is_some();
            let looks_text = !document && !head.contains(&0) && std::str::from_utf8(&head).is_ok_or_cut();
            if looks_text {
                if size > MAX_READ_TEXT_BYTES {
                    return Err(FileError::TooLarge { size });
                }
                let bytes = fs::read(&target)?;
                if let Ok(content) = String::from_utf8(bytes) {
                    // An outside file's text is untrusted content: this turn
                    // may no longer run anything, and copies are traced.
                    if let Some(source) = self.gate.external_file(&target)? {
                        self.gate.register_content(self.recorder.session_id(), &call.turn_id, &source, &content)?;
                    }
                    let digest = sha(content.as_bytes());
                    let summary = format!("text {size} bytes sha256={digest}");
                    let lines = lines(&content).len();
                    return Ok((ReadResult::Text { content, size, sha256: digest, lines }, summary));
                }
            }
            let (_, digest) = sha_file(&target)?;
            let mut note = None;
            if document {
                match self.extract(call, &target, size, None, reader::DEFAULT_MAX_CHARS) {
                    Ok(doc) => {
                        let summary = format!("document {} {size} bytes sha256={digest}", doc.format.as_str());
                        let result = ReadResult::Document {
                            format: doc.format.as_str(),
                            text: doc.text,
                            truncated: doc.truncated,
                            sections: doc.total_sections,
                            size,
                            sha256: digest,
                        };
                        return Ok((result, summary));
                    }
                    Err(FileError::Document(e)) => note = Some(e.to_string()),
                    Err(e) => return Err(e),
                }
            }
            let kind = sniff(&head, &target);
            let preview: String = head.iter().take(HEX_PREVIEW_BYTES).map(|b| format!("{b:02x}")).collect();
            let summary = format!("binary {kind} {size} bytes sha256={digest}");
            Ok((ReadResult::Binary { kind, size, sha256: digest, hex_preview: preview, note }, summary))
        })
    }

    /// Extract a document's text and register it with the origin gate when
    /// the file came from outside the PC: its words are data, never orders.
    fn extract(&self, call: &Call, target: &Path, size: u64, selector: Option<&str>, max_chars: usize) -> Result<Extracted, FileError> {
        if size > reader::MAX_DOCUMENT_BYTES {
            return Err(FileError::Document(ReaderError::TooLarge { size }));
        }
        let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let doc = reader::extract(&name, &fs::read(target)?, selector, max_chars).map_err(FileError::Document)?;
        if let Some(source) = self.gate.external_file(target)? {
            self.gate.register_content(self.recorder.session_id(), &call.turn_id, &source, &doc.text)?;
        }
        Ok(doc)
    }

    /// Read one page, sheet, slide (or its notes) or chapter of a document:
    /// `page:3`, `sheet:Budget`, `slide:2`, `notes:2`, `chapter:4`; or the
    /// whole document with no selector. This is the "Read documents"
    /// utility; the basic read already returns a document's full text.
    pub fn read_document(&self, call: &Call, path: &Path, selector: Option<&str>, max_chars: usize) -> Result<Extracted, FileError> {
        let target = resolve(path)?;
        self.recorded(call, "lu.files.read_document", &target, || {
            let size = fs::metadata(&target)?.len();
            let doc = self.extract(call, &target, size, selector, max_chars)?;
            let summary = format!("document {} {} sections, {} chars{}", doc.format.as_str(), doc.total_sections, doc.text.chars().count(), selector.map(|s| format!(" ({s})")).unwrap_or_default());
            Ok((doc, summary))
        })
    }

    /// List an archive (zip, tar, tar.gz/.xz/.bz2, gz, 7z, iso) without
    /// extracting it. Names from an outside archive are untrusted content.
    pub fn list_archive(&self, call: &Call, path: &Path) -> Result<Listing, FileError> {
        let target = resolve(path)?;
        self.recorded(call, "lu.files.list_archive", &target, || {
            let listing = archive::list(&target).map_err(FileError::Archive)?;
            if let Some(source) = self.gate.external_file(&target)? {
                let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
                self.gate.register_content(self.recorder.session_id(), &call.turn_id, &source, &names.join("
"))?;
            }
            let summary = format!(
                "{} archive, {} entries, {} bytes, {} unsafe",
                listing.kind.as_str(),
                listing.total_entries,
                listing.total_bytes,
                listing.unsafe_entries.len()
            );
            Ok((listing, summary))
        })
    }

    /// Extract an archive into `destination`, a new folder inside the
    /// working scope. Unsafe entries are skipped and reported; a failure or
    /// limit leaves nothing behind. Files extracted from an outside archive
    /// are themselves outside files: reading them cannot authorize anything.
    pub fn extract_archive(&self, call: &Call, path: &Path, destination: &Path, limits: &ArchiveLimits) -> Result<ExtractReport, FileError> {
        let source_path = resolve(path)?;
        let target = self.writable_for(call, EffectKind::Write, "lu.files.extract_archive", destination)?;
        self.recorded(call, "lu.files.extract_archive", &target, || {
            let report = archive::extract(&source_path, &target, limits).map_err(|e| match e {
                ArchiveError::DestinationExists(_) => FileError::AlreadyExists,
                other => FileError::Archive(other),
            })?;
            if let Some(outer) = self.gate.external_file(&source_path)? {
                let source = Source {
                    kind: SourceKind::ExternalFile,
                    reference: format!("extracted from {} ({})", source_path.display(), outer.reference),
                };
                for rel in &report.written {
                    let file = rel.split('/').fold(target.clone(), |p, c| p.join(c));
                    self.gate.mark_external_file(&file, &source)?;
                }
            }
            let summary = format!(
                "{} archive: {} files, {} folders, {} bytes, {} skipped",
                report.kind.as_str(),
                report.files,
                report.dirs,
                report.bytes,
                report.skipped.len()
            );
            Ok((report, summary))
        })
    }

    /// Lines `first..=last` (1-based) of a text file of any size.
    pub fn read_lines(&self, call: &Call, path: &Path, first: usize, last: usize) -> Result<String, FileError> {
        let target = resolve(path)?;
        if first == 0 || last < first || last - first >= 100_000 {
            return Err(FileError::Invalid("bad line range".into()));
        }
        self.recorded(call, "lu.files.read_lines", &target, || {
            use std::io::BufRead;
            let reader = std::io::BufReader::new(File::open(&target)?);
            let mut out = String::new();
            for (i, line) in reader.split(b'\n').enumerate() {
                let n = i + 1;
                if n > last {
                    break;
                }
                let line = line?;
                if n >= first {
                    out.push_str(std::str::from_utf8(&line).map_err(|_| FileError::Invalid("not text".into()))?);
                    out.push('\n');
                }
            }
            let summary = format!("lines {first}..={last}, {} bytes", out.len());
            Ok((out, summary))
        })
    }

    /// Write content to a path through a staged, checked, atomic swap.
    pub fn write(&self, call: &Call, request: &WriteRequest) -> Result<WriteReceipt, FileError> {
        let target = self.writable_for(call, EffectKind::Write, "lu.files.write", &request.path)?;
        let bytes = decode(&request.content)?;
        self.recorded(call, "lu.files.write", &target, || {
            let exists = target.is_file();
            if target.exists() && !exists {
                return Err(FileError::Invalid("the path is a folder".into()));
            }
            match (&request.mode, exists) {
                (WriteMode::CreateNew, true) => return Err(FileError::AlreadyExists),
                (WriteMode::Replace { .. }, false) => return Err(FileError::NotFound),
                _ => {}
            }
            let old = if exists { Some(fs::read(&target)?) } else { None };
            let before = old.as_deref().map(sha);
            if let (WriteMode::Replace { expected_sha256: Some(expected) }, Some(actual)) = (&request.mode, &before) {
                if expected != actual {
                    return Err(FileError::Stale { expected: expected.clone(), actual: actual.clone() });
                }
            }
            if let Some((first, last)) = request.intended_lines {
                let old = old.as_deref().ok_or(FileError::Invalid("intended lines need an existing file".into()))?;
                check_intended_lines(old, &bytes, first, last).map_err(FileError::CheckFailed)?;
            }
            let receipt = self.stage_and_swap(call, &target, &bytes, before)?;
            // Origin travels with content: text copied from untrusted
            // content marks the written file as external.
            if let Ok(text) = std::str::from_utf8(&bytes) {
                self.gate.propagate_to_written_file(self.recorder.session_id(), &self.instruction(call)?, &target, text)?;
            }
            let summary = format!("wrote {} bytes sha256={}", receipt.size, receipt.sha256);
            Ok((receipt, summary))
        })
    }

    /// Replace original lines `first..=last` (1-based) with `replacement`.
    /// `replacement` should end with a newline unless it is the file's end.
    pub fn edit_lines(
        &self,
        call: &Call,
        path: &Path,
        first: usize,
        last: usize,
        replacement: &str,
        expected_sha256: Option<String>,
    ) -> Result<WriteReceipt, FileError> {
        let target = self.writable_for(call, EffectKind::Write, "lu.files.write", path)?;
        let old = fs::read_to_string(&target).map_err(|_| FileError::Invalid("not a readable text file".into()))?;
        let o = lines(&old);
        if first == 0 || last < first || last > o.len() {
            return Err(FileError::Invalid(format!("line range {first}..={last} outside a {}-line file", o.len())));
        }
        let new = [o[..first - 1].concat(), replacement.to_owned(), o[last..].concat()].concat();
        self.write(
            call,
            &WriteRequest {
                path: target,
                content: Content::Text(new),
                mode: WriteMode::Replace { expected_sha256: expected_sha256.or_else(|| Some(sha(old.as_bytes()))) },
                intended_lines: Some((first, last)),
            },
        )
    }

    /// Steps 1-3 of the write discipline for content that passed its checks.
    fn stage_and_swap(&self, call: &Call, target: &Path, bytes: &[u8], before: Option<String>)
        -> Result<WriteReceipt, FileError> {
        let dir = target.parent().ok_or(FileError::Invalid("no parent folder".into()))?;
        fs::create_dir_all(dir)?;
        let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let temp = dir.join(format!(".lu-stage-{}-{name}", unique_suffix()));
        let intended = sha(bytes);
        let staged = (|| -> Result<(), FileError> {
            let mut f = File::create(&temp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            drop(f);
            let (size, digest) = sha_file(&temp)?;
            if size != bytes.len() as u64 || digest != intended {
                return Err(FileError::CheckFailed("the staged file does not match the requested content".into()));
            }
            Ok(())
        })();
        if let Err(e) = staged {
            let _ = fs::remove_file(&temp);
            return Err(e);
        }
        let snapshot = if before.is_some() {
            match self.snapshots.take_file(target, self.limits) {
                Ok(s) => Some(s),
                Err(e) => {
                    let _ = fs::remove_file(&temp);
                    return Err(e.into());
                }
            }
        } else {
            None
        };
        let undo_available = snapshot.as_ref().is_none_or(|s| s.not_restorable == 0);
        if let Err(e) = fs::rename(&temp, target) {
            let _ = fs::remove_file(&temp);
            return Err(e.into());
        }
        let (size, digest) = sha_file(target)?;
        let reference = snapshot.as_ref().map(|s| s.reference());
        self.changed(call, target, before.clone(), Some(digest.clone()), reference.clone())?;
        if digest != intended {
            if let Some(s) = &snapshot {
                let name = target.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                self.snapshots.restore(s.id, name)?;
            }
            return Err(FileError::CheckFailed("the file differs after the swap; the original was restored".into()));
        }
        Ok(WriteReceipt { path: target.to_path_buf(), size, sha256: digest, sha256_before: before, snapshot: reference, undo_available })
    }

    /// Copy a file. The copy is staged and checked against the source hash.
    pub fn copy(&self, call: &Call, from: &Path, to: &Path, overwrite: bool) -> Result<WriteReceipt, FileError> {
        let source = resolve(from)?;
        let target = self.writable_for(call, EffectKind::Write, "lu.files.copy", to)?;
        self.recorded(call, "lu.files.copy", &target, || {
            if target.exists() && !overwrite {
                return Err(FileError::AlreadyExists);
            }
            let bytes = fs::read(&source)?;
            let before = if target.is_file() { Some(sha_file(&target)?.1) } else { None };
            let receipt = self.stage_and_swap(call, &target, &bytes, before)?;
            let summary = format!("copied {} bytes from {}", receipt.size, source.display());
            Ok((receipt, summary))
        })
    }

    /// Move a file within the working scope. Never overwrites.
    pub fn move_file(&self, call: &Call, from: &Path, to: &Path) -> Result<PathBuf, FileError> {
        let source = self.writable_for(call, EffectKind::Write, "lu.files.move", from)?;
        let target = self.writable_for(call, EffectKind::Write, "lu.files.move", to)?;
        self.recorded(call, "lu.files.move", &source, || {
            if target.exists() {
                return Err(FileError::AlreadyExists);
            }
            if !source.is_file() {
                return Err(FileError::NotFound);
            }
            let (_, digest) = sha_file(&source)?;
            if let Some(dir) = target.parent() {
                fs::create_dir_all(dir)?;
            }
            if fs::rename(&source, &target).is_err() {
                // Different volume: copy, check, then remove the source.
                fs::copy(&source, &target)?;
                if sha_file(&target)?.1 != digest {
                    let _ = fs::remove_file(&target);
                    return Err(FileError::CheckFailed("the copy did not match the source".into()));
                }
                fs::remove_file(&source)?;
            }
            self.changed(call, &source, Some(digest.clone()), None, None)?;
            self.changed(call, &target, None, Some(digest), None)?;
            Ok((target.clone(), format!("moved to {}", target.display())))
        })
    }

    /// Delete a file by moving it to Lu's recoverable trash.
    pub fn trash(&self, call: &Call, path: &Path) -> Result<TrashEntry, FileError> {
        let target = self.writable_for(call, EffectKind::Delete, "lu.files.trash", path)?;
        self.recorded(call, "lu.files.trash", &target, || {
            let entry = self.snapshots.trash(&target)?;
            self.changed(call, &target, Some(entry.sha256.clone()), None, Some(format!("trash:{}", entry.id)))?;
            let summary = format!("moved to trash as trash:{}", entry.id);
            Ok((entry, summary))
        })
    }

    /// Put a trashed file back. Refused if its old path is taken.
    pub fn restore_from_trash(&self, call: &Call, entry: &TrashEntry) -> Result<PathBuf, FileError> {
        self.writable_for(call, EffectKind::Write, "lu.files.restore_trash", &entry.original_path)?;
        self.recorded(call, "lu.files.restore_trash", &entry.original_path, || {
            let path = self.snapshots.restore_trashed(entry.id)?;
            self.changed(call, &path, None, Some(entry.sha256.clone()), Some(format!("trash:{}", entry.id)))?;
            Ok((path, format!("restored trash:{}", entry.id)))
        })
    }

    /// Entries of a folder, folders first, then by name.
    pub fn list(&self, call: &Call, dir: &Path) -> Result<(Vec<DirEntry>, bool), FileError> {
        let target = resolve(dir)?;
        self.recorded(call, "lu.files.list", &target, || {
            let mut entries = Vec::new();
            for entry in fs::read_dir(&target)? {
                let entry = entry?;
                let meta = entry.metadata()?;
                entries.push(DirEntry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    is_dir: meta.is_dir(),
                    size: if meta.is_dir() { 0 } else { meta.len() },
                });
            }
            entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
            let truncated = entries.len() > MAX_LIST;
            entries.truncate(MAX_LIST);
            let summary = format!("{} entries{}", entries.len(), if truncated { " (truncated)" } else { "" });
            Ok(((entries, truncated), summary))
        })
    }
}

/// `from_utf8` on a prefix fails if the prefix cut a character in half.
/// Accept that case; it is still text.
trait Utf8Prefix {
    fn is_ok_or_cut(&self) -> bool;
}
impl Utf8Prefix for Result<&str, std::str::Utf8Error> {
    fn is_ok_or_cut(&self) -> bool {
        match self {
            Ok(_) => true,
            Err(e) => e.error_len().is_none(),
        }
    }
}
