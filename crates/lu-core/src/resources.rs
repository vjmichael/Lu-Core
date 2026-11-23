//! Resource management: measure before loading, one owner for the model,
//! pressure notices.
//!
//! - **Measure, never guess.** Free RAM, each GPU's dedicated memory (total
//!   and in use, system-wide) and free disk are read from the OS before a
//!   model is loaded or a heavy job starts. A value Lu cannot measure is
//!   reported as unmeasured; it never counts as free capacity.
//! - **Admission.** A load or job states what it needs; Lu admits it,
//!   refuses it with the reason, or queues it until memory frees up. The
//!   reserve Lu keeps free is a setting: the defaults are provisional until
//!   set with the owner after measuring on the owner's machine.
//! - **Footprints are learned.** After each load Lu records how much RAM and
//!   VRAM the model really took and uses that for the next load. Only a
//!   never-loaded model falls back to an estimate, and says so.
//! - **One owner.** [`ModelOwner`] is the only thing that loads and unloads
//!   the model. A plug-in (NVP before a render) asks for the model to be put
//!   aside and gets a lease; when every lease is released, Lu reloads it.
//!   A generation in progress is never interrupted.
//! - **Notices.** Refusals and pressure changes are journalled in SQLite
//!   and handed to the model once per chat as short lines.
//!
//! Written for Lu-Core. The old repo's native_resource_pressure.rs (RAM
//! parsing, ported in spirit: Linux `MemAvailable` in kB only) and
//! warm_model_residency.rs (idle and pressure eviction rules, kept) were
//! reviewed; host_resource_authority.rs is about access rights, not
//! machine resources, and was not used.

use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

use rusqlite::{params, Connection, OptionalExtension};

const MIB: u64 = 1024 * 1024;

