//! List and extract archives the same way on Windows and Linux.
//!
//! Archive command-line tools differ between systems (Windows `tar.exe`
//! is bsdtar, Linux `tar` is GNU, 7-Zip may or may not be installed), so
//! Lu reads archives itself: zip, tar, tar.gz/.tgz, tar.xz/.txz,
//! tar.bz2/.tbz2, single .gz, 7z and ISO 9660 disc images (with Joliet
//! long names).
//!
//! An archive is a file someone else made, so extraction never trusts its
//! names or sizes:
//! - every entry path is checked: no absolute paths, drive letters, `..`,
//!   Windows device names (CON, NUL, ...), alternate data streams (`:`),
//!   characters Windows cannot store, or two entries that differ only by
//!   case; such entries are skipped and reported, never written elsewhere;
//! - links, devices and other special entries are skipped and reported;
//! - extraction goes into a fresh staging folder next to the destination
//!   and is renamed into place only when every entry is out; a failure
//!   removes the staging folder and leaves nothing half-extracted;
//! - limits stop zip bombs: total bytes, entry count and expansion ratio
//!   are counted while writing, not taken from the headers.
//!
//! Written for Lu-Core. The old repo's archive tools (native_zip_extract*,
//! native_iso9660.rs, native_linux_archive*.rs, native_libarchive.rs) were
//! reviewed, not ported: its ISO reader refused Joliet names, which most
//! real discs use, and its Linux path shelled out to libarchive.
//! Not covered: UDF-only discs (most Windows install ISOs carry ISO 9660 as
//! well; a UDF-only one is reported), encrypted zip/7z, RAR.

use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Zip,
    Tar,
    TarGz,
    TarXz,
    TarBz2,
    Gz,
    SevenZ,
    Iso,
}
impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::Tar => "tar",
            Self::TarGz => "tar.gz",
            Self::TarXz => "tar.xz",
            Self::TarBz2 => "tar.bz2",
            Self::Gz => "gz",
            Self::SevenZ => "7z",
            Self::Iso => "iso",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveLimits {
    /// Bytes written by one extraction.
    pub max_total_bytes: u64,
    pub max_entries: usize,
    /// Bytes written per byte of archive, checked once past 64 MiB.
    pub max_ratio: u64,
}
impl Default for ArchiveLimits {
    fn default() -> Self {
        Self { max_total_bytes: 8 * 1024 * 1024 * 1024, max_entries: 200_000, max_ratio: 250 }
    }
}
const RATIO_FLOOR: u64 = 64 * 1024 * 1024;
/// Entries returned in a listing (the totals still count all of them).
pub const MAX_LISTED: usize = 5_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveError {
    /// Not an archive Lu reads (or a damaged one), with the reason.
    Unsupported(String),
    Invalid(String),
    /// A limit was hit; nothing was left behind.
    Limit(String),
    /// The destination already exists.
    DestinationExists(PathBuf),
    Io(String),
}
impl From<io::Error> for ArchiveError {
    fn from(e: io::Error) -> Self {
        Self::Io(e.to_string())
    }
}
impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(w) => write!(f, "unsupported archive: {w}"),
            Self::Invalid(w) => write!(f, "damaged archive: {w}"),
            Self::Limit(w) => write!(f, "stopped at a limit: {w}"),
            Self::DestinationExists(p) => write!(f, "{} already exists", p.display()),
            Self::Io(w) => write!(f, "{w}"),
        }
    }
}
impl std::error::Error for ArchiveError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    /// Symbolic or hard link: listed, never extracted.
    Link,
    /// Device, fifo or anything else: listed, never extracted.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The name as stored in the archive.
    pub name: String,
    pub kind: EntryKind,
    /// Declared size (for tar and gz, the real size).
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub kind: Kind,
    /// Up to [`MAX_LISTED`] entries.
    pub entries: Vec<Entry>,
    pub total_entries: usize,
    pub total_bytes: u64,
    /// Entries extraction would skip, with the reason.
    pub unsafe_entries: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractReport {
    pub kind: Kind,
    pub destination: PathBuf,
    pub files: usize,
    pub dirs: usize,
    pub bytes: u64,
    /// Entries not written, with the reason.
    pub skipped: Vec<(String, String)>,
    /// Paths written, relative to the destination, with `/`.
    pub written: Vec<String>,
}

