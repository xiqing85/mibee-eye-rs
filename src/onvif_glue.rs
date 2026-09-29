//! Product-side glue for the ONVIF Device stack (`onvif-device-rs` 0.8).
//!
//! The protocol logic lives in the library; this module is exactly the
//! "call + business glue" layer the protocol-first rule allows:
//!
//! - [`wire_onvif_server`] — the single wiring point that assembles the
//!   `OnvifServer` the binary (and the wire tests) start: Device service
//!   family + host hooks, Media store migration (`register_media_actions`),
//!   PTZ registration, the Imaging face, Media2 gating, the IP filter and
//!   HTTP Digest config seams. Extracted from `main.rs` so the integration
//!   tests exercise byte-for-byte the same registration the product ships.
//! - [`ImagingGlue`] — the minimal [`ImagingParams`] source. The OV5647
//!   capture pipeline exposes no runtime parameter manager yet, so values
//!   live in an honest in-memory store (reads reflect writes — the same
//!   convention the virtual PTZ state always used); the sensor is fixed
//!   focus (no motor), so focus Move acknowledges without acting.
//! - [`DeviceHooksGlue`] — host effects for Device service writes:
//!   clock/reboot/factory-default requests are **observed and logged,
//!   never executed** (a network command must not restart the device),
//!   and the system log / support info texts are built from real
//!   application state ([`DeviceReport`]).
//! - [`parse_ip_filter`] — `[onvif] ip_filter` config entries (IPv4 or
//!   IPv4/prefix) into the library's [`IpFilter`] allow-list.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use async_trait::async_trait;

use crate::onvif::device::{
    DeviceHandler, DeviceHooks, DeviceServiceHandlers, IpEntry, IpFilter, IpFilterMode,
};
use crate::onvif::events::EventsService;
use crate::onvif::imaging::{ImagingParamError, ImagingParams};
use crate::onvif::media::{
    OnvifMediaConfig, SetVideoEncoderConfigurationHandler, SharedMediaConfig,
};
use crate::onvif::ptz::PtzHandler;
use crate::onvif::ptz_state::PtzState;
use crate::onvif::server::{OnvifActionHandler, OnvifConfig, OnvifServer};
use crate::onvif::types::{OnvifError, RequestInfo};

// ---------------------------------------------------------------------------
// IP filter config parsing
// ---------------------------------------------------------------------------

/// Parse `[onvif] ip_filter` entries into an allow-mode filter.
///
/// Each entry is a dotted-quad IPv4 address (implicitly `/32`) or
/// `address/prefix` with prefix 0–32. An empty (or all-blank) list is
/// `Ok(None)` — no filter installed, the historical open listener.
///
/// # Errors
/// Names the first malformed entry.
pub fn parse_ip_filter(entries: &[String]) -> Result<Option<IpFilter>, String> {
    if entries.is_empty() {
        return Ok(None);
    }
    let mut parsed = Vec::with_capacity(entries.len());
    for raw in entries {
        let entry = raw.trim();
        if entry.is_empty() {
            continue;
        }
        let (addr, prefix_len) = match entry.split_once('/') {
            Some((addr, prefix)) => {
                let prefix_len: u8 = prefix
                    .trim()
                    .parse()
                    .map_err(|_| format!("bad prefix length in '{raw}'"))?;
                if prefix_len > 32 {
                    return Err(format!("prefix length out of range 0-32 in '{raw}'"));
                }
                (addr.trim(), prefix_len)
            }
            None => (entry, 32),
        };
        addr.parse::<std::net::Ipv4Addr>()
            .map_err(|_| format!("not an IPv4 address: '{raw}'"))?;
        parsed.push(IpEntry {
            ipv4: addr.to_string(),
            prefix_len,
        });
    }
    if parsed.is_empty() {
        return Ok(None);
    }
    Ok(Some(IpFilter {
        enabled: true,
        mode: IpFilterMode::Allow,
        entries: parsed,
    }))
}

