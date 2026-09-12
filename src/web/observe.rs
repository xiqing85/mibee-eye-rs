//! Observability backend (SPEC §3.2): real-time system/process resource
//! sampling, a bounded log ring, request tracing, and Prometheus export.
//!
//! Everything is real-time only — the sampler keeps just the previous
//! snapshot to compute rates, and the rings are bounded, so memory use does
//! not grow with uptime. The `/proc` parsers are pure functions over file
//! contents so they are unit-testable on any machine.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// ─────────────────────────────────────────────────────────────────────────
// /proc parsers (pure)
// ─────────────────────────────────────────────────────────────────────────

/// Cumulative CPU times from `/proc/stat`'s aggregate `cpu` line, in ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuTimes {
    pub idle: u64,
    pub total: u64,
}

/// Parse the aggregate `cpu` line of `/proc/stat`.
pub fn parse_cpu_stat(content: &str) -> Option<CpuTimes> {
    let line = content.lines().find(|l| l.starts_with("cpu "))?;
    let vals: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse::<u64>().ok())
        .collect();
    if vals.is_empty() {
        return None;
    }
    // Linux: user nice system idle [iowait irq softirq steal ...]
    let idle = vals.get(3).copied().unwrap_or(0) + vals.get(4).copied().unwrap_or(0);
    let total: u64 = vals.iter().sum();
    Some(CpuTimes { idle, total })
}

/// (MemTotal, MemAvailable) in bytes from `/proc/meminfo`.
pub fn parse_meminfo(content: &str) -> Option<(u64, u64)> {
    let field = |name: &str| {
        content.lines().find_map(|l| {
            let rest = l.strip_prefix(name)?;
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            Some(kb * 1024)
        })
    };
    Some((field("MemTotal:")?, field("MemAvailable:").unwrap_or(0)))
}

/// Aggregate (rx_bytes, tx_bytes) over all physical interfaces from
/// `/proc/net/dev` (loopback excluded so traffic to ourselves is not counted).
pub fn parse_net_dev(content: &str) -> (u64, u64) {
    let mut rx = 0;
    let mut tx = 0;
    for line in content.lines().skip(2) {
        let Some((iface, data)) = line.split_once(':') else {
            continue;
        };
        if iface.trim() == "lo" {
            continue;
        }
        let vals: Vec<u64> = data
            .split_whitespace()
            .filter_map(|v| v.parse::<u64>().ok())
            .collect();
        rx += vals.first().copied().unwrap_or(0);
        tx += vals.get(8).copied().unwrap_or(0);
    }
    (rx, tx)
}

/// (utime, stime, starttime) in ticks from `/proc/<pid>/stat`.
///
/// The comm field `(…) may contain spaces` — split after the last `)`.
pub fn parse_self_stat(content: &str) -> Option<(u64, u64, u64)> {
    let rest = content.rsplit_once(')')?.1;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After the comm field, kernel fields are numbered from `state`=2:
    // utime=14, stime=15, starttime=22 → indexes 12, 13, 20 in `fields`.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let starttime: u64 = fields.get(19)?.parse().ok()?;
    Some((utime, stime, starttime))
}

/// (rchar, wchar) from `/proc/<pid>/io`.
pub fn parse_self_io(content: &str) -> Option<(u64, u64)> {
    let field = |name: &str| {
        content.lines().find_map(|l| {
            let rest = l.strip_prefix(name)?;
            rest.trim().parse::<u64>().ok()
        })
    };
    Some((field("rchar:")?, field("wchar:")?))
}

/// One CPU-tick duration in seconds (`clk_tck`, effectively always 100).
fn clk_tck() -> f64 {
    100.0
}