/// Which archive this is, from its first bytes and its name.
pub fn detect(name: &str, head: &[u8]) -> Option<Kind> {
    let lower = name.to_ascii_lowercase();
    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        return Some(Kind::Zip);
    }
    if head.starts_with(b"7z\xBC\xAF\x27\x1C") {
        return Some(Kind::SevenZ);
    }
    if head.starts_with(&[0x1F, 0x8B]) {
        return Some(if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") { Kind::TarGz } else { Kind::Gz });
    }
    if head.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0]) {
        return Some(Kind::TarXz).filter(|_| lower.ends_with(".tar.xz") || lower.ends_with(".txz"));
    }
    if head.starts_with(b"BZh") {
        return Some(Kind::TarBz2).filter(|_| lower.ends_with(".tar.bz2") || lower.ends_with(".tbz2") || lower.ends_with(".tbz"));
    }
    if head.len() >= 262 && &head[257..262] == b"ustar" {
        return Some(Kind::Tar);
    }
    if lower.ends_with(".tar") {
        return Some(Kind::Tar);
    }
    if lower.ends_with(".iso") {
        return Some(Kind::Iso);
    }
    None
}

fn open_kind(path: &Path) -> Result<(Kind, u64), ArchiveError> {
    let mut f = File::open(path)?;
    let size = f.metadata()?.len();
    let mut head = vec![0u8; 512.min(size as usize)];
    f.read_exact(&mut head)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    detect(&name, &head)
        .map(|k| (k, size))
        .ok_or_else(|| ArchiveError::Unsupported(format!("{name} is not a zip, tar(.gz/.xz/.bz2), gz, 7z or iso archive")))
}

// ---------------------------------------------------------------- names

const WINDOWS_RESERVED: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2",
    "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// The safe relative path for an entry name, or why there is none. The same
/// rules apply on every system, so an archive extracts the same everywhere.
pub fn safe_relative(name: &str) -> Result<Vec<String>, String> {
    let unified = name.replace('\\', "/");
    if unified.starts_with('/') {
        return Err("absolute path".into());
    }
    let bytes = unified.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        return Err("drive letter".into());
    }
    let mut parts = Vec::new();
    for part in unified.split('/') {
        match part {
            "" | "." => continue,
            ".." => return Err("climbs out with ..".into()),
            p => {
                if p.chars().any(|c| c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*')) {
                    return Err(format!("{p:?} has a character Windows cannot store (or an alternate stream)"));
                }
                if p.ends_with('.') || p.ends_with(' ') {
                    return Err(format!("{p:?} ends in a dot or space"));
                }
                let stem = p.split('.').next().unwrap_or(p).trim_end().to_ascii_lowercase();
                if WINDOWS_RESERVED.contains(&stem.as_str()) {
                    return Err(format!("{p:?} is a Windows device name"));
                }
                if p.len() > 255 {
                    return Err("a name over 255 bytes".into());
                }
                parts.push(p.to_owned());
            }
        }
    }
    if parts.is_empty() {
        return Err("empty path".into());
    }
    Ok(parts)
}

// ---------------------------------------------------------------- reading
// Each format is walked by one function that hands every entry, with a
// reader for its bytes (files only), to a visitor.

type Visit<'a> = dyn FnMut(Entry, Option<&mut dyn Read>) -> Result<(), ArchiveError> + 'a;

fn walk(path: &Path, kind: Kind, visit: &mut Visit<'_>) -> Result<(), ArchiveError> {
    match kind {
        Kind::Zip => walk_zip(path, visit),
        Kind::Tar => walk_tar(BufReader::new(File::open(path)?), visit),
        Kind::TarGz => walk_tar(flate2::read::MultiGzDecoder::new(BufReader::new(File::open(path)?)), visit),
        Kind::TarXz => walk_tar(lzma_rust2::XzReader::new(BufReader::new(File::open(path)?), true), visit),
        Kind::TarBz2 => walk_tar(bzip2::read::MultiBzDecoder::new(BufReader::new(File::open(path)?)), visit),
        Kind::Gz => walk_gz(path, visit),
        Kind::SevenZ => walk_7z(path, visit),
        Kind::Iso => walk_iso(path, visit),
    }
}

