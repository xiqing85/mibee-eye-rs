//! Thermal extension endpoints (SPEC §3.7): radiometric readings,
//! pseudo-color snapshots and the web hardware bring-up API. All handlers
//! are session-gated by the shared `auth_gate` layer; every one of them
//! 404s when the thermal subsystem isn't running (`[thermal] enabled`),
//! keeping the negative-compat contract (unknown /api/* stays 401/404,
//! never the SPA).

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use super::api::{err_env, ok_env, AppState};
use crate::thermal::{self, ThermalService};

fn svc(state: &AppState) -> Result<Arc<ThermalService>, (StatusCode, Json<serde_json::Value>)> {
    state
        .thermal
        .clone()
        .ok_or_else(|| err_env(StatusCode::NOT_FOUND, "thermal not enabled"))
}

/// `GET /api/thermal/current[?grid=1]` — one reading (SPEC §3.7).
pub async fn thermal_current(
    State(state): State<Arc<AppState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = svc(&state)?;
    let grid = q.get("grid").map(|v| v == "1").unwrap_or(false);
    Ok(ok_env(svc.payload(grid)))
}

/// `GET /api/thermal/snapshot?format=bmp|json[&lo=&hi=&overlay=0]` —
/// pseudo-color BMP (on-image temperature overlays by default) or the
/// radiometric grid document.
pub async fn thermal_snapshot(
    State(state): State<Arc<AppState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    let svc = svc(&state)?;
    if q.get("format").map(|v| v == "bmp").unwrap_or(false) {
        let lo = q.get("lo").and_then(|v| v.parse::<f32>().ok());
        let hi = q.get("hi").and_then(|v| v.parse::<f32>().ok());
        let to = svc.grid();
        let rgb = if q.get("overlay").map(|v| v == "0").unwrap_or(false) {
            thermal::render::render_rgb_range(&to, 640, 480, lo, hi)
        } else {
            thermal::render::render_rgb_annotated(&to, 640, 480, lo, hi)
        };
        let bmp = thermal::render::rgb_to_bmp(&rgb, 640, 480);
        return Ok((StatusCode::OK, [(header::CONTENT_TYPE, "image/bmp")], bmp).into_response());
    }
    Ok(ok_env(svc.payload(true)).into_response())
}

/// `GET /api/hardware` — current target + last scan state.
pub async fn hardware_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = svc(&state)?;
    let t = svc.hardware().target();
    Ok(ok_env(serde_json::json!({
        "bus": serde_json::to_value(&t.bus).unwrap_or(serde_json::Value::Null),
        "addr": t.addr,
        "persist_path": svc.persist_path().map(|p| p.display().to_string()),
        "scan": thermal::scan_json(&svc.hardware().scan_state()),
    })))
}

/// `GET /api/hardware/scan` — poll the background sweep.
pub async fn hardware_scan_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = svc(&state)?;
    Ok(ok_env(thermal::scan_json(&svc.hardware().scan_state())))
}

#[derive(Deserialize, Default)]
pub struct ScanBody {
    /// Optional `[[sda,scl], ...]` GPIO candidate pairs; default is the
    /// classic Orange Pi Zero 3 PH2/PH3 pair (a Pi on TWI finds its sensor
    /// in the TWI sweep before GPIO pairs are touched).
    gpio_candidates: Option<Vec<(u32, u32)>>,
}

/// `POST /api/hardware/scan` — start a background sweep (409 while one
/// runs). The sensor loop yields the bus for the duration.
pub async fn hardware_scan_post(
    State(state): State<Arc<AppState>>,
    body: Option<Json<ScanBody>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = svc(&state)?;
    if svc.hardware().scan_state().running {
        return Err(err_env(StatusCode::CONFLICT, "scan already running"));
    }
    let mut cands: Vec<(u32, u32)> = vec![(226, 227)];
    if let Some(Json(ScanBody {
        gpio_candidates: Some(list),
    })) = body
    {
        if !list.is_empty() {
            cands = list;
        }
    }
    let hw = svc.hardware_arc();
    let _ = std::thread::Builder::new()
        .name("thermal-hw-scan".into())
        .spawn(move || hw.run_scan(&cands));
    Ok(ok_env(serde_json::json!({ "running": true })))
}

#[derive(Deserialize)]
pub struct HardwarePut {
    bus: crate::config::BusSpec,
    addr: u8,
}

/// `PUT /api/hardware/config` — validate, apply at runtime (next poll pass
/// uses the new wiring; a swapped module re-reads its EEPROM) and persist
/// the `thermal-hardware.toml` overlay for reboot.
pub async fn hardware_config_put(
    State(state): State<Arc<AppState>>,
    Json(put): Json<HardwarePut>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = svc(&state)?;
    if !crate::thermal::scan::ADDR_RANGE.contains(&put.addr) {
        return Err(err_env(
            StatusCode::BAD_REQUEST,
            "addr must be 0x30..=0x3E (MLX90640 strapping window)",
        ));
    }
    if let crate::config::BusSpec::Gpio { sda, scl } = &put.bus {
        if sda == scl || *sda == 0 || *scl == 0 {
            return Err(err_env(
                StatusCode::BAD_REQUEST,
                "gpio sda/scl must be distinct non-zero pins",
            ));
        }
    }
    svc.hardware().set_target(thermal::SensorTarget {
        bus: put.bus.clone(),
        addr: put.addr,
    });
    // Best-effort persistence — runtime apply already happened.
    let (persisted, persist_error) = match svc.persist_path() {
        None => (false, Some("persistence disabled".to_string())),
        Some(path) => match std::fs::write(path, thermal::overlay_toml(&put.bus, put.addr)) {
            Ok(()) => (true, None),
            Err(e) => (false, Some(format!("{e}"))),
        },
    };
    Ok(ok_env(serde_json::json!({
        "bus": serde_json::to_value(&put.bus).unwrap_or(serde_json::Value::Null),
        "addr": put.addr,
        "persisted": persisted,
        "persist_error": persist_error,
    })))
}
