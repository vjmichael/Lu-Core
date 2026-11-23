//! GGUF preflight: is this file a GGUF model, and what does it say it is.
//!
//! Reads the 24-byte header and walks the metadata key/value section with
//! hard limits, never the tensors. GGUF strings are untrusted descriptive
//! text: they are shown and recorded, never acted on. Passing this check
//! does not mean a backend can run the model; that is the engine's own test.
//!
//! Ported from Lu-AI-GUI src-tauri/lu-core/src/gguf_preflight.rs (header
//! rules kept: magic, version 2 or 3, plausible counts) and
//! gguf_metadata_summary.rs (bounded metadata walk), merged into one module
//! and extended with `general.name` and projector (`clip`) detection for
//! pairing a vision projector with its model.

use std::{
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::Path,
};

pub const HEADER_BYTES: u64 = 24;
const MAX_KV: u64 = 65_536;
const MAX_METADATA_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ARRAY: u64 = 10_000_000;
const MAX_KEPT_STRING: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GgufInfo {
    pub version: u32,
    pub tensor_count: u64,
    pub metadata_count: u64,
    pub file_bytes: u64,
    /// `general.architecture`, e.g. `qwen3`, `gemma3`, `clip`.
    pub architecture: Option<String>,
    /// `general.name`, as the file describes itself (untrusted text).
    pub name: Option<String>,
    /// `<arch>.context_length`.
    pub context_length: Option<u64>,
    /// `general.file_type` (the quantization as llama.cpp numbers it).
    pub file_type: Option<u32>,
    /// `split.count` for a model split across several files.
    pub split_count: Option<u32>,
}
impl GgufInfo {
    /// A vision/audio projector (mmproj), not a model on its own.
    pub fn is_projector(&self) -> bool {
        self.architecture.as_deref() == Some("clip")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GgufError {
    NotGguf,
    UnsupportedVersion(u32),
    TooShort(u64),
    Implausible(String),
    Io(String),
}
impl From<io::Error> for GgufError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Self::Implausible("the metadata runs past the end of the file".into())
        } else {
            Self::Io(e.to_string())
        }
    }
}
impl std::fmt::Display for GgufError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotGguf => write!(f, "not a GGUF file"),
            Self::UnsupportedVersion(v) => write!(f, "GGUF version {v} is not supported (2 or 3)"),
            Self::TooShort(n) => write!(f, "{n} bytes is too short for a GGUF file"),
            Self::Implausible(w) => write!(f, "damaged GGUF: {w}"),
            Self::Io(w) => write!(f, "{w}"),
        }
    }
}
impl std::error::Error for GgufError {}

struct Walker<R> {
    r: R,
    pos: u64,
    end: u64,
}
impl<R: Read + Seek> Walker<R> {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N], GgufError> {
        let mut b = [0u8; N];
        self.r.read_exact(&mut b)?;
        self.pos += N as u64;
        Ok(b)
    }
    fn u32(&mut self) -> Result<u32, GgufError> {
        Ok(u32::from_le_bytes(self.bytes()?))
    }
    fn u64(&mut self) -> Result<u64, GgufError> {
        Ok(u64::from_le_bytes(self.bytes()?))
    }
    fn skip(&mut self, n: u64) -> Result<(), GgufError> {
        let to = self.pos.checked_add(n).filter(|t| *t <= self.end).ok_or_else(|| GgufError::Implausible("a value runs past the metadata limit".into()))?;
        self.r.seek(SeekFrom::Start(to))?;
        self.pos = to;
        Ok(())
    }
    fn string(&mut self) -> Result<Option<String>, GgufError> {
        let n = self.u64()?;
        if n > MAX_KEPT_STRING {
            self.skip(n)?;
            return Ok(None);
        }
        if self.pos + n > self.end {
            return Err(GgufError::Implausible("a string runs past the metadata limit".into()));
        }
        let mut b = vec![0u8; n as usize];
        self.r.read_exact(&mut b)?;
        self.pos += n;
        Ok(String::from_utf8(b).ok())
    }
    fn number(&mut self, kind: u32) -> Result<Option<u64>, GgufError> {
        Ok(match kind {
            0 => Some(u64::from(self.bytes::<1>()?[0])),
            2 => Some(u64::from(u16::from_le_bytes(self.bytes()?))),
            4 => Some(u64::from(self.u32()?)),
            5 => Some(i32::from_le_bytes(self.bytes()?).max(0) as u64),
            10 => Some(self.u64()?),
            11 => Some(i64::from_le_bytes(self.bytes()?).max(0) as u64),
            _ => {
                self.skip_value(kind)?;
                None
            }
        })
    }
    fn skip_value(&mut self, kind: u32) -> Result<(), GgufError> {
        if let Some(w) = width(kind) {
            return self.skip(w);
        }
        match kind {
            8 => {
                let n = self.u64()?;
                self.skip(n)
            }
            9 => {
                let element = self.u32()?;
                let count = self.u64()?;
                if count > MAX_ARRAY {
                    return Err(GgufError::Implausible("an array is too long".into()));
                }
                if let Some(w) = width(element) {
                    self.skip(count.checked_mul(w).ok_or_else(|| GgufError::Implausible("array size overflows".into()))?)
                } else if element == 8 {
                    for _ in 0..count {
                        let n = self.u64()?;
                        self.skip(n)?;
                    }
                    Ok(())
                } else {
                    Err(GgufError::Implausible(format!("array of type {element}")))
                }
            }
            other => Err(GgufError::Implausible(format!("unknown value type {other}"))),
        }
    }
}
fn width(kind: u32) -> Option<u64> {
    match kind {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4..=6 => Some(4),
        10..=12 => Some(8),
        _ => None,
    }
}