fn walk_zip(path: &Path, visit: &mut Visit<'_>) -> Result<(), ArchiveError> {
    let mut z = zip::ZipArchive::new(BufReader::new(File::open(path)?)).map_err(|e| ArchiveError::Invalid(format!("zip: {e}")))?;
    for i in 0..z.len() {
        let mut f = z.by_index(i).map_err(|e| match e {
            zip::result::ZipError::UnsupportedArchive(w) => ArchiveError::Unsupported(format!("zip: {w}")),
            other => ArchiveError::Invalid(format!("zip entry {i}: {other}")),
        })?;
        if f.encrypted() {
            return Err(ArchiveError::Unsupported("the zip is password-protected".into()));
        }
        let name = f.name().map_err(|e| ArchiveError::Invalid(format!("zip entry {i}: {e}")))?.into_owned();
        let kind = if f.is_dir() {
            EntryKind::Dir
        } else if f.is_symlink() {
            EntryKind::Link
        } else {
            EntryKind::File
        };
        let size = f.size();
        if kind == EntryKind::File {
            visit(Entry { name, kind, size }, Some(&mut f))?;
        } else {
            visit(Entry { name, kind, size }, None)?;
        }
    }
    Ok(())
}

fn walk_tar<R: Read>(reader: R, visit: &mut Visit<'_>) -> Result<(), ArchiveError> {
    let mut archive = tar::Archive::new(reader);
    let entries = archive.entries().map_err(|e| ArchiveError::Invalid(format!("tar: {e}")))?;
    for entry in entries {
        let mut e = entry.map_err(|e| ArchiveError::Invalid(format!("tar: {e}")))?;
        let name = String::from_utf8_lossy(&e.path_bytes()).into_owned();
        let size = e.header().size().unwrap_or(0);
        let kind = match e.header().entry_type() {
            tar::EntryType::Regular | tar::EntryType::Continuous => EntryKind::File,
            tar::EntryType::Directory => EntryKind::Dir,
            tar::EntryType::Symlink | tar::EntryType::Link => EntryKind::Link,
            // Metadata records (pax headers, GNU long names) are consumed by
            // the tar crate; anything else is not a plain file.
            tar::EntryType::XGlobalHeader | tar::EntryType::XHeader | tar::EntryType::GNULongName | tar::EntryType::GNULongLink => continue,
            _ => EntryKind::Other,
        };
        if kind == EntryKind::File {
            visit(Entry { name, kind, size }, Some(&mut e))?;
        } else {
            visit(Entry { name, kind, size }, None)?;
        }
    }
    Ok(())
}

fn walk_gz(path: &Path, visit: &mut Visit<'_>) -> Result<(), ArchiveError> {
    let mut gz = flate2::read::MultiGzDecoder::new(BufReader::new(File::open(path)?));
    // The name stored in the gzip header, or the file name without .gz.
    let stored = gz.header().and_then(|h| h.filename()).map(|n| String::from_utf8_lossy(n).into_owned());
    let fallback = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let fallback = fallback.strip_suffix(".gz").or_else(|| fallback.strip_suffix(".GZ")).unwrap_or(&fallback).to_owned();
    let name = stored.filter(|n| !n.is_empty()).map(|n| n.rsplit(['/', '\\']).next().unwrap_or(&n).to_owned()).unwrap_or(fallback);
    visit(Entry { name, kind: EntryKind::File, size: 0 }, Some(&mut gz))
}