// ---------------------------------------------------------------- measuring

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gpu {
    /// Adapter name as the driver reports it.
    pub name: String,
    /// Stable within a boot: the adapter LUID on Windows, the DRM card on Linux.
    pub id: String,
    pub total_mib: u64,
    /// Dedicated memory in use by all processes; `None` when unmeasurable.
    pub used_mib: Option<u64>,
}
impl Gpu {
    pub fn free_mib(&self) -> Option<u64> {
        self.used_mib.map(|u| self.total_mib.saturating_sub(u))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sample {
    pub ram_total_mib: Option<u64>,
    pub ram_available_mib: Option<u64>,
    pub gpus: Vec<Gpu>,
    /// Free space on the disk holding the path asked about.
    pub disk_free_mib: Option<u64>,
}

/// Read RAM, GPUs and (for `disk_path`) free disk now.
pub fn measure(disk_path: Option<&Path>) -> Sample {
    let (ram_total_mib, ram_available_mib) = ram();
    Sample { ram_total_mib, ram_available_mib, gpus: gpus(), disk_free_mib: disk_path.and_then(disk_free_mib) }
}

/// Linux `/proc/meminfo` value in MiB; only the kernel's own `kB` unit.
pub fn meminfo_mib(text: &str, key: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.split(':').next() == Some(key))?;
    let mut words = line.split(':').nth(1)?.split_whitespace();
    let kib: u64 = words.next()?.parse().ok()?;
    (words.next() == Some("kB")).then_some(kib / 1024)
}

#[cfg(windows)]
fn ram() -> (Option<u64>, Option<u64>) {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut s: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    s.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: s is a properly sized, writable MEMORYSTATUSEX.
    if unsafe { GlobalMemoryStatusEx(&mut s) } == 0 {
        return (None, None);
    }
    (Some(s.ullTotalPhys / MIB), Some(s.ullAvailPhys / MIB))
}
#[cfg(not(windows))]
fn ram() -> (Option<u64>, Option<u64>) {
    let Ok(text) = std::fs::read_to_string("/proc/meminfo") else { return (None, None) };
    (meminfo_mib(&text, "MemTotal"), meminfo_mib(&text, "MemAvailable"))
}

#[cfg(windows)]
fn disk_free_mib(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    // The nearest existing ancestor: the target folder may not exist yet.
    let existing = path.ancestors().find(|p| p.exists())?;
    let wide: Vec<u16> = existing.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free = 0u64;
    // SAFETY: wide is NUL-terminated; the out pointer is valid; the others may be null.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(free / MIB)
}
#[cfg(unix)]
fn disk_free_mib(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let existing = path.ancestors().find(|p| p.exists())?;
    let c = std::ffi::CString::new(existing.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c is NUL-terminated; s is writable.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    Some((s.f_bavail as u64).saturating_mul(s.f_frsize as u64) / MIB)
}

#[cfg(windows)]
fn gpus() -> Vec<Gpu> {
    let used = windows_gpu::dedicated_usage_by_luid();
    windows_gpu::adapters()
        .into_iter()
        .map(|(name, luid, total)| Gpu { used_mib: used.get(&luid).map(|b| b / MIB), id: luid, name, total_mib: total / MIB })
        .collect()
}

#[cfg(windows)]
mod windows_gpu {
    //! DXGI for each adapter's name, LUID and dedicated memory; the "GPU
    //! Adapter Memory" performance counter for what all processes use, keyed
    //! by the same LUID. windows-sys has no COM interfaces, so the two DXGI
    //! calls needed go through their vtables directly.
    use std::{collections::HashMap, ffi::c_void};
    use windows_sys::core::GUID;
    use windows_sys::Win32::System::Performance::{
        PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
        PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_LARGE, PDH_MORE_DATA,
    };

    #[link(name = "dxgi")]
    extern "system" {
        fn CreateDXGIFactory1(riid: *const GUID, factory: *mut *mut c_void) -> i32;
    }
    const IID_IDXGI_FACTORY1: GUID = GUID { data1: 0x770a_ae78, data2: 0xf26f, data3: 0x4dba, data4: [0xa8, 0x29, 0x25, 0x3c, 0x83, 0xd1, 0xb3, 0x87] };
    const DXGI_ERROR_NOT_FOUND: i32 = 0x887A_0002_u32 as i32;
    const DXGI_ADAPTER_FLAG_SOFTWARE: u32 = 2;

    #[repr(C)]
    struct AdapterDesc1 {
        description: [u16; 128],
        vendor_id: u32,
        device_id: u32,
        sub_sys_id: u32,
        revision: u32,
        dedicated_video_memory: usize,
        dedicated_system_memory: usize,
        shared_system_memory: usize,
        luid_low: u32,
        luid_high: i32,
        flags: u32,
    }
    type Release = unsafe extern "system" fn(*mut c_void) -> u32;
    type EnumAdapters1 = unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32;
    type GetDesc1 = unsafe extern "system" fn(*mut c_void, *mut AdapterDesc1) -> i32;
    // Vtable slots: IUnknown (3) + IDXGIObject (4) + IDXGIFactory (5) puts
    // EnumAdapters1 at 12; IUnknown + IDXGIObject + IDXGIAdapter (3) puts
    // GetDesc1 at 10.
    unsafe fn slot<T: Copy>(obj: *mut c_void, index: usize) -> T {
        let vtable = *(obj as *const *const usize);
        std::mem::transmute_copy(&*vtable.add(index))
    }

    pub fn luid_key(high: i32, low: u32) -> String {
        format!("luid_0x{:08x}_0x{:08x}", high as u32, low)
    }

    /// (name, LUID key, dedicated bytes) for each hardware adapter.
    pub fn adapters() -> Vec<(String, String, u64)> {
        let mut out = Vec::new();
        let mut factory: *mut c_void = std::ptr::null_mut();
        // SAFETY: standard DXGI factory creation; every interface obtained
        // is released before returning, and slots match the documented
        // IDXGIFactory1 / IDXGIAdapter1 layouts.
        unsafe {
            if CreateDXGIFactory1(&IID_IDXGI_FACTORY1, &mut factory) < 0 || factory.is_null() {
                return out;
            }
            let enum_adapters: EnumAdapters1 = slot(factory, 12);
            for i in 0..16u32 {
                let mut adapter: *mut c_void = std::ptr::null_mut();
                let hr = enum_adapters(factory, i, &mut adapter);
                if hr == DXGI_ERROR_NOT_FOUND || hr < 0 || adapter.is_null() {
                    break;
                }
                let mut desc: AdapterDesc1 = std::mem::zeroed();
                let get_desc: GetDesc1 = slot(adapter, 10);
                if get_desc(adapter, &mut desc) >= 0 && desc.flags & DXGI_ADAPTER_FLAG_SOFTWARE == 0 && desc.dedicated_video_memory > 0 {
                    let len = desc.description.iter().position(|c| *c == 0).unwrap_or(128);
                    out.push((
                        String::from_utf16_lossy(&desc.description[..len]),
                        luid_key(desc.luid_high, desc.luid_low),
                        desc.dedicated_video_memory as u64,
                    ));
                }
                let release: Release = slot(adapter, 2);
                release(adapter);
            }
            let release: Release = slot(factory, 2);
            release(factory);
        }
        out
    }

    /// Dedicated GPU memory in use, by all processes, per adapter LUID.
    pub fn dedicated_usage_by_luid() -> HashMap<String, u64> {
        let mut out = HashMap::new();
        let path: Vec<u16> = "\\GPU Adapter Memory(*)\\Dedicated Usage".encode_utf16().chain(Some(0)).collect();
        let mut query = 0isize;
        // SAFETY: PDH query lifecycle with valid out pointers; the query is
        // closed on every path; the item buffer is sized as PDH asks.
        unsafe {
            if PdhOpenQueryW(std::ptr::null(), 0, &mut query) != 0 {
                return out;
            }
            let mut counter = 0isize;
            if PdhAddEnglishCounterW(query, path.as_ptr(), 0, &mut counter) == 0 && PdhCollectQueryData(query) == 0 {
                let mut size = 0u32;
                let mut count = 0u32;
                if PdhGetFormattedCounterArrayW(counter, PDH_FMT_LARGE, &mut size, &mut count, std::ptr::null_mut()) == PDH_MORE_DATA {
                    let mut buf = vec![0u8; size as usize];
                    let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
                    if PdhGetFormattedCounterArrayW(counter, PDH_FMT_LARGE, &mut size, &mut count, items) == 0 {
                        for i in 0..count as usize {
                            let item = &*items.add(i);
                            let mut len = 0;
                            while *item.szName.add(len) != 0 {
                                len += 1;
                            }
                            let name = String::from_utf16_lossy(std::slice::from_raw_parts(item.szName, len));
                            // "luid_0x00000000_0x0000db69_phys_0": sum the physical parts.
                            let key = name.split("_phys").next().unwrap_or(&name).to_ascii_lowercase();
                            if item.FmtValue.CStatus == 0 {
                                *out.entry(key).or_insert(0) += item.FmtValue.Anonymous.largeValue.max(0) as u64;
                            }
                        }
                    }
                }
            }
            PdhCloseQuery(query);
        }
        out
    }
}

#[cfg(not(windows))]
fn gpus() -> Vec<Gpu> {
    // amdgpu exposes exact VRAM totals and system-wide use in sysfs.
    let mut out = Vec::new();
    let Ok(cards) = std::fs::read_dir("/sys/class/drm") else { return out };
    for card in cards.flatten() {
        let name = card.file_name().to_string_lossy().into_owned();
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let dev = card.path().join("device");
        let read = |f: &str| std::fs::read_to_string(dev.join(f)).ok().and_then(|s| s.trim().parse::<u64>().ok());
        if let Some(total) = read("mem_info_vram_total") {
            let product = std::fs::read_to_string(dev.join("product_name")).map(|s| s.trim().to_owned()).unwrap_or_else(|_| format!("GPU {name}"));
            out.push(Gpu { name: product, id: name, total_mib: total / MIB, used_mib: read("mem_info_vram_used").map(|u| u / MIB) });
        }
    }
    out
}

// ---------------------------------------------------------------- settings and journal

/// What Lu keeps free. Provisional until the owner sets them after measuring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reserves {
    pub ram_mib: u64,
    pub vram_mib: u64,
    pub disk_mib: u64,
    /// True once the owner has set these after measuring.
    pub owner_set: bool,
}
impl Default for Reserves {
    fn default() -> Self {
        Self { ram_mib: 4096, vram_mib: 512, disk_mib: 10 * 1024, owner_set: false }
    }
}

/// What a load or job needs on top of what is in use now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Need {
    pub ram_mib: u64,
    pub vram_mib: u64,
    /// The GPU by [`Gpu::id`]; `None` means the first one.
    pub gpu: Option<String>,
    pub disk_mib: u64,
    pub disk_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Admit,
    /// Not enough; each reason names what is short and by how much.
    Refuse(Vec<String>),
    /// Something needed could not be measured. Not admitted: an unknown is
    /// not free capacity. The user can still choose to go ahead.
    Unmeasured(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footprint {
    pub ram_mib: u64,
    pub vram_mib: u64,
    /// Measured from a real load, or an estimate for a model never loaded.
    pub measured: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub id: i64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceError {
    Database(String),
    Locked,
    Refused(Vec<String>),
    Unmeasured(Vec<String>),
    /// A generation is running; it is never interrupted.
    Busy,
    NoModel,
    UnknownLease(u64),
    Engine(String),
}
impl From<rusqlite::Error> for ResourceError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Database(e.to_string())
    }
}
impl std::fmt::Display for ResourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ResourceError {}

pub type Sampler = Box<dyn Fn(Option<&Path>) -> Sample + Send + Sync>;

/// Settings, learned footprints and the resource journal, machine-wide.
pub struct Governor {
    db: Mutex<Connection>,
    sampler: Sampler,
    queue: Mutex<VecDeque<(String, Need)>>,
    /// Last pressure state, so a notice is written on change, not every check.
    pressured: Mutex<Option<bool>>,
}

impl Governor {
    /// Open with the real OS measurements.
    pub fn open(path: &Path) -> Result<Self, ResourceError> {
        Self::with_sampler(path, Box::new(measure))
    }
    /// Open with a given sampler (tests, or a remote host later).
    pub fn with_sampler(path: &Path, sampler: Sampler) -> Result<Self, ResourceError> {
        let db = Connection::open(path)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS lu_resource_settings (
                id INTEGER PRIMARY KEY CHECK(id = 1),
                ram_mib INTEGER NOT NULL, vram_mib INTEGER NOT NULL, disk_mib INTEGER NOT NULL,
                owner_set INTEGER NOT NULL CHECK(owner_set IN (0,1)),
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS lu_model_footprints (
                model TEXT PRIMARY KEY NOT NULL,
                ram_mib INTEGER NOT NULL, vram_mib INTEGER NOT NULL,
                loads INTEGER NOT NULL,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS lu_resource_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                kind TEXT NOT NULL CHECK(kind IN ('refused','unmeasured','pressure','relieved','load','unload','reload','lease','release','evict')),
                text TEXT NOT NULL,
                for_model INTEGER NOT NULL CHECK(for_model IN (0,1)),
                at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS lu_notice_delivery (
                session_id TEXT PRIMARY KEY NOT NULL,
                last_id INTEGER NOT NULL
            );
            CREATE TRIGGER IF NOT EXISTS lu_resource_events_no_update
              BEFORE UPDATE ON lu_resource_events BEGIN SELECT RAISE(ABORT, 'events are immutable'); END;",
        )?;
        Ok(Self { db: Mutex::new(db), sampler, queue: Mutex::new(VecDeque::new()), pressured: Mutex::new(None) })
    }
    fn db(&self) -> Result<std::sync::MutexGuard<'_, Connection>, ResourceError> {
        self.db.lock().map_err(|_| ResourceError::Locked)
    }