/// CPU percent between two samples (0..100, or 0 when no time elapsed).
pub fn cpu_percent_between(prev: CpuTimes, cur: CpuTimes) -> f64 {
    let d_total = cur.total.saturating_sub(prev.total);
    let d_idle = cur.idle.saturating_sub(prev.idle);
    if d_total == 0 {
        return 0.0;
    }
    ((d_total - d_idle) as f64 / d_total as f64 * 100.0).clamp(0.0, 100.0)
}

/// Process CPU percent between two samples against `num_cpus`.
pub fn proc_cpu_percent(prev: (u64, u64), cur: (u64, u64), dt_secs: f64, num_cpus: f64) -> f64 {
    if dt_secs <= 0.0 || num_cpus <= 0.0 {
        return 0.0;
    }
    let d_ticks = cur.0.saturating_sub(prev.0) as f64 + cur.1.saturating_sub(prev.1) as f64;
    (d_ticks / clk_tck() / dt_secs * 100.0 / num_cpus).clamp(0.0, 100.0 * num_cpus)
}

// ─────────────────────────────────────────────────────────────────────────
// Sampler state
// ─────────────────────────────────────────────────────────────────────────

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One raw sampling point (cumulative counters; rates derive from deltas).
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    pub ts: u64,
    pub cpu: CpuTimes,
    pub proc_ticks: (u64, u64),
    pub net: (u64, u64),
}

/// A rendered snapshot as served by `GET /api/metrics/summary` (SPEC §3.2).
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub ts: u64,
    pub interval_ms: u64,
    pub system_cpu_percent: f64,
    pub load_avg: [f64; 3],
    pub mem_total: u64,
    pub mem_available: u64,
    /// `(path, total, used, free)` per relevant mount.
    pub disks: Vec<(String, u64, u64, u64)>,
    pub net_rx: u64,
    pub net_tx: u64,
    pub net_rx_rate: f64,
    pub net_tx_rate: f64,
    pub proc_cpu_percent: f64,
    pub rss_bytes: u64,
    pub open_fds: u64,
    pub uptime_secs: u64,
    pub io_read_bytes: u64,
    pub io_write_bytes: u64,
}

impl Snapshot {
    /// First-sample placeholder: shapes are real, rates are zero.
    fn initial() -> Self {
        Self {
            ts: 0,
            interval_ms: 0,
            system_cpu_percent: 0.0,
            load_avg: [0.0; 3],
            mem_total: 0,
            mem_available: 0,
            disks: Vec::new(),
            net_rx: 0,
            net_tx: 0,
            net_rx_rate: 0.0,
            net_tx_rate: 0.0,
            proc_cpu_percent: 0.0,
            rss_bytes: 0,
            open_fds: 0,
            uptime_secs: 0,
            io_read_bytes: 0,
            io_write_bytes: 0,
        }
    }
}

/// Application-attributed traffic counters (SPEC §3.2 `process.traffic`).
/// Linux exposes no per-process kernel network counters, so the HTTP
/// middleware and the protocol bridges bump these directly.
#[derive(Debug, Default)]
pub struct Traffic {
    pub http_rx: AtomicU64,
    pub http_tx: AtomicU64,
    pub rtsp_tx: AtomicU64,
    pub gb28181_tx: AtomicU64,
}

/// Bounded FIFO ring for log / request entries.
struct Ring<T> {
    items: Mutex<VecDeque<T>>,
    cap: usize,
}

impl<T> Ring<T> {
    fn new(cap: usize) -> Self {
        Self {
            items: Mutex::new(VecDeque::with_capacity(cap)),
            cap,
        }
    }

    fn push(&self, item: T) {
        let mut guard = self.items.lock().unwrap();
        if guard.len() == self.cap {
            guard.pop_front();
        }
        guard.push_back(item);
    }

    fn newest_first(&self) -> Vec<T>
    where
        T: Clone,
    {
        let guard = self.items.lock().unwrap();
        guard.iter().rev().cloned().collect()
    }
}

use std::collections::VecDeque;

