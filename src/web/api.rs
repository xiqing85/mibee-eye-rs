use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use image::codecs::jpeg::JpegEncoder;
use image::ImageBuffer;
use image::Rgb;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::RwLock;

use crate::config::Config;
use crate::features::ai::Detection;
use crate::web::auth;

// Shared state
// ---------------------------------------------------------------------------

/// Latest camera frame slot: shared `(width, height, YUV420 bytes)`.
pub type SharedYuvFrame = Arc<Mutex<Option<(u32, u32, Vec<u8>)>>>;

/// Shared application state used by API handlers.
pub struct AppState {
    /// Filesystem path to the TOML config file (empty = no persistence).
    pub config_path: String,
    /// In-memory application configuration.
    pub config: RwLock<Config>,
    /// Current PTZ (pan / tilt / zoom) position.
    pub ptz: RwLock<PtzStatus>,
    /// Latest YUV420 frame from camera (for web snapshots).
    pub latest_yuv: Option<SharedYuvFrame>,
    /// H.264 access unit hub for fMP4 streaming.
    pub au_hub: Option<Arc<crate::h264::hub::AuHub>>,
    /// Latest AI detection results (for /api/detections).
    pub last_detections: Option<Arc<RwLock<Vec<Detection>>>>,
    /// Name of the ACTIVE detector (e.g. "mock-detector-v1" or the ONNX
    /// model path). Reported by /api/detections instead of the configured
    /// model path, which may not be what is actually running.
    pub ai_model: Option<String>,
    /// The running AI module — enables model hot-swap and authoritative
    /// active-model reporting (SPEC §4.6).
    pub ai_module: Option<Arc<crate::ai::AiModule>>,
    /// Detector factory for registry model ids (SPEC §4.6 activate). Absent
    /// in plain builds (no `ai` feature) — capability `ai_models` stays false.
    pub ai_loader: Option<crate::ai::registry::AiLoader>,
    /// Runtime model registry: builtin entries + uploaded overlay
    /// (SPEC §4.6). Shared with the detector loader.
    pub registry: std::sync::Arc<std::sync::RwLock<crate::ai::registry::Registry>>,
    /// Event bus for pipeline events.
    pub event_bus: Option<crate::pipeline::bus::EventBus>,
    /// Login sessions (SPEC §2; in-memory — see auth.rs).
    pub sessions: Arc<auth::SessionStore>,
    /// Process-restart action behind POST /api/system/restart (SPEC §5.1).
    /// `None` = not wired (endpoint no-ops); tests inject a flag-setter.
    pub restart_action: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Monotonic service start. Backs the `uptime` field of `/api/status`
    /// and `/api/health` (SPEC §1/§3) so uptime is anchored at state
    /// construction, not at the first request that happens to read it.
    pub started: std::time::Instant,
    /// Observability state (SPEC §3.2): sampler snapshot, log ring,
    /// request-trace ring, and app-attributed traffic counters.
    pub observe: Arc<crate::web::observe::Observe>,
}

/// Current PTZ position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtzStatus {
    pub pan: f64,
    pub tilt: f64,
    pub zoom: f64,
}

impl Default for PtzStatus {
    fn default() -> Self {
        Self {
            pan: 0.5,
            tilt: 0.5,
            zoom: 1.0,
        }
    }
}
impl Default for AppState {
    fn default() -> Self {
        Self {
            config_path: String::new(),
            config: RwLock::new(Config::default()),
            ptz: RwLock::new(PtzStatus::default()),
            latest_yuv: None,
            au_hub: None,
            last_detections: None,
            ai_model: None,
            ai_module: None,
            ai_loader: None,
            registry: std::sync::Arc::new(std::sync::RwLock::new(
                crate::ai::registry::Registry::builtin_only(),
            )),
            event_bus: None,
            sessions: Arc::new(auth::SessionStore::new()),
            restart_action: None,
            started: std::time::Instant::now(),
            observe: Arc::new(crate::web::observe::Observe::new()),
        }
    }
}

// ---------------------------------------------------------------------------
// Unified response envelope (SPEC v1 §0)
// ---------------------------------------------------------------------------

/// Wrap a payload in the unified success envelope `{"ok":true,"data":…}`.
pub(crate) fn ok_env(data: serde_json::Value) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true, "data": data }))
}

/// Build the unified error envelope
/// `{"ok":false,"error":"<machine code>","message":"<human text>"}` with an
/// HTTP status. The machine code is derived from the status (SPEC §0 table).
pub(crate) type EnvError = (StatusCode, Json<serde_json::Value>);

fn error_code(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "bad_request",
        StatusCode::UNAUTHORIZED => "unauthorized",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::CONFLICT => "conflict",
        StatusCode::TOO_MANY_REQUESTS => "rate_limited",
        StatusCode::NOT_IMPLEMENTED => "not_implemented",
        StatusCode::SERVICE_UNAVAILABLE => "setup_required",
        _ => "internal_error",
    }
}