/// Check the header and read the metadata Lu needs.
pub fn inspect(path: &Path) -> Result<GgufInfo, GgufError> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    if len < HEADER_BYTES {
        return Err(GgufError::TooShort(len));
    }
    let mut w = Walker { r: BufReader::new(file), pos: 0, end: len.min(MAX_METADATA_BYTES) };
    if &w.bytes::<4>()? != b"GGUF" {
        return Err(GgufError::NotGguf);
    }
    let version = w.u32()?;
    if version != 2 && version != 3 {
        return Err(GgufError::UnsupportedVersion(version));
    }
    let tensor_count = w.u64()?;
    let metadata_count = w.u64()?;
    if tensor_count == 0 || tensor_count > 100_000_000 || metadata_count > MAX_KV {
        return Err(GgufError::Implausible(format!("{tensor_count} tensors, {metadata_count} metadata entries")));
    }
    let mut info = GgufInfo { version, tensor_count, metadata_count, file_bytes: len, ..GgufInfo::default() };
    let mut contexts: Vec<(String, u64)> = Vec::new();
    for _ in 0..metadata_count {
        let key = w.string()?.unwrap_or_default();
        let kind = w.u32()?;
        match (key.as_str(), kind) {
            ("general.architecture", 8) => info.architecture = w.string()?,
            ("general.name", 8) => info.name = w.string()?,
            ("general.file_type", _) => info.file_type = w.number(kind)?.map(|v| v as u32),
            ("split.count", _) => info.split_count = w.number(kind)?.map(|v| v as u32),
            (k, _) if k.ends_with(".context_length") => {
                if let Some(v) = w.number(kind)? {
                    contexts.push((k.trim_end_matches(".context_length").to_owned(), v));
                }
            }
            _ => w.skip_value(kind)?,
        }
    }
    // The context length that belongs to the declared architecture.
    info.context_length = contexts
        .iter()
        .find(|(arch, _)| Some(arch.as_str()) == info.architecture.as_deref())
        .or(contexts.first())
        .map(|(_, v)| *v);
    Ok(info)
}

/// Build a minimal GGUF file (for tests and fixtures).
pub fn minimal(architecture: &str, name: &str, context_length: u64, padding: usize) -> Vec<u8> {
    let mut b = b"GGUF".to_vec();
    b.extend(3u32.to_le_bytes());
    b.extend(1u64.to_le_bytes()); // tensors
    b.extend(3u64.to_le_bytes()); // metadata entries
    let s = |b: &mut Vec<u8>, v: &str| {
        b.extend((v.len() as u64).to_le_bytes());
        b.extend(v.as_bytes());
    };
    s(&mut b, "general.architecture");
    b.extend(8u32.to_le_bytes());
    s(&mut b, architecture);
    s(&mut b, "general.name");
    b.extend(8u32.to_le_bytes());
    s(&mut b, name);
    s(&mut b, &format!("{architecture}.context_length"));
    b.extend(4u32.to_le_bytes());
    b.extend((context_length as u32).to_le_bytes());
    b.resize(b.len() + padding, 0);
    b
}