/// One structured log entry (SPEC §3.2 `/api/logs`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct LogEntry {
    pub ts: u64,
    pub level: String,
    pub target: String,
    pub message: String,
    pub request_id: Option<String>,
}

/// One traced Web API request (SPEC §3.2 `/api/requests`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RequestEntry {
    pub id: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub duration_ms: f64,
    pub ts: u64,
}

/// Shared observability state owned by [`crate::web::api::AppState`].
///
/// Prefer [`Observe::new`]; a `Default` impl is provided for test helpers.
pub struct Observe {
    /// Latest rendered snapshot (replaced by the sampler every tick).
    snapshot: Mutex<Snapshot>,
    /// Previous raw sample backing rate computation.
    prev: Mutex<Option<Sample>>,
    logs: Ring<LogEntry>,
    requests: Ring<RequestEntry>,
    pub traffic: Traffic,
    /// Next request id (rendered as 6 hex digits).
    next_request_id: AtomicU64,
}

impl Default for Observe {
    fn default() -> Self {
        Self::new()
    }
}

impl Observe {
    pub fn new() -> Self {
        Self {
            snapshot: Mutex::new(Snapshot::initial()),
            prev: Mutex::new(None),
            logs: Ring::new(1000),
            requests: Ring::new(500),
            traffic: Traffic::default(),
            next_request_id: AtomicU64::new(1),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().unwrap().clone()
    }

    pub fn logs_newest_first(&self) -> Vec<LogEntry> {
        self.logs.newest_first()
    }

    pub fn requests_newest_first(&self) -> Vec<RequestEntry> {
        self.requests.newest_first()
    }

    pub fn push_log(&self, entry: LogEntry) {
        self.logs.push(entry);
    }

    pub fn alloc_request_id(&self) -> String {
        format!(
            "{:06x}",
            self.next_request_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    pub fn push_request(&self, entry: RequestEntry) {
        self.requests.push(entry);
    }

    /// Record one raw sample and render the new [`Snapshot`] (real /proc
    /// reads plus the previous sample's deltas).
    pub fn record_sample(&self, cur: Sample, num_cpus: f64, recording_root: &str) {
        let prev = self.prev.lock().unwrap().replace(cur);
        let dt_secs = match prev {
            Some(p) => (cur.ts.saturating_sub(p.ts)).max(1),
            None => 0,
        } as f64
            / 1000.0;

        let mut snapshot = self.snapshot.lock().unwrap();
        snapshot.ts = cur.ts;
        snapshot.interval_ms = (dt_secs * 1000.0).round() as u64;

        if let Some(prev) = prev {
            snapshot.system_cpu_percent = cpu_percent_between(prev.cpu, cur.cpu);
            snapshot.proc_cpu_percent =
                proc_cpu_percent(prev.proc_ticks, cur.proc_ticks, dt_secs, num_cpus);
            snapshot.net_rx_rate = (cur.net.0.saturating_sub(prev.net.0)) as f64 / dt_secs;
            snapshot.net_tx_rate = (cur.net.1.saturating_sub(prev.net.1)) as f64 / dt_secs;
        }
        snapshot.net_rx = cur.net.0;
        snapshot.net_tx = cur.net.1;

        if let Some((total, avail)) = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .as_deref()
            .and_then(parse_meminfo)
        {
            snapshot.mem_total = total;
            snapshot.mem_available = avail;
        }
        if let Ok(avg) = std::fs::read_to_string("/proc/loadavg") {
            let vals: Vec<f64> = avg
                .split_whitespace()
                .filter_map(|v| v.parse().ok())
                .collect();
            for (slot, v) in snapshot.load_avg.iter_mut().zip(vals) {
                *slot = v;
            }
        }
        snapshot.disks = relevant_mounts(recording_root);
        snapshot.rss_bytes = read_rss_bytes();
        snapshot.open_fds = count_open_fds();
        if let Some((r, w)) = std::fs::read_to_string("/proc/self/io")
            .ok()
            .as_deref()
            .and_then(parse_self_io)
        {
            snapshot.io_read_bytes = r;
            snapshot.io_write_bytes = w;
        }
    }
}

fn read_rss_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                let rest = l.strip_prefix("VmRSS:")?;
                let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
                Some(kb * 1024)
            })
        })
        .unwrap_or(0)
}