fn walk_7z(path: &Path, visit: &mut Visit<'_>) -> Result<(), ArchiveError> {
    let mut reader = sevenz_rust2::ArchiveReader::open(path, sevenz_rust2::Password::empty()).map_err(|e| {
        let why = e.to_string();
        if why.to_ascii_lowercase().contains("password") {
            ArchiveError::Unsupported("the 7z archive is password-protected".into())
        } else {
            ArchiveError::Invalid(format!("7z: {why}"))
        }
    })?;
    // Empty files and folders have no stream; report them first.
    let entries: Vec<_> = reader.archive().files.iter().filter(|f| !f.has_stream).cloned().collect();
    for f in entries {
        if f.is_anti_item {
            continue;
        }
        let kind = if f.is_directory { EntryKind::Dir } else { EntryKind::File };
        let mut empty = io::empty();
        visit(Entry { name: f.name.clone(), kind, size: 0 }, (kind == EntryKind::File).then_some(&mut empty as &mut dyn Read))?;
    }
    let mut failure: Option<ArchiveError> = None;
    let result = reader.for_each_entries(|f, data| {
        if !f.has_stream || f.is_anti_item {
            return Ok(true);
        }
        // Verify the stored CRC while the bytes are read.
        let mut checked = Crc32Reader { inner: data, hasher: crc32fast::Hasher::new() };
        let entry = Entry { name: f.name.clone(), kind: EntryKind::File, size: f.size };
        let outcome = visit(entry, Some(&mut checked)).and_then(|()| {
            io::copy(&mut checked, &mut io::sink())?; // drain what the visitor skipped
            if f.has_crc && u64::from(checked.hasher.clone().finalize()) != f.crc {
                return Err(ArchiveError::Invalid(format!("7z: {} fails its checksum", f.name)));
            }
            Ok(())
        });
        match outcome {
            Ok(()) => Ok(true),
            Err(e) => {
                failure = Some(e);
                Ok(false)
            }
        }
    });
    if let Some(e) = failure {
        return Err(e);
    }
    result.map_err(|e| ArchiveError::Invalid(format!("7z: {e}")))
}

struct Crc32Reader<'a> {
    inner: &'a mut dyn Read,
    hasher: crc32fast::Hasher,
}
impl Read for Crc32Reader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

// ---- ISO 9660 with Joliet

const SECTOR: u64 = 2048;
const ISO_MAX_DEPTH: usize = 32;
const ISO_MAX_DIR_BYTES: u64 = 16 * 1024 * 1024;

struct IsoRecord {
    extent: u64,
    size: u64,
    dir: bool,
    multi_extent: bool,
    name: Vec<u8>,
}