// ---------------------------------------------------------------------------
// ImagingParams — minimal in-memory source
// ---------------------------------------------------------------------------

/// The four imaging parameters the ONVIF Imaging service reads. The
/// OV5647 pipeline has no runtime parameter bridge yet, so these are the
/// protocol-visible names only.
const IMAGING_PARAM_NAMES: [&str; 4] = ["Brightness", "Contrast", "Saturation", "Sharpness"];

/// Minimal [`ImagingParams`] source: an in-memory store seeded with the
/// neutral 0.5 for the four advertised parameters.
///
/// Honesty notes (deliberate product semantics, mirroring the virtual
/// PTZ state):
///
/// - reads reflect writes — a client that sets Brightness and reads it
///   back sees its own value (real round-trip state, no fake hardware
///   data);
/// - the capture pipeline does not consume these values yet — wiring
///   them to the V4L2 control layer is future camera work;
/// - `focus_move` keeps the trait's default no-op acknowledgment (the
///   sensor is fixed focus — there is no motor to drive), and exposure /
///   white-balance modes report `"AUTO"` (the trait defaults, matching
///   what the sensor's auto modes actually do).
pub struct ImagingGlue {
    values: RwLock<HashMap<String, f64>>,
}

impl ImagingGlue {
    /// A fresh store with every advertised parameter at the neutral 0.5.
    #[must_use]
    pub fn new() -> Self {
        Self {
            values: RwLock::new(
                IMAGING_PARAM_NAMES
                    .iter()
                    .map(|name| ((*name).to_string(), 0.5))
                    .collect(),
            ),
        }
    }