    pub fn sample(&self, disk: Option<&Path>) -> Sample {
        (self.sampler)(disk)
    }

    pub fn reserves(&self) -> Result<Reserves, ResourceError> {
        let row = self
            .db()?
            .query_row("SELECT ram_mib, vram_mib, disk_mib, owner_set FROM lu_resource_settings WHERE id = 1", [], |r| {
                Ok(Reserves { ram_mib: r.get::<_, i64>(0)? as u64, vram_mib: r.get::<_, i64>(1)? as u64, disk_mib: r.get::<_, i64>(2)? as u64, owner_set: r.get(3)? })
            })
            .optional()?;
        Ok(row.unwrap_or_default())
    }
    /// The owner sets the reserves after measuring on their machine.
    pub fn set_reserves(&self, ram_mib: u64, vram_mib: u64, disk_mib: u64) -> Result<(), ResourceError> {
        self.db()?.execute(
            "INSERT INTO lu_resource_settings(id, ram_mib, vram_mib, disk_mib, owner_set) VALUES (1, ?1, ?2, ?3, 1)
             ON CONFLICT(id) DO UPDATE SET ram_mib = excluded.ram_mib, vram_mib = excluded.vram_mib,
               disk_mib = excluded.disk_mib, owner_set = 1, updated_at = CURRENT_TIMESTAMP",
            params![ram_mib as i64, vram_mib as i64, disk_mib as i64],
        )?;
        Ok(())
    }

