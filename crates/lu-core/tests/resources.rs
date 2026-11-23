use lu_core::resources::{
    measure, meminfo_mib, Admission, Engine, Governor, Gpu, GpuRequest, IdlePolicy, ModelOwner, Need, ResourceError, Sample,
};
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn base(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lu-res-{name}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A machine whose numbers the test controls: 32 GiB RAM, a 32 GiB GPU.
#[derive(Clone)]
struct Machine(Arc<Mutex<Sample>>);
impl Machine {
    fn new(ram_free: u64, vram_used: u64) -> Self {
        Self(Arc::new(Mutex::new(Sample {
            ram_total_mib: Some(32_768),
            ram_available_mib: Some(ram_free),
            gpus: vec![Gpu { name: "Test GPU".into(), id: "gpu0".into(), total_mib: 32_624, used_mib: Some(vram_used) }],
            disk_free_mib: Some(600_000),
        })))
    }
    fn governor(&self, dir: &std::path::Path) -> Governor {
        let m = self.0.clone();
        Governor::with_sampler(&dir.join("resources.sqlite"), Box::new(move |_| m.lock().unwrap().clone())).unwrap()
    }
    fn set(&self, f: impl FnOnce(&mut Sample)) {
        f(&mut self.0.lock().unwrap());
    }
}

/// An engine that takes real-looking memory from the machine when loading.
struct FakeEngine {
    machine: Machine,
    /// (RAM, VRAM) MiB each model really takes.
    takes: (u64, u64),
    loaded: Arc<Mutex<Vec<String>>>,
}
impl Engine for FakeEngine {
    fn load(&mut self, model: &str) -> Result<(), String> {
        let (r, v) = self.takes;
        self.machine.set(|s| {
            s.ram_available_mib = s.ram_available_mib.map(|x| x - r);
            s.gpus[0].used_mib = s.gpus[0].used_mib.map(|x| x + v);
        });
        self.loaded.lock().unwrap().push(format!("load {model}"));
        Ok(())
    }
    fn unload(&mut self) -> Result<(), String> {
        let (r, v) = self.takes;
        self.machine.set(|s| {
            s.ram_available_mib = s.ram_available_mib.map(|x| x + r);
            s.gpus[0].used_mib = s.gpus[0].used_mib.map(|x| x - v);
        });
        self.loaded.lock().unwrap().push("unload".into());
        Ok(())
    }
}

#[test]
fn a_load_that_does_not_fit_is_refused_with_the_numbers() {
    let dir = base("refuse");
    let machine = Machine::new(20_000, 30_000); // only 2,624 MiB VRAM free
    let gov = machine.governor(&dir);
    let need = Need { ram_mib: 1024, vram_mib: 18_000, ..Need::default() };
    match gov.admit("loading qwen3.8-27b", &need).unwrap() {
        Admission::Refuse(why) => {
            assert_eq!(why.len(), 1);
            assert!(why[0].contains("VRAM on Test GPU: needs 18000 MiB plus 512 MiB kept free, 2624 MiB free"), "{}", why[0]);
        }
        other => panic!("{other:?}"),
    }
    // Unknown is not free: an unmeasurable GPU is not admitted.
    machine.set(|s| s.gpus[0].used_mib = None);
    assert!(matches!(gov.admit("loading", &need).unwrap(), Admission::Unmeasured(_)));
    machine.set(|s| s.gpus.clear());
    assert!(matches!(gov.admit("loading", &need).unwrap(), Admission::Unmeasured(_)));
    // RAM and disk are checked the same way.
    machine.set(|s| s.ram_available_mib = Some(3000));
    let cpu_job = Need { ram_mib: 1000, disk_mib: 1000, disk_path: Some(dir.clone()), ..Need::default() };
    assert!(matches!(gov.admit("archive extraction", &cpu_job).unwrap(), Admission::Refuse(w) if w[0].starts_with("RAM")));
    // Refusals are journalled and handed to each chat's model once.
    let notices = gov.notices_for("c-1").unwrap();
    assert_eq!(notices.len(), 4);
    assert!(notices[0].text.starts_with("Not started: loading qwen3.8-27b."));
    assert!(gov.notices_for("c-1").unwrap().is_empty());
    assert_eq!(gov.notices_for("c-2").unwrap().len(), 4);
}

#[test]
fn reserves_are_provisional_until_the_owner_sets_them() {
    let dir = base("reserves");
    let gov = Machine::new(20_000, 0).governor(&dir);
    let r = gov.reserves().unwrap();
    assert!(!r.owner_set);
    gov.set_reserves(6144, 1024, 20_480).unwrap();
    let r = gov.reserves().unwrap();
    assert_eq!((r.ram_mib, r.vram_mib, r.disk_mib, r.owner_set), (6144, 1024, 20_480, true));
    // Kept across a restart.
    drop(gov);
    let again = Machine::new(20_000, 0).governor(&dir);
    assert!(again.reserves().unwrap().owner_set);
}

#[test]
fn queued_jobs_start_in_order_when_memory_frees_up() {
    let dir = base("queue");
    let machine = Machine::new(6000, 0);
    let gov = machine.governor(&dir);
    gov.enqueue("big", Need { ram_mib: 8000, ..Need::default() }).unwrap();
    gov.enqueue("small", Need { ram_mib: 100, ..Need::default() }).unwrap();
    // The small job fits but waits behind the big one: no starvation.
    assert!(gov.ready().unwrap().is_empty());
    machine.set(|s| s.ram_available_mib = Some(13_000));
    assert_eq!(gov.ready().unwrap(), ["big", "small"]);
    assert!(gov.ready().unwrap().is_empty());
}

#[test]
fn the_owner_measures_footprints_and_reuses_them() {
    let dir = base("footprint");
    let machine = Machine::new(24_000, 1000);
    let gov = machine.governor(&dir);
    let log = Arc::new(Mutex::new(Vec::new()));
    let engine = FakeEngine { machine: machine.clone(), takes: (1500, 17_000), loaded: log.clone() };
    let mut owner = ModelOwner::new(&gov, engine, IdlePolicy::default());
    // Never loaded: an estimate from the file size (16 GiB file -> 20.5 GiB VRAM).
    let est = gov.footprint("qwen3.8-27b-Q4", 16 * 1024 * 1024 * 1024, true).unwrap();
    assert!(!est.measured);
    assert_eq!(est.vram_mib, 16_384 + 4096 + 512);
    let got = owner.load("qwen3.8-27b-Q4", 16 * 1024 * 1024 * 1024, true).unwrap();
    assert!(got.measured);
    assert_eq!((got.ram_mib, got.vram_mib), (1500, 17_000));
    assert_eq!(owner.loaded_model(), Some("qwen3.8-27b-Q4"));
    // Next time the measured number is used.
    let again = gov.footprint("qwen3.8-27b-Q4", 16 * 1024 * 1024 * 1024, true).unwrap();
    assert!(again.measured && again.vram_mib == 17_000);
    // Loading another model replaces it; the journal shows both.
    owner.load("gemma4-12b-Q4", 8 * 1024 * 1024 * 1024, true).unwrap();
    assert_eq!(*log.lock().unwrap(), ["load qwen3.8-27b-Q4", "unload", "load gemma4-12b-Q4"]);
    let kinds: Vec<_> = gov.events().unwrap().into_iter().map(|(k, _)| k).collect();
    assert_eq!(kinds, ["load", "unload", "load"]);
}

#[test]
fn a_plugin_borrows_the_memory_and_the_model_comes_back() {
    let dir = base("lease");
    let machine = Machine::new(24_000, 1000);
    let gov = machine.governor(&dir);
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut owner = ModelOwner::new(&gov, FakeEngine { machine: machine.clone(), takes: (1500, 17_000), loaded: log.clone() }, IdlePolicy::default());
    owner.load("qwen3.8-27b", 16 << 30, true).unwrap();

    // Never during a generation.
    owner.begin_generation().unwrap();
    assert_eq!(owner.request_unload("nvp", "4K render"), Err(ResourceError::Busy));
    owner.end_generation();

    let nvp = owner.request_unload("nvp", "4K render").unwrap();
    // A second plug-in cannot share a GPU leased exclusively.
    assert!(matches!(owner.request_unload("lu-ide", "full build"), Err(ResourceError::Refused(_))));
    assert_eq!(owner.loaded_model(), None);
    assert_eq!(machine.0.lock().unwrap().gpus[0].used_mib, Some(1000), "the VRAM is free for the plug-in");
    // Nothing else may load while the model is set aside.
    assert!(matches!(owner.load("other", 1 << 30, true), Err(ResourceError::Refused(_))));
    assert_eq!(owner.release(nvp.id).unwrap().as_deref(), Some("qwen3.8-27b"));
    assert_eq!(owner.loaded_model(), Some("qwen3.8-27b"));
    assert_eq!(owner.release(nvp.id), Err(ResourceError::UnknownLease(nvp.id)));
    // The model was told, in short lines.
    let notices: Vec<_> = gov.notices_for("c-1").unwrap().into_iter().map(|n| n.text).collect();
    assert!(notices[0].contains("set aside while nvp uses the GPU"), "{notices:?}");
    assert!(notices.last().unwrap().contains("loaded again"));

    // If memory is gone when the lease ends, the reload is refused, not forced.
    let lease = owner.request_unload("nvp", "render").unwrap();
    machine.set(|s| s.gpus[0].used_mib = Some(30_000)); // something else took the card
    assert_eq!(owner.release(lease.id).unwrap(), None);
    assert_eq!(owner.loaded_model(), None);
    assert!(gov.notices_for("c-1").unwrap().iter().any(|n| n.text.contains("could not be reloaded yet")));
    // When the memory comes back, the next tick reloads it.
    machine.set(|s| s.gpus[0].used_mib = Some(1000));
    owner.tick().unwrap();
    assert_eq!(owner.loaded_model(), Some("qwen3.8-27b"));
}

#[test]
fn an_idle_model_is_put_aside_under_pressure_but_never_mid_generation() {
    let dir = base("idle");
    let machine = Machine::new(24_000, 1000);
    let gov = machine.governor(&dir);
    let policy = IdlePolicy { idle_grace: Duration::from_secs(3600), min_resident: Duration::from_millis(50) };
    let mut owner = ModelOwner::new(&gov, FakeEngine { machine: machine.clone(), takes: (1500, 17_000), loaded: Arc::default() }, policy);
    owner.load("qwen3.6-35b-a3b", 20 << 30, true).unwrap();
    assert!(!owner.tick().unwrap(), "fresh and not under pressure");
    thread::sleep(Duration::from_millis(80));
    machine.set(|s| s.ram_available_mib = Some(2000)); // below the 4096 reserve
    owner.begin_generation().unwrap();
    assert!(!owner.tick().unwrap(), "a generation is never interrupted");
    owner.end_generation();
    assert!(owner.tick().unwrap());
    assert_eq!(owner.loaded_model(), None);
    let texts: Vec<_> = gov.events().unwrap().into_iter().map(|(_, t)| t).collect();
    assert!(texts.iter().any(|t| t.starts_with("RAM is low")), "{texts:?}");
    assert!(texts.iter().any(|t| t.contains("unloaded because memory is low")), "{texts:?}");
    // Pressure is noted once, and its end once.
    machine.set(|s| s.ram_available_mib = Some(20_000));
    gov.check_pressure().unwrap();
    gov.check_pressure().unwrap();
    let kinds: Vec<_> = gov.events().unwrap().into_iter().map(|(k, _)| k).filter(|k| k == "pressure" || k == "relieved").collect();
    assert_eq!(kinds, ["pressure", "relieved"]);
}

#[test]
fn real_measurements_on_this_machine() {
    let s = measure(Some(&std::env::temp_dir()));
    let total = s.ram_total_mib.expect("total RAM is measurable on Windows and Linux");
    let free = s.ram_available_mib.expect("free RAM is measurable on Windows and Linux");
    assert!(total > 512 && free <= total, "{s:?}");
    assert!(s.disk_free_mib.is_some_and(|d| d > 0), "{s:?}");
    for g in &s.gpus {
        assert!(g.total_mib > 0 && g.used_mib.is_none_or(|u| u <= g.total_mib + 64), "{g:?}");
    }
    assert_eq!(meminfo_mib("MemTotal:  32768000 kB\nMemAvailable: 1048576 kB\n", "MemAvailable"), Some(1024));
    assert_eq!(meminfo_mib("MemAvailable: 1048576 bytes\n", "MemAvailable"), None);
}

#[test]
#[ignore = "prints what Lu measures on this machine"]
fn print_measurements() {
    println!("{:#?}", measure(Some(&std::env::temp_dir())));
}

fn lease(vram_mib: u64, ttl_ms: u64) -> GpuRequest {
    GpuRequest { gpu: None, vram_mib, exclusive: false, ttl: Duration::from_millis(ttl_ms) }
}

#[test]
fn a_gpu_lease_that_fits_leaves_lus_model_loaded() {
    let dir = base("lease-fits");
    let machine = Machine::new(24_000, 1000);
    let gov = machine.governor(&dir);
    let mut owner = ModelOwner::new(&gov, FakeEngine { machine: machine.clone(), takes: (1500, 17_000), loaded: Arc::default() }, IdlePolicy::default());
    owner.load("qwen3.8-27b", 16 << 30, true).unwrap(); // 18,000 of 32,624 MiB used
    let l = owner.request_gpu("nvp", "LTX 2.5 clip", &lease(10_000, 60_000)).unwrap();
    assert!(!l.model_set_aside);
    assert_eq!(owner.loaded_model(), Some("qwen3.8-27b"));
    // Before NVP allocates, the 10,000 MiB is held back from Lu's loads.
    let need = Need { vram_mib: 5000, ..Need::default() };
    match gov.admit("a second model", &need).unwrap() {
        Admission::Refuse(w) => assert!(w[0].contains("after 10000 MiB held for plug-ins"), "{}", w[0]),
        other => panic!("{other:?}"),
    }
    // Once NVP has taken it, it is not counted twice.
    machine.set(|s| s.gpus[0].used_mib = Some(28_000));
    let small = Need { vram_mib: 3000, ..Need::default() };
    assert_eq!(gov.admit("a small model", &small).unwrap(), Admission::Admit);
    owner.release(l.id).unwrap();
    assert!(owner.leases().is_empty());
}

#[test]
fn lus_model_is_set_aside_only_if_that_makes_room() {
    let dir = base("lease-aside");
    let machine = Machine::new(24_000, 1000);
    let gov = machine.governor(&dir);
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut owner = ModelOwner::new(&gov, FakeEngine { machine: machine.clone(), takes: (1500, 17_000), loaded: log.clone() }, IdlePolicy::default());
    owner.load("qwen3.8-27b", 16 << 30, true).unwrap();
    // 40 GB cannot fit even with Lu's model gone: refused, model untouched.
    match owner.request_gpu("nvp", "LTX 2.5 at 4K", &lease(40_000, 60_000)) {
        Err(ResourceError::Refused(w)) => assert!(w[0].contains("would free only 17000 MiB more"), "{}", w[0]),
        other => panic!("{other:?}"),
    }
    assert_eq!(owner.loaded_model(), Some("qwen3.8-27b"));
    // 20 GB fits once Lu's model is set aside.
    let l = owner.request_gpu("nvp", "LTX 2.5", &lease(20_000, 60_000)).unwrap();
    assert!(l.model_set_aside);
    assert_eq!(owner.loaded_model(), None);
    // While NVP holds 20 GB, Lu's 17 GB model cannot come back.
    assert!(matches!(owner.load("qwen3.8-27b", 16 << 30, true), Err(ResourceError::Refused(_))));
    // NVP takes its 20 GB, releases the lease while still freeing it: the
    // reload measures the memory still in use and waits instead of forcing.
    machine.set(|s| s.gpus[0].used_mib = Some(21_000));
    assert_eq!(owner.release(l.id).unwrap(), None);
    assert_eq!(owner.loaded_model(), None);
    machine.set(|s| s.gpus[0].used_mib = Some(1000));
    owner.tick().unwrap();
    assert_eq!(owner.loaded_model(), Some("qwen3.8-27b"));
    assert_eq!(log.lock().unwrap().last().map(String::as_str), Some("load qwen3.8-27b"));
}

#[test]
fn a_crashed_plugins_lease_expires_and_the_model_returns() {
    let dir = base("lease-expire");
    let machine = Machine::new(24_000, 1000);
    let gov = machine.governor(&dir);
    let policy = IdlePolicy { idle_grace: Duration::from_secs(3600), min_resident: Duration::from_secs(3600) };
    let mut owner = ModelOwner::new(&gov, FakeEngine { machine: machine.clone(), takes: (1500, 17_000), loaded: Arc::default() }, policy);
    owner.load("qwen3.8-27b", 16 << 30, true).unwrap();
    let l = owner.request_gpu("nvp", "LTX 2.5", &GpuRequest { gpu: None, vram_mib: 0, exclusive: true, ttl: Duration::from_millis(100) }).unwrap();
    thread::sleep(Duration::from_millis(60));
    owner.renew(l.id).unwrap();
    thread::sleep(Duration::from_millis(60));
    owner.tick().unwrap();
    assert_eq!(owner.leases().len(), 1, "renewed in time");
    thread::sleep(Duration::from_millis(120));
    owner.tick().unwrap();
    assert!(owner.leases().is_empty());
    assert_eq!(owner.loaded_model(), Some("qwen3.8-27b"));
    let notices: Vec<_> = gov.notices_for("c-1").unwrap().into_iter().map(|n| n.text).collect();
    assert!(notices.iter().any(|t| t.contains("did not renew its GPU lease")), "{notices:?}");
    assert_eq!(owner.renew(l.id), Err(ResourceError::UnknownLease(l.id)));
}