fn count_open_fds() -> u64 {
    std::fs::read_dir("/proc/self/fd")
        .map(|entries| entries.filter_map(|e| e.ok()).count() as u64)
        .unwrap_or(0)
}

/// Stat a mount's capacity via `statvfs(2)`. `None` when the path is absent.
fn statvfs(path: &str) -> Option<(u64, u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(std::path::Path::new(path).as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    if rc != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    let total = st.f_blocks * frsize;
    let free = st.f_bfree * frsize;
    let used = total.saturating_sub(st.f_bavail * frsize);
    Some((total, used, free))
}

/// Relevant mounts for the summary: root plus the recording data partition
/// when it is a separate, existing path.
fn relevant_mounts(recording_root: &str) -> Vec<(String, u64, u64, u64)> {
    let mut out = Vec::new();
    if let Some((total, used, free)) = statvfs("/") {
        out.push(("/".to_string(), total, used, free));
    }
    if recording_root != "/" {
        if let Some((total, used, free)) = statvfs(recording_root) {
            out.push((recording_root.to_string(), total, used, free));
        }
    }
    out
}

/// Collect one raw [`Sample`] from the live system.
pub fn read_sample() -> Sample {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let cpu = std::fs::read_to_string("/proc/stat")
        .ok()
        .as_deref()
        .and_then(parse_cpu_stat)
        .unwrap_or(CpuTimes { idle: 0, total: 0 });
    let proc_ticks = std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| parse_self_stat(&s))
        .map(|(u, st, _)| (u, st))
        .unwrap_or((0, 0));
    let net = std::fs::read_to_string("/proc/net/dev")
        .map(|s| parse_net_dev(&s))
        .unwrap_or((0, 0));
    Sample {
        ts: now.as_millis() as u64,
        cpu,
        proc_ticks,
        net,
    }
}

fn num_cpus() -> f64 {
    std::thread::available_parallelism()
        .map(|n| n.get() as f64)
        .unwrap_or(1.0)
}

/// Background sampler: refresh the shared [`Snapshot`] every `interval` and
/// mirror the values into Prometheus gauges. `recording_root` is the disk
/// path reported in the summary's disk list.
pub fn spawn_sampler(observe: Arc<Observe>, interval: Duration, recording_root: String) {
    tokio::spawn(async move {
        let cpus = num_cpus();
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let sample = read_sample();
            observe.record_sample(sample, cpus, &recording_root);
            publish_gauges(&observe.snapshot());
        }
    });
}

fn publish_gauges(snapshot: &Snapshot) {
    use metrics::gauge;
    gauge!("mibee_system_cpu_percent").set(snapshot.system_cpu_percent);
    gauge!("mibee_process_cpu_percent").set(snapshot.proc_cpu_percent);
    gauge!("mibee_process_memory_bytes").set(snapshot.rss_bytes as f64);
    gauge!("mibee_process_open_fds").set(snapshot.open_fds as f64);
    gauge!("mibee_system_memory_used_bytes")
        .set((snapshot.mem_total - snapshot.mem_available) as f64);
    gauge!("mibee_system_memory_total_bytes").set(snapshot.mem_total as f64);
    for (path, total, used, _) in &snapshot.disks {
        gauge!("mibee_system_disk_total_bytes", "path" => path.clone()).set(*total as f64);
        gauge!("mibee_system_disk_used_bytes", "path" => path.clone()).set(*used as f64);
    }
    metrics::counter!("mibee_system_net_rx_bytes").absolute(snapshot.net_rx);
    metrics::counter!("mibee_system_net_tx_bytes").absolute(snapshot.net_tx);
}