    fn event(&self, kind: &str, text: &str, for_model: bool) -> Result<(), ResourceError> {
        self.db()?.execute("INSERT INTO lu_resource_events(kind, text, for_model) VALUES (?1, ?2, ?3)", params![kind, text, for_model])?;
        Ok(())
    }
    /// The resource journal, oldest first: (kind, text).
    pub fn events(&self) -> Result<Vec<(String, String)>, ResourceError> {
        let db = self.db()?;
        let mut q = db.prepare("SELECT kind, text FROM lu_resource_events ORDER BY id")?;
        let rows = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
    /// Notices this chat's model has not seen yet; each is handed over once.
    pub fn notices_for(&self, session_id: &str) -> Result<Vec<Notice>, ResourceError> {
        let db = self.db()?;
        let last: i64 = db
            .query_row("SELECT last_id FROM lu_notice_delivery WHERE session_id = ?1", params![session_id], |r| r.get(0))
            .optional()?
            .unwrap_or(0);
        let mut q = db.prepare("SELECT id, text FROM lu_resource_events WHERE for_model = 1 AND id > ?1 ORDER BY id LIMIT 20")?;
        let notices: Vec<Notice> = q.query_map(params![last], |r| Ok(Notice { id: r.get(0)?, text: r.get(1)? }))?.collect::<Result<_, _>>()?;
        if let Some(n) = notices.last() {
            db.execute(
                "INSERT INTO lu_notice_delivery(session_id, last_id) VALUES (?1, ?2)
                 ON CONFLICT(session_id) DO UPDATE SET last_id = excluded.last_id",
                params![session_id, n.id],
            )?;
        }
        Ok(notices)
    }

    /// What `model` takes when loaded: measured from its last load, or an
    /// estimate from the file size for a model never loaded.
    pub fn footprint(&self, model: &str, file_bytes: u64, gpu_offload: bool) -> Result<Footprint, ResourceError> {
        let row: Option<(i64, i64)> = self
            .db()?
            .query_row("SELECT ram_mib, vram_mib FROM lu_model_footprints WHERE model = ?1", params![model], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if let Some((ram, vram)) = row {
            return Ok(Footprint { ram_mib: ram as u64, vram_mib: vram as u64, measured: true });
        }
        // Weights plus 25% for context cache and runtime buffers. Only used
        // until the first real load measures the model.
        let weights = file_bytes.div_ceil(MIB);
        let with_cache = weights + weights / 4 + 512;
        Ok(if gpu_offload {
            Footprint { ram_mib: 1024, vram_mib: with_cache, measured: false }
        } else {
            Footprint { ram_mib: with_cache, vram_mib: 0, measured: false }
        })
    }
    fn record_footprint(&self, model: &str, ram_mib: u64, vram_mib: u64) -> Result<(), ResourceError> {
        self.db()?.execute(
            "INSERT INTO lu_model_footprints(model, ram_mib, vram_mib, loads) VALUES (?1, ?2, ?3, 1)
             ON CONFLICT(model) DO UPDATE SET ram_mib = excluded.ram_mib, vram_mib = excluded.vram_mib,
               loads = loads + 1, updated_at = CURRENT_TIMESTAMP",
            params![model, ram_mib as i64, vram_mib as i64],
        )?;
        Ok(())
    }

    fn judge(&self, need: &Need, sample: &Sample, reserves: &Reserves) -> Admission {
        let mut short = Vec::new();
        let mut unknown = Vec::new();
        if need.ram_mib > 0 {
            match sample.ram_available_mib {
                Some(free) if free >= need.ram_mib + reserves.ram_mib => {}
                Some(free) => short.push(format!(
                    "RAM: needs {} MiB plus {} MiB kept free, {} MiB available",
                    need.ram_mib, reserves.ram_mib, free
                )),
                None => unknown.push("free RAM could not be measured".to_owned()),
            }
        }
        if need.vram_mib > 0 {
            let gpu = match &need.gpu {
                Some(id) => sample.gpus.iter().find(|g| &g.id == id),
                None => sample.gpus.first(),
            };
            match gpu.map(|g| (g, g.free_mib())) {
                Some((_, Some(free))) if free >= need.vram_mib + reserves.vram_mib => {}
                Some((g, Some(free))) => short.push(format!(
                    "VRAM on {}: needs {} MiB plus {} MiB kept free, {} MiB free of {} MiB",
                    g.name, need.vram_mib, reserves.vram_mib, free, g.total_mib
                )),
                Some((g, None)) => unknown.push(format!("VRAM in use on {} could not be measured", g.name)),
                None => unknown.push("no GPU found to measure".to_owned()),
            }
        }
        if need.disk_mib > 0 {
            match sample.disk_free_mib {
                Some(free) if free >= need.disk_mib + reserves.disk_mib => {}
                Some(free) => short.push(format!(
                    "disk: needs {} MiB plus {} MiB kept free, {} MiB free",
                    need.disk_mib, reserves.disk_mib, free
                )),
                None => unknown.push("free disk space could not be measured".to_owned()),
            }
        }
        if !short.is_empty() {
            Admission::Refuse(short)
        } else if !unknown.is_empty() {
            Admission::Unmeasured(unknown)
        } else {
            Admission::Admit
        }
    }

    /// Measure now and decide. A refusal is journalled and becomes a notice.
    pub fn admit(&self, what: &str, need: &Need) -> Result<Admission, ResourceError> {
        let sample = self.sample(need.disk_path.as_deref());
        let decision = self.judge(need, &sample, &self.reserves()?);
        match &decision {
            Admission::Refuse(why) => self.event("refused", &format!("Not started: {what}. {}", why.join("; ")), true)?,
            Admission::Unmeasured(why) => self.event("unmeasured", &format!("Not started: {what}. {}", why.join("; ")), true)?,
            Admission::Admit => {}
        }
        Ok(decision)
    }

    /// Queue a heavy job until there is room. Returns its place (0 = next).
    pub fn enqueue(&self, job_id: &str, need: Need) -> Result<usize, ResourceError> {
        let mut q = self.queue.lock().map_err(|_| ResourceError::Locked)?;
        q.push_back((job_id.to_owned(), need));
        Ok(q.len() - 1)
    }
    /// Jobs that fit now, in queue order. The first job that does not fit
    /// stops the scan, so a large job is not starved by smaller ones.
    pub fn ready(&self) -> Result<Vec<String>, ResourceError> {
        let reserves = self.reserves()?;
        let mut q = self.queue.lock().map_err(|_| ResourceError::Locked)?;
        let mut out = Vec::new();
        while let Some((id, need)) = q.front() {
            let sample = self.sample(need.disk_path.as_deref());
            if self.judge(need, &sample, &reserves) != Admission::Admit {
                break;
            }
            out.push(id.clone());
            q.pop_front();
        }
        Ok(out)
    }

    /// Check for memory pressure; journal and notify only when it changes.
    /// Returns whether the machine is under pressure now.
    pub fn check_pressure(&self) -> Result<bool, ResourceError> {
        let sample = self.sample(None);
        let reserves = self.reserves()?;
        let mut lines = Vec::new();
        if let Some(free) = sample.ram_available_mib.filter(|f| *f < reserves.ram_mib) {
            lines.push(format!("RAM is low: {free} MiB available (keeping {} MiB free)", reserves.ram_mib));
        }
        for g in &sample.gpus {
            if let Some(free) = g.free_mib().filter(|f| *f < reserves.vram_mib) {
                lines.push(format!("VRAM is low on {}: {free} MiB free", g.name));
            }
        }
        let now = !lines.is_empty();
        let mut last = self.pressured.lock().map_err(|_| ResourceError::Locked)?;
        if *last != Some(now) {
            if now {
                self.event("pressure", &format!("{}. Avoid starting large jobs.", lines.join("; ")), true)?;
            } else if last.is_some() {
                self.event("relieved", "Memory pressure has eased.", true)?;
            }
            *last = Some(now);
        }
        Ok(now)
    }
}

// ---------------------------------------------------------------- the one owner

/// The inference engine as the owner sees it (piece 13 provides the real one).
pub trait Engine {
    fn load(&mut self, model: &str) -> Result<(), String>;
    fn unload(&mut self) -> Result<(), String>;
}

/// When an idle model is put aside. Kept from the old warm-residency rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdlePolicy {
    /// Keep a loaded model this long after its last use.
    pub idle_grace: Duration,
    /// Never evict sooner than this after a load (no load/unload thrash).
    pub min_resident: Duration,
}
impl Default for IdlePolicy {
    fn default() -> Self {
        Self { idle_grace: Duration::from_secs(15 * 60), min_resident: Duration::from_secs(30) }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub id: u64,
    pub requester: String,
}

struct Loaded {
    model: String,
    file_bytes: u64,
    gpu_offload: bool,
    loaded_at: Instant,
    last_used: Instant,
}

/// The only place a model is loaded or unloaded.
pub struct ModelOwner<'g, E: Engine> {
    governor: &'g Governor,
    engine: E,
    loaded: Option<Loaded>,
    generating: bool,
    leases: BTreeMap<u64, String>,
    next_lease: u64,
    /// What to bring back when the last lease is released.
    put_aside: Option<(String, u64, bool)>,
    policy: IdlePolicy,
}

impl<'g, E: Engine> ModelOwner<'g, E> {
    pub fn new(governor: &'g Governor, engine: E, policy: IdlePolicy) -> Self {
        Self { governor, engine, loaded: None, generating: false, leases: BTreeMap::new(), next_lease: 1, put_aside: None, policy }
    }
    pub fn loaded_model(&self) -> Option<&str> {
        self.loaded.as_ref().map(|l| l.model.as_str())
    }
    pub fn leases(&self) -> Vec<Lease> {
        self.leases.iter().map(|(id, r)| Lease { id: *id, requester: r.clone() }).collect()
    }