fn le32(b: &[u8]) -> u64 {
    u64::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn iso_record(b: &[u8]) -> Option<IsoRecord> {
    let len = *b.first()? as usize;
    if len < 34 || len > b.len() {
        return None;
    }
    let name_len = b[32] as usize;
    if 33 + name_len > len {
        return None;
    }
    Some(IsoRecord {
        extent: le32(&b[2..6]),
        size: le32(&b[10..14]),
        dir: b[25] & 0x02 != 0,
        multi_extent: b[25] & 0x80 != 0,
        name: b[33..33 + name_len].to_vec(),
    })
}

fn iso_name(raw: &[u8], joliet: bool) -> String {
    let name = if joliet {
        let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(raw).into_owned()
    };
    // Strip the ";1" version and a bare trailing dot.
    let name = name.split(';').next().unwrap_or(&name).to_owned();
    name.strip_suffix('.').map(str::to_owned).unwrap_or(name)
}

fn walk_iso(path: &Path, visit: &mut Visit<'_>) -> Result<(), ArchiveError> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let bad = |w: &str| ArchiveError::Invalid(format!("iso: {w}"));
    // Volume descriptors from sector 16: prefer Joliet (long names).
    let mut primary = None;
    let mut joliet = None;
    let mut udf = false;
    for n in 16..64u64 {
        if (n + 1) * SECTOR > len {
            break;
        }
        let mut d = [0u8; 2048];
        f.seek(SeekFrom::Start(n * SECTOR))?;
        f.read_exact(&mut d)?;
        match (&d[1..6], d[0]) {
            (b"CD001", 1) => primary = Some(d),
            (b"CD001", 2) if d[88] == 0x25 && d[89] == 0x2F && matches!(d[90], 0x40 | 0x43 | 0x45) => joliet = Some(d),
            (b"CD001", 255) => break,
            (b"NSR02", _) | (b"NSR03", _) => udf = true,
            _ => {}
        }
    }
    let (descriptor, is_joliet) = match (joliet, primary) {
        (Some(j), _) => (j, true),
        (None, Some(p)) => (p, false),
        (None, None) if udf => return Err(ArchiveError::Unsupported("a UDF-only disc image (no ISO 9660 view); 7-Zip can extract it".into())),
        _ => return Err(bad("no ISO 9660 volume descriptor")),
    };
    let root = iso_record(&descriptor[156..190]).ok_or_else(|| bad("bad root directory record"))?;
    let mut seen = HashSet::new();
    iso_dir(&mut f, len, &root, "", is_joliet, 0, &mut seen, visit)
}

#[allow(clippy::too_many_arguments)]
fn iso_dir(
    f: &mut File,
    len: u64,
    dir: &IsoRecord,
    prefix: &str,
    joliet: bool,
    depth: usize,
    seen: &mut HashSet<u64>,
    visit: &mut Visit<'_>,
) -> Result<(), ArchiveError> {
    let bad = |w: String| ArchiveError::Invalid(format!("iso: {w}"));
    if depth > ISO_MAX_DEPTH || !seen.insert(dir.extent) {
        return Err(bad("directory loop or nesting too deep".into()));
    }
    if dir.size > ISO_MAX_DIR_BYTES || dir.extent * SECTOR + dir.size > len {
        return Err(bad(format!("directory {prefix:?} lies outside the image")));
    }
    let mut data = vec![0u8; dir.size as usize];
    f.seek(SeekFrom::Start(dir.extent * SECTOR))?;
    f.read_exact(&mut data)?;
    let mut at = 0usize;
    let mut children = Vec::new();
    while at < data.len() {
        let rec_len = data[at] as usize;
        if rec_len == 0 {
            // Records never cross a sector; skip to the next one.
            at = (at / SECTOR as usize + 1) * SECTOR as usize;
            continue;
        }
        let rec = iso_record(&data[at..]).ok_or_else(|| bad(format!("bad record in {prefix:?}")))?;
        at += rec_len;
        if rec.name == [0] || rec.name == [1] {
            continue; // . and ..
        }
        children.push(rec);
    }
    for rec in children {
        let name = format!("{prefix}{}", iso_name(&rec.name, joliet));
        if rec.dir {
            visit(Entry { name: format!("{name}/"), kind: EntryKind::Dir, size: 0 }, None)?;
            iso_dir(f, len, &rec, &format!("{name}/"), joliet, depth + 1, seen, visit)?;
        } else if rec.multi_extent {
            visit(Entry { name, kind: EntryKind::Other, size: rec.size }, None)?;
        } else {
            if rec.extent * SECTOR + rec.size > len {
                return Err(bad(format!("{name} lies outside the image")));
            }
            f.seek(SeekFrom::Start(rec.extent * SECTOR))?;
            let mut part = (&mut *f).take(rec.size);
            visit(Entry { name, kind: EntryKind::File, size: rec.size }, Some(&mut part))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- public API

/// List an archive's entries, sizes, and which ones extraction would skip.
pub fn list(path: &Path) -> Result<Listing, ArchiveError> {
    let (kind, _) = open_kind(path)?;
    let mut listing = Listing { kind, entries: Vec::new(), total_entries: 0, total_bytes: 0, unsafe_entries: Vec::new() };
    let mut keys = HashSet::new();
    walk(path, kind, &mut |entry, data| {
        let mut size = entry.size;
        if let (Some(r), Kind::Gz) = (data, kind) {
            size = io::copy(r, &mut io::sink())?; // gzip does not store the size reliably
        }
        listing.total_entries += 1;
        listing.total_bytes += size;
        if let Some(why) = skip_reason(&entry, &mut keys) {
            listing.unsafe_entries.push((entry.name.clone(), why));
        }
        if listing.entries.len() < MAX_LISTED {
            listing.entries.push(Entry { size, ..entry });
        }
        Ok(())
    })?;
    Ok(listing)
}

fn skip_reason(entry: &Entry, keys: &mut HashSet<String>) -> Option<String> {
    match entry.kind {
        EntryKind::Link => return Some("a link (never extracted)".into()),
        EntryKind::Other => return Some("not a plain file or folder".into()),
        _ => {}
    }
    match safe_relative(&entry.name) {
        Err(why) => Some(why),
        Ok(parts) => {
            // Same file twice, or names that clash on a case-insensitive disk.
            let key = parts.join("/").to_lowercase();
            if entry.kind == EntryKind::File && !keys.insert(key) {
                Some("a second entry with the same name (or differing only by case)".into())
            } else {
                None
            }
        }
    }
}

/// Extract into `destination`, which must not exist yet. Everything is
/// written to a staging folder first and renamed into place at the end.
pub fn extract(path: &Path, destination: &Path, limits: &ArchiveLimits) -> Result<ExtractReport, ArchiveError> {
    let (kind, archive_size) = open_kind(path)?;
    if destination.exists() {
        return Err(ArchiveError::DestinationExists(destination.to_path_buf()));
    }
    let parent = destination.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let leaf = destination.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "extract".into());
    let staging = parent.join(format!(".{leaf}.lu-partial-{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir(&staging)?;
    let result = extract_into(path, kind, archive_size, &staging, limits);
    match result {
        Ok(mut report) => match fs::rename(&staging, destination) {
            Ok(()) => {
                report.destination = destination.to_path_buf();
                Ok(report)
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&staging);
                Err(e.into())
            }
        },
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            Err(e)
        }
    }
}

fn extract_into(path: &Path, kind: Kind, archive_size: u64, root: &Path, limits: &ArchiveLimits) -> Result<ExtractReport, ArchiveError> {
    let mut report = ExtractReport { kind, destination: root.to_path_buf(), files: 0, dirs: 0, bytes: 0, skipped: Vec::new(), written: Vec::new() };
    let mut keys = HashSet::new();
    let mut entries = 0usize;
    let mut buf = vec![0u8; 256 * 1024];
    walk(path, kind, &mut |entry, data| {
        entries += 1;
        if entries > limits.max_entries {
            return Err(ArchiveError::Limit(format!("more than {} entries", limits.max_entries)));
        }
        if let Some(why) = skip_reason(&entry, &mut keys) {
            report.skipped.push((entry.name, why));
            return Ok(());
        }
        let parts = safe_relative(&entry.name).expect("checked by skip_reason");
        let target = parts.iter().fold(root.to_path_buf(), |p, c| p.join(c));
        let rel = parts.join("/");
        if entry.kind == EntryKind::Dir {
            fs::create_dir_all(&target)?;
            report.dirs += 1;
            return Ok(());
        }
        if let Some(dir) = target.parent() {
            fs::create_dir_all(dir)?;
        }
        if target.is_dir() {
            report.skipped.push((entry.name, "a folder already has this name".into()));
            return Ok(());
        }
        let mut out = fs::OpenOptions::new().write(true).create_new(true).open(&target)?;
        let reader = data.expect("files come with their bytes");
        loop {
            let n = reader.read(&mut buf).map_err(|e| ArchiveError::Invalid(format!("{}: {e}", entry.name)))?;
            if n == 0 {
                break;
            }
            report.bytes += n as u64;
            if report.bytes > limits.max_total_bytes {
                return Err(ArchiveError::Limit(format!("more than {} bytes", limits.max_total_bytes)));
            }
            if report.bytes > RATIO_FLOOR && report.bytes / archive_size.max(1) > limits.max_ratio {
                return Err(ArchiveError::Limit(format!("expands more than {}x (a zip bomb?)", limits.max_ratio)));
            }
            out.write_all(&buf[..n])?;
        }
        out.sync_all()?;
        report.files += 1;
        report.written.push(rel);
        Ok(())
    })?;
    Ok(report)
}