// ─────────────────────────────────────────────────────────────────────────
// Handlers (SPEC §3.2)
// ─────────────────────────────────────────────────────────────────────────

use axum::extract::{Query, State};
use axum::Json;
use serde_json::json;
use std::collections::HashMap;

use super::api::{ok_env, AppState};

/// `GET /api/metrics/summary` — real-time system + process snapshot.
pub async fn metrics_summary(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let s = state.observe.snapshot();
    let traffic = &state.observe.traffic;
    ok_env(json!({
        "ts": s.ts,
        "interval_ms": s.interval_ms,
        "system": {
            "cpu_percent": s.system_cpu_percent,
            "load_avg": s.load_avg,
            "memory": {
                "total": s.mem_total,
                "used": s.mem_total.saturating_sub(s.mem_available),
                "available": s.mem_available,
            },
            "disks": s.disks.iter().map(|(p, t, u, f)| json!({
                "path": p, "total": t, "used": u, "free": f,
            })).collect::<Vec<_>>(),
            "network": {
                "rx_bytes": s.net_rx, "tx_bytes": s.net_tx,
                "rx_rate": s.net_rx_rate, "tx_rate": s.net_tx_rate,
            },
        },
        "process": {
            "cpu_percent": s.proc_cpu_percent,
            "rss_bytes": s.rss_bytes,
            "open_fds": s.open_fds,
            "uptime": state.started.elapsed().as_secs(),
            "io_read_bytes": s.io_read_bytes,
            "io_write_bytes": s.io_write_bytes,
            "storage_bytes": crate::recording::recorded_bytes(),
            "traffic": {
                "http_rx_bytes": traffic.http_rx.load(Ordering::Relaxed),
                "http_tx_bytes": traffic.http_tx.load(Ordering::Relaxed),
                "rtsp_tx_bytes": traffic.rtsp_tx.load(Ordering::Relaxed),
                "gb28181_tx_bytes": traffic.gb28181_tx.load(Ordering::Relaxed),
            },
        },
    }))
}

/// `GET /api/logs?limit=&level=` — recent log ring, newest first.
pub async fn logs_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(200)
        .clamp(1, 1000);
    let min_level = params.get("level").and_then(|l| level_rank(l)).unwrap_or(0);
    let entries: Vec<_> = state
        .observe
        .logs_newest_first()
        .into_iter()
        .filter(|e| level_rank(&e.level).unwrap_or(0) >= min_level)
        .take(limit)
        .collect();
    ok_env(json!({ "entries": entries }))
}

fn level_rank(level: &str) -> Option<u8> {
    match level {
        "debug" => Some(0),
        "info" => Some(1),
        "warn" => Some(2),
        "error" => Some(3),
        _ => None,
    }
}

/// `GET /api/requests?limit=` — recent Web API request traces, newest first.
pub async fn requests_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(100)
        .clamp(1, 500);
    let entries = state
        .observe
        .requests_newest_first()
        .into_iter()
        .take(limit)
        .collect::<Vec<_>>();
    ok_env(json!({ "entries": entries }))
}

/// Compose a [`RequestEntry`] — used by the server's request middleware.
pub fn make_request_entry(
    id: String,
    method: &str,
    path: &str,
    status: u16,
    duration: Duration,
) -> RequestEntry {
    RequestEntry {
        id,
        method: method.to_string(),
        path: path.to_string(),
        status,
        duration_ms: duration.as_secs_f64() * 1000.0,
        ts: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Process-wide logger: stderr formatting + ring capture
// ─────────────────────────────────────────────────────────────────────────

/// Composite `log` implementation: delegates rendering to `env_logger`
/// (stderr / journald) and tees every record into the observability ring
/// backing `GET /api/logs`. The protocol libraries log through the `log`
/// facade, so their events are captured verbatim.
pub struct TeeLogger {
    env: env_logger::Logger,
    observe: Arc<Observe>,
}

impl log::Log for TeeLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.env.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        self.env.log(record);
        self.observe.push_log(LogEntry {
            ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            level: record.level().to_string().to_lowercase(),
            target: record.target().to_string(),
            message: record.args().to_string(),
            request_id: None,
        });
    }

    fn flush(&self) {}
}