    /// Load `model` after measuring. Replaces a loaded model (not during a
    /// generation, and not while a plug-in holds the model aside).
    pub fn load(&mut self, model: &str, file_bytes: u64, gpu_offload: bool) -> Result<Footprint, ResourceError> {
        if self.generating {
            return Err(ResourceError::Busy);
        }
        if !self.leases.is_empty() {
            let holders: Vec<_> = self.leases.values().cloned().collect();
            return Err(ResourceError::Refused(vec![format!("the model is set aside for {}", holders.join(", "))]));
        }
        if self.loaded_model() == Some(model) {
            return self.governor.footprint(model, file_bytes, gpu_offload);
        }
        if self.loaded.is_some() {
            self.unload_now("unload", "to load another model")?;
        }
        let estimate = self.governor.footprint(model, file_bytes, gpu_offload)?;
        let need = Need { ram_mib: estimate.ram_mib, vram_mib: estimate.vram_mib, ..Need::default() };
        let what = format!("loading {model} ({} footprint: {} MiB RAM, {} MiB VRAM)", if estimate.measured { "measured" } else { "estimated" }, estimate.ram_mib, estimate.vram_mib);
        match self.governor.admit(&what, &need)? {
            Admission::Admit => {}
            Admission::Refuse(why) => return Err(ResourceError::Refused(why)),
            Admission::Unmeasured(why) => return Err(ResourceError::Unmeasured(why)),
        }
        let before = self.governor.sample(None);
        self.engine.load(model).map_err(ResourceError::Engine)?;
        let after = self.governor.sample(None);
        // The real footprint: what disappeared from free RAM and VRAM.
        let ram = before.ram_available_mib.zip(after.ram_available_mib).map(|(b, a)| b.saturating_sub(a));
        let vram = before.gpus.first().and_then(Gpu::free_mib).zip(after.gpus.first().and_then(Gpu::free_mib)).map(|(b, a)| b.saturating_sub(a));
        let measured = match (ram, vram) {
            (Some(r), Some(v)) => {
                self.governor.record_footprint(model, r, v)?;
                Footprint { ram_mib: r, vram_mib: v, measured: true }
            }
            (Some(r), None) if !gpu_offload => {
                self.governor.record_footprint(model, r, 0)?;
                Footprint { ram_mib: r, vram_mib: 0, measured: true }
            }
            _ => estimate,
        };
        self.governor.event(
            "load",
            &format!("Loaded {model}: {} MiB RAM, {} MiB VRAM ({})", measured.ram_mib, measured.vram_mib, if measured.measured { "measured" } else { "estimate" }),
            false,
        )?;
        let now = Instant::now();
        self.loaded = Some(Loaded { model: model.to_owned(), file_bytes, gpu_offload, loaded_at: now, last_used: now });
        Ok(measured)
    }

