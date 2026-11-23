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
//!   Lu's models and the only one who hands out GPU memory. A plug-in
//!   (NVP running a diffusion model such as LTX) asks for a GPU lease: an
//!   amount of VRAM, or the whole GPU. Lu grants it from free VRAM, or sets
//!   its own model aside if that frees enough, or refuses with the numbers.
//!   Leased VRAM is held back from Lu's own loads until the plug-in uses it.
//!   A lease ends when released or when not renewed in time (a crashed
//!   plug-in); then Lu reloads its model. A generation is never interrupted.
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
    /// GPU memory promised to plug-ins, per GPU id.
    leased: Mutex<BTreeMap<String, LeaseBook>>,
}

/// GPU memory promised to plug-ins on one GPU. A plug-in may not have
/// allocated it yet, so it is held back from Lu's own loads; once the
/// plug-in's use shows up in the measurement it is not counted twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LeaseBook {
    leased_mib: u64,
    /// Measured use when the first lease on this GPU was granted (plus any
    /// load Lu made since), so growth beyond it is the plug-ins' own.
    baseline_used_mib: u64,
    exclusive: bool,
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
                kind TEXT NOT NULL CHECK(kind IN ('refused','unmeasured','pressure','relieved','load','unload','reload','lease','release','evict','expire')),
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
        Ok(Self { db: Mutex::new(db), sampler, queue: Mutex::new(VecDeque::new()), pressured: Mutex::new(None), leased: Mutex::new(BTreeMap::new()) })
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
            let held = gpu.map_or(0, |g| self.held_for_plugins(g));
            match gpu.map(|g| (g, g.free_mib().map(|f| f.saturating_sub(held)))) {
                Some((_, Some(free))) if free >= need.vram_mib + reserves.vram_mib => {}
                Some((g, Some(free))) => short.push(format!(
                    "VRAM on {}: needs {} MiB plus {} MiB kept free, {} MiB free of {} MiB{}",
                    g.name,
                    need.vram_mib,
                    reserves.vram_mib,
                    free,
                    g.total_mib,
                    if held > 0 { format!(" after {held} MiB held for plug-ins") } else { String::new() }
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

    /// VRAM promised to plug-ins on `gpu` that they have not taken yet.
    fn held_for_plugins(&self, gpu: &Gpu) -> u64 {
        let Ok(books) = self.leased.lock() else { return gpu.total_mib };
        match books.get(&gpu.id) {
            None => 0,
            Some(b) if b.exclusive => gpu.total_mib,
            Some(b) => {
                let taken = gpu.used_mib.map_or(0, |u| u.saturating_sub(b.baseline_used_mib));
                b.leased_mib.saturating_sub(taken)
            }
        }
    }
    fn set_lease_book(&self, gpu: &str, book: Option<LeaseBook>) {
        if let Ok(mut books) = self.leased.lock() {
            match book {
                Some(b) => books.insert(gpu.to_owned(), b),
                None => books.remove(gpu),
            };
        }
    }
    fn lease_book(&self, gpu: &str) -> Option<LeaseBook> {
        self.leased.lock().ok().and_then(|b| b.get(gpu).copied())
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

/// What a plug-in asks for. NVP running a diffusion model (LTX) asks for
/// the VRAM that model needs; a job that must have the card to itself
/// asks for it exclusively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuRequest {
    /// The GPU by [`Gpu::id`]; `None` means the first one.
    pub gpu: Option<String>,
    /// VRAM the plug-in will use. Ignored when `exclusive`.
    pub vram_mib: u64,
    /// The whole GPU: Lu's model is set aside and nothing else loads on it.
    pub exclusive: bool,
    /// The lease ends unless renewed within this time, so a plug-in that
    /// crashes cannot hold the GPU forever.
    pub ttl: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub id: u64,
    pub requester: String,
    pub purpose: String,
    pub gpu: String,
    pub vram_mib: u64,
    pub exclusive: bool,
    /// Lu set its own model aside to make room for this lease.
    pub model_set_aside: bool,
}

struct Held {
    lease: Lease,
    ttl: Duration,
    renewed: Instant,
}

struct Loaded {
    model: String,
    file_bytes: u64,
    gpu_offload: bool,
    gpu: Option<String>,
    vram_mib: u64,
    loaded_at: Instant,
    last_used: Instant,
}

/// The only place a model is loaded or unloaded, and the only one who
/// grants GPU memory to plug-ins.
pub struct ModelOwner<'g, E: Engine> {
    governor: &'g Governor,
    engine: E,
    loaded: Option<Loaded>,
    generating: bool,
    leases: BTreeMap<u64, Held>,
    next_lease: u64,
    /// What to bring back when the leases that displaced it end.
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
        self.leases.values().map(|h| h.lease.clone()).collect()
    }

    /// Load `model` after measuring. Replaces a loaded model (not during a
    /// generation). VRAM leased to plug-ins is not available; a GPU leased
    /// exclusively takes no GPU load at all.
    pub fn load(&mut self, model: &str, file_bytes: u64, gpu_offload: bool) -> Result<Footprint, ResourceError> {
        if self.generating {
            return Err(ResourceError::Busy);
        }
        if self.loaded_model() == Some(model) {
            return self.governor.footprint(model, file_bytes, gpu_offload);
        }
        let gpu = self.governor.sample(None).gpus.first().map(|g| g.id.clone());
        if gpu_offload {
            if let Some(h) = self.leases.values().find(|h| h.lease.exclusive && Some(&h.lease.gpu) == gpu.as_ref()) {
                return Err(ResourceError::Refused(vec![format!("the GPU is leased to {} for {}", h.lease.requester, h.lease.purpose)]));
            }
        }
        if self.loaded.is_some() {
            self.unload_now("unload", "to load another model")?;
        }
        let estimate = self.governor.footprint(model, file_bytes, gpu_offload)?;
        let need = Need { ram_mib: estimate.ram_mib, vram_mib: estimate.vram_mib, gpu: gpu.clone(), ..Need::default() };
        let what = format!(
            "loading {model} ({} footprint: {} MiB RAM, {} MiB VRAM)",
            if estimate.measured { "measured" } else { "estimated" },
            estimate.ram_mib,
            estimate.vram_mib
        );
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
        // Lu's own use is not a plug-in taking its lease.
        if let Some(g) = &gpu {
            if let Some(mut b) = self.governor.lease_book(g) {
                b.baseline_used_mib += measured.vram_mib;
                self.governor.set_lease_book(g, Some(b));
            }
        }
        self.governor.event(
            "load",
            &format!(
                "Loaded {model}: {} MiB RAM, {} MiB VRAM ({})",
                measured.ram_mib,
                measured.vram_mib,
                if measured.measured { "measured" } else { "estimate" }
            ),
            false,
        )?;
        let now = Instant::now();
        self.loaded = Some(Loaded { model: model.to_owned(), file_bytes, gpu_offload, gpu, vram_mib: measured.vram_mib, loaded_at: now, last_used: now });
        Ok(measured)
    }

    fn unload_now(&mut self, kind: &str, why: &str) -> Result<(), ResourceError> {
        let Some(l) = self.loaded.take() else { return Ok(()) };
        if let Err(e) = self.engine.unload() {
            self.loaded = Some(l);
            return Err(ResourceError::Engine(e));
        }
        if let Some(g) = &l.gpu {
            if let Some(mut b) = self.governor.lease_book(g) {
                b.baseline_used_mib = b.baseline_used_mib.saturating_sub(l.vram_mib);
                self.governor.set_lease_book(g, Some(b));
            }
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

    fn rebuild_book(&self, gpu: &str, baseline: Option<u64>) {
        let on_gpu: Vec<&Lease> = self.leases.values().map(|h| &h.lease).filter(|l| l.gpu == gpu).collect();
        if on_gpu.is_empty() {
            self.governor.set_lease_book(gpu, None);
            return;
        }
        let old = self.governor.lease_book(gpu);
        self.governor.set_lease_book(
            gpu,
            Some(LeaseBook {
                leased_mib: on_gpu.iter().map(|l| l.vram_mib).sum(),
                baseline_used_mib: baseline.or(old.map(|b| b.baseline_used_mib)).unwrap_or(0),
                exclusive: on_gpu.iter().any(|l| l.exclusive),
            }),
        );
    }

    /// A plug-in asks for GPU memory (NVP for a diffusion model, a render).
    /// Granted from free VRAM if it fits; otherwise Lu sets its own model
    /// aside, but only if that frees enough. Never during a generation.
    pub fn request_gpu(&mut self, requester: &str, purpose: &str, request: &GpuRequest) -> Result<Lease, ResourceError> {
        if self.generating {
            return Err(ResourceError::Busy);
        }
        let sample = self.governor.sample(None);
        let gpu = match &request.gpu {
            Some(id) => sample.gpus.iter().find(|g| &g.id == id),
            None => sample.gpus.first(),
        }
        .cloned()
        .ok_or_else(|| ResourceError::Refused(vec!["no such GPU".into()]))?;
        if let Some(h) = self.leases.values().find(|h| h.lease.gpu == gpu.id && (h.lease.exclusive || request.exclusive)) {
            return Err(ResourceError::Refused(vec![format!("{} is leased to {} for {}", gpu.name, h.lease.requester, h.lease.purpose)]));
        }
        let reserve = self.governor.reserves()?.vram_mib;
        let free = gpu
            .free_mib()
            .ok_or_else(|| ResourceError::Unmeasured(vec![format!("VRAM in use on {} could not be measured", gpu.name)]))?;
        let available = free.saturating_sub(self.governor.held_for_plugins(&gpu)).saturating_sub(reserve);
        let ours = self.loaded.as_ref().filter(|l| l.gpu.as_ref() == Some(&gpu.id) && l.gpu_offload).map(|l| l.vram_mib);
        let fits_now = !request.exclusive && available >= request.vram_mib;
        let mut set_aside = false;
        if !fits_now {
            let freed = ours.unwrap_or(0);
            if !request.exclusive && available + freed < request.vram_mib {
                let why = format!(
                    "{requester} asked for {} MiB on {}; {available} MiB is available{}",
                    request.vram_mib,
                    gpu.name,
                    if freed > 0 { format!(" and setting Lu's model aside would free only {freed} MiB more") } else { String::new() }
                );
                self.governor.event("refused", &format!("GPU lease refused: {why}"), false)?;
                return Err(ResourceError::Refused(vec![why]));
            }
            if let Some(l) = self.loaded.as_ref().filter(|_| ours.is_some()) {
                self.put_aside = Some((l.model.clone(), l.file_bytes, l.gpu_offload));
                let model = l.model.clone();
                self.unload_now("lease", &format!("for {requester}: {purpose}"))?;
                self.governor.event("lease", &format!("{model} is set aside while {requester} uses the GPU ({purpose})."), true)?;
                set_aside = true;
            }
        }
        let id = self.next_lease;
        self.next_lease += 1;
        let vram_mib = if request.exclusive { gpu.total_mib } else { request.vram_mib };
        let lease = Lease {
            id,
            requester: requester.to_owned(),
            purpose: purpose.to_owned(),
            gpu: gpu.id.clone(),
            vram_mib,
            exclusive: request.exclusive,
            model_set_aside: set_aside,
        };
        // The baseline is measured after any unload, before the plug-in allocates.
        let first_on_gpu = !self.leases.values().any(|h| h.lease.gpu == gpu.id);
        let baseline = if first_on_gpu {
            self.governor.sample(None).gpus.into_iter().find(|g| g.id == gpu.id).and_then(|g| g.used_mib)
        } else {
            None
        };
        self.leases.insert(id, Held { lease: lease.clone(), ttl: request.ttl, renewed: Instant::now() });
        self.rebuild_book(&gpu.id, baseline);
        let amount = if request.exclusive { "the whole GPU".to_owned() } else { format!("{vram_mib} MiB") };
        self.governor.event("lease", &format!("GPU lease {id} to {requester} for {purpose}: {amount} on {}", gpu.name), false)?;
        Ok(lease)
    }

    /// Set the whole GPU aside for a plug-in (the simple form of a lease).
    pub fn request_unload(&mut self, requester: &str, reason: &str) -> Result<Lease, ResourceError> {
        self.request_gpu(requester, reason, &GpuRequest { gpu: None, vram_mib: 0, exclusive: true, ttl: Duration::from_secs(3600) })
    }

    /// Keep a lease alive. A lease not renewed within its time ends.
    pub fn renew(&mut self, lease_id: u64) -> Result<(), ResourceError> {
        let h = self.leases.get_mut(&lease_id).ok_or(ResourceError::UnknownLease(lease_id))?;
        h.renewed = Instant::now();
        Ok(())
    }

    /// Release a lease. When no lease is left in the way, the set-aside
    /// model is reloaded (after measuring again). Returns the reloaded model;
    /// if there is not room yet, the release still stands and `tick` retries.
    pub fn release(&mut self, lease_id: u64) -> Result<Option<String>, ResourceError> {
        self.end_lease(lease_id, "release", "released the GPU")
    }

    fn end_lease(&mut self, lease_id: u64, kind: &str, what: &str) -> Result<Option<String>, ResourceError> {
        let held = self.leases.remove(&lease_id).ok_or(ResourceError::UnknownLease(lease_id))?;
        self.rebuild_book(&held.lease.gpu, None);
        self.governor.event(kind, &format!("{} {what} (lease {lease_id}, {})", held.lease.requester, held.lease.purpose), kind == "expire")?;
        self.reload_if_possible()
    }

    fn reload_if_possible(&mut self) -> Result<Option<String>, ResourceError> {
        if self.leases.values().any(|h| h.lease.exclusive) || self.loaded.is_some() {
            return Ok(None);
        }
        let Some((model, bytes, gpu)) = self.put_aside.take() else { return Ok(None) };
        match self.load(&model, bytes, gpu) {
            Ok(_) => {
                self.governor.event("reload", &format!("{model} is loaded again."), true)?;
                Ok(Some(model))
            }
            Err(e @ (ResourceError::Refused(_) | ResourceError::Unmeasured(_))) => {
                // Not enough room yet (the plug-in may still be freeing it).
                // The release stands; tick() tries the reload again.
                let why = match &e {
                    ResourceError::Refused(w) | ResourceError::Unmeasured(w) => w.join("; "),
                    _ => String::new(),
                };
                self.governor.event("refused", &format!("{model} could not be reloaded yet: {why}. Lu will try again."), true)?;
                self.put_aside = Some((model, bytes, gpu));
                Ok(None)
            }
            Err(e) => {
                self.put_aside = Some((model, bytes, gpu));
                Err(e)
            }
        }
    }

    /// Call periodically. Ends leases not renewed in time (a crashed
    /// plug-in), then puts an idle model aside after the grace period, or
    /// sooner under measured memory pressure. Never during a generation or
    /// within `min_resident` of loading. Returns whether it unloaded.
    pub fn tick(&mut self) -> Result<bool, ResourceError> {
        let expired: Vec<u64> = self.leases.iter().filter(|(_, h)| h.renewed.elapsed() >= h.ttl).map(|(id, _)| *id).collect();
        for id in expired {
            self.end_lease(id, "expire", "did not renew its GPU lease; it has ended")?;
        }
        let pressure = self.governor.check_pressure()?;
        if self.loaded.is_none() && self.put_aside.is_some() && self.leases.is_empty() {
            self.reload_if_possible()?;
        }
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