/// Install the tee logger exactly once. Later calls are no-ops (the `log`
/// crate only accepts one global logger).
pub fn init_logger(observe: Arc<Observe>) {
    let env =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).build();
    let max_level = env.filter();
    let _ = log::set_boxed_logger(Box::new(TeeLogger { env, observe }));
    log::set_max_level(max_level);
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// Ring evicts the oldest entry at capacity and `newest_first` reverses.
    #[test]
    fn test_log_ring_capacity_and_order() {
        let observe = Observe::new();
        for i in 0..5u64 {
            observe.push_log(LogEntry {
                ts: i,
                level: "info".into(),
                target: "t".into(),
                message: format!("m{i}"),
                request_id: None,
            });
        }
        let all = observe.logs_newest_first();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0].message, "m4");
        assert_eq!(all[4].message, "m0");
    }

    /// One full sample round-trip: rates zero on the first sample, real
    /// values present on the second (this machine has a /proc).
    #[test]
    fn test_record_sample_snapshot_shape() {
        let observe = Observe::new();
        observe.record_sample(read_sample(), 4.0, "/mnt/data");
        std::thread::sleep(Duration::from_millis(20));
        observe.record_sample(read_sample(), 4.0, "/mnt/data");
        let s = observe.snapshot();
        assert!(s.ts > 0);
        assert!(s.mem_total > 0, "meminfo parsed");
        assert!(s.rss_bytes > 0, "VmRSS parsed");
        assert!(s.disks.iter().any(|(p, _, _, _)| p == "/"));
        // The recording root is only listed when it exists on this machine
        // (CI/workstation layouts differ from the Pi).
        if std::path::Path::new("/mnt/data").exists() {
            assert!(s.disks.iter().any(|(p, _, _, _)| p == "/mnt/data"));
        }
        assert!((0.0..=400.0).contains(&s.proc_cpu_percent));
    }

    /// Request ids are unique, hex-formatted, and entries keep insertion
    /// order (newest first on read).
    #[test]
    fn test_request_ring_and_ids() {
        let observe = Observe::new();
        let id1 = observe.alloc_request_id();
        let id2 = observe.alloc_request_id();
        assert_ne!(id1, id2);
        observe.push_request(make_request_entry(
            id1,
            "GET",
            "/api/status",
            200,
            Duration::from_millis(3),
        ));
        observe.push_request(make_request_entry(
            id2.clone(),
            "POST",
            "/api/config",
            200,
            Duration::from_millis(9),
        ));
        let entries = observe.requests_newest_first();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "/api/config");
        assert_eq!(entries[0].id.clone(), id2.clone());
        assert!((entries[1].duration_ms - 3.0).abs() < 1e-9);
    }

    /// `logs_handler` filters by minimum level (`?level=warn` drops info).
    #[tokio::test]
    async fn test_logs_handler_level_filter() {
        let observe = Arc::new(Observe::new());
        observe.push_log(LogEntry {
            ts: 1,
            level: "info".into(),
            target: "t".into(),
            message: "keep-out".into(),
            request_id: None,
        });
        observe.push_log(LogEntry {
            ts: 2,
            level: "warn".into(),
            target: "t".into(),
            message: "keep-in".into(),
            request_id: None,
        });
        let state = Arc::new(AppState::default());
        // Swap in the populated ring through the public API surface.
        for e in observe.logs_newest_first().into_iter().rev() {
            state.observe.push_log(e);
        }

        let app = axum::Router::new()
            .route("/api/logs", axum::routing::get(logs_handler))
            .with_state(state.clone());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/logs?limit=10&level=warn")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let entries = json["data"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "only the warn entry survives: {json}");
        assert_eq!(entries[0]["message"], "keep-in");
    }
}