    fn unload_now(&mut self, kind: &str, why: &str) -> Result<(), ResourceError> {
        let Some(l) = self.loaded.take() else { return Ok(()) };
        if let Err(e) = self.engine.unload() {
            self.loaded = Some(l);
            return Err(ResourceError::Engine(e));
        }
        self.governor.event(kind, &format!("Unloaded {} {why}", l.model), false)?;
        Ok(())
    }

    /// A generation starts; until it ends nothing unloads the model.
    pub fn begin_generation(&mut self) -> Result<(), ResourceError> {
        if self.loaded.is_none() {
            return Err(ResourceError::NoModel);
        }
        self.generating = true;
        Ok(())
    }
    pub fn end_generation(&mut self) {
        self.generating = false;
        if let Some(l) = self.loaded.as_mut() {
            l.last_used = Instant::now();
        }
    }

    /// A plug-in asks for the model to be set aside (NVP before a render).
    /// Refused while a generation runs. The model reloads when every lease
    /// is released.
    pub fn request_unload(&mut self, requester: &str, reason: &str) -> Result<Lease, ResourceError> {
        if self.generating {
            return Err(ResourceError::Busy);
        }
        if let Some(l) = &self.loaded {
            self.put_aside = Some((l.model.clone(), l.file_bytes, l.gpu_offload));
            let model = l.model.clone();
            self.unload_now("lease", &format!("for {requester}: {reason}"))?;
            self.governor.event("lease", &format!("{model} is set aside while {requester} works ({reason})."), true)?;
        }
        let id = self.next_lease;
        self.next_lease += 1;
        self.leases.insert(id, requester.to_owned());
        Ok(Lease { id, requester: requester.to_owned() })
    }