pub(crate) fn err_env(status: StatusCode, msg: impl std::fmt::Display) -> EnvError {
    (
        status,
        Json(serde_json::json!({
            "ok": false,
            "error": error_code(status),
            "message": msg.to_string(),
        })),
    )
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/status` — unified device status (SPEC v1 §3).
///
/// Field set is device-dependent per the spec; the Pi reports the common
/// core plus its own recording / AI / GB28181 state.
pub async fn status_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let cfg = state.config.read().await;
    ok_env(serde_json::json!({
        "device_name": cfg.device.name,
        "model": cfg.device.model,
        "vendor": cfg.device.manufacturer,
        "firmware": env!("CARGO_PKG_VERSION"),
        "resolution": format!("{}x{}", cfg.camera.width, cfg.camera.height),
        "fps": cfg.camera.fps,
        "bitrate_bps": cfg.camera.bitrate,
        "uptime": state.started.elapsed().as_secs(),
        "recording": cfg.recording.enabled,
        "ai": cfg.features.ai.enabled,
        "gb28181": cfg.gb28181.enabled,
    }))
}

/// `GET /api/capabilities` — the capability superset (SPEC v1 §3.1).
///
/// Feature flags reflect what the device exposes right now (enablement, not
/// just support): `ai` follows the config switch, `mse` requires a running
/// encoder hub, `mjpeg`/snapshot require a camera frame source.
pub async fn capabilities_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let cfg = state.config.read().await;
    let ai_hot_swap = state.ai_module.is_some() && state.ai_loader.is_some();
    let ai_upload = ai_hot_swap && cfg.features.ai.allow_upload;
    let mut events = vec!["ai_detection", "recording"];
    if ai_hot_swap {
        events.push("ai_model_changed");
    }
    ok_env(serde_json::json!({
        "spec_version": "1",
        "device": {
            "name": cfg.device.name,
            "model": cfg.device.model,
            "vendor": cfg.device.manufacturer,
        },
        "auth": {"model": "session", "setup": true},
        "multi_camera": false,
        "camera_management": false,
        "camera_control": false,
        "imaging": false,
        "ai": cfg.features.ai.enabled,
        "ai_models": ai_hot_swap,
        "ai_upload": ai_upload,
        "ptz": true,
        "hls": false,
        "recording": true,
        "watermark": true,
        "devices": false,
        "mjpeg": state.latest_yuv.is_some(),
        "mse": state.au_hub.is_some(),
        "webrtc": false,
        "events": events,
        "config_apply": {"default": "restart", "sections": {"recording": "restart", "watermark": "restart"}},
        "restart": true,
        "observability": {"metrics": true, "logs": true, "requests": true},
    }))
}

/// `POST /api/system/restart` — restart the service process to apply saved
/// `restart`-semantics config (SPEC §5.1). Responds immediately; the action
/// fires after a short grace period so the response can flush. The action is
/// injected via [`AppState::restart_action`] — production wires a process
/// exit (systemd `Restart=always` brings the service back); tests inject a
/// flag. Absent wiring, the endpoint is a safe no-op.
pub async fn system_restart(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    if let Some(action) = state.restart_action.clone() {
        tokio::task::spawn_blocking(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            action();
        });
    }
    ok_env(serde_json::json!({ "status": "restarting" }))
}

// ---------------------------------------------------------------------------
// Camera resources (SPEC v1 §4) — the Pi is a single-camera device with the
// fixed id "0".
// ---------------------------------------------------------------------------

/// The single camera document (SPEC §4).
fn camera_doc(state: &AppState, cfg: &Config) -> serde_json::Value {
    serde_json::json!({
        "id": "0",
        "name": cfg.device.name,
        "status": if state.latest_yuv.is_some() || state.au_hub.is_some() { "online" } else { "offline" },
        "camera_type": "csi",
        "resolution": format!("{}x{}", cfg.camera.width, cfg.camera.height),
        "fps": cfg.camera.fps,
    })
}

/// `GET /api/cameras` — the (single-element) camera list.
pub async fn cameras_list(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let cfg = state.config.read().await;
    ok_env(serde_json::json!([camera_doc(&state, &cfg)]))
}

/// `GET /api/cameras/{id}` — the single camera; 404 for any other id.
pub async fn camera_get(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, EnvError> {
    if id != "0" {
        return Err(err_env(StatusCode::NOT_FOUND, "no such camera"));
    }
    let cfg = state.config.read().await;
    Ok(ok_env(camera_doc(&state, &cfg)))
}

/// `GET /api/cameras/{id}/recording` — recording status (SPEC v1 §4.4).
pub async fn recording_status(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, EnvError> {
    if id != "0" {
        return Err(err_env(StatusCode::NOT_FOUND, "no such camera"));
    }
    let cfg = state.config.read().await;
    Ok(ok_env(serde_json::json!({
        "active": cfg.recording.enabled,
        "storage_path": cfg.recording.storage_path,
        "segment_secs": cfg.recording.segment_secs,
        "retention_days": cfg.recording.retention_days,
    })))
}

/// Body of `POST /api/cameras/{id}/recording` (SPEC v1 §4.4).
#[derive(Debug, Deserialize)]
pub struct RecordingRequest {
    pub active: bool,
}

/// `POST /api/cameras/{id}/recording` — toggle the recording configuration.
/// The recorder is wired at process start, so the change persists to the TOML
/// config and takes effect on the next restart (announced via
/// `config_apply.sections.recording`); an SSE `recording` event is broadcast.
pub async fn recording_set(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<RecordingRequest>,
) -> Result<Json<serde_json::Value>, EnvError> {
    if id != "0" {
        return Err(err_env(StatusCode::NOT_FOUND, "no such camera"));
    }

    let mut cfg = state.config.write().await.clone();
    cfg.recording.enabled = req.active;
    persist_config(&state, &cfg).await?;
    *state.config.write().await = cfg;

    super::events::global_hub().broadcast(
        "recording",
        &serde_json::json!({"camera_id": "0", "active": req.active}),
    );

    Ok(ok_env(serde_json::json!({
        "active": req.active,
        "applies_at": "restart",
    })))
}

/// Persist a config to the TOML file when a path is configured. Shared by
/// `PUT /api/config`, the recording toggle and the auth handlers so all
/// write through the same serialization path.
pub(crate) async fn persist_config(state: &AppState, config: &Config) -> Result<(), EnvError> {
    if state.config_path.is_empty() {
        return Ok(());
    }
    let toml_str = toml::to_string(config).map_err(|e| {
        err_env(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("TOML serialization failed: {e}"),
        )
    })?;
    tokio::fs::write(&state.config_path, &toml_str)
        .await
        .map_err(|e| {
            err_env(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to write config: {e}"),
            )
        })?;
    Ok(())
}

/// `GET /api/config` — current configuration with every password masked
/// (SPEC v1 §5: `"****"` when set, `""` when unset — the stored value is
/// never echoed). The Web UI's config editor round-trips this document:
/// masked secrets are restored server-side on write.
pub async fn get_config(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let cfg = state.config.read().await.clone();
    let masked = auth::mask_passwords(serde_json::to_value(cfg).unwrap_or_default());
    ok_env(masked)
}

/// `PUT /api/config` — update the configuration (SPEC v1 §5, partial merge:
/// a full or partial `Config` document; `"****"`-masked password fields are
/// restored from the stored config so a client that GETs (masked) and PUTs
/// the same document back does not wipe secrets).
///
/// Most sections apply on restart (see `capabilities.config_apply`); the
/// response announces the effective semantics.
pub async fn put_config(
    State(state): State<Arc<AppState>>,
    Json(mut value): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, EnvError> {
    // Restore masked secrets from the stored config before merging.
    let current = serde_json::to_value(&*state.config.read().await).unwrap_or_default();
    auth::unmask_passwords(&mut value, &current);
    // SPEC §5: partial merge — deep-merge the submitted subtree into the
    // current document BEFORE deserializing. Deserializing the body alone
    // would apply serde defaults to every absent field, silently wiping
    // whatever the client did not send.
    let merged = merge_json(current, value);

    let config: Config = serde_json::from_value(merged)
        .map_err(|e| err_env(StatusCode::BAD_REQUEST, format!("invalid config: {e}")))?;
    config
        .validate()
        .map_err(|e| err_env(StatusCode::BAD_REQUEST, e))?;

    persist_config(&state, &config).await?;
    *state.config.write().await = config;
    Ok(ok_env(serde_json::json!({ "applied": "restart" })))
}

/// Recursive JSON object merge for SPEC §5 partial writes: objects merge
/// key-by-key (right wins on type mismatch), everything else replaces.
fn merge_json(mut base: serde_json::Value, patch: serde_json::Value) -> serde_json::Value {
    match (&mut base, patch) {
        (serde_json::Value::Object(b), serde_json::Value::Object(p)) => {
            for (k, v) in p {
                let slot = b.entry(k).or_insert(serde_json::Value::Null);
                *slot = merge_json(slot.take(), v);
            }
            base
        }
        (_, patch) => patch,
    }
}

/// `POST /api/reset` — factory reset: restore default config (clearing the
/// admin credentials back to first-boot setup) and persist it.
pub async fn reset_config(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, EnvError> {
    let config = Config::default();
    persist_config(&state, &config).await?;
    *state.config.write().await = config;
    state.sessions.clear();
    println!("web: config reset to factory defaults (auth back to first-time setup)");
    Ok(ok_env(serde_json::json!({ "message": "config reset" })))
}

/// `GET /api/cameras/{id}/live` — continuous MJPEG stream from camera YUV
/// frames (SPEC v1 §4.1, capability `mjpeg`).
pub async fn live_handler(Path(id): Path<String>, State(state): State<Arc<AppState>>) -> Response {
    if id != "0" {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("no such camera"))
            .unwrap();
    }
    stream_body(state).await
}

async fn stream_body(state: Arc<AppState>) -> Response {
    let yuv_arc = match &state.latest_yuv {
        Some(a) => a.clone(),
        None => {
            return Response::builder()
                .status(503)
                .body(Body::from("camera not available"))
                .unwrap()
        }
    };

    // Build a streaming MJPEG response.
    let stream = async_stream::stream! {
        loop {
            // Clone YUV data under lock, then drop guard before any await.
            let frame_data = {
                let guard = yuv_arc.lock().unwrap();
                guard.as_ref().map(|(w, h, d)| (*w, *h, d.clone()))
            };
            let jpeg = match frame_data {
                Some((w, h, data)) => {
                    let (dw, dh) = (w / 2, h / 2);
                    let rgb = yuv420_to_rgb_scaled(&data, w, h, dw, dh);
                    let img = match ImageBuffer::<Rgb<u8>, Vec<u8>>::from_raw(dw, dh, rgb) {
                        Some(i) => image::DynamicImage::ImageRgb8(i),
                        None => { tokio::time::sleep(std::time::Duration::from_millis(100)).await; continue; }
                    };
                    let mut buf = Vec::new();
                    let mut enc = JpegEncoder::new_with_quality(&mut buf, 35);
                    if enc.encode_image(&img).is_err() { tokio::time::sleep(std::time::Duration::from_millis(100)).await; continue; }
                    buf
                }
                None => { tokio::time::sleep(std::time::Duration::from_millis(200)).await; continue; }
            };
            yield Ok::<_, std::convert::Infallible>(format!("--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", jpeg.len()).into_bytes());
            yield Ok(b"\r\n".to_vec());
            yield Ok(jpeg);
            yield Ok(b"\r\n".to_vec());
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };

    let body = Body::from_stream(stream);
    Response::builder()
        .header("content-type", "multipart/x-mixed-replace; boundary=frame")
        .header("cache-control", "no-cache")
        .body(body)
        .unwrap()
}

/// Downscaled YUV420 → RGB888 (skip-sampling for speed).
fn yuv420_to_rgb_scaled(data: &[u8], w: u32, h: u32, dw: u32, dh: u32) -> Vec<u8> {
    let (w, h, dw, dh) = (w as usize, h as usize, dw as usize, dh as usize);
    let y_size = w * h;
    let u_off = y_size;
    let v_off = y_size + y_size / 4;
    let mut rgb = Vec::with_capacity(dw * dh * 3);
    for j in 0..dh {
        for i in 0..dw {
            let sy = j * 2;
            let sx = i * 2;
            let y = data[sy * w + sx] as f32;
            let u = data[u_off + (sy / 2) * (w / 2) + (sx / 2)] as f32 - 128.0;
            let v = data[v_off + (sy / 2) * (w / 2) + (sx / 2)] as f32 - 128.0;
            rgb.push((y + 1.402 * v).clamp(0.0, 255.0) as u8);
            rgb.push((y - 0.344 * u - 0.714 * v).clamp(0.0, 255.0) as u8);
            rgb.push((y + 1.772 * u).clamp(0.0, 255.0) as u8);
        }
    }
    rgb
}

/// `GET /snapshot` — legacy unauthenticated JPEG endpoint mirroring the
/// Go device dialect (SPEC appendix A): the URI that ONVIF GetSnapshotUri
/// advertises, fetchable by NVRs without a web session.
pub async fn legacy_snapshot_handler(State(state): State<Arc<AppState>>) -> Response {
    snapshot_body(state).await
}

/// `GET /api/cameras/{id}/snapshot` — returns a JPEG snapshot from the
/// camera (SPEC v1 §4.1).
pub async fn snapshot_handler(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Response {
    if id != "0" {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("no such camera"))
            .unwrap();
    }
    snapshot_body(state).await
}

async fn snapshot_body(state: Arc<AppState>) -> Response {
    let yuv_arc = match &state.latest_yuv {
        Some(a) => a,
        None => {
            return Response::builder()
                .status(503)
                .body(Body::from("camera not available"))
                .unwrap()
        }
    };
    let guard = yuv_arc.lock().unwrap();
    let (w, h, data) = match guard.as_ref() {
        Some(f) => (f.0, f.1, &f.2[..]),
        None => {
            return Response::builder()
                .status(503)
                .body(Body::from("no frame"))
                .unwrap()
        }
    };
    // Convert YUV420 → RGB → JPEG.
    let rgb = yuv420_to_rgb(data, w, h);
    let img: ImageBuffer<Rgb<u8>, Vec<u8>> = match ImageBuffer::from_raw(w, h, rgb) {
        Some(i) => i,
        None => {
            return Response::builder()
                .status(500)
                .body(Body::from("image error"))
                .unwrap()
        }
    };
    let mut buf = Vec::new();
    let mut enc = JpegEncoder::new_with_quality(&mut buf, 40);
    match enc.encode_image(&image::DynamicImage::ImageRgb8(img)) {
        Ok(()) => Response::builder()
            .header("content-type", "image/jpeg")
            .header("cache-control", "no-cache")
            .body(Body::from(buf))
            .unwrap(),
        Err(_) => Response::builder()
            .status(500)
            .body(Body::from("jpeg error"))
            .unwrap(),
    }
}

/// Convert YUV420 planar to RGB888.
/// YUV420 → RGB888 (the /snapshot + snapshot-upload conversion).
pub(crate) fn yuv420_to_rgb(data: &[u8], w: u32, h: u32) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    let y_size = w * h;
    let u_off = y_size;
    let v_off = y_size + y_size / 4;
    let mut rgb = Vec::with_capacity(w * h * 3);
    for j in 0..h {
        for i in 0..w {
            let y = data[j * w + i] as f32;
            let u = data[u_off + (j / 2) * (w / 2) + (i / 2)] as f32 - 128.0;
            let v = data[v_off + (j / 2) * (w / 2) + (i / 2)] as f32 - 128.0;
            rgb.push((y + 1.402 * v).clamp(0.0, 255.0) as u8);
            rgb.push((y - 0.344 * u - 0.714 * v).clamp(0.0, 255.0) as u8);
            rgb.push((y + 1.772 * u).clamp(0.0, 255.0) as u8);
        }
    }
    rgb
}

/// `GET /api/ptz/status` — returns the current pan/tilt/zoom position.
pub async fn ptz_status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let pos = state.ptz.read().await.clone();
    ok_env(serde_json::to_value(pos).unwrap_or_default())
}

/// Request body for `POST /api/ptz/move`.
#[derive(Debug, Deserialize)]
pub struct PtzMoveRequest {
    #[serde(default = "default_pan")]
    pub pan: f64,
    #[serde(default = "default_tilt")]
    pub tilt: f64,
    #[serde(default = "default_zoom")]
    pub zoom: f64,
}

fn default_pan() -> f64 {
    0.5
}
fn default_tilt() -> f64 {
    0.5
}
fn default_zoom() -> f64 {
    1.0
}

/// `POST /api/ptz/move` — updates the PTZ position, returning the new
/// position in the unified envelope.
pub async fn ptz_move(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PtzMoveRequest>,
) -> Json<serde_json::Value> {
    let mut ptz = state.ptz.write().await;
    ptz.pan = req.pan;
    ptz.tilt = req.tilt;
    ptz.zoom = req.zoom;
    ok_env(serde_json::to_value(&*ptz).unwrap_or_default())
}
/// `GET /api/detections` — returns the latest AI detection results.
pub async fn detections_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    match &state.last_detections {
        Some(det_arc) => {
            let detections = det_arc.read().await;
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            // The ACTIVE model id from the running module (not the
            // configured one — the config may name a model that isn't
            // actually running). Falls back to the startup-detected name.
            let model = match &state.ai_module {
                Some(module) => module.active_model(),
                None => state
                    .ai_model
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            };
            ok_env(serde_json::json!({
                "detections": *detections,
                "model": model,
                "timestamp": timestamp
            }))
        }
        None => ok_env(serde_json::json!({ "enabled": false })),
    }
}

/// `GET /api/ai/models` — the model registry plus the active model
/// (SPEC §4.6, capability `ai_models`). `available` reflects the presence
/// of the model file on this device; unavailable entries must not be
/// activated. When uploads are enabled (capability `ai_upload`) the
/// response carries the upload constraints.
pub async fn ai_models_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let cfg = state.config.read().await;
    let registry = state.registry.read().expect("registry lock");
    let resolved = registry.resolve_active(&cfg.features.ai).ok();
    let active = match &state.ai_module {
        Some(module) => module.active_model(),
        None => resolved
            .as_ref()
            .map_or_else(|| "unknown".to_string(), |m| m.id.clone()),
    };
    let models: Vec<serde_json::Value> = registry
        .list()
        .iter()
        .map(|spec| {
            serde_json::json!({
                "id": spec.id,
                "family": spec.family,
                "input": spec.input,
                "source": spec.source,
                "available": crate::ai::registry::is_available(&spec.path),
            })
        })
        .collect();
    let mut payload = serde_json::json!({ "active": active, "models": models });
    if let Some(m) = resolved.as_ref().filter(|m| m.source == "custom") {
        payload["models"]
            .as_array_mut()
            .expect("models array")
            .push(serde_json::json!({
                "id": "custom",
                "family": m.family,
                "input": m.input,
                "source": "custom",
                "available": crate::ai::registry::is_available(&m.path),
            }));
    }
    if cfg.features.ai.allow_upload {
        payload["upload"] = serde_json::json!({
            "allowed": state.ai_loader.is_some(),
            "max_bytes": crate::ai::registry::UPLOAD_MAX_BYTES,
        });
    }
    ok_env(payload)
}

/// `POST /api/ai/models/{id}/activate` — hot-switch the running model
/// (SPEC §4.6). The new detector is fully constructed BEFORE the running
/// slot is touched, so a failed load leaves the old model running
/// (rollback by construction). On success the choice persists to
/// `ai.model` and an `ai_model_changed` SSE event is broadcast.
pub async fn ai_model_activate(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, EnvError> {
    let module = state
        .ai_module
        .clone()
        .ok_or_else(|| err_env(StatusCode::NOT_IMPLEMENTED, "AI module not running"))?;
    let loader = state.ai_loader.clone().ok_or_else(|| {
        err_env(
            StatusCode::NOT_IMPLEMENTED,
            "model hot-switch unavailable in this build",
        )
    })?;
    // Unknown ids 404 before any load attempt; the loader owns the
    // availability check (ActivateError::Unavailable → 409) so tests can
    // inject a filesystem-free factory.
    let spec = state
        .registry
        .read()
        .expect("registry lock")
        .find(&id)
        .ok_or_else(|| err_env(StatusCode::NOT_FOUND, format!("unknown model id: {id}")))?;
    let family = crate::ai::registry::Family::parse_family(&spec.family)
        .unwrap_or(crate::ai::registry::Family::NanoDet);
    let path = spec.path.clone();
    // ORT session creation is heavy and synchronous — keep it off the
    // async executor.
    let built = tokio::task::spawn_blocking(move || loader(&path, family))
        .await
        .map_err(|e| {
            err_env(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("model loader task failed: {e}"),
            )
        })?
        .map_err(map_activate_error)?;

    module.set_detector(&id, built).await;
    {
        let mut cfg = state.config.write().await;
        cfg.features.ai.model = id.clone();
        // Reset any custom-path override so the registry id survives reboots.
        cfg.features.ai.model_path = crate::config::default_ai_model_path();
        persist_config(&state, &cfg).await?;
    }
    super::events::global_hub().broadcast(
        "ai_model_changed",
        &serde_json::json!({ "camera_id": "0", "model": id }),
    );
    Ok(ok_env(serde_json::json!({
        "active": id,
        "applied": "immediate"
    })))
}

/// Map a loader failure to its SPEC §4.6 envelope.
fn map_activate_error(e: crate::ai::registry::ActivateError) -> EnvError {
    match e {
        crate::ai::registry::ActivateError::Unavailable(msg) => err_env(StatusCode::CONFLICT, msg),
        crate::ai::registry::ActivateError::LoadFailed(msg) => {
            err_env(StatusCode::INTERNAL_SERVER_ERROR, msg)
        }
    }
}

/// `POST /api/ai/models/{id}` — upload a model file into the registry
/// (SPEC §4.6, capability `ai_upload`). Multipart fields: `family`
/// (declared decoder family) + `file` (ONNX binary). The file is fully
/// loaded and shape-validated BEFORE it enters the registry; failures
/// leave no trace on disk.
pub async fn ai_model_upload(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
    mut multipart: axum::extract::Multipart,
) -> Result<Response, EnvError> {
    if !state.config.read().await.features.ai.allow_upload {
        return Err(err_env(
            StatusCode::NOT_IMPLEMENTED,
            "model upload disabled (features.ai.allow_upload)",
        ));
    }
    let loader = state.ai_loader.clone().ok_or_else(|| {
        err_env(
            StatusCode::NOT_IMPLEMENTED,
            "model upload unavailable in this build",
        )
    })?;
    if !crate::ai::registry::valid_model_id(&id) {
        return Err(err_env(
            StatusCode::BAD_REQUEST,
            format!("invalid model id {id:?} (want ^[a-z0-9][a-z0-9-]{{0,63}}$)"),
        ));
    }
    if state
        .registry
        .read()
        .expect("registry lock")
        .find(&id)
        .is_some()
    {
        return Err(err_env(
            StatusCode::CONFLICT,
            format!("model id already exists: {id}"),
        ));
    }
    let models_dir = state
        .registry
        .read()
        .expect("registry lock")
        .models_dir()
        .ok_or_else(|| {
            err_env(
                StatusCode::NOT_IMPLEMENTED,
                "no models directory configured",
            )
        })?
        .to_path_buf();

    // Pull the multipart fields: `family` (text) + `file` (binary, capped).
    let mut family: Option<String> = None;
    let mut file: Option<Vec<u8>> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| err_env(StatusCode::BAD_REQUEST, format!("invalid multipart: {e}")))?
    {
        match field.name().unwrap_or_default() {
            "family" => {
                family =
                    Some(field.text().await.map_err(|e| {
                        err_env(StatusCode::BAD_REQUEST, format!("family field: {e}"))
                    })?);
            }
            "file" => {
                let mut buf: Vec<u8> = Vec::new();
                let mut field = field;
                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|e| err_env(StatusCode::BAD_REQUEST, format!("file field: {e}")))?
                {
                    if buf.len() + chunk.len() > crate::ai::registry::UPLOAD_MAX_BYTES {
                        return Err(err_env(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            format!(
                                "model exceeds max_bytes ({})",
                                crate::ai::registry::UPLOAD_MAX_BYTES
                            ),
                        ));
                    }
                    buf.extend_from_slice(&chunk);
                }
                file = Some(buf);
            }
            _ => {}
        }
    }
    let family = crate::ai::registry::Family::parse_family(
        &family.ok_or_else(|| err_env(StatusCode::BAD_REQUEST, "missing family field"))?,
    )
    .ok_or_else(|| err_env(StatusCode::BAD_REQUEST, "family must be nanodet or yolox"))?;
    let file = file.ok_or_else(|| err_env(StatusCode::BAD_REQUEST, "missing file field"))?;

    // Land the bytes in a temp file, validate by fully loading a session,
    // then atomically rename into place and enter the registry.
    std::fs::create_dir_all(&models_dir).map_err(|e| {
        err_env(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("models dir: {e}"),
        )
    })?;
    let tmp = models_dir.join(format!(".upload-{id}.tmp"));
    std::fs::write(&tmp, &file)
        .map_err(|e| err_env(StatusCode::INTERNAL_SERVER_ERROR, format!("write: {e}")))?;
    let tmp_path = tmp.to_string_lossy().into_owned();
    let tmp_for_cleanup = tmp_path.clone();
    let validated = tokio::task::spawn_blocking(move || loader(&tmp_path, family))
        .await
        .map_err(|e| {
            err_env(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("model validator task failed: {e}"),
            )
        })?
        .map_err(|e| {
            // Validation failure: no trace left behind.
            let _ = std::fs::remove_file(&tmp_for_cleanup);
            let msg = match e {
                crate::ai::registry::ActivateError::Unavailable(m) => m,
                crate::ai::registry::ActivateError::LoadFailed(m) => m,
            };
            err_env(
                StatusCode::BAD_REQUEST,
                format!("model failed validation: {msg}"),
            )
        })?;
    let input = validated.input_size();
    drop(validated);

    let final_path = models_dir.join(format!("{id}.onnx"));
    std::fs::rename(&tmp, &final_path)
        .map_err(|e| err_env(StatusCode::INTERNAL_SERVER_ERROR, format!("rename: {e}")))?;
    let spec = crate::ai::registry::ModelSpec {
        id: id.clone(),
        family: family.as_str().to_string(),
        input,
        path: final_path.to_string_lossy().into_owned(),
        source: "uploaded".into(),
    };
    state
        .registry
        .write()
        .expect("registry lock")
        .insert_uploaded(spec.clone())
        .map_err(|e| {
            err_env(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("manifest write failed: {e}"),
            )
        })?;
    println!(
        "web: ai model uploaded: {id} (family {}, input {input})",
        family.as_str()
    );
    let mut resp = ok_env(serde_json::json!({
        "id": spec.id,
        "family": spec.family,
        "input": spec.input,
        "source": spec.source,
        "available": true,
    }))
    .into_response();
    *resp.status_mut() = StatusCode::CREATED;
    Ok(resp)
}

/// `DELETE /api/ai/models/{id}` — remove an uploaded model (SPEC §4.6).
/// Builtin entries and the currently-running model are refused.
pub async fn ai_model_delete(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Response, EnvError> {
    if !state.config.read().await.features.ai.allow_upload {
        return Err(err_env(
            StatusCode::NOT_IMPLEMENTED,
            "model upload disabled (features.ai.allow_upload)",
        ));
    }
    if let Some(module) = &state.ai_module {
        if module.active_model() == id {
            return Err(err_env(
                StatusCode::CONFLICT,
                "cannot delete the active model; activate another first",
            ));
        }
    }
    let removed = state
        .registry
        .write()
        .expect("registry lock")
        .remove_uploaded(&id);
    match removed {
        Some(spec) => {
            let _ = std::fs::remove_file(&spec.path);
            println!("web: ai model removed: {id}");
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        None => {
            let builtin = state
                .registry
                .read()
                .expect("registry lock")
                .find(&id)
                .is_some();
            if builtin {
                Err(err_env(
                    StatusCode::CONFLICT,
                    "builtin models cannot be deleted",
                ))
            } else {
                Err(err_env(
                    StatusCode::NOT_FOUND,
                    format!("unknown model id: {id}"),
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use axum::middleware::from_fn_with_state;
    use axum::routing::{get, post};
    use axum::Router;
    use tower::ServiceExt;

    /// The full API router with the SPEC §2 auth gate wired. `pw` empty =
    /// first-boot state (no admin yet).
    fn api_router(pw: &str) -> Router {
        let state = {
            let mut s = AppState::default();
            if !pw.is_empty() {
                s.config.get_mut().web.password = pw.to_string();
                s.config.get_mut().web.username = "admin".to_string();
            }
            Arc::new(s)
        };
        api_router_with(pw, state)
    }

    /// [`api_router`] over a caller-built state (AI hot-swap wiring tests).
    fn api_router_with(_pw: &str, state: Arc<AppState>) -> Router {
        Router::new()
            .route("/api/auth/me", get(crate::web::auth::me_handler))
            .route("/api/auth/setup", post(crate::web::auth::setup))
            .route("/api/auth/login", post(crate::web::auth::login))
            .route("/api/auth/logout", post(crate::web::auth::logout))
            .route("/api/auth/reset", post(crate::web::auth::reset))
            .route("/api/status", get(status_handler))
            .route("/api/capabilities", get(capabilities_handler))
            .route("/api/cameras", get(cameras_list))
            .route("/api/cameras/:id", get(camera_get))
            .route("/api/cameras/:id/snapshot", get(snapshot_handler))
            .route("/api/cameras/:id/live", get(live_handler))
            .route(
                "/api/cameras/:id/recording",
                get(recording_status).post(recording_set),
            )
            .route("/api/config", get(get_config).put(put_config))
            .route("/api/reset", post(reset_config))
            .route("/api/ptz/status", get(ptz_status))
            .route("/api/ptz/move", post(ptz_move))
            .route("/api/detections", get(detections_handler))
            .route("/api/ai/models", get(ai_models_handler))
            .route("/api/ai/models/:id/activate", post(ai_model_activate))
            .route(
                "/api/ai/models/:id",
                post(ai_model_upload).delete(ai_model_delete),
            )
            // Model uploads exceed axum's 2 MB default body limit.
            .layer(axum::extract::DefaultBodyLimit::max(
                crate::ai::registry::UPLOAD_MAX_BYTES + (1 << 20),
            ))
            .route("/api/system/restart", post(system_restart))
            .route("/api/events", get(crate::web::events::events_handler))
            .layer(from_fn_with_state(
                state.clone(),
                crate::web::auth::auth_gate,
            ))
            .with_state(state)
    }

    fn configured_router() -> Router {
        api_router("pw-test-123")
    }

    /// Log in through the real endpoint, returning (cookie header value,
    /// csrf token) — mirroring what a browser keeps after login.
    async fn login(app: &Router) -> (String, String) {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/login")
                    .method(Method::POST)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "username": "admin", "password": "pw-test-123"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "login must succeed");
        let mut session = String::new();
        let mut csrf = String::new();
        for v in res.headers().get_all("set-cookie") {
            let v = v.to_str().unwrap();
            if v.starts_with("session=") {
                session = v.split(';').next().unwrap().to_string();
            } else if v.starts_with("csrf-token=") {
                csrf = v
                    .split(';')
                    .next()
                    .unwrap()
                    .trim_start_matches("csrf-token=")
                    .to_string();
            }
        }
        assert!(
            !session.is_empty() && !csrf.is_empty(),
            "cookies must be issued"
        );
        (session, csrf)
    }

    async fn body_json(res: axum::response::Response) -> serde_json::Value {
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn authed_get(uri: &str, cookie: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap()
    }

    fn authed_post(uri: &str, cookie: &str, csrf: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .method(Method::POST)
            .header("cookie", cookie)
            .header("x-csrf-token", csrf)
            .body(Body::empty())
            .unwrap()
    }

    // -- AI model registry / hot-swap (SPEC §4.6) ---------------------------

    use crate::ai::mock::MockAiDetector;
    use crate::ai::registry::{ActivateError, AiLoader};
    use crate::ai::AiModule;

    /// State with the AI module wired for hot-swap plus the given loader
    /// (mock detectors — no ONNX runtime, no filesystem).
    fn ai_state(loader: AiLoader) -> (Arc<AppState>, Arc<AiModule>) {
        let module = Arc::new(AiModule::new(
            Arc::new(MockAiDetector::new()),
            "nanodet-plus-m-320".to_string(),
            Arc::new(std::sync::Mutex::new(None)),
            None,
            crate::config::AiFeatureConfig::default(),
        ));
        let mut s = AppState {
            ai_module: Some(Arc::clone(&module)),
            ai_loader: Some(loader),
            ..AppState::default()
        };
        s.config.get_mut().web.password = "pw-test-123".to_string();
        s.config.get_mut().web.username = "admin".to_string();
        s.last_detections = Some(module.last_detections_arc());
        (Arc::new(s), module)
    }

    fn ok_loader() -> AiLoader {
        Arc::new(|_path: &str, _family: crate::ai::registry::Family| {
            Ok(Arc::new(MockAiDetector::new()) as Arc<dyn crate::features::ai::AiDetector>)
        })
    }

    #[tokio::test]
    async fn test_ai_models_requires_session() {
        let app = api_router("pw-test-123");
        let res = app.oneshot(authed_get("/api/ai/models", "")).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_ai_models_lists_registry_and_active() {
        let (state, _m) = ai_state(ok_loader());
        let app = api_router_with("pw-test-123", state);
        let (cookie, _csrf) = login(&app).await;
        let res = app
            .oneshot(authed_get("/api/ai/models", &cookie))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let json = body_json(res).await;
        let data = &json["data"];
        assert_eq!(data["active"], "nanodet-plus-m-320");
        let models = data["models"].as_array().expect("models array");
        let ids: Vec<_> = models.iter().filter_map(|m| m["id"].as_str()).collect();
        assert!(ids.contains(&"nanodet-plus-m-320"));
        assert!(ids.contains(&"nanodet-plus-m-416"));
        assert!(ids.contains(&"yolox-nano-416"));
        for m in models {
            assert_eq!(m["source"], "builtin");
            // Families must be decoders this build actually implements.
            assert!(matches!(
                m["family"].as_str(),
                Some("nanodet") | Some("yolox")
            ));
            assert!(m["input"].is_u64());
            assert!(m["available"].is_boolean());
        }
    }

    #[tokio::test]
    async fn test_ai_models_reports_custom_source_for_path_override() {
        let (state, _m) = ai_state(ok_loader());
        state.config.write().await.features.ai.model_path = "/opt/my-nanodet.onnx".to_string();
        let app = api_router_with("pw-test-123", state);
        let (cookie, _csrf) = login(&app).await;
        let res = app
            .oneshot(authed_get("/api/ai/models", &cookie))
            .await
            .unwrap();
        let json = body_json(res).await;
        let models = json["data"]["models"].as_array().expect("models array");
        let custom = models
            .iter()
            .find(|m| m["source"] == "custom")
            .expect("custom entry for path override");
        assert_eq!(custom["id"], "custom");
    }

    #[tokio::test]
    async fn test_activate_swaps_model_persists_and_reports_id() {
        let (state, module) = ai_state(ok_loader());
        let app = api_router_with("pw-test-123", state.clone());
        let (cookie, csrf) = login(&app).await;
        let res = app
            .clone()
            .oneshot(authed_post(
                "/api/ai/models/nanodet-plus-m-416/activate",
                &cookie,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let json = body_json(res).await;
        assert_eq!(json["data"]["active"], "nanodet-plus-m-416");
        assert_eq!(json["data"]["applied"], "immediate");

        // The module slot swapped for real.
        assert_eq!(module.active_model(), "nanodet-plus-m-416");

        // The choice persisted to ai.model (and a custom path would reset).
        {
            let cfg = state.config.read().await;
            assert_eq!(cfg.features.ai.model, "nanodet-plus-m-416");
            assert_eq!(
                cfg.features.ai.model_path,
                "/var/lib/mibee-eye/models/nanodet-m.onnx"
            );
        }

        // /api/detections now reports the new model id.
        let res = app
            .oneshot(authed_get("/api/detections", &cookie))
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(json["data"]["model"], "nanodet-plus-m-416");
    }

    #[tokio::test]
    async fn test_activate_unknown_model_is_404() {
        let (state, module) = ai_state(ok_loader());
        let app = api_router_with("pw-test-123", state);
        let (cookie, csrf) = login(&app).await;
        let res = app
            .oneshot(authed_post(
                "/api/ai/models/yolo-9000/activate",
                &cookie,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(module.active_model(), "nanodet-plus-m-320");
    }

    #[tokio::test]
    async fn test_activate_unavailable_model_is_409_and_keeps_old() {
        let loader: AiLoader = Arc::new(|_path: &str, _f: crate::ai::registry::Family| {
            Err(ActivateError::Unavailable("file missing".to_string()))
        });
        let (state, module) = ai_state(loader);
        let app = api_router_with("pw-test-123", state.clone());
        let (cookie, csrf) = login(&app).await;
        let res = app
            .clone()
            .oneshot(authed_post(
                "/api/ai/models/nanodet-plus-m-416/activate",
                &cookie,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        assert_eq!(json_error_code(body_json(res).await), "conflict");
        // Rollback: the old model keeps running.
        assert_eq!(module.active_model(), "nanodet-plus-m-320");
        let cfg = state.config.read().await;
        assert_eq!(cfg.features.ai.model, "nanodet-plus-m-320");
    }

    #[tokio::test]
    async fn test_activate_load_failure_is_500_and_rolls_back() {
        let loader: AiLoader = Arc::new(|_path: &str, _f: crate::ai::registry::Family| {
            Err(ActivateError::LoadFailed("bad graph".to_string()))
        });
        let (state, module) = ai_state(loader);
        let app = api_router_with("pw-test-123", state);
        let (cookie, csrf) = login(&app).await;
        let res = app
            .oneshot(authed_post(
                "/api/ai/models/nanodet-plus-m-416/activate",
                &cookie,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(module.active_model(), "nanodet-plus-m-320");
    }

    #[tokio::test]
    async fn test_capabilities_announce_ai_models_and_event() {
        let (state, _m) = ai_state(ok_loader());
        let app = api_router_with("pw-test-123", state);
        let (cookie, _csrf) = login(&app).await;
        let res = app
            .oneshot(authed_get("/api/capabilities", &cookie))
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(json["data"]["ai_models"], true);
        let events = json["data"]["events"].as_array().expect("events");
        assert!(
            events.iter().any(|e| e == "ai_model_changed"),
            "events must announce ai_model_changed"
        );

        // Without hot-swap wiring the capability stays off.
        let app = api_router("pw-test-123");
        let (cookie, _csrf) = login(&app).await;
        let res = app
            .oneshot(authed_get("/api/capabilities", &cookie))
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(json["data"]["ai_models"], false);
        let events = json["data"]["events"].as_array().expect("events");
        assert!(!events.iter().any(|e| e == "ai_model_changed"));
    }

    #[tokio::test]
    async fn test_activate_broadcasts_ai_model_changed_sse() {
        let (state, _m) = ai_state(ok_loader());
        let app = api_router_with("pw-test-123", state);
        let (cookie, csrf) = login(&app).await;

        let sse = app
            .clone()
            .oneshot(authed_get("/api/events", &cookie))
            .await
            .unwrap();
        assert_eq!(sse.status(), StatusCode::OK);
        let mut stream = sse.into_body().into_data_stream();

        let (c, t) = (cookie.clone(), csrf.clone());
        let activation = tokio::spawn(async move {
            app.clone()
                .oneshot(authed_post(
                    "/api/ai/models/nanodet-plus-m-416/activate",
                    &c,
                    &t,
                ))
                .await
                .unwrap()
        });

        use futures_util::StreamExt;
        let mut buf: Vec<u8> = Vec::new();
        let seen = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(Ok(chunk)) = stream.next().await {
                buf.extend_from_slice(&chunk);
                if String::from_utf8_lossy(&buf).contains("ai_model_changed") {
                    return true;
                }
            }
            false
        })
        .await
        .expect("SSE stream must not stall");

        assert_eq!(activation.await.unwrap().status(), StatusCode::OK);
        assert!(
            seen,
            "ai_model_changed frame must arrive, got: {}",
            String::from_utf8_lossy(&buf)
        );
        assert!(String::from_utf8_lossy(&buf).contains("\"model\":\"nanodet-plus-m-416\""));
    }

    fn json_error_code(json: serde_json::Value) -> String {
        json["error"].as_str().expect("error code").to_string()
    }

    /// State with uploads enabled and a real (temp) models dir.
    fn ai_upload_state(loader: AiLoader) -> (Arc<AppState>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "mibee-upload-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let module = Arc::new(AiModule::new(
            Arc::new(MockAiDetector::new()),
            "nanodet-plus-m-320".to_string(),
            Arc::new(std::sync::Mutex::new(None)),
            None,
            crate::config::AiFeatureConfig::default(),
        ));
        let mut s = AppState {
            ai_module: Some(Arc::clone(&module)),
            ai_loader: Some(loader),
            registry: Arc::new(std::sync::RwLock::new(crate::ai::registry::Registry::load(
                &dir,
            ))),
            ..AppState::default()
        };
        s.config.get_mut().web.password = "pw-test-123".to_string();
        s.config.get_mut().web.username = "admin".to_string();
        s.config.get_mut().features.ai.allow_upload = true;
        s.last_detections = Some(module.last_detections_arc());
        (Arc::new(s), dir)
    }

    /// Minimal multipart/form-data body: (name, value, is_file).
    fn multipart_body(fields: &[(&str, &str, bool)]) -> (String, Vec<u8>) {
        let boundary = "mibee-test-boundary";
        let mut body = Vec::new();
        for (name, value, is_file) in fields {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            if *is_file {
                body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{name}\"; filename=\"m.onnx\"\r\n\r\n"
                    )
                    .as_bytes(),
                );
                body.extend_from_slice(value.as_bytes());
            } else {
                body.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}")
                        .as_bytes(),
                );
            }
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        (format!("multipart/form-data; boundary={boundary}"), body)
    }

    fn authed_upload(
        uri: &str,
        cookie: &str,
        csrf: &str,
        ctype: &str,
        body: Vec<u8>,
    ) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .method(Method::POST)
            .header("cookie", cookie)
            .header("x-csrf-token", csrf)
            .header("content-type", ctype)
            .body(Body::from(body))
            .unwrap()
    }

    fn authed_delete(uri: &str, cookie: &str, csrf: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .method(Method::DELETE)
            .header("cookie", cookie)
            .header("x-csrf-token", csrf)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn test_upload_activate_delete_roundtrip() {
        let (state, dir) = ai_upload_state(ok_loader());
        let app = api_router_with("pw-test-123", state.clone());
        let (cookie, csrf) = login(&app).await;

        let (ctype, body) =
            multipart_body(&[("family", "yolox", false), ("file", "fake-onnx", true)]);
        let res = app
            .clone()
            .oneshot(authed_upload(
                "/api/ai/models/my-uploaded-model",
                &cookie,
                &csrf,
                &ctype,
                body,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED, "upload must 201");
        let json = body_json(res).await;
        assert_eq!(json["data"]["id"], "my-uploaded-model");
        assert_eq!(json["data"]["source"], "uploaded");
        assert_eq!(json["data"]["family"], "yolox");

        // The file and manifest landed, and the entry lists as uploaded.
        assert!(dir.join("my-uploaded-model.onnx").exists());
        assert!(dir.join("uploaded.json").exists());
        let res = app
            .clone()
            .oneshot(authed_get("/api/ai/models", &cookie))
            .await
            .unwrap();
        let json = body_json(res).await;
        let entry = json["data"]["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == "my-uploaded-model")
            .expect("uploaded entry listed");
        assert_eq!(entry["source"], "uploaded");

        // It activates like any registry model.
        let res = app
            .clone()
            .oneshot(authed_post(
                "/api/ai/models/my-uploaded-model/activate",
                &cookie,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Deleting the ACTIVE model is refused…
        let res = app
            .clone()
            .oneshot(authed_delete(
                "/api/ai/models/my-uploaded-model",
                &cookie,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);

        // …switch back, then delete succeeds and removes the file.
        let res = app
            .clone()
            .oneshot(authed_post(
                "/api/ai/models/nanodet-plus-m-320/activate",
                &cookie,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app
            .clone()
            .oneshot(authed_delete(
                "/api/ai/models/my-uploaded-model",
                &cookie,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert!(!dir.join("my-uploaded-model.onnx").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_upload_rejects_duplicates_and_bad_ids() {
        let (state, dir) = ai_upload_state(ok_loader());
        let app = api_router_with("pw-test-123", state);
        let (cookie, csrf) = login(&app).await;

        let (ctype, body) = multipart_body(&[("family", "nanodet", false), ("file", "x", true)]);
        let res = app
            .clone()
            .oneshot(authed_upload(
                "/api/ai/models/nanodet-plus-m-320",
                &cookie,
                &csrf,
                &ctype,
                body,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);

        let (ctype, body) = multipart_body(&[("family", "nanodet", false), ("file", "x", true)]);
        let res = app
            .clone()
            .oneshot(authed_upload(
                "/api/ai/models/Bad_ID!",
                &cookie,
                &csrf,
                &ctype,
                body,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_upload_validation_failure_leaves_no_trace() {
        let loader: AiLoader = Arc::new(|_path: &str, _f: crate::ai::registry::Family| {
            Err(ActivateError::LoadFailed("corrupt graph".to_string()))
        });
        let (state, dir) = ai_upload_state(loader);
        let app = api_router_with("pw-test-123", state);
        let (cookie, csrf) = login(&app).await;

        let (ctype, body) = multipart_body(&[("family", "yolox", false), ("file", "junk", true)]);
        let res = app
            .clone()
            .oneshot(authed_upload(
                "/api/ai/models/broken-model",
                &cookie,
                &csrf,
                &ctype,
                body,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(dir.read_dir().unwrap().next().is_none(), "no files left");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_upload_disabled_answers_501() {
        let (state, dir) = ai_upload_state(ok_loader());
        state.config.write().await.features.ai.allow_upload = false;
        let app = api_router_with("pw-test-123", state);
        let (cookie, csrf) = login(&app).await;
        let (ctype, body) = multipart_body(&[("family", "yolox", false), ("file", "x", true)]);
        let res = app
            .oneshot(authed_upload(
                "/api/ai/models/whatever-model",
                &cookie,
                &csrf,
                &ctype,
                body,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_delete_builtin_is_409_and_unknown_404() {
        let (state, dir) = ai_upload_state(ok_loader());
        let app = api_router_with("pw-test-123", state);
        let (cookie, csrf) = login(&app).await;
        let res = app
            .clone()
            .oneshot(authed_delete(
                "/api/ai/models/nanodet-plus-m-320",
                &cookie,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let res = app
            .oneshot(authed_delete(
                "/api/ai/models/no-such-model",
                &cookie,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        std::fs::remove_dir_all(&dir).ok();
    }

    // -- auth state machine --------------------------------------------------

    #[tokio::test]
    async fn test_me_reports_setup_required_on_first_boot() {
        let app = api_router("");
        let res = app.oneshot(authed_get("/api/auth/me", "")).await.unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        let json = body_json(res).await;
        assert_eq!(json["error"], "setup_required");
    }

    #[tokio::test]
    async fn test_setup_creates_admin_and_session() {
        let app = api_router("");
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/setup")
                    .method(Method::POST)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "username": "admin", "password": "longenough1"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().get_all("set-cookie").iter().count() >= 2);

        // me now answers 401 (session cookie not replayed here) — the
        // password being set is proven by login working next.
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/login")
                    .method(Method::POST)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "username": "admin", "password": "longenough1"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "login must work after setup");
    }

    #[tokio::test]
    async fn test_setup_rejects_short_password() {
        let app = api_router("");
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/setup")
                    .method(Method::POST)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "username": "admin", "password": "short"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_login_wrong_password_is_401() {
        let app = configured_router();
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/login")
                    .method(Method::POST)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "username": "admin", "password": "nope-nope"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let json = body_json(res).await;
        assert_eq!(json["error"], "unauthorized");
    }

    #[tokio::test]
    async fn test_gated_reads_need_session() {
        let app = configured_router();
        let res = app
            .clone()
            .oneshot(authed_get("/api/config", ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let (cookie, _csrf) = login(&app).await;
        let res = app
            .oneshot(authed_get("/api/config", &cookie))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let json = body_json(res).await;
        assert_eq!(json["ok"], serde_json::json!(true));
        assert_eq!(json["data"]["camera"]["device"], "/dev/video0");
        assert_eq!(json["data"]["web"]["port"], 8088);
    }

    #[tokio::test]
    async fn test_csrf_required_on_writes() {
        let app = configured_router();
        let (cookie, csrf) = login(&app).await;

        // Without the X-CSRF-Token header → 401.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/cameras/0/recording")
                    .method(Method::POST)
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({"active": true})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // With it → 200.
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/cameras/0/recording")
                    .method(Method::POST)
                    .header("cookie", &cookie)
                    .header("x-csrf-token", &csrf)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({"active": true})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_logout_invalidates_session() {
        let app = configured_router();
        let (cookie, _csrf) = login(&app).await;

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/logout")
                    .method(Method::POST)
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        let res = app
            .oneshot(authed_get("/api/config", &cookie))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "session must die");
    }

    // -- status / capabilities / cameras --------------------------------------

    #[tokio::test]
    async fn test_status_reports_spec_core_fields() {
        let app = configured_router();
        let (cookie, _) = login(&app).await;
        let res = app
            .oneshot(authed_get("/api/status", &cookie))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let json = body_json(res).await;
        let data = &json["data"];
        assert!(data["device_name"].is_string());
        assert!(data["model"].is_string());
        assert!(data["vendor"].is_string());
        assert!(data["firmware"].is_string());
        assert!(data["resolution"].is_string());
        assert!(data["uptime"].is_u64());
    }

    #[tokio::test]
    async fn test_capabilities_superset_shape() {
        let app = configured_router();
        let (cookie, _) = login(&app).await;
        let res = app
            .oneshot(authed_get("/api/capabilities", &cookie))
            .await
            .unwrap();
        let json = body_json(res).await;
        let data = &json["data"];
        assert_eq!(data["spec_version"], "1");
        assert_eq!(data["auth"]["model"], "session");
        // Single-camera Pi: no registry, no imaging, virtual PTZ present.
        assert_eq!(data["multi_camera"], false);
        assert_eq!(data["imaging"], false);
        assert_eq!(data["ptz"], true);
        assert_eq!(data["mse"], false, "no AuHub in test state");
        assert!(data["events"].is_array());
    }

    #[tokio::test]
    async fn test_cameras_list_is_single_element() {
        let app = configured_router();
        let (cookie, _) = login(&app).await;
        let res = app
            .oneshot(authed_get("/api/cameras", &cookie))
            .await
            .unwrap();
        let json = body_json(res).await;
        let arr = json["data"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "0");
        assert!(arr[0]["resolution"].is_string());
    }

    #[tokio::test]
    async fn test_camera_get_wrong_id_is_404() {
        let app = configured_router();
        let (cookie, _) = login(&app).await;
        let res = app
            .oneshot(authed_get("/api/cameras/9", &cookie))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let json = body_json(res).await;
        assert_eq!(json["error"], "not_found");
        assert!(json["message"].is_string(), "SPEC §0 message field");
    }

    #[tokio::test]
    async fn test_recording_flow() {
        let app = configured_router();
        let (cookie, csrf) = login(&app).await;

        let res = app
            .clone()
            .oneshot(authed_get("/api/cameras/0/recording", &cookie))
            .await
            .unwrap();
        let json = body_json(res).await;
        assert!(json["data"]["active"].is_boolean());

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/cameras/0/recording")
                    .method(Method::POST)
                    .header("cookie", &cookie)
                    .header("x-csrf-token", &csrf)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({"active": true})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let json = body_json(res).await;
        assert_eq!(json["data"]["active"], true);

        // Status reflects the toggle on the shared state.
        let res = app
            .oneshot(authed_get("/api/cameras/0/recording", &cookie))
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(json["data"]["active"], true);
    }

    // -- config ----------------------------------------------------------------

    /// SPEC §5: PUT /api/config is a PARTIAL merge — sections the client
    /// omits keep their stored values. Regression: the body used to be
    /// deserialized as a whole `Config`, so serde defaults silently reset
    /// every absent field — one partial PUT wiped the admin credentials,
    /// AI enablement and the GB28181 section on a live device.
    #[tokio::test]
    async fn test_put_config_partial_merge_preserves_absent_sections() {
        let app = configured_router();
        let (cookie, csrf) = login(&app).await;

        // Partial body: only gb28181.platform_sip_address.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/config")
                    .method(Method::PUT)
                    .header("cookie", &cookie)
                    .header("x-csrf-token", &csrf)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "gb28181": {"platform_sip_address": "192.168.1.41"}
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // The web admin must survive a gb28181-only write.
        let res = app
            .oneshot(authed_get("/api/config", &cookie))
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(
            json["data"]["web"]["password"], "****",
            "partial PUT must not reset web credentials"
        );
        assert_eq!(
            json["data"]["gb28181"]["platform_sip_address"],
            "192.168.1.41"
        );
    }

    #[tokio::test]
    async fn test_put_config_valid() {
        let app = configured_router();
        let (cookie, csrf) = login(&app).await;
        let body = serde_json::json!({
            "camera": {
                "device": "/dev/video1",
                "width": 640,
                "height": 480,
                "fps": 15,
                "codec": "h264",
                "bitrate": 1000000
            }
        });
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/config")
                    .method(Method::PUT)
                    .header("cookie", &cookie)
                    .header("x-csrf-token", &csrf)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let json = body_json(res).await;
        assert_eq!(json["data"]["applied"], "restart");
    }

    /// SPEC §5.2: the watermark subtree is part of the config document with
    /// documented defaults, and PUT merges it partially like any section.
    #[tokio::test]
    async fn test_config_watermark_section_roundtrip() {
        let app = configured_router();
        let (cookie, csrf) = login(&app).await;

        // Defaults are served.
        let res = app
            .clone()
            .oneshot(authed_get("/api/config", &cookie))
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(json["data"]["watermark"]["enabled"], false);
        assert_eq!(json["data"]["watermark"]["position"], "top-left");
        assert_eq!(json["data"]["watermark"]["font_size"], 24);
        assert_eq!(json["data"]["watermark"]["show_timestamp"], true);

        // Full section write.
        let body = serde_json::json!({
            "watermark": {
                "enabled": true,
                "text": "前门",
                "position": "bottom-right",
                "font_size": 32,
                "font_path": "/usr/local/share/fonts/NotoSansSC-Common.otf"
            }
        });
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/config")
                    .method(Method::PUT)
                    .header("cookie", &cookie)
                    .header("x-csrf-token", &csrf)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Partial write touches only the submitted key.
        let body = serde_json::json!({"watermark": {"text": "后院"}});
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/config")
                    .method(Method::PUT)
                    .header("cookie", &cookie)
                    .header("x-csrf-token", &csrf)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app
            .oneshot(authed_get("/api/config", &cookie))
            .await
            .unwrap();
        let json = body_json(res).await;
        let wm = &json["data"]["watermark"];
        assert_eq!(wm["enabled"], true, "partial PUT must not reset watermark");
        assert_eq!(wm["text"], "后院");
        assert_eq!(wm["position"], "bottom-right");
        assert_eq!(wm["font_size"], 32);
    }

    #[tokio::test]
    async fn test_put_config_watermark_invalid_is_400() {
        let app = configured_router();
        let (cookie, csrf) = login(&app).await;
        for body in [
            serde_json::json!({"watermark": {"font_size": 5}}),
            serde_json::json!({"watermark": {"enabled": true, "show_timestamp": false}}),
            serde_json::json!({"watermark": {"timestamp_format": "%y"}}),
            serde_json::json!({"watermark": {"position": "middle"}}),
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/config")
                        .method(Method::PUT)
                        .header("cookie", &cookie)
                        .header("x-csrf-token", &csrf)
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "body: {body}");
        }
    }

    #[tokio::test]
    async fn test_capabilities_announce_watermark() {
        let app = configured_router();
        let (cookie, _) = login(&app).await;
        let res = app
            .oneshot(authed_get("/api/capabilities", &cookie))
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(json["data"]["watermark"], true);
        assert_eq!(
            json["data"]["config_apply"]["sections"]["watermark"],
            "restart"
        );
    }

    #[tokio::test]
    async fn test_put_config_invalid_returns_400() {
        let app = configured_router();
        let (cookie, csrf) = login(&app).await;
        let body = serde_json::json!({
            "camera": {
                "device": "/dev/video1",
                "fps": 0,
                "width": 640,
                "height": 480,
                "codec": "h264",
                "bitrate": 1000000
            }
        });
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/config")
                    .method(Method::PUT)
                    .header("cookie", &cookie)
                    .header("x-csrf-token", &csrf)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let json = body_json(res).await;
        assert_eq!(json["error"], "bad_request");
    }

    #[tokio::test]
    async fn test_config_masks_passwords_and_roundtrips() {
        let app = configured_router();
        let (cookie, csrf) = login(&app).await;
        let res = app
            .clone()
            .oneshot(authed_get("/api/config", &cookie))
            .await
            .unwrap();
        let json = body_json(res).await;
        assert_eq!(
            json["data"]["web"]["password"], "****",
            "set password must be masked"
        );

        // PUT the masked document back: the masked secret must be restored,
        // not overwritten with literal "****" — login keeps working.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/config")
                    .method(Method::PUT)
                    .header("cookie", &cookie)
                    .header("x-csrf-token", &csrf)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json["data"].clone()).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/login")
                    .method(Method::POST)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "username": "admin", "password": "pw-test-123"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "stored secret survived");
    }

    // -- PTZ -------------------------------------------------------------------

    #[tokio::test]
    async fn test_ptz_move_updates_position() {
        let app = configured_router();
        let (cookie, csrf) = login(&app).await;

        let move_body = serde_json::json!({"pan": 0.8, "tilt": 0.2, "zoom": 2.0});
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/ptz/move")
                    .method(Method::POST)
                    .header("cookie", &cookie)
                    .header("x-csrf-token", &csrf)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&move_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app
            .oneshot(authed_get("/api/ptz/status", &cookie))
            .await
            .unwrap();
        let json = body_json(res).await;
        assert!((json["data"]["pan"].as_f64().unwrap() - 0.8).abs() < 1e-9);
        assert!((json["data"]["zoom"].as_f64().unwrap() - 2.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn test_ptz_status_returns_defaults() {
        let app = configured_router();
        let (cookie, _) = login(&app).await;
        let res = app
            .oneshot(authed_get("/api/ptz/status", &cookie))
            .await
            .unwrap();
        let json = body_json(res).await;
        assert!((json["data"]["pan"].as_f64().unwrap() - 0.5).abs() < 1e-9);
        assert!((json["data"]["tilt"].as_f64().unwrap() - 0.5).abs() < 1e-9);
        assert!((json["data"]["zoom"].as_f64().unwrap() - 1.0).abs() < 1e-9);
    }

    // -- AI detections -----------------------------------------------------------

    /// POST /api/system/restart responds immediately and fires the injected
    /// restart action after the grace period (SPEC §5.1). The real exit is
    /// wired in main.rs; this proves the endpoint contract without killing
    /// the test process.
    #[tokio::test]
    async fn test_system_restart_fires_action_and_responds() {
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        let state = Arc::new(AppState {
            restart_action: Some(Arc::new(move || {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            })),
            ..AppState::default()
        });
        let app = Router::new()
            .route("/api/system/restart", post(system_restart))
            .with_state(state);

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/system/restart")
                    .method(Method::POST)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "restart must be accepted");
        let json = body_json(res).await;
        assert_eq!(json["data"]["status"], "restarting");

        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(
            fired.load(std::sync::atomic::Ordering::SeqCst),
            "restart action must fire after the grace period"
        );
    }

    /// Capabilities advertise the restart extension so the UI can offer the
    /// one-click restart entry (SPEC §3.1 / §5.1).
    #[tokio::test]
    async fn test_capabilities_advertise_restart() {
        let app = api_router("pw-test-123");
        let (cookie, _) = login(&app).await;
        let res = app
            .oneshot(authed_get("/api/capabilities", &cookie))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let json = body_json(res).await;
        assert_eq!(json["data"]["restart"], true);
    }

    /// /api/detections must report the ACTIVE detector (`AppState::ai_model`),
    /// not the configured model path — the two diverge when the runtime falls
    /// back to a different detector than the config names (e.g. a mock or a
    /// failed ONNX load), which made the mock look like a stuck real model.
    #[tokio::test]
    async fn test_detections_report_active_detector_not_config_path() {
        let state = Arc::new(AppState {
            ai_model: Some("mock-detector-v1".to_string()),
            last_detections: Some(Arc::new(tokio::sync::RwLock::new(vec![Detection {
                label: "person".to_string(),
                confidence: 0.95,
                bbox: (100, 200, 150, 300),
            }]))),
            ..AppState::default()
        });
        state.config.write().await.features.ai.model_path =
            "/var/lib/mibee-eye/models/nanodet-m.onnx".to_string();
        let app = Router::new()
            .route("/api/detections", get(detections_handler))
            .with_state(state);

        let res = app
            .oneshot(authed_get("/api/detections", ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let json = body_json(res).await;
        assert_eq!(json["data"]["detections"].as_array().map(Vec::len), Some(1));
        assert_eq!(json["data"]["model"], "mock-detector-v1");
    }

    // -- camera media ------------------------------------------------------------

    /// Router with a synthetic 4x2 YUV420 frame so camera-dependent
    /// endpoints are deterministic without hardware.
    fn camera_router() -> Router {
        // 4x2 YUV420 planar: 8 Y + 2 U + 2 V.
        let yuv = vec![128u8; 12];
        let state = Arc::new(AppState {
            latest_yuv: Some(Arc::new(Mutex::new(Some((4, 2, yuv))))),
            ..AppState::default()
        });
        Router::new()
            .route("/api/cameras/:id/live", get(live_handler))
            .route("/api/cameras/:id/snapshot", get(snapshot_handler))
            .route("/snapshot", get(legacy_snapshot_handler))
            .with_state(state)
    }

    #[tokio::test]
    async fn test_live_returns_mjpeg() {
        let app = camera_router();
        let res = app
            .oneshot(authed_get("/api/cameras/0/live", ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let content_type = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.starts_with("multipart/x-mixed-replace"),
            "expected MJPEG content-type, got: {content_type}",
        );
    }

    /// Legacy unauthenticated `/snapshot` (SPEC appendix A, Go dialect
    /// mirrored for the rs device): NVRs and onvif-device-manager tools
    /// fetch the JPEG GetSnapshotUri advertises without a web session.
    #[tokio::test]
    async fn test_legacy_snapshot_public_jpeg() {
        let app = camera_router();
        let res = app.oneshot(authed_get("/snapshot", "")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let content_type = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(content_type, "image/jpeg");
    }

    #[tokio::test]
    async fn test_snapshot_returns_jpeg() {
        let app = camera_router();
        let res = app
            .oneshot(authed_get("/api/cameras/0/snapshot", ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let content_type = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(content_type, "image/jpeg");
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.len() > 2, "JPEG body too short");
        assert_eq!(body[0], 0xFF, "missing JPEG SOI marker byte 0");
        assert_eq!(body[1], 0xD8, "missing JPEG SOI marker byte 1");
    }

    #[tokio::test]
    async fn test_snapshot_wrong_camera_is_404() {
        let app = camera_router();
        let res = app
            .oneshot(authed_get("/api/cameras/5/snapshot", ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
}
