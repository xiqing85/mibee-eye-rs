//! Thermal subsystem (SPEC §3.7): an MLX90640 radiometric side sensor for
//! dual-spectrum devices — absolute temperature readings + pseudo-color
//! snapshots + web hardware bring-up (pins/bus/address config, discovery
//! scan). Ported from the mibee-eye-thermal bring-up repo; the GB28181
//! channel stays the visible-light camera, this module only feeds the
//! family readings surface (webui thermal card, SSE `thermal.temperature`).
//!
//! Bus ownership follows the proven single-mutex design: the poll loop
//! holds `pass` for one read pass, a scan sweep acquires it for the probe
//! walk — whoever holds it owns the pins.

pub mod ee;
pub mod error;
pub mod i2c;
pub mod mlx90640;
pub mod render;
pub mod scan;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{BusSpec, ThermalConfig};
use i2c::{BitBang, MlxBus, Twi};
use mlx90640::{AuxReadings, Mlx90640, HEIGHT, PIXELS, WIDTH};

/// One scan run's outcome (hits + why unusable pairs failed).
#[derive(Debug, Default, Clone)]
pub struct ScanState {
    pub running: bool,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
    pub hits: Vec<scan::Hit>,
    pub pair_errors: Vec<String>,
    pub error: Option<String>,
}

/// Runtime sensor target (bus + address), hot-reconfigured by the web
/// hardware API and re-read by the poll loop every pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensorTarget {
    pub bus: BusSpec,
    pub addr: u8,
}

/// Shared control block for the hardware API: live target, bus-ownership
/// handshake, last scan result.
pub struct HardwareCtl {
    target: RwLock<SensorTarget>,
    pass: Mutex<()>,
    scan: Mutex<ScanState>,
}

impl HardwareCtl {
    fn new(bus: BusSpec, addr: u8) -> Self {
        Self {
            target: RwLock::new(SensorTarget { bus, addr }),
            pass: Mutex::new(()),
            scan: Mutex::new(ScanState::default()),
        }
    }

    pub fn target(&self) -> SensorTarget {
        self.target.read().expect("hw target lock").clone()
    }

    pub fn set_target(&self, t: SensorTarget) {
        *self.target.write().expect("hw target lock") = t;
    }

    pub fn scan_state(&self) -> ScanState {
        self.scan.lock().expect("scan lock").clone()
    }

    /// Run one discovery sweep on the calling thread. Returns false (and
    /// does nothing) when a scan is already running. Blocks until the poll
    /// loop releases the bus — up to one bit-bang pass, ~40 s.
    pub fn run_scan(&self, gpio_candidates: &[(u32, u32)]) -> bool {
        {
            let mut st = self.scan.lock().expect("scan lock");
            if st.running {
                return false;
            }
            *st = ScanState {
                running: true,
                started_ms: now_ms(),
                ..ScanState::default()
            };
        }
        let (hits, pair_errors, error) = self.scan_sweep(gpio_candidates);
        let mut st = self.scan.lock().expect("scan lock");
        st.running = false;
        st.finished_ms = Some(now_ms());
        st.hits = hits;
        st.pair_errors = pair_errors;
        st.error = error;
        true
    }

    fn scan_sweep(
        &self,
        gpio_candidates: &[(u32, u32)],
    ) -> (Vec<scan::Hit>, Vec<String>, Option<String>) {
        // Acquire bus ownership; a bit-bang frame pass holds it for ~40 s.
        let sweep_started = Instant::now();
        let _bus = loop {
            match self.pass.try_lock() {
                Ok(g) => break g,
                Err(_) if sweep_started.elapsed() < Duration::from_secs(90) => {
                    thread::sleep(Duration::from_millis(150))
                }
                Err(_) => {
                    return (
                        Vec::new(),
                        Vec::new(),
                        Some("sensor loop did not release the bus within 90s".into()),
                    )
                }
            }
        };
        // Hardware TWI first; GPIO pairs only when no TWI hit — a working
        // hardware bus beats bit-bang, and GPIO probing unbinds the TWI
        // controller on boards where the pins are muxed to it.
        let mut hits = scan::scan_twi();
        let mut pair_errors = Vec::new();
        if hits.is_empty() {
            for &(sda, scl) in gpio_candidates {
                let (found, err) = scan::scan_gpio_pair(sda, scl);
                if let Some(e) = err {
                    pair_errors.push(e);
                }
                hits.extend(found);
            }
        }
        (hits, pair_errors, None)
    }
}