    /// Release a lease. When none is left, the set-aside model is reloaded
    /// (after measuring again). Returns the reloaded model, if any.
    pub fn release(&mut self, lease_id: u64) -> Result<Option<String>, ResourceError> {
        let requester = self.leases.remove(&lease_id).ok_or(ResourceError::UnknownLease(lease_id))?;
        self.governor.event("release", &format!("{requester} released the model"), false)?;
        if !self.leases.is_empty() {
            return Ok(None);
        }
        let Some((model, bytes, gpu)) = self.put_aside.take() else { return Ok(None) };
        match self.load(&model, bytes, gpu) {
            Ok(_) => {
                self.governor.event("reload", &format!("{model} is loaded again."), true)?;
                Ok(Some(model))
            }
            Err(e) => {
                // Keep it set aside so a later attempt can bring it back.
                self.put_aside = Some((model, bytes, gpu));
                Err(e)
            }
        }
    }

    /// Call periodically. Puts an idle model aside after the grace period,
    /// or sooner under measured memory pressure. Never during a generation
    /// or within `min_resident` of loading. Returns whether it unloaded.
    pub fn tick(&mut self) -> Result<bool, ResourceError> {
        let pressure = self.governor.check_pressure()?;
        let Some(l) = &self.loaded else { return Ok(false) };
        if self.generating || l.loaded_at.elapsed() < self.policy.min_resident {
            return Ok(false);
        }
        let idle = l.last_used.elapsed();
        let reason = if pressure {
            "because memory is low"
        } else if idle >= self.policy.idle_grace {
            "after being idle"
        } else {
            return Ok(false);
        };
        let model = l.model.clone();
        self.unload_now("evict", reason)?;
        self.governor.event("evict", &format!("{model} was unloaded {reason}; it reloads on the next message."), true)?;
        Ok(true)
    }
}