    fn read_values(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, f64>> {
        self.values
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_values(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, f64>> {
        self.values
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for ImagingGlue {
    fn default() -> Self {
        Self::new()
    }
}

impl ImagingParams for ImagingGlue {
    fn get_param(&self, name: &str) -> Result<f64, ImagingParamError> {
        self.read_values()
            .get(name)
            .copied()
            .ok_or_else(|| ImagingParamError::InvalidName(name.to_string()))
    }

    fn set_param(&self, name: &str, value: f64) -> Result<(), ImagingParamError> {
        if !IMAGING_PARAM_NAMES.contains(&name) {
            return Err(ImagingParamError::InvalidName(name.to_string()));
        }
        if !(0.0..=1.0).contains(&value) {
            return Err(ImagingParamError::OutOfRange {
                value,
                min: 0.0,
                max: 1.0,
            });
        }
        self.write_values().insert(name.to_string(), value);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DeviceHooks — observe-and-log, never execute
// ---------------------------------------------------------------------------

/// The UTC date-time fields the library hands `set_date_time`
/// (year, month, day, hour, minute, second).
pub type UtcDateTimeFields = (i32, i32, i32, i32, i32, i32);

/// Real application state the [`DeviceHooksGlue`] texts are built from.
/// Constructed by the binary from the loaded config at wiring time.
#[derive(Debug, Clone)]
pub struct DeviceReport {
    /// Firmware version string (the product's own `CARGO_PKG_VERSION`).
    pub version: String,
    /// One-line device identity, e.g. `"Pi Camera V1 — Raspberry Pi OV5647"`.
    pub device_line: String,
    /// One-line capture summary, e.g. `"1280x720@15fps h264 2000000bps"`.
    pub camera_line: String,
    /// Extra free-form status lines (GB28181 / AI enablement, …).
    pub notes: Vec<String>,
}

/// Host-side effects for the ONVIF Device service write operations.
///
/// Policy (matching the product's SIP-Date clock stance):
///
/// - `SetSystemDateAndTime` is **observed only** — a WARN is logged and
///   the request recorded for inspection, the system clock is never
///   touched by a network command;
/// - `SystemReboot` / `SetSystemFactoryDefault` are **refused as
///   effects** — logged and counted, never executed. Restarting or
///   wiping a deployed camera because a LAN client asked is not a
///   behavior this product ships; operators use systemctl / the web UI;
/// - `GetSystemLog` / `GetSystemSupportInformation` return real
///   application-level summaries from [`DeviceReport`] plus the live
///   uptime — never placeholder text.
pub struct DeviceHooksGlue {
    report: DeviceReport,
    started: Instant,
    last_clock_request: Mutex<Option<(UtcDateTimeFields, String)>>,
    reboot_requests: AtomicUsize,
    factory_default_requests: AtomicUsize,
}

impl DeviceHooksGlue {
    /// New hooks over the given report; uptime counts from here.
    #[must_use]
    pub fn new(report: DeviceReport) -> Self {
        Self {
            report,
            started: Instant::now(),
            last_clock_request: Mutex::new(None),
            reboot_requests: AtomicUsize::new(0),
            factory_default_requests: AtomicUsize::new(0),
        }
    }

    fn record_clock_request(&self, utc: UtcDateTimeFields, tz: &str) {
        let mut slot = self
            .last_clock_request
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = Some((utc, tz.to_string()));
    }

    /// The most recent `SetSystemDateAndTime` request, if any
    /// `(UTC fields, timezone)` — the observation latch tests read.
    pub fn last_clock_request(&self) -> Option<(UtcDateTimeFields, String)> {
        self.last_clock_request
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// How many `SystemReboot` requests have been observed (all refused).
    pub fn reboot_requests(&self) -> usize {
        self.reboot_requests.load(Ordering::Relaxed)
    }

    /// How many `SetSystemFactoryDefault` requests have been observed
    /// (all refused).
    pub fn factory_default_requests(&self) -> usize {
        self.factory_default_requests.load(Ordering::Relaxed)
    }

    fn uptime_line(&self) -> String {
        format!("uptime {}s", self.started.elapsed().as_secs())
    }
}

impl DeviceHooks for DeviceHooksGlue {
    fn set_date_time(&self, utc: UtcDateTimeFields, tz: &str) {
        // Observation only — same policy as the GB28181 SIP-Date drift
        // watcher: a network peer never adjusts this device's clock.
        log::warn!(
            "onvif: SetSystemDateAndTime({utc:?} tz={tz}) observed — the system clock is never adjusted by a network command"
        );
        self.record_clock_request(utc, tz);
    }

    fn reboot(&self) {
        log::warn!(
            "onvif: SystemReboot requested over ONVIF — refused (log-only); restart the service with systemctl instead"
        );
        self.reboot_requests.fetch_add(1, Ordering::Relaxed);
    }

    fn factory_default(&self, hard: bool) {
        log::warn!(
            "onvif: SetSystemFactoryDefault (hard={hard}) requested over ONVIF — refused (log-only); use the web UI reset"
        );
        self.factory_default_requests
            .fetch_add(1, Ordering::Relaxed);
    }

    fn system_log(&self) -> String {
        let mut text = format!(
            "mibee-eye-raspi-rs v{} — {}\n  {}\n  {}",
            self.report.version,
            self.uptime_line(),
            self.report.device_line,
            self.report.camera_line
        );
        for note in &self.report.notes {
            text.push_str("\n  ");
            text.push_str(note);
        }
        text
    }

    fn support_info(&self) -> String {
        let mut text = format!(
            "mibee-eye-raspi-rs v{} ONVIF camera service\n  {}\n  {}\n  {}\n  reboot requests refused: {}\n  factory-default requests refused: {}",
            self.report.version,
            self.uptime_line(),
            self.report.device_line,
            self.report.camera_line,
            self.reboot_requests(),
            self.factory_default_requests(),
        );
        for note in &self.report.notes {
            text.push_str("\n  ");
            text.push_str(note);
        }
        text
    }
}

// ---------------------------------------------------------------------------
// SetVideoEncoderConfiguration — accept-and-say-so wrapper
// ---------------------------------------------------------------------------

/// Delegating wrapper around the library's
/// `SetVideoEncoderConfigurationHandler` that adds the honest INFO line:
/// the shared media store accepts the write, but the capture pipeline
/// applies new encoder parameters on restart (it does not re-configure a
/// running V4L2 encoder mid-stream). The response bytes are the
/// library's, verbatim.
struct LoggingSetVideoEncoderConfigurationHandler {
    inner: SetVideoEncoderConfigurationHandler,
}

#[async_trait]
impl OnvifActionHandler for LoggingSetVideoEncoderConfigurationHandler {
    async fn handle(&self, body: &str, info: &RequestInfo) -> Result<String, OnvifError> {
        let response = self.inner.handle(body, info).await?;
        log::info!(
            "onvif: SetVideoEncoderConfiguration accepted (shared media store updated) — the capture pipeline applies new encoder parameters on restart"
        );
        Ok(response)
    }
}

// ---------------------------------------------------------------------------
// The wiring
// ---------------------------------------------------------------------------

/// Everything `wire_onvif_server` needs. Built by the binary from the
/// loaded config (and by the wire tests from fixtures).
pub struct OnvifWiringInput {
    /// The `[onvif]` config section (port, credentials, feature gates,
    /// IP filter entries).
    pub host: crate::config::ONVIFConfig,
    /// The `[device]` identity section.
    pub device: crate::config::DeviceConfig,
    /// The device's own IP (XAddr host when the request arrives on a
    /// wildcard-bound listener).
    pub device_ip: String,
    /// The Media service configuration (dims, RTSP/snapshot endpoints,
    /// profile tokens).
    pub media: OnvifMediaConfig,
    /// "Force an IDR frame now" seam — fired on every
    /// SetSynchronizationPoint (Media1 and Media2 faces). `None`
    /// acknowledges only.
    pub keyframe_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Host-side effects for Device service writes.
    pub hooks: Arc<dyn DeviceHooks>,
    /// The Imaging service parameter source.
    pub imaging: Arc<dyn ImagingParams>,
}

/// What `wire_onvif_server` hands back.
pub struct OnvifWiringOutput {
    /// The assembled server — call `start()`/`start_on()` on it.
    pub server: OnvifServer,
    /// The events pull-point publish seam, present exactly when
    /// `host.events_enabled` (the alarm bridge publishes MotionAlarm
    /// through it).
    pub events: Option<Arc<EventsService>>,
    /// The shared media store — `SetVideoEncoderConfiguration` writes
    /// land here; readers (both media faces) snapshot through it.
    pub media_store: SharedMediaConfig,
    /// The virtual PTZ state the PTZ service answers from.
    pub ptz_state: Arc<PtzState>,
}

/// The Device service family the product registers. Everything the
/// library's `DeviceHandler` dispatches except `GetServiceCapabilities`:
/// that local name is shared with the PTZ/media/imaging families and the
/// shared action map lets the media capabilities answer own it (library
/// routing recommendation) — registering a device-flavored twin first
/// would only be overwritten.
const DEVICE_SERVICE_ACTIONS: [&str; 36] = [
    // historical five
    "GetSystemDateAndTime",
    "GetDeviceInformation",
    "GetCapabilities",
    "GetServices",
    "GetScopes",
    // device completion (onvif-device-rs 0.8): clock, scopes, hostname,
    // network reads, discovery mode, users, system info, effects
    "SetSystemDateAndTime",
    "SetScopes",
    "AddScopes",
    "RemoveScopes",
    "SetHostname",
    "GetHostname",
    "GetNetworkDefaultGateway",
    "GetNetworkInterfaces",
    "GetNetworkProtocols",
    "GetDNS",
    "GetNTP",
    "SetDiscoveryMode",
    "GetDiscoveryMode",
    "CreateUsers",
    "DeleteUsers",
    "SetUser",
    "GetUsers",
    "GetWsdlUrl",
    "GetEndpointReference",
    "GetSystemSupportInformation",
    "GetSystemLog",
    "SetSystemFactoryDefault",
    "UpgradeSystemFirmware",
    "StartSystemRestore",
    "SystemReboot",
    // security family (issue #54): IP filter + access policy
    "GetIPAddressFilter",
    "SetIPAddressFilter",
    "AddIPAddressFilter",
    "RemoveIPAddressFilter",
    "GetAccessPolicy",
    "SetAccessPolicy",
];

/// The full PTZ dispatch set the library's `PtzHandler` routes
/// internally — the eleven historical actions plus the completion batch
/// (configuration options, home position, auxiliary commands,
/// capabilities).
const PTZ_ACTIONS: [&str; 17] = [
    "ContinuousMove",
    "AbsoluteMove",
    "RelativeMove",
    "Stop",
    "GetStatus",
    "GetPresets",
    "SetPreset",
    "GotoPreset",
    "RemovePreset",
    "GetNodes",
    "GetConfigurations",
    "GetConfigurationOptions",
    "SetConfiguration",
    "GotoHomePosition",
    "SetHomePosition",
    "SendAuxiliaryCommand",
    "GetServiceCapabilities",
];

/// Assemble the product's ONVIF SOAP server.
///
/// Registration order is load-bearing for the shared action names
/// (library routing semantics):
///
/// 1. Device family first (plain names, no conflicts at this point);
/// 2. PTZ, then **Media** — media's capabilities answer takes over the
///    bare `GetServiceCapabilities` slot (the library's documented
///    recommendation for full-stack wiring);
/// 3. **Imaging last** — it takes over `GetStatus` / `Stop` /
///    `GetServiceCapabilities` and routes by request shape
///    (`VideoSourceToken` / `timg:` ⇒ imaging), falling back to the
///    previous handler so non-imaging bodies keep the historical PTZ /
///    media answers.
///
/// Media2 is enabled after the Media1 faces so both read (and the one
/// write) the SAME shared store.
#[must_use = "the assembled server is useless without start()/start_on()"]
pub fn wire_onvif_server(input: OnvifWiringInput) -> OnvifWiringOutput {
    let OnvifWiringInput {
        host,
        device,
        device_ip,
        media,
        keyframe_hook,
        hooks,
        imaging,
    } = input;

    let onvif_cfg = OnvifConfig {
        port: host.port,
        username: host.username.clone(),
        password: host.password.clone(),
        // An empty password has always meant "auth off" for this host
        // (Config::load only warns); onvif-device-rs fail-closes unless
        // that is stated explicitly.
        allow_no_auth: host.password.is_empty(),
        http_digest: host.http_digest,
        ..Default::default()
    };
    let mut server = OnvifServer::new(&onvif_cfg);

    // IP filter (issue #54): ONE shared store behind both the
    // per-connection gate on the SOAP server and the device handlers'
    // Get/Set/Add/RemoveIPAddressFilter SOAP ops. Empty list = no gate.
    let filter_state = parse_ip_filter(&host.ip_filter)
        .ok()
        .flatten()
        .map(|filter| {
            Arc::new(std::sync::RwLock::new(filter)) as crate::onvif::device::IpFilterState
        });
    if let Some(state) = filter_state.clone() {
        server = server.with_ip_filter(state);
    }

    // Pull-Point events service: AI motion alarms publish as
    // MotionAlarm while an NVR holds a subscription. The publish seam
    // must be taken before start; None = disabled by config. The
    // wsnt:Subscribe push interface (library #50) rides the same route
    // with no extra wiring here.
    let events = host.events_enabled.then(|| server.enable_events());

    // Device service handlers. The library fail-closes on neutral
    // identity placeholders; Config::load backfills the documented
    // defaults, so an error here means the host explicitly configured
    // placeholder/empty identity — keep the remaining ONVIF services up
    // and say so instead of dying.
    let device_svc = DeviceServiceHandlers::new(device, host.port, device_ip)
        // Advertise the events service exactly when its routes are
        // served (enable_events above) — the pair must not disagree.
        .map(|svc| svc.with_events_support(host.events_enabled))
        // Same contract for the Media2 GetServices entry and the
        // /onvif/media2_service route (enable_media2 below).
        .map(|svc| svc.with_media2_support(host.media2_enabled))
        // ONVIF-side user directory (GetUsers et al.): the configured
        // WS-Security account, read-only view. Not an auth source.
        .and_then(|svc| {
            if host.username.is_empty() {
                return Ok(svc);
            }
            svc.with_users(vec![(host.username.clone(), "Administrator".to_string())])
        })
        .map(|svc| svc.with_hooks(hooks))
        .map(|svc| match &filter_state {
            Some(state) => svc.with_ip_filter(state.clone()),
            None => svc,
        })
        // AccessPolicy: default empty policy blob — Get answers empty,
        // Set is accepted into the store by the library (interpretation
        // is host-side and this host defers none).
        .map(|svc| svc.with_access_policy(Arc::new(std::sync::RwLock::new(Vec::new()))));
    match device_svc {
        Ok(svc) => {
            let device_svc = Arc::new(svc);
            for action in DEVICE_SERVICE_ACTIONS {
                server.register_handler(
                    action,
                    Box::new(DeviceHandler(Arc::clone(&device_svc))),
                );
            }
        }
        Err(e) => eprintln!(
            "onvif: device identity config rejected ({e}) — device service actions stay unregistered; set real values in [device]"
        ),
    }

    // Pre-auth actions per ONVIF Core spec — reachable without
    // authentication:
    //   GetCapabilities / GetServices: needed during NVR discovery so
    //     clients can read service endpoints before they have
    //     credentials to compute a digest.
    //   GetSystemDateAndTime: clients sync the clock before computing
    //     WS-Security username-token digests.
    for action in ["GetSystemDateAndTime", "GetCapabilities", "GetServices"] {
        server.register_anonymous_action(action);
    }

    // Virtual PTZ (in-memory state, no motor behind it).
    let ptz_state = Arc::new(PtzState::new());
    for action in PTZ_ACTIONS {
        server.register_handler(action, Box::new(PtzHandler(Arc::clone(&ptz_state))));
    }

    // Media service (onvif-device-rs 0.8 migration): one registration
    // covers the four historical actions plus the encoder configuration
    // family, the sync point (keyframe hook), the empty audio/OSD sets
    // and the media capabilities — all reading the shared store.
    let media_store: SharedMediaConfig = Arc::new(std::sync::RwLock::new(media));
    crate::onvif::media::register_media_actions(
        &mut server,
        Arc::clone(&media_store),
        keyframe_hook.clone(),
    );

    // Honest write semantics for SetVideoEncoderConfiguration (see the
    // wrapper's doc).
    server.register_handler(
        "SetVideoEncoderConfiguration",
        Box::new(LoggingSetVideoEncoderConfigurationHandler {
            inner: SetVideoEncoderConfigurationHandler::new(Arc::clone(&media_store)),
        }),
    );

    // Imaging face — takes over the shape-routed shared names
    // (GetStatus / Stop / GetServiceCapabilities) with the handlers
    // above as fallback.
    crate::onvif::imaging::register_imaging_actions(&mut server, imaging);

    // Media2 (ver20/media) — same shared store, same keyframe hook;
    // `media2_enabled = false` leaves both the route and the
    // GetServices advertisement at the historical bytes.
    if host.media2_enabled {
        server.enable_media2(Arc::clone(&media_store), keyframe_hook);
    }

    OnvifWiringOutput {
        server,
        events,
        media_store,
        ptz_state,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_ip_filter --------------------------------------------------

    #[test]
    fn ip_filter_empty_list_installs_nothing() {
        assert!(parse_ip_filter(&[]).unwrap().is_none());
    }

    #[test]
    fn ip_filter_blank_entries_installs_nothing() {
        let entries: Vec<String> = vec!["  ".to_string(), String::new()];
        assert!(parse_ip_filter(&entries).unwrap().is_none());
    }

    #[test]
    fn ip_filter_bare_address_is_host_prefix() {
        let filter = parse_ip_filter(&["192.168.1.5".to_string()])
            .unwrap()
            .expect("filter");
        assert!(filter.enabled);
        assert_eq!(filter.mode, IpFilterMode::Allow);
        assert_eq!(filter.entries.len(), 1);
        assert_eq!(filter.entries[0].ipv4, "192.168.1.5");
        assert_eq!(filter.entries[0].prefix_len, 32);
    }

    #[test]
    fn ip_filter_cidr_entry_parses() {
        let filter = parse_ip_filter(&["10.0.0.0/8".to_string(), " 172.16.0.0/12 ".to_string()])
            .unwrap()
            .expect("filter");
        assert_eq!(filter.entries.len(), 2);
        assert_eq!(filter.entries[0].prefix_len, 8);
        assert_eq!(filter.entries[1].ipv4, "172.16.0.0");
        assert_eq!(filter.entries[1].prefix_len, 12);
    }

    #[test]
    fn ip_filter_rejects_bad_address() {
        let err = parse_ip_filter(&["not-an-ip".to_string()]).unwrap_err();
        assert!(err.contains("not-an-ip"), "{err}");
    }

    #[test]
    fn ip_filter_rejects_bad_prefix() {
        let err = parse_ip_filter(&["192.168.1.0/33".to_string()]).unwrap_err();
        assert!(err.contains("33"), "{err}");
        let err = parse_ip_filter(&["192.168.1.0/abc".to_string()]).unwrap_err();
        assert!(err.contains("abc"), "{err}");
    }

    #[test]
    fn ip_filter_allow_mode_semantics() {
        let filter = parse_ip_filter(&["127.0.0.0/8".to_string()])
            .unwrap()
            .expect("filter");
        assert!(filter.allows_client_ip("127.0.0.1"));
        assert!(!filter.allows_client_ip("192.168.1.9"));
    }

    // -- ImagingGlue ------------------------------------------------------

    #[test]
    fn imaging_defaults_are_neutral() {
        let glue = ImagingGlue::new();
        for name in IMAGING_PARAM_NAMES {
            let v = glue.get_param(name).unwrap();
            assert!((v - 0.5).abs() < f64::EPSILON, "{name}: {v}");
        }
    }

    #[test]
    fn imaging_set_then_get_round_trips() {
        let glue = ImagingGlue::new();
        glue.set_param("Brightness", 0.25).unwrap();
        assert!((glue.get_param("Brightness").unwrap() - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn imaging_unknown_name_is_invalid() {
        let glue = ImagingGlue::new();
        assert!(matches!(
            glue.get_param("Hue"),
            Err(ImagingParamError::InvalidName(_))
        ));
        assert!(matches!(
            glue.set_param("Iris", 0.5),
            Err(ImagingParamError::InvalidName(_))
        ));
    }

    #[test]
    fn imaging_out_of_range_rejected() {
        let glue = ImagingGlue::new();
        assert!(matches!(
            glue.set_param("Contrast", 1.5),
            Err(ImagingParamError::OutOfRange { .. })
        ));
        assert!(matches!(
            glue.set_param("Contrast", -0.1),
            Err(ImagingParamError::OutOfRange { .. })
        ));
    }

    #[test]
    fn imaging_focus_move_acks_without_acting() {
        // OV5647 is fixed focus: the trait default acknowledges without
        // driving anything.
        let glue = ImagingGlue::new();
        glue.focus_move(crate::onvif::imaging::FocusMoveCmd {
            kind: crate::onvif::imaging::FocusMoveKind::Absolute,
            position: 0.5,
            speed: 1.0,
        })
        .unwrap();
    }

    #[test]
    fn imaging_modes_report_auto() {
        let glue = ImagingGlue::new();
        assert_eq!(glue.exposure_mode(), "AUTO");
        assert_eq!(glue.white_balance_mode(), "AUTO");
    }

    // -- DeviceHooksGlue --------------------------------------------------

    fn test_report() -> DeviceReport {
        DeviceReport {
            version: "9.9.9-test".to_string(),
            device_line: "Test Cam — TestCorp TC-2000".to_string(),
            camera_line: "1280x720@15fps h264 2000000bps".to_string(),
            notes: vec!["gb28181: enabled".to_string()],
        }
    }

    #[test]
    fn hooks_record_clock_request_without_touching_anything() {
        let hooks = DeviceHooksGlue::new(test_report());
        assert!(hooks.last_clock_request().is_none());
        hooks.set_date_time((2026, 9, 29, 12, 0, 0), "CST-8");
        assert_eq!(
            hooks.last_clock_request(),
            Some(((2026, 9, 29, 12, 0, 0), "CST-8".to_string()))
        );
    }

    #[test]
    fn hooks_count_refused_reboots_and_factory_defaults() {
        let hooks = DeviceHooksGlue::new(test_report());
        hooks.reboot();
        hooks.reboot();
        assert_eq!(hooks.reboot_requests(), 2);
        assert_eq!(hooks.factory_default_requests(), 0);
        hooks.factory_default(true);
        assert_eq!(hooks.factory_default_requests(), 1);
    }

    #[test]
    fn hooks_system_log_carries_real_report_data() {
        let hooks = DeviceHooksGlue::new(test_report());
        let log = hooks.system_log();
        assert!(log.contains("9.9.9-test"), "{log}");
        assert!(log.contains("Test Cam — TestCorp TC-2000"), "{log}");
        assert!(log.contains("1280x720@15fps h264 2000000bps"), "{log}");
        assert!(log.contains("gb28181: enabled"), "{log}");
        assert!(log.contains("uptime"), "{log}");
    }

    #[test]
    fn hooks_support_info_non_empty_and_counts_refusals() {
        let hooks = DeviceHooksGlue::new(test_report());
        hooks.reboot();
        let info = hooks.support_info();
        assert!(info.contains("9.9.9-test"), "{info}");
        assert!(info.contains("reboot requests refused: 1"), "{info}");
        assert!(
            info.contains("factory-default requests refused: 0"),
            "{info}"
        );
    }

    // -- wiring smoke (full wire behavior is covered by
    //    tests/onvif_wire.rs over a real socket) --------------------------

    fn wiring_input() -> OnvifWiringInput {
        OnvifWiringInput {
            host: crate::config::ONVIFConfig::default(),
            device: crate::config::DeviceConfig {
                name: "Test Cam".into(),
                manufacturer: "TestCorp".into(),
                model: "TC-2000".into(),
                firmware: "1.0.0".into(),
                hardware_id: "TC2000".into(),
                serial_number: "SN-1".into(),
            },
            device_ip: "127.0.0.1".to_string(),
            media: OnvifMediaConfig::new(1280, 720, 15, 2_000_000, 8554, "127.0.0.1".into()),
            keyframe_hook: None,
            hooks: Arc::new(DeviceHooksGlue::new(test_report())),
            imaging: Arc::new(ImagingGlue::new()),
        }
    }

    #[test]
    fn wiring_gates_events_seam_on_config() {
        let mut input = wiring_input();
        input.host.events_enabled = false;
        let out = wire_onvif_server(input);
        assert!(out.events.is_none());

        let out = wire_onvif_server(wiring_input());
        assert!(out.events.is_some());
    }

    #[test]
    fn wiring_survives_invalid_device_identity() {
        // Neutral placeholder identity is rejected by the library; the
        // wiring keeps the remaining services (media/PTZ/imaging) up.
        let mut input = wiring_input();
        input.device = crate::config::DeviceConfig::default();
        let out = wire_onvif_server(input);
        assert!(out.events.is_some(), "non-device services stay up");
    }
}