/// Last-known-good snapshot kept for the untrusted payload shape
/// (SPEC §3.7 M4 dialect: `trusted:false` + `last_known_good` + `reason`).
#[derive(Debug, Clone)]
struct GoodFrame {
    timestamp_ms: u64,
    ta: f64,
    /// Kept for diagnostics parity with the live reading.
    #[allow(dead_code)]
    vdd: f64,
    to: [f32; PIXELS],
}

/// Shared latest reading. Present from construction; `trusted:false` with
/// `reason:"warmup"` until the first sensor pass lands.
struct ReadingState {
    frame_number: u64,
    timestamp_ms: u64,
    trusted: bool,
    reason: &'static str,
    ta: f64,
    vdd: f64,
    to: [f32; PIXELS],
    sensor_fps: f32,
    last_good: Option<GoodFrame>,
}

pub struct ThermalService {
    hw: Arc<HardwareCtl>,
    state: RwLock<ReadingState>,
    stop: Arc<AtomicBool>,
    /// Write-once-at-boot wiring (the poll thread already holds a clone of
    /// the Arc, so the path lives behind a lock instead of get_mut).
    persist_path: RwLock<Option<std::path::PathBuf>>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Inline TOML value for a bus spec (overlay + scan snippets share the
/// shape; must parse back through `BusSpec`).
pub fn bus_toml(bus: &BusSpec) -> String {
    match bus {
        BusSpec::Twi { device } => format!("bus = {{ type = \"twi\", device = \"{device}\" }}"),
        BusSpec::Gpio { sda, scl } => {
            format!("bus = {{ type = \"gpio\", sda = {sda}, scl = {scl} }}")
        }
        BusSpec::Auto => "bus = { type = \"auto\" }".to_string(),
    }
}

/// Persisted overlay written by `PUT /api/hardware/config` (the `[thermal]`
/// table). The wrapper matters: fields live *under* `[thermal]`, and
/// parsing straight into `ThermalConfig` would silently fall back to
/// defaults.
pub fn overlay_toml(bus: &BusSpec, addr: u8) -> String {
    format!(
        "# mibee-eye thermal hardware overlay -- written by PUT /api/hardware/config\n\
         [thermal]\n{}\naddr = {:#04x}\n",
        bus_toml(bus),
        addr
    )
}

#[derive(serde::Deserialize)]
struct OverlayFile {
    thermal: ThermalConfig,
}

/// Load the boot overlay (sibling `thermal-hardware.toml` of the config
/// file). Best-effort: a bad overlay logs and is ignored.
pub fn load_overlay(path: &std::path::Path) -> Option<(BusSpec, u8)> {
    let text = std::fs::read_to_string(path).ok()?;
    match toml::from_str::<OverlayFile>(&text) {
        Ok(f) => Some((f.thermal.bus, f.thermal.addr)),
        Err(e) => {
            eprintln!(
                "thermal: ignoring bad hardware overlay {}: {e}",
                path.display()
            );
            None
        }
    }
}

enum AnyBus {
    Twi(Twi),
    Gpio(BitBang),
}

impl MlxBus for AnyBus {
    fn read_reg16(&mut self, reg: u16) -> error::Result<u16> {
        match self {
            AnyBus::Twi(b) => b.read_reg16(reg),
            AnyBus::Gpio(b) => b.read_reg16(reg),
        }
    }
    fn read_block(&mut self, reg: u16, buf: &mut [u8]) -> error::Result<()> {
        match self {
            AnyBus::Twi(b) => b.read_block(reg, buf),
            AnyBus::Gpio(b) => b.read_block(reg, buf),
        }
    }
    #[allow(dead_code)]
    fn write_reg16(&mut self, reg: u16, value: u16) -> error::Result<()> {
        match self {
            AnyBus::Twi(b) => b.write_reg16(reg, value),
            AnyBus::Gpio(b) => b.write_reg16(reg, value),
        }
    }
}

fn open_bus(spec: &BusSpec, addr: u8) -> error::Result<AnyBus> {
    match spec {
        BusSpec::Twi { device } => Ok(AnyBus::Twi(Twi::open(device, addr)?)),
        BusSpec::Gpio { sda, scl } => {
            scan::release_gpio_pair(*sda, *scl);
            Ok(AnyBus::Gpio(BitBang::new(*sda, *scl, addr)?))
        }
        BusSpec::Auto => match scan::auto_detect() {
            Some((hit, false)) => {
                let device = hit.bus.trim_start_matches("twi:").to_string();
                Ok(AnyBus::Twi(Twi::open(&device, hit.addr)?))
            }
            Some((hit, true)) => {
                let pins = hit.bus.trim_start_matches("gpio:");
                let (sda, scl) = pins
                    .split_once(',')
                    .ok_or_else(|| error::Error::Bus("auto-detect returned bad pair".into()))?;
                let (sda, scl): (u32, u32) = (
                    sda.parse()
                        .map_err(|_| error::Error::Bus("bad pin".into()))?,
                    scl.parse()
                        .map_err(|_| error::Error::Bus("bad pin".into()))?,
                );
                scan::release_gpio_pair(sda, scl);
                Ok(AnyBus::Gpio(BitBang::new(sda, scl, hit.addr)?))
            }
            None => Err(error::Error::Bus(
                "auto-detect found no MLX90640 (run a scan from the hardware card)".into(),
            )),
        },
    }
}

impl ThermalService {
    /// Build the service, apply the boot overlay, spawn the poll thread.
    /// Inert until the first pass lands — the payload reports
    /// `trusted:false` + `reason:"warmup"` in the meantime.
    pub fn start(cfg: &ThermalConfig) -> Arc<Self> {
        let (bus, addr) = (cfg.bus.clone(), cfg.addr);
        let svc = Arc::new(Self {
            hw: Arc::new(HardwareCtl::new(bus, addr)),
            state: RwLock::new(ReadingState {
                frame_number: 0,
                timestamp_ms: now_ms(),
                trusted: false,
                reason: "warmup",
                ta: 0.0,
                vdd: 0.0,
                to: [0.0; PIXELS],
                sensor_fps: 0.0,
                last_good: None,
            }),
            stop: Arc::new(AtomicBool::new(false)),
            persist_path: RwLock::new(None),
        });
        {
            let svc = Arc::clone(&svc);
            let emissivity = cfg.emissivity;
            thread::Builder::new()
                .name("mlx90640-poll".into())
                .spawn(move || poll_loop(svc, emissivity))
                .expect("spawn thermal poll thread");
        }
        svc
    }

    /// Boot wiring for the overlay file (before the web server accepts
    /// requests, so no race with readers).
    pub fn set_persist_path(&self, path: Option<std::path::PathBuf>) {
        *self.persist_path.write().expect("persist lock") = path;
    }

    pub fn hardware(&self) -> &HardwareCtl {
        &self.hw
    }

    /// Shared handle for spawning background work (the scan sweep).
    pub fn hardware_arc(&self) -> Arc<HardwareCtl> {
        Arc::clone(&self.hw)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// SPEC §3.7 payload: `current` (grid=true) and the SSE shape
    /// (grid=false) come from the same builder.
    pub fn payload(&self, grid: bool) -> serde_json::Value {
        let st = self.state.read().expect("thermal state lock");
        let (max_i, min_i) = arg_extremes(&st.to);
        let mut v = serde_json::json!({
            "timestamp_ms": st.timestamp_ms,
            "frame_number": st.frame_number,
            "trusted": st.trusted,
            "ta": (st.ta * 100.0).round() / 100.0,
            "vdd": (st.vdd * 1000.0).round() / 1000.0,
            "sensor_fps": (st.sensor_fps * 1000.0).round() / 1000.0,
            "max": spot_json(&st.to, max_i),
            "min": spot_json(&st.to, min_i),
            "spots": [],
        });
        if !st.trusted {
            v["reason"] = serde_json::json!(st.reason);
            if let Some(g) = &st.last_good {
                let (g_max, g_min) = arg_extremes(&g.to);
                v["last_known_good"] = serde_json::json!({
                    "timestamp_ms": g.timestamp_ms,
                    "ta": (g.ta * 100.0).round() / 100.0,
                    "max": spot_json(&g.to, g_max),
                    "min": spot_json(&g.to, g_min),
                    "age_ms": st.timestamp_ms.saturating_sub(g.timestamp_ms),
                });
            }
        }
        if grid {
            v["grid"] = serde_json::json!({
                "w": WIDTH,
                "h": HEIGHT,
                "unit": "deg_c",
                "values": st.to.as_slice(),
            });
        }
        v
    }

    /// To grid for rendering (snapshots).
    pub fn grid(&self) -> [f32; PIXELS] {
        self.state.read().expect("thermal state lock").to
    }

    pub fn available(&self) -> bool {
        self.state.read().expect("thermal state lock").trusted
    }

    pub fn radiometric(&self) -> bool {
        self.state
            .read()
            .expect("thermal state lock")
            .last_good
            .is_some()
    }

    pub fn persist_path(&self) -> Option<std::path::PathBuf> {
        self.persist_path.read().expect("persist lock").clone()
    }
}

fn spot_json(to: &[f32], idx: usize) -> serde_json::Value {
    serde_json::json!({
        "value_c": (f64::from(to[idx]) * 10.0).round() / 10.0,
        "x": idx % WIDTH,
        "y": idx / WIDTH,
    })
}

fn arg_extremes(to: &[f32]) -> (usize, usize) {
    let mut max_i = 0usize;
    let mut min_i = 0usize;
    for (i, &v) in to.iter().enumerate() {
        if v > to[max_i] {
            max_i = i;
        }
        if v < to[min_i] {
            min_i = i;
        }
    }
    (max_i, min_i)
}

fn poll_loop(svc: Arc<ThermalService>, emissivity: f64) {
    let mut eeprom: Option<ee::Eeprom> = None;
    let mut failures = 0u32;
    let mut last_target = svc.hw.target();
    while !svc.stop.load(Ordering::Relaxed) {
        // Bus ownership: a scan sweep holds the pass lock — idle until it
        // is done instead of fighting it over the pins.
        let _pass_guard = match svc.hw.pass.try_lock() {
            Ok(g) => g,
            Err(_) => {
                thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        let started = Instant::now();
        let target = svc.hw.target();
        if target != last_target {
            eeprom = None; // different sensor → calibration block is stale
            last_target = target.clone();
            eprintln!(
                "thermal: target changed to {:?} (re-reading EEPROM)",
                target
            );
        }
        let pass = (|| -> Option<([f32; PIXELS], ee::CommonParams, AuxReadings)> {
            let bus = open_bus(&target.bus, target.addr).ok()?;
            let mut sensor = Mlx90640::new(bus);
            let ee = match &eeprom {
                Some(ee) => ee.clone(),
                None => {
                    let words = sensor.read_eeprom().ok()?;
                    let ee = ee::Eeprom::from_words(&words);
                    eeprom = Some(ee.clone());
                    ee
                }
            };
            let aux = sensor.read_aux().ok()?;
            let frame = sensor.read_frame_raw().ok()?;
            let (to, common) = ee.compute_to(&frame, &aux, emissivity, None);
            Some((to, common, aux))
        })();
        let mut st = svc.state.write().expect("thermal state lock");
        match pass {
            Some((to, common, _aux)) => {
                failures = 0;
                st.sensor_fps = 1.0f32 / started.elapsed().as_secs_f32().max(1.0 / 32.0);
                st.frame_number = st.frame_number.wrapping_add(1);
                st.timestamp_ms = now_ms();
                st.trusted = true;
                st.reason = "warmup";
                st.ta = common.ta;
                st.vdd = common.vdd;
                st.to = to;
                st.last_good = Some(GoodFrame {
                    timestamp_ms: st.timestamp_ms,
                    ta: common.ta,
                    vdd: common.vdd,
                    to,
                });
                if st.frame_number == 1 {
                    eprintln!("thermal: first real frame landed (ta={:.2})", common.ta);
                }
            }
            None => {
                failures += 1;
                st.sensor_fps = 0.0;
                st.timestamp_ms = now_ms();
                st.trusted = false;
                // Bus that never opened = wiring/bus trouble; opened but no
                // frame = sensor handshake lost. Both keep the last good.
                st.reason = "sensor_lost";
                if failures == 1 {
                    eprintln!("thermal: sensor pass failed (bus or wiring)");
                }
            }
        }
        drop(st);
        // Bus released with the pass guard — yield ownership before the
        // backoff sleep so a scan can slip in between passes.
        drop(_pass_guard);
        let backoff = if failures == 0 {
            1u64
        } else {
            u64::from(failures.min(30))
        };
        thread::sleep(Duration::from_secs(backoff));
    }
}

/// SSE pump: broadcast `thermal.temperature` (same payload as `current`
/// minus the grid) on a ≥1 s cadence (SPEC §3.7).
pub fn spawn_sse_pump(svc: Arc<ThermalService>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            crate::web::events::global_hub().broadcast("thermal.temperature", &svc.payload(false));
        }
    });
}

/// ScanState → SPEC §3.7 hardware JSON (hits carry their paste-ready
/// config snippet so the UI never re-implements formatting).
pub fn scan_json(st: &ScanState) -> serde_json::Value {
    serde_json::json!({
        "running": st.running,
        "started_ms": st.started_ms,
        "finished_ms": st.finished_ms,
        "hits": st.hits.iter().map(|h| serde_json::json!({
            "bus": h.bus,
            "addr": h.addr,
            "id": h.id,
            "config": scan::config_snippet(h),
        })).collect::<Vec<_>>(),
        "pair_errors": st.pair_errors,
        "error": st.error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_toml_roundtrips() {
        let text = overlay_toml(&BusSpec::Gpio { sda: 2, scl: 3 }, 0x33);
        assert!(text.contains("[thermal]"));
        let dir = std::env::temp_dir().join("mibee-thermal-overlay-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("thermal-hardware.toml");
        std::fs::write(&p, &text).unwrap();
        let (bus, addr) = load_overlay(&p).expect("overlay loads");
        assert_eq!(bus, BusSpec::Gpio { sda: 2, scl: 3 });
        assert_eq!(addr, 0x33);
        // Non-default addr so a silent-default parse would fail loudly.
        let text = overlay_toml(&BusSpec::Auto, 0x36);
        std::fs::write(&p, &text).unwrap();
        let (bus, addr) = load_overlay(&p).expect("overlay loads");
        assert_eq!(bus, BusSpec::Auto);
        assert_eq!(addr, 0x36);
    }

    #[test]
    fn payload_shape_warmup_then_fields() {
        let svc = dummy_service();
        let v = svc.payload(true);
        assert_eq!(v["trusted"], serde_json::json!(false));
        assert_eq!(v["reason"], serde_json::json!("warmup"));
        assert!(v["grid"]["values"].as_array().unwrap().len() == PIXELS);
        let v = svc.payload(false);
        assert!(v.get("grid").is_none());
    }

    #[test]
    fn scan_state_machine_transitions() {
        let hw = HardwareCtl::new(BusSpec::Auto, 0x33);
        assert!(!hw.scan_state().running);
        assert!(hw.run_scan(&[(226, 227)]));
        let st = hw.scan_state();
        assert!(!st.running);
        assert!(st.finished_ms.is_some());
        assert!(st.error.is_none());
        hw.set_target(SensorTarget {
            bus: BusSpec::Gpio { sda: 8, scl: 10 },
            addr: 0x30,
        });
        assert_eq!(hw.target().addr, 0x30);
    }

    fn dummy_service() -> Arc<ThermalService> {
        Arc::new(ThermalService {
            hw: Arc::new(HardwareCtl::new(BusSpec::Auto, 0x33)),
            state: RwLock::new(ReadingState {
                frame_number: 0,
                timestamp_ms: now_ms(),
                trusted: false,
                reason: "warmup",
                ta: 0.0,
                vdd: 0.0,
                to: [20.0; PIXELS],
                sensor_fps: 0.0,
                last_good: None,
            }),
            stop: Arc::new(AtomicBool::new(false)),
            persist_path: RwLock::new(None),
        })
    }
}