#[cfg(test)]
mod parser_tests {
    use super::*;

    const CPU_STAT: &str = "cpu  100 0 100 300 100 0 0 0 0 0\ncpu0 50 0 50 150 50 0 0 0 0 0\ncpu1 50 0 50 150 50 0 0 0 0 0\nintr 123\n";

    #[test]
    fn test_parse_cpu_stat_aggregate_includes_iowait_in_idle() {
        let t = parse_cpu_stat(CPU_STAT).expect("parses");
        // idle=300, iowait=100 → 400; total = 100+0+100+300+100 = 600
        assert_eq!(t.idle, 400);
        assert_eq!(t.total, 600);
    }

    #[test]
    fn test_cpu_percent_between() {
        let prev = CpuTimes {
            idle: 400,
            total: 600,
        };
        // +200 total, +150 idle → 25% busy
        let cur = CpuTimes {
            idle: 550,
            total: 800,
        };
        assert!((cpu_percent_between(prev, cur) - 25.0).abs() < 1e-9);
        assert_eq!(cpu_percent_between(prev, prev), 0.0);
    }

    #[test]
    fn test_parse_meminfo() {
        let src = "MemTotal:       8000000 kB\nMemFree:        100000 kB\nMemAvailable:   3000000 kB\nSwapTotal:           0 kB\n";
        let (total, avail) = parse_meminfo(src).expect("parses");
        assert_eq!(total, 8_000_000 * 1024);
        assert_eq!(avail, 3_000_000 * 1024);
    }

    #[test]
    fn test_parse_net_dev_skips_loopback() {
        // Each interface line carries 8 receive stats then 8 transmit stats;
        // tx_bytes therefore sits at index 8 of the value list.
        let src = "Inter-|   Receive                                                \
                   |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast\
                   |bytes    packets errs drop fifo colls carrier\n\
                   lo:  999999     999    0    0    0     0          0         0    9999      999    0    0    0     0       0          0\n\
                   eth0: 150000     200    0    0    0     0          0         0   90000      150    0    0    0     0       0          0\n";
        let (rx, tx) = parse_net_dev(src);
        assert_eq!(rx, 150_000); // lo excluded
        assert_eq!(tx, 90_000);
    }

    #[test]
    fn test_parse_self_stat_with_spaces_in_comm() {
        // Fields after the last ')' are indexed from state=2.
        let src = "42 (camera worke) S 1 2 3 0 -1 4194560 100 0 0 0 77 33 0 0 20 0 4 0 123456 1 1 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0\n";
        let (utime, stime, starttime) = parse_self_stat(src).expect("parses");
        assert_eq!(utime, 77);
        assert_eq!(stime, 33);
        assert_eq!(starttime, 123456);
    }

    #[test]
    fn test_parse_self_io() {
        let src = "rchar: 123456\nwchar: 654321\nsyscr: 100\nsyscw: 50\n";
        let (rchar, wchar) = parse_self_io(src).expect("parses");
        assert_eq!(rchar, 123_456);
        assert_eq!(wchar, 654_321);
    }

    #[test]
    fn test_proc_cpu_percent_one_core_all_busy() {
        // 100 ticks over 1s on 1 CPU = 100%.
        assert_eq!(proc_cpu_percent((0, 0), (100, 0), 1.0, 1.0), 100.0);
        // Same on 4 cores = 25%.
        assert_eq!(proc_cpu_percent((0, 0), (100, 0), 1.0, 4.0), 25.0);
        assert_eq!(proc_cpu_percent((0, 0), (0, 0), 1.0, 4.0), 0.0);
    }
}
